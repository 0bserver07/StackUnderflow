//! `routes/forks.py` — 1 endpoint, wave 5 (batch E, the deferred remainder).
//!
//! | Item | Method | FastAPI path | axum path | State |
//! |---|---|---|---|---|
//! | `RS-5-074` | `GET` | `/api/forks` | `/api/forks` | ported |
//!
//! A thin route over `reports/forks.py::analyze_forks` — 534 lines that load
//! every scoped `messages` row (383 580 on the harness store), walk the
//! conversation DAG by `parent_uuid`, price the sidechain share, and infer
//! abandoned branches. DIV-142 deferred it out of batch D for two reasons; both
//! are answered here, and neither was a line count.
//!
//! # 1. The cache boundary, which is a contract and not an optimisation
//!
//! `analyze_forks` has no mart to lean on — a DAG is not aggregate grain — so
//! Python memoises it in a **process-wide** `dict`, and deliberately performs
//! the currency conversion *outside* the memo. Reproducing the computation
//! without reproducing that boundary would be a port of half the module:
//!
//! | | value |
//! |---|---|
//! | cached | the **raw USD** report, exactly `ForkReport.to_dict()` |
//! | cache key | `(str(deps.store_path), scope.label, tuple(sorted(project_ids)) or None)` |
//! | validity token | `(MAX(sessions.last_ts), SUM(sessions.message_count))` over the scoped sessions — NOT part of the key |
//! | applied after | the FX multiply, onto a **deep copy**, so an entry is never poisoned |
//!
//! Three consequences a paraphrase loses:
//!
//! * **The key is the scope LABEL, not the period.** `?period=week` and
//!   `?period=7days` both label `"last 7 days"`, so the second request is
//!   served the first's report — including its rolling `now − 7d` bounds. That
//!   is answer-affecting and it is Python's, so the case rows below are ordered
//!   for it.
//! * **The signature is a validity token, not a key component.** A stale entry
//!   is found, rejected and left in place until something overwrites it — the
//!   same shape `routes/optimize.rs` records under DIV-111. On a read-only
//!   harness store nothing moves it, so `week` drifts at most ONCE per label.
//! * **A currency change after a cached computation must convert, not serve
//!   stale dollars.** [`convert_report`] is a pure function of `(report, rate)`
//!   for exactly that reason, and `the_conversion_is_outside_the_cache` pins it.
//!
//! # 2. The DAG walk
//!
//! Transliterated in `services/forks.rs`, branch for branch, with unit tests on
//! hand-built fixture DAGs for the degenerate shapes: a cycle, a missing
//! parent, a self-parent, two roots, an orphan sidechain, a uuid-less child, a
//! duplicate child uuid. Every expectation was derived from the Python source,
//! not from a Rust run.
//!
//! # Cost, and why it is not a bug
//!
//! DIV-142 measured ~6 s warm and 12.7 s cold on the harness store. This
//! handler is slow for the same reason Python's is. No index, no short-circuit
//! and no mart was added: a faster answer that differs is a divergence.
//!
//! # LAW 7 clearance
//!
//! Read-only. The one `SELECT` is a join over `messages`/`sessions`/`projects`,
//! the signature query reads `sessions`, the pricing engine reads `models.toml`
//! plus the `price_book` table, and the only mutable state is this module's
//! in-process memo. Both sidecar rows are safe to execute.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use axum::Router;
use axum::extract::{RawQuery, State};
use axum::http::StatusCode;
use axum::routing::get;
use rusqlite::Connection;
use serde_json::{Map, Value};

use crate::currency::active_currency_payload;
use crate::json::{HandlerResult, HttpError, JsonBody, join_failure};
use crate::pyops::path_name;
use crate::qs::Query;
use crate::services::forks::{TOP_N, analyze_forks};
use crate::services::scope::{Instant, Scope, parse_period};
use crate::state::AppState;

/// `_PERIOD_ALIASES` — the friendly superset, in the dict literal's order.
///
/// The ORDER is load-bearing: the 400 message is `', '.join(_PERIOD_ALIASES)`,
/// which iterates keys in insertion order and is NOT sorted. Mirrors
/// `routes/yield_route.py`'s contract so the two beta tabs accept the same
/// selector values.
const PERIOD_ALIASES: [(&str, &str); 6] = [
    ("today", "today"),
    ("week", "7days"),
    ("7days", "7days"),
    ("month", "month"),
    ("30days", "30days"),
    ("all", "all"),
];

/// `', '.join(_PERIOD_ALIASES)`, written out because the join order is the
/// dict's and a `sorted()` here would be a silent byte divergence.
const PERIOD_LIST: &str = "today, week, 7days, month, 30days, all";

/// `_SUMMARY_COST_FIELDS` — the top-level dollar fields the FX pass converts.
///
/// Kept explicit so a schema change cannot silently double-convert.
const SUMMARY_COST_FIELDS: [&str; 3] =
    ["sidechain_cost_usd", "total_cost_usd", "abandoned_cost_usd"];

/// `_WARNING` — the heuristic caveat, verbatim, em-dashes and all.
const WARNING: &str = "Branch abandonment is inferred from the message DAG (parent_uuid): a fork \
whose branch stops before the session's last activity is read as dropped. Edits, retries, and tool \
re-runs all look like branches, so treat the abandoned-branch list as a signal to review, not a \
verdict.";

/// Mount this module's endpoints onto `router`.
pub fn register(router: Router<AppState>) -> Router<AppState> {
    router.route("/api/forks", get(get_forks))
}

// ── the process-wide memo ────────────────────────────────────────────────────

/// `(str(deps.store_path), scope.label, tuple(sorted(project_ids)) or None)`.
///
/// `None` (whole store) and `Some(vec![])` (a filter that matched no project)
/// are DIFFERENT keys, because `tuple(sorted([]))` is `()` and `None` is `None`.
type ForkKey = (String, String, Option<Vec<i64>>);

/// `(MAX(last_ts), SUM(message_count))` over the scoped sessions.
type ForkSignature = (Option<String>, i64);

/// `_FORK_CACHE` — a module-level `dict` with **no** size cap.
///
/// Unlike `_OPTIMIZE_CACHE` (16-entry FIFO, DIV-111) there is no trim here, so
/// a `HashMap` reproduces it exactly: nothing ever depends on insertion order.
#[derive(Debug, Default)]
struct ForkCache {
    entries: HashMap<ForkKey, (ForkSignature, Value)>,
}

/// The process-wide cache — Python's module-level `dict` behind
/// `_FORK_CACHE_LOCK`.
fn cache() -> &'static Mutex<ForkCache> {
    static CACHE: OnceLock<Mutex<ForkCache>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(ForkCache::default()))
}

fn lock_error(err: &impl std::fmt::Display) -> HttpError {
    HttpError::new(
        StatusCode::INTERNAL_SERVER_ERROR,
        format!("cache lock: {err}"),
    )
}

/// `_fork_signature(conn, project_ids)`.
///
/// `project_ids is None` is the whole store, so the signature spans every
/// session. Any ingest that writes a message bumps `last_ts`/`message_count`,
/// which changes the signature and forces a recompute — the self-invalidation
/// contract `routes/cost.py` relies on.
///
/// A bad store returns the sentinel `(None, -1)`. Python's comment calls that
/// "simply misses the cache", which is true only for a store that was healthy
/// when the entry was written: a *consistently* failing signature query stores
/// `(None, -1)` and then matches it. Reproduced as written.
fn fork_signature(conn: &Connection, project_ids: Option<&[i64]>) -> ForkSignature {
    let row = match project_ids {
        None => conn.query_row(
            "SELECT MAX(last_ts) AS max_ts, COALESCE(SUM(message_count), 0) AS n FROM sessions",
            [],
            |row| {
                Ok((
                    row.get::<_, Option<String>>(0)?,
                    row.get::<_, Option<i64>>(1)?,
                ))
            },
        ),
        // `elif not project_ids: return (None, 0)`.
        Some([]) => return (None, 0),
        Some(ids) => {
            let sql = format!(
                "SELECT MAX(last_ts) AS max_ts, COALESCE(SUM(message_count), 0) AS n \
                 FROM sessions WHERE project_id IN ({})",
                vec!["?"; ids.len()].join(",")
            );
            conn.query_row(&sql, rusqlite::params_from_iter(ids.iter()), |row| {
                Ok((
                    row.get::<_, Option<String>>(0)?,
                    row.get::<_, Option<i64>>(1)?,
                ))
            })
        }
    };
    match row {
        // `int(row["n"] or 0)`.
        Ok((max_ts, n)) => (max_ts, n.unwrap_or(0)),
        // `except Exception: return (None, -1)` — advisory, never raises.
        Err(rusqlite::Error::QueryReturnedNoRows) => (None, 0),
        Err(_) => (None, -1),
    }
}

/// `_analyze_forks_cached` — read-through cache around `analyze_forks`.
///
/// Returns the **USD** report. The clone on read is `copy.deepcopy`: the caller
/// converts currency in place and must not poison the shared entry.
///
/// The lock is released before the recompute and re-taken to store, exactly as
/// Python's two `with _FORK_CACHE_LOCK:` blocks do — a concurrent second miss
/// computes twice and the later writer wins, which is Python's behaviour too.
fn analyze_forks_cached(
    state: &AppState,
    conn: &Connection,
    scope: &Scope,
    project_ids: Option<&[i64]>,
) -> Result<Value, HttpError> {
    let sig = fork_signature(conn, project_ids);
    let key: ForkKey = (
        state.store_path().display().to_string(),
        scope.label.clone(),
        project_ids.map(|ids| {
            let mut sorted = ids.to_vec();
            sorted.sort_unstable();
            sorted
        }),
    );
    let cached = cache()
        .lock()
        .map_err(|err| lock_error(&err))?
        .entries
        .get(&key)
        .filter(|(cached_sig, _)| *cached_sig == sig)
        .map(|(_, report)| report.clone());
    if let Some(report) = cached {
        return Ok(report);
    }

    // LAW 2: the engine is built from the live connection's `price_book`, never
    // from `default_engine()`'s bare manifest — a silent 2% error (DIV-056) on
    // the one module whose job is attributing dollars to sidechains. Built here
    // rather than by the caller so a cache HIT does not pay for it.
    let engine = crate::pricing::engine(conn, state.package_dir())
        .map_err(|err| HttpError::new(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))?;
    let report = analyze_forks(conn, Some(scope), project_ids, TOP_N, &engine);
    cache()
        .lock()
        .map_err(|err| lock_error(&err))?
        .entries
        .insert(key, (sig, report.clone()));
    Ok(report)
}

// ── GET /api/forks ───────────────────────────────────────────────────────────

/// `_PERIOD_ALIASES.get(period)`.
fn period_spec(period: &str) -> Option<&'static str> {
    PERIOD_ALIASES
        .iter()
        .find(|(alias, _)| *alias == period)
        .map(|(_, spec)| *spec)
}

/// `_project_ids_for` — a log path's slug → the `projects.id` list.
///
/// This route owns its resolver (no `store/queries.py` dependency): a plain
/// slug lookup guarded so a missing project yields an EMPTY scope rather than a
/// 404. That is the opposite of `routes/cost.rs::project_ids_for`, which 404s —
/// same query, different contract, and the difference is the whole reason
/// `/api/forks` answers 200 for an unknown project.
fn project_ids_for(conn: &Connection, path: &str) -> Vec<i64> {
    let slug = path_name(path);
    // `except Exception: return []` — advisory route, never 500 on a bad store.
    let Ok(mut stmt) = conn.prepare("SELECT id FROM projects WHERE slug = ?") else {
        return Vec::new();
    };
    let Ok(rows) = stmt.query_map([&slug], |row| row.get::<_, i64>(0)) else {
        return Vec::new();
    };
    rows.collect::<rusqlite::Result<Vec<i64>>>()
        .unwrap_or_default()
}

/// `if rate != 1.0:` — the FX pass, applied to the report AFTER the memo.
///
/// Two field sets: [`SUMMARY_COST_FIELDS`] on the report itself, then
/// `cost_usd` on every entry of `abandoned_branches`. `float(report[k])` is
/// explicit in Python, so the `int 0` that `sum([])` produces for
/// `abandoned_cost_usd` becomes a **float** the moment any conversion happens.
///
/// UNREACHABLE over HTTP today: `crate::currency::active_currency_payload` only
/// resolves USD and returns `rate_from_usd = 1.0` (DIV-052). It is written
/// anyway — and not left as a comment the way `routes/optimize.rs` left
/// `_convert_routing` (DIV-112) — because here the branch is what proves the
/// cache boundary, and a pure `(report, rate)` function is directly testable
/// without the unported Frankfurter chain.
fn convert_report(mut report: Value, rate: f64) -> Value {
    if rate == 1.0 {
        return report;
    }
    let Value::Object(obj) = &mut report else {
        return report;
    };
    for key in SUMMARY_COST_FIELDS {
        // `if k in report:` — a present key only.
        if let Some(slot) = obj.get_mut(key) {
            let converted = slot.as_f64().unwrap_or(0.0) * rate;
            *slot = stax_etl::stats::aggregator::jf(converted);
        }
    }
    // `report.get("abandoned_branches", [])` — a non-list is left alone.
    if let Some(Value::Array(branches)) = obj.get_mut("abandoned_branches") {
        for branch in branches.iter_mut() {
            if let Value::Object(branch) = branch {
                // `float(branch.get("cost_usd", 0.0)) * rate` — a MISSING key
                // is 0.0 and the key is then CREATED at the end of the dict.
                let converted = branch
                    .get("cost_usd")
                    .and_then(Value::as_f64)
                    .unwrap_or(0.0)
                    * rate;
                branch.insert(
                    "cost_usd".to_owned(),
                    stax_etl::stats::aggregator::jf(converted),
                );
            }
        }
    }
    report
}

/// `get_forks(period, log_path)` → `{period, scope, report, currency, warning}`.
async fn get_forks(State(state): State<AppState>, RawQuery(raw): RawQuery) -> HandlerResult {
    let query = Query::parse(raw.as_deref().unwrap_or_default());
    // `period: str = Query("all")` — a PRESENT-but-empty `?period=` is `""`,
    // not the default, so it falls through to the 400 below. That is NOT the
    // `/api/compare?provider=` trap DIV-086 recorded: there an empty string
    // reached a filter and pruned everything; here it reaches a lookup table
    // that has no `""` key and answers a deterministic 400.
    let period = query.str_or("period", "all").to_owned();
    let Some(spec) = period_spec(&period) else {
        return Err(HttpError::bad_request(format!(
            "Invalid period '{period}'. Valid: {PERIOD_LIST}"
        )));
    };
    // `week` is a ROLLING `now - 7d` INSTANT — `services::scope::parse_period`
    // owns that arithmetic and is not re-derived here. Unreachable `Err`: the
    // alias table already rejected everything it raises on.
    let scope = parse_period(spec, Instant::now_utc()).map_err(HttpError::bad_request)?;

    // `log_path if isinstance(log_path, str) else None` then
    // `log_path_str or deps.current_log_path` — an empty `?log_path=` is falsy
    // and falls back to the selected project.
    let from_query = query.get("log_path").unwrap_or_default().to_owned();
    let path = if from_query.is_empty() {
        state.current_project().log_path.unwrap_or_default()
    } else {
        from_query
    };

    let worker = state.clone();
    let worker_scope = scope.clone();
    let report = tokio::task::spawn_blocking(move || forks_report(&worker, &worker_scope, &path))
        .await
        .map_err(|err| join_failure(&err))??;

    let currency = active_currency_payload(&state.config().currency)
        .map_err(|err| HttpError::new(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))?;
    let rate = currency
        .get("rate_from_usd")
        .and_then(Value::as_f64)
        .unwrap_or(1.0);
    let report = convert_report(report, rate);

    let mut payload = Map::new();
    payload.insert("period".to_owned(), Value::from(period));
    payload.insert("scope".to_owned(), Value::from(scope.label));
    payload.insert("report".to_owned(), report);
    payload.insert("currency".to_owned(), currency);
    payload.insert("warning".to_owned(), Value::from(WARNING));
    Ok(JsonBody::ok(Value::Object(payload)))
}

/// The blocking body: open the store, resolve the scope, answer from the memo.
fn forks_report(state: &AppState, scope: &Scope, path: &str) -> Result<Value, HttpError> {
    let conn = state
        .connect()
        .map_err(|err| HttpError::new(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))?;
    // `project_ids = _project_ids_for(conn, path) if path else None` — no path
    // at all is the whole store; a path that resolves to nothing is an EMPTY
    // list, which scopes to nothing rather than widening back to the store.
    let project_ids: Option<Vec<i64>> = if path.is_empty() {
        None
    } else {
        Some(project_ids_for(&conn, path))
    };
    analyze_forks_cached(state, &conn, scope, project_ids.as_deref())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt as _;

    use super::*;
    use crate::state::Config;

    fn sample_report() -> Value {
        let mut branch = Map::new();
        branch.insert("branch_head_uuid".to_owned(), Value::from("b"));
        branch.insert("cost_usd".to_owned(), Value::from(2.5));
        let mut obj = Map::new();
        obj.insert("sidechain_cost_usd".to_owned(), Value::from(1.5));
        obj.insert("total_cost_usd".to_owned(), Value::from(10.0));
        // The `int` zero `sum([])` produces — the conversion must FLOAT it.
        obj.insert("abandoned_cost_usd".to_owned(), Value::from(0));
        obj.insert(
            "abandoned_branches".to_owned(),
            Value::Array(vec![Value::Object(branch)]),
        );
        Value::Object(obj)
    }

    #[test]
    fn the_alias_table_maps_the_friendly_names_onto_parse_period_specs() {
        assert_eq!(period_spec("week"), Some("7days"));
        assert_eq!(period_spec("7days"), Some("7days"));
        assert_eq!(period_spec("today"), Some("today"));
        assert_eq!(period_spec("month"), Some("month"));
        assert_eq!(period_spec("30days"), Some("30days"));
        assert_eq!(period_spec("all"), Some("all"));
        // The 400 legs: unknown, empty, and case-shifted.
        assert_eq!(period_spec("nonsense"), None);
        assert_eq!(period_spec(""), None);
        assert_eq!(period_spec("Week"), None);
    }

    #[test]
    fn the_400_lists_the_aliases_in_dict_order_not_sorted_order() {
        // `', '.join(_PERIOD_ALIASES)` — insertion order. Sorted order would be
        // "30days, 7days, all, month, today, week", which is a different body.
        let joined = PERIOD_ALIASES
            .iter()
            .map(|(alias, _)| *alias)
            .collect::<Vec<_>>()
            .join(", ");
        assert_eq!(joined, PERIOD_LIST);
        assert_ne!(joined, "30days, 7days, all, month, today, week");
        let err =
            HttpError::bad_request(format!("Invalid period 'nonsense'. Valid: {PERIOD_LIST}"));
        assert_eq!(
            err.body().render(),
            r#"{"detail":"Invalid period 'nonsense'. Valid: today, week, 7days, month, 30days, all"}"#
        );
    }

    #[test]
    fn week_and_7days_share_one_cache_key_because_the_key_is_the_label() {
        let now = Instant::from_parts(2026, 7, 31, 12, 34, 56, 789_012);
        let week = parse_period(period_spec("week").expect("alias"), now).expect("spec");
        let seven = parse_period(period_spec("7days").expect("alias"), now).expect("spec");
        assert_eq!(week.label, "last 7 days");
        assert_eq!(week.label, seven.label);
        // …and `today` / `month` / `all` do not collide with them.
        let today = parse_period("today", now).expect("spec");
        let month = parse_period("month", now).expect("spec");
        let all = parse_period("all", now).expect("spec");
        assert_eq!(today.label, "today");
        assert_eq!(month.label, "this month (July 2026)");
        assert_eq!(all.label, "all time");
    }

    #[test]
    fn the_key_separates_the_whole_store_from_a_filter_that_matched_nothing() {
        let none: Option<Vec<i64>> = None;
        let empty: Option<Vec<i64>> = Some(Vec::new());
        assert_ne!(none, empty, "`None` and `()` are different Python keys");
        // …and the id list is SORTED into the key, so two orders are one key.
        let mut a = vec![3_i64, 1, 2];
        a.sort_unstable();
        let mut b = vec![2_i64, 3, 1];
        b.sort_unstable();
        assert_eq!(a, b);
    }

    #[test]
    fn a_moved_signature_misses_and_leaves_the_stale_entry_in_place() {
        let mut cache = ForkCache::default();
        let key: ForkKey = ("/store.db".to_owned(), "all time".to_owned(), None);
        let old: ForkSignature = (Some("2026-07-01T00:00:00+00:00".to_owned()), 10);
        let moved: ForkSignature = (Some("2026-07-02T00:00:00+00:00".to_owned()), 11);
        cache
            .entries
            .insert(key.clone(), (old.clone(), Value::from("cached")));

        let hit = cache
            .entries
            .get(&key)
            .filter(|(sig, _)| *sig == old)
            .map(|(_, report)| report.clone());
        assert_eq!(hit, Some(Value::from("cached")));

        let miss = cache
            .entries
            .get(&key)
            .filter(|(sig, _)| *sig == moved)
            .map(|(_, report)| report.clone());
        assert_eq!(miss, None, "the signature moved");
        assert_eq!(cache.entries.len(), 1, "…and the entry is NOT evicted");
    }

    #[test]
    fn the_conversion_is_outside_the_cache() {
        // THE cache-boundary test. A cached USD report converted at a NEW rate
        // must produce converted dollars, and the cached entry must be
        // untouched — `copy.deepcopy` on read is what makes that true.
        let mut cache = ForkCache::default();
        let key: ForkKey = ("/store.db".to_owned(), "all time".to_owned(), None);
        let sig: ForkSignature = (None, 0);
        cache.entries.insert(key.clone(), (sig, sample_report()));

        let read = || {
            cache
                .entries
                .get(&key)
                .map(|(_, report)| report.clone())
                .expect("cached")
        };

        // First reader: USD, so nothing moves.
        let usd = convert_report(read(), 1.0);
        assert_eq!(usd["total_cost_usd"], Value::from(10.0));
        assert_eq!(usd["abandoned_cost_usd"], Value::from(0));

        // Second reader, FX rate now 2.0. The SAME cached entry converts —
        // it does not serve the stale dollars the first reader saw.
        let eur = convert_report(read(), 2.0);
        assert_eq!(eur["sidechain_cost_usd"], Value::from(3.0));
        assert_eq!(eur["total_cost_usd"], Value::from(20.0));
        // `float(report[k]) * rate` floats the `int` zero on the way through.
        assert_eq!(
            stax_memory::pyjson::dumps_http(&eur["abandoned_cost_usd"]),
            "0.0"
        );
        assert_eq!(eur["abandoned_branches"][0]["cost_usd"], Value::from(5.0));

        // …and the cached value is still the raw USD report.
        let after = read();
        assert_eq!(after["total_cost_usd"], Value::from(10.0));
        assert_eq!(after["abandoned_branches"][0]["cost_usd"], Value::from(2.5));
        assert_eq!(
            stax_memory::pyjson::dumps_http(&after["abandoned_cost_usd"]),
            "0",
            "the memo holds the int zero, unconverted"
        );

        // A third reader back at 1.0 gets the original bytes, not 2× anything.
        let back = convert_report(read(), 1.0);
        assert_eq!(back["total_cost_usd"], Value::from(10.0));
    }

    #[test]
    fn the_conversion_touches_exactly_the_named_fields() {
        let converted = convert_report(sample_report(), 3.0);
        let obj = converted.as_object().expect("object");
        // The three summary fields plus every branch `cost_usd` — and the key
        // ORDER is unchanged, because each is an in-place overwrite.
        assert_eq!(
            obj.keys().map(String::as_str).collect::<Vec<_>>(),
            vec![
                "sidechain_cost_usd",
                "total_cost_usd",
                "abandoned_cost_usd",
                "abandoned_branches"
            ]
        );
        assert_eq!(converted["sidechain_cost_usd"], Value::from(4.5));
        // A field the list does not name is never multiplied — proven by the
        // branch's non-cost key surviving verbatim.
        assert_eq!(
            converted["abandoned_branches"][0]["branch_head_uuid"],
            Value::from("b")
        );
    }

    #[test]
    fn a_missing_project_resolves_to_an_empty_scope_and_not_a_404() {
        let conn = Connection::open_in_memory().expect("in-memory");
        conn.execute_batch(
            "CREATE TABLE projects (id INTEGER PRIMARY KEY, slug TEXT);
             INSERT INTO projects VALUES (4, 'known'), (5, 'known');",
        )
        .expect("schema");
        // Two rows per slug is normal: one `projects` row PER PROVIDER.
        assert_eq!(project_ids_for(&conn, "/home/u/known"), vec![4, 5]);
        assert!(project_ids_for(&conn, "/home/u/nope").is_empty());
        // A store with no `projects` relation at all: `[]`, never a 500.
        let bare = Connection::open_in_memory().expect("in-memory");
        assert!(project_ids_for(&bare, "/home/u/known").is_empty());
    }

    #[test]
    fn the_signature_is_the_scoped_max_last_ts_and_summed_message_count() {
        let conn = Connection::open_in_memory().expect("in-memory");
        conn.execute_batch(
            "CREATE TABLE sessions (id INTEGER PRIMARY KEY, project_id INTEGER,
                                    last_ts TEXT, message_count INTEGER);
             INSERT INTO sessions VALUES
                 (1, 4, '2026-07-01T00:00:00+00:00', 10),
                 (2, 4, '2026-07-09T00:00:00+00:00', 5),
                 (3, 9, '2026-07-20T00:00:00+00:00', 100);",
        )
        .expect("schema");
        assert_eq!(
            fork_signature(&conn, None),
            (Some("2026-07-20T00:00:00+00:00".to_owned()), 115)
        );
        assert_eq!(
            fork_signature(&conn, Some(&[4])),
            (Some("2026-07-09T00:00:00+00:00".to_owned()), 15)
        );
        // A filter that matched no project short-circuits without a query.
        assert_eq!(fork_signature(&conn, Some(&[])), (None, 0));
        // A project with no sessions: `MAX` is NULL and `COALESCE(SUM…)` is 0.
        assert_eq!(fork_signature(&conn, Some(&[77])), (None, 0));
        // A store with no `sessions` relation is the advisory sentinel.
        let bare = Connection::open_in_memory().expect("in-memory");
        assert_eq!(fork_signature(&bare, None), (None, -1));
    }

    // ── the mounted route, driven in-process ────────────────────────────────

    /// A scratch `STACKUNDERFLOW_HOME` that cleans itself up.
    ///
    /// One per test, which also keeps the PROCESS-WIDE memo from leaking
    /// between them: the store path is the first element of the cache key.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "stax-forks-{tag}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |delta| delta.as_nanos())
            ));
            std::fs::create_dir_all(&dir).expect("mkdir");
            Self(dir)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).ok();
        }
    }

    /// One project, one session, one fork whose loser is free — so every dollar
    /// figure is a hard zero and the whole envelope is byte-assertable without
    /// pinning a rate card.
    fn seeded_state(scratch: &Scratch) -> AppState {
        let store = scratch.0.join("store.db");
        let conn = Connection::open(&store).expect("open");
        conn.execute_batch(
            "CREATE TABLE projects (id INTEGER PRIMARY KEY, slug TEXT, provider TEXT);
             CREATE TABLE sessions (id INTEGER PRIMARY KEY, project_id INTEGER,
                                    session_id TEXT, last_ts TEXT, message_count INTEGER);
             CREATE TABLE messages (
                 id INTEGER PRIMARY KEY, session_fk INTEGER, seq INTEGER, timestamp TEXT,
                 role TEXT, model TEXT, input_tokens INTEGER, output_tokens INTEGER,
                 cache_create_tokens INTEGER, cache_read_tokens INTEGER,
                 is_sidechain INTEGER, uuid TEXT, parent_uuid TEXT, speed TEXT);
             INSERT INTO projects VALUES (1, '-p-one', 'anthropic');
             INSERT INTO sessions VALUES (7, 1, 'sess-a', '2026-07-01T09:00:00+00:00', 3);
             INSERT INTO messages VALUES
                 (1, 7, 1, '2026-07-01T00:00:00+00:00', 'user', NULL, 0,0,0,0, 0, 'a', NULL, NULL),
                 (2, 7, 2, '2026-07-01T01:00:00+00:00', 'user', NULL, 0,0,0,0, 1, 'b', 'a',  NULL),
                 (3, 7, 3, '2026-07-01T09:00:00+00:00', 'user', NULL, 0,0,0,0, 0, 'c', 'a',  NULL);",
        )
        .expect("seed");
        drop(conn);
        // The pricing engine needs the SHIPPED manifest — LAW 2's engine is
        // built from it plus the store's `price_book`, so `package_dir` cannot
        // be the scratch dir here the way it can for a route that never prices.
        let package = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../stackunderflow");
        AppState::new(store, package, Config::default())
    }

    /// Drive the mounted route in-process — no port, so nothing can collide
    /// with the reserved `:8095` / `:8096`.
    async fn call(state: &AppState, target: &str) -> (StatusCode, String) {
        let app = register(Router::new()).with_state(state.clone());
        let response = app
            .oneshot(
                Request::builder()
                    .uri(target)
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .expect("body");
        (status, String::from_utf8(bytes.to_vec()).expect("utf-8"))
    }

    /// The whole 200 body for [`seeded_state`], with `scope` substituted.
    fn expected(period: &str, scope: &str) -> String {
        format!(
            r#"{{"period":"{period}","scope":"{scope}","report":{{"sidechain_message_count":1,"sidechain_cost_usd":0.0,"sidechain_token_total":0,"total_cost_usd":0.0,"total_token_total":0,"total_message_count":3,"sidechain_cost_share":0.0,"sidechain_token_share":0.0,"fork_point_count":1,"abandoned_branch_count":0,"abandoned_cost_usd":0,"abandoned_branches":[]}},"currency":{{"code":"USD","symbol":"$","rate_from_usd":1.0,"warning":null}},"warning":"Branch abandonment is inferred from the message DAG (parent_uuid): a fork whose branch stops before the session's last activity is read as dropped. Edits, retries, and tool re-runs all look like branches, so treat the abandoned-branch list as a signal to review, not a verdict."}}"#
        )
    }

    #[tokio::test]
    async fn the_default_period_is_all_and_the_envelope_is_the_five_keys() {
        let scratch = Scratch::new("default");
        let state = seeded_state(&scratch);
        let (status, body) = call(&state, "/api/forks").await;
        assert_eq!(status, StatusCode::OK);
        // `period` echoes the DEFAULT string "all", and `scope` is the LABEL.
        // Note `"abandoned_cost_usd":0` — the `sum([])` int beside three float
        // zeros, which is the finding this whole module turns on.
        assert_eq!(body, expected("all", "all time"));

        // The second identical request is served from the memo and must be
        // byte-identical: unlike `/api/optimize` there is no `"cache"` field,
        // so the memo is not answer-visible.
        let (status, again) = call(&state, "/api/forks?period=all").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(again, expected("all", "all time"));
    }

    #[tokio::test]
    async fn an_unknown_project_is_a_200_with_the_defaults_report_not_a_404() {
        let scratch = Scratch::new("noproject");
        let state = seeded_state(&scratch);
        let (status, body) = call(&state, "/api/forks?log_path=/x/no-such-project").await;
        assert_eq!(status, StatusCode::OK);
        // `[]` scopes to NOTHING, so this is `ForkReport()`'s defaults — and
        // `abandoned_cost_usd` is the float `0.0` here, not the int `0`.
        assert!(
            body.contains(
                r#""report":{"sidechain_message_count":0,"sidechain_cost_usd":0.0,"sidechain_token_total":0,"total_cost_usd":0.0,"total_token_total":0,"total_message_count":0,"sidechain_cost_share":0.0,"sidechain_token_share":0.0,"fork_point_count":0,"abandoned_branch_count":0,"abandoned_cost_usd":0.0,"abandoned_branches":[]}"#
            ),
            "{body}"
        );
        // An EMPTY `?log_path=` is falsy and falls back — here to no selected
        // project at all, so it is the whole store and NOT the empty scope.
        let (status, fallback) = call(&state, "/api/forks?log_path=").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(fallback, expected("all", "all time"));
    }

    #[tokio::test]
    async fn every_valid_period_answers_200_with_its_own_label() {
        let scratch = Scratch::new("periods");
        let state = seeded_state(&scratch);
        // `today` / `month` exclude the July-2026 fixture unless the clock says
        // otherwise, so only the LABEL is asserted for those two.
        for (period, label) in [("week", "last 7 days"), ("30days", "last 30 days")] {
            let (status, body) = call(&state, &format!("/api/forks?period={period}")).await;
            assert_eq!(status, StatusCode::OK, "{period}");
            assert!(
                body.starts_with(&format!(r#"{{"period":"{period}","scope":"{label}","#)),
                "{body}"
            );
        }
        // `7days` shares `week`'s LABEL and therefore its cache entry — the
        // `period` field still echoes what was asked for, because it is stamped
        // outside the memo.
        let (status, body) = call(&state, "/api/forks?period=7days").await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            body.starts_with(r#"{"period":"7days","scope":"last 7 days","#),
            "{body}"
        );
        for period in ["today", "month"] {
            let (status, _) = call(&state, &format!("/api/forks?period={period}")).await;
            assert_eq!(status, StatusCode::OK, "{period}");
        }
    }

    #[tokio::test]
    async fn the_three_bad_period_legs_are_one_400_shape() {
        let scratch = Scratch::new("badperiod");
        let state = seeded_state(&scratch);
        for (target, shown) in [
            ("/api/forks?period=nonsense", "nonsense"),
            ("/api/forks?period=", ""),
            ("/api/forks?period=Week", "Week"),
        ] {
            let (status, body) = call(&state, target).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{target}");
            assert_eq!(
                body,
                format!(
                    r#"{{"detail":"Invalid period '{shown}'. Valid: today, week, 7days, month, 30days, all"}}"#
                ),
                "{target}"
            );
        }
        // Last-wins on a repeated scalar param: the VALID second value decides.
        let (status, _) = call(&state, "/api/forks?period=nonsense&period=all").await;
        assert_eq!(status, StatusCode::OK);
    }
}
