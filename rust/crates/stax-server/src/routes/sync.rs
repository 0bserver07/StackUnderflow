//! `routes/sync.py` — 2 endpoints, wave 5 (batch D).
//!
//! | Item | Method | FastAPI path | axum path | State |
//! |---|---|---|---|---|
//! | `RS-5-108` | `GET` | `/api/sync/status  ` | `/api/sync/status`   | **open** — DIV-133 |
//! | `RS-5-109` | `GET` | `/api/sync/overview` | `/api/sync/overview` | ported |
//!
//! # `/api/sync/overview` — the default leg is the point
//!
//! One path, two endpoints. `?scope` defaults to anything-but-`all-devices`, and
//! that leg returns a four-key stub **without running a single union query** — a
//! sync-off store behaves as if the feature were absent. It is also completely
//! deterministic, so it is a green parity row rather than a shrug.
//!
//! The `all-devices` leg is ported too, and eleven lines of `merge.py` carry two
//! of this campaign's named traps:
//!
//! * **`totals["cost_usd"]` is `sum(…)` over a generator — Neumaier-compensated
//!   (DIV-057)** — while `by_day`'s costs accumulate with `+=` four lines later.
//!   Each is reproduced with the operation Python used. They are not
//!   interchangeable, and "more accurate" is a divergence.
//! * **`sum([])` is the `int` `0`, not `0.0`.** With no rows at all the totals
//!   block renders `"cost_usd":0` — an integer — while `by_day`'s buckets, which
//!   start at a literal `0.0`, stay floats however empty they are. [`PyNum`]
//!   carries that distinction to the writer instead of flattening it.
//!
//! Its one non-deterministic field is `generated_at` (`datetime.now(UTC)`), so
//! `Y-overview-all` is a `!` row whose diff can only ever be that timestamp —
//! which is itself the evidence that every field before it agreed.
//!
//! # `/api/sync/status` — DIV-133, deferred with its reason
//!
//! `runner.status()` is not a status read. On a store carrying a `sync_identity`
//! row — which the harness home does — it calls `serialize.build_shards(conn)`
//! (`sync/serialize.py`, 227 lines): re-serialise every mart into shard
//! documents, content-hash each one, diff against `sync_outbox`. That is a
//! byte-exact canonicalisation-and-hash port, an order of magnitude more work
//! than the route it feeds, and it belongs to whichever wave ports the sync
//! *writer*. The endpoint also stamps `scanned_at = datetime.now(UTC)`, so even
//! a perfect port could not produce a green row.
//!
//! Left unmounted, so Rust 404s and `!Y-status` reports it every run. Safe to
//! execute: both sides only ever read.

use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::Router;
use axum::extract::{RawQuery, State};
use axum::http::StatusCode;
use axum::routing::get;
use rusqlite::Connection;
use rusqlite::types::ValueRef;
use serde_json::{Map, Value};
use stax_etl::stats::aggregator::{Neumaier, PyNum};

use crate::currency::active_currency_payload;
use crate::json::{HandlerResult, HttpError, JsonBody, join_failure};
use crate::qs::Query;
use crate::services::mart_queries::table_exists;
use crate::state::AppState;
use stax_etl::stats::pydatetime::civil_from_epoch;

/// Mount this module's endpoints onto `router`.
///
/// `/api/sync/status` is deliberately absent — see the module docs (DIV-133).
pub fn register(router: Router<AppState>) -> Router<AppState> {
    router.route("/api/sync/overview", get(get_sync_overview))
}

// ── GET /api/sync/overview ───────────────────────────────────────────────────

async fn get_sync_overview(
    State(state): State<AppState>,
    RawQuery(raw): RawQuery,
) -> HandlerResult {
    let query = Query::parse(raw.as_deref().unwrap_or_default());
    // `scope: str = Query("this-device", …)` then `scope if isinstance(scope,
    // str) else "this-device"` — the second guard only fires when the handler is
    // called directly from a test, never over HTTP.
    let scope = query.str_or("scope", "this-device").to_owned();

    let worker = state.clone();
    let merged =
        tokio::task::spawn_blocking(move || -> Result<Option<Map<String, Value>>, HttpError> {
            let conn = worker.connect().map_err(|err| any_500(&err))?;
            let enabled = is_enabled(&conn).map_err(sql_500)?;
            if scope != "all-devices" || !enabled {
                // The DEFAULT leg. No union runs; the payload's only store
                // dependency is the existence check above.
                return Ok(None);
            }
            Ok(Some(merged_overview(&conn).map_err(sql_500)?))
        })
        .await
        .map_err(|err| join_failure(&err))??;

    let Some(mut payload) = merged else {
        // `enabled` is recomputed here only in the sense that the worker already
        // returned it inside the stub; keep the stub construction next to the
        // literal it mirrors.
        let enabled = sync_enabled_flag(&state).await?;
        return Ok(JsonBody::ok(Value::Object(this_device_stub(enabled))));
    };

    // `_apply_currency(payload, currency)` returns immediately at rate 1.0,
    // which is the only rate the port resolves (DIV-052) — nothing to scale.
    let currency = active_currency_payload(&state.config().currency)
        .map_err(|err| HttpError::new(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))?;
    payload.insert("scope".to_owned(), Value::from("all-devices"));
    payload.insert("merged".to_owned(), Value::Bool(true));
    // A literal `True`, not the computed flag: this branch is only reachable
    // when sync IS enabled, and Python writes the constant.
    payload.insert("sync_enabled".to_owned(), Value::Bool(true));
    payload.insert("currency".to_owned(), currency);
    payload.insert("generated_at".to_owned(), Value::from(now_iso()));
    Ok(JsonBody::ok(Value::Object(payload)))
}

/// Re-read `runner.is_enabled` for the stub leg, off the event loop.
async fn sync_enabled_flag(state: &AppState) -> Result<bool, HttpError> {
    let worker = state.clone();
    tokio::task::spawn_blocking(move || -> Result<bool, HttpError> {
        let conn = worker.connect().map_err(|err| any_500(&err))?;
        is_enabled(&conn).map_err(sql_500)
    })
    .await
    .map_err(|err| join_failure(&err))?
}

/// The default `this-device` payload — four keys, in the literal's order.
fn this_device_stub(enabled: bool) -> Map<String, Value> {
    let mut payload = Map::new();
    payload.insert("scope".to_owned(), Value::from("this-device"));
    payload.insert("merged".to_owned(), Value::Bool(false));
    payload.insert("sync_enabled".to_owned(), Value::Bool(enabled));
    payload.insert(
        "hint".to_owned(),
        Value::from("pass ?scope=all-devices to union pulled peers"),
    );
    payload
}

/// `runner.is_enabled` — `load_identity(conn) is not None`.
///
/// The table-existence guard has no Python counterpart (the reference would
/// raise `OperationalError` on a store with no sync schema, which the server's
/// lifespan `schema.apply` makes unreachable). It is here because the port must
/// not be the reason a fixture store 500s, and it cannot change the answer on
/// any store the reference can serve.
fn is_enabled(conn: &Connection) -> rusqlite::Result<bool> {
    if !table_exists(conn, "sync_identity")? {
        return Ok(false);
    }
    let mut stmt = conn.prepare("SELECT device_uuid FROM sync_identity WHERE id = 1")?;
    let mut rows = stmt.query([])?;
    Ok(rows.next()?.is_some())
}

// ── merge.merged_overview ────────────────────────────────────────────────────

/// `_UNIONED_DAILY` — local `daily_mart` JOIN `projects`, UNION ALL the remote.
const UNIONED_DAILY: &str = "
SELECT day, provider, slug, model, speed,
       SUM(input_tokens)  AS input_tokens,
       SUM(output_tokens) AS output_tokens,
       SUM(cache_read)    AS cache_read,
       SUM(cache_create)  AS cache_create,
       SUM(message_count) AS message_count,
       SUM(session_count) AS session_count,
       SUM(cost_usd)      AS cost_usd
FROM (
    SELECT d.day, d.provider, p.slug, d.model, d.speed,
           d.input_tokens, d.output_tokens, d.cache_read, d.cache_create,
           d.message_count, d.session_count, d.cost_usd
    FROM daily_mart d JOIN projects p ON p.id = d.project_id
    UNION ALL
    SELECT day, provider, slug, model, speed,
           input_tokens, output_tokens, cache_read, cache_create,
           message_count, session_count, cost_usd
    FROM daily_mart_remote
)
GROUP BY day, provider, slug, model, speed
ORDER BY day, provider, slug, model, speed
";

/// `_UNIONED_PROVIDER_DAY`.
const UNIONED_PROVIDER_DAY: &str = "
SELECT day, provider,
       SUM(cost_usd)       AS cost_usd,
       SUM(message_count)  AS message_count,
       SUM(session_count)  AS session_count,
       SUM(project_count)  AS project_count
FROM (
    SELECT day, provider, cost_usd, message_count, session_count, project_count
    FROM provider_day_mart
    UNION ALL
    SELECT day, provider, cost_usd, message_count, session_count, project_count
    FROM provider_day_mart_remote
)
GROUP BY day, provider
ORDER BY day, provider
";

/// `_UNIONED_PROJECTS`.
const UNIONED_PROJECTS: &str = "
SELECT provider, slug,
       MAX(display_name)         AS display_name,
       MIN(first_ts)             AS first_ts,
       MAX(last_ts)              AS last_ts,
       SUM(total_messages)       AS total_messages,
       SUM(total_sessions)       AS total_sessions,
       SUM(total_input_tokens)   AS total_input_tokens,
       SUM(total_output_tokens)  AS total_output_tokens,
       SUM(total_cache_read)     AS total_cache_read,
       SUM(total_cache_create)   AS total_cache_create,
       SUM(total_cost_usd)       AS total_cost_usd
FROM (
    SELECT provider, slug, display_name, first_ts, last_ts,
           total_messages, total_sessions, total_input_tokens, total_output_tokens,
           total_cache_read, total_cache_create, total_cost_usd
    FROM project_mart
    UNION ALL
    SELECT provider, slug, display_name, first_ts, last_ts,
           total_messages, total_sessions, total_input_tokens, total_output_tokens,
           total_cache_read, total_cache_create, total_cost_usd
    FROM project_mart_remote
)
GROUP BY provider, slug
ORDER BY provider, slug
";

/// `_UNIONED_SESSIONS` — the one non-additive family.
///
/// The local arm hardcodes `device_uuid = ''`, which sorts before any hex UUID,
/// so `ORDER BY session_id, device_uuid` makes **local win** the dedup tiebreak
/// with no wall clock involved. Reproduced verbatim, including the column list.
const UNIONED_SESSIONS: &str = "
SELECT '' AS device_uuid, s.session_id, s.provider, p.slug, s.primary_model,
       s.first_ts, s.last_ts, s.message_count, s.user_message_count,
       s.assistant_message_count, s.input_tokens, s.output_tokens,
       s.cache_read, s.cache_create, s.cost_usd, s.is_one_shot
FROM session_mart s JOIN projects p ON p.id = s.project_id
UNION ALL
SELECT device_uuid, session_id, provider, slug, primary_model,
       first_ts, last_ts, message_count, user_message_count,
       assistant_message_count, input_tokens, output_tokens,
       cache_read, cache_create, cost_usd, is_one_shot
FROM session_mart_remote
ORDER BY session_id, device_uuid
";

/// `merge.merged_overview` — key order `totals`, `by_day`, `by_project`,
/// `by_provider_day`, `devices`, `merge_warnings`.
fn merged_overview(conn: &Connection) -> rusqlite::Result<Map<String, Value>> {
    let daily = query_rows(conn, UNIONED_DAILY)?;
    let projects = query_rows(conn, UNIONED_PROJECTS)?;
    let provider_day = query_rows(conn, UNIONED_PROVIDER_DAY)?;
    let (session_count, merge_warnings) = deduped_session_count(conn)?;
    let devices = device_breakdown(conn)?;

    // `sum(r["cost_usd"] for r in daily)` — a generator into `builtins.sum`,
    // which is Neumaier-compensated on the float fast path and returns the
    // `int` 0 for an empty iterable. Both halves matter (DIV-057).
    let mut cost = Neumaier::default();
    for row in &daily {
        cost.add(number_at(row, "cost_usd"));
    }
    let mut totals = Map::new();
    totals.insert("cost_usd".to_owned(), cost.finish_pynum().to_json());
    for key in [
        "input_tokens",
        "output_tokens",
        "cache_read",
        "cache_create",
        "message_count",
    ] {
        // The token/message sums are over `int`s, where CPython's `sum` is
        // exact and an empty iterable is likewise the `int` 0 — so a plain
        // integer accumulator IS the faithful port here.
        let total: i64 = daily.iter().map(|row| integer_at(row, key)).sum();
        totals.insert((*key).to_owned(), Value::from(total));
    }
    // NOT from `daily`: the deduped unique session count across devices.
    totals.insert("session_count".to_owned(), Value::from(session_count));

    // `by_day` accumulates with `+=` from a literal `0.0` / `0`. Plain, on
    // purpose — this is the counter-example sitting four lines from the `sum()`.
    let mut by_day: BTreeMap<String, (f64, i64, i64, i64)> = BTreeMap::new();
    for row in &daily {
        let day = string_at(row, "day");
        let bucket = by_day.entry(day).or_insert((0.0, 0, 0, 0));
        bucket.0 += number_at(row, "cost_usd");
        bucket.1 += integer_at(row, "input_tokens");
        bucket.2 += integer_at(row, "output_tokens");
        bucket.3 += integer_at(row, "message_count");
    }
    // `[by_day[d] for d in sorted(by_day)]`. The keys are `YYYY-MM-DD` strings,
    // so a `BTreeMap`'s byte order is `sorted()`'s order.
    let by_day_rows: Vec<Value> = by_day
        .into_iter()
        .map(|(day, (cost_usd, input, output, messages))| {
            let mut obj = Map::new();
            obj.insert("day".to_owned(), Value::from(day));
            obj.insert("cost_usd".to_owned(), PyNum::Float(cost_usd).to_json());
            obj.insert("input_tokens".to_owned(), Value::from(input));
            obj.insert("output_tokens".to_owned(), Value::from(output));
            obj.insert("message_count".to_owned(), Value::from(messages));
            Value::Object(obj)
        })
        .collect();

    let mut payload = Map::new();
    payload.insert("totals".to_owned(), Value::Object(totals));
    payload.insert("by_day".to_owned(), Value::Array(by_day_rows));
    payload.insert(
        "by_project".to_owned(),
        Value::Array(projects.into_iter().map(Value::Object).collect()),
    );
    payload.insert(
        "by_provider_day".to_owned(),
        Value::Array(provider_day.into_iter().map(Value::Object).collect()),
    );
    payload.insert("devices".to_owned(), Value::Array(devices));
    payload.insert("merge_warnings".to_owned(), Value::from(merge_warnings));
    Ok(payload)
}

/// `unioned_sessions` — the deduped count and the dropped-duplicate tally.
///
/// The route only ever reads `len(sessions)` and the warning count, so the rows
/// themselves are not materialised; the dedup rule (first sighting in
/// `session_id, device_uuid` order wins) is what has to be identical, and it is.
fn deduped_session_count(conn: &Connection) -> rusqlite::Result<(i64, i64)> {
    let mut stmt = conn.prepare(UNIONED_SESSIONS)?;
    let mut rows = stmt.query([])?;
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut warnings = 0_i64;
    while let Some(row) = rows.next()? {
        let session_id: String = row.get("session_id")?;
        if !seen.insert(session_id) {
            warnings += 1;
        }
    }
    Ok((i64::try_from(seen.len()).unwrap_or(i64::MAX), warnings))
}

/// `merge.device_breakdown` — the local row, then one per pulled peer.
fn device_breakdown(conn: &Connection) -> rusqlite::Result<Vec<Value>> {
    let mut out = Vec::new();
    let (projects, cost) = conn.query_row(
        "SELECT COUNT(*) AS projects, COALESCE(SUM(total_cost_usd), 0.0) AS cost_usd \
         FROM project_mart",
        [],
        |row| Ok((row.get::<_, i64>(0)?, row.get::<_, f64>(1)?)),
    )?;
    let mut local = Map::new();
    local.insert("device_uuid".to_owned(), Value::from("(local)"));
    local.insert("alias".to_owned(), Value::Null);
    local.insert("is_local".to_owned(), Value::Bool(true));
    local.insert("projects".to_owned(), Value::from(projects));
    // `float(local["cost_usd"])` — a float even when the sum is 0.
    local.insert("cost_usd".to_owned(), PyNum::Float(cost).to_json());
    out.push(Value::Object(local));

    let mut stmt = conn.prepare(
        "SELECT r.device_uuid AS device_uuid, d.alias AS alias, \
                COUNT(*) AS projects, COALESCE(SUM(r.total_cost_usd), 0.0) AS cost_usd \
         FROM project_mart_remote r \
         LEFT JOIN sync_remote_devices d ON d.remote_device_uuid = r.device_uuid \
         GROUP BY r.device_uuid, d.alias \
         ORDER BY r.device_uuid",
    )?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let mut peer = Map::new();
        peer.insert(
            "device_uuid".to_owned(),
            Value::from(row.get::<_, String>(0)?),
        );
        peer.insert(
            "alias".to_owned(),
            row.get::<_, Option<String>>(1)?
                .map_or(Value::Null, Value::from),
        );
        peer.insert("is_local".to_owned(), Value::Bool(false));
        peer.insert("projects".to_owned(), Value::from(row.get::<_, i64>(2)?));
        peer.insert(
            "cost_usd".to_owned(),
            PyNum::Float(row.get::<_, f64>(3)?).to_json(),
        );
        out.push(Value::Object(peer));
    }
    Ok(out)
}

// ── row plumbing ─────────────────────────────────────────────────────────────

/// `[dict(r) for r in conn.execute(SQL)]` — column order preserved, storage
/// class preserved.
///
/// The storage class matters: `sqlite3.Row` hands Python whatever SQLite stored,
/// so an `INTEGER` column comes back as an `int` and renders without a decimal
/// point. Mapping everything to `f64` would put `.0` on every token count.
fn query_rows(conn: &Connection, sql: &str) -> rusqlite::Result<Vec<Map<String, Value>>> {
    let mut stmt = conn.prepare(sql)?;
    let names: Vec<String> = stmt.column_names().into_iter().map(str::to_owned).collect();
    let mut rows = stmt.query([])?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        let mut obj = Map::new();
        for (index, name) in names.iter().enumerate() {
            obj.insert(name.clone(), sqlite_value(row.get_ref(index)?));
        }
        out.push(obj);
    }
    Ok(out)
}

fn sqlite_value(value: ValueRef<'_>) -> Value {
    match value {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(i) => Value::from(i),
        ValueRef::Real(f) => PyNum::Float(f).to_json(),
        ValueRef::Text(bytes) => Value::from(String::from_utf8_lossy(bytes).into_owned()),
        // No BLOB column appears in any of these marts; `str` is what
        // `sqlite3.Row` would surface for text and this is the honest fallback.
        ValueRef::Blob(bytes) => Value::from(String::from_utf8_lossy(bytes).into_owned()),
    }
}

fn number_at(row: &Map<String, Value>, key: &str) -> f64 {
    row.get(key).and_then(Value::as_f64).unwrap_or(0.0)
}

fn integer_at(row: &Map<String, Value>, key: &str) -> i64 {
    row.get(key).and_then(Value::as_i64).unwrap_or(0)
}

fn string_at(row: &Map<String, Value>, key: &str) -> String {
    row.get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

fn sql_500(err: rusqlite::Error) -> HttpError {
    HttpError::new(StatusCode::INTERNAL_SERVER_ERROR, err.to_string())
}

fn any_500(err: &anyhow::Error) -> HttpError {
    HttpError::new(StatusCode::INTERNAL_SERVER_ERROR, err.to_string())
}

/// `datetime.now(UTC).isoformat()`.
///
/// FLAGGED FOR THE ARCHITECT'S DEDUP LIST: `stax_adapters::pytime::Clock` owns
/// the measured version of this, but `stax-adapters` is not a dependency of
/// `stax-server` and adding one is a manifest edit batch D is not permitted to
/// make. Same output contract: microseconds are elided entirely when zero, which
/// is CPython's rule and not a rounding artefact.
fn now_iso() -> String {
    let Ok(delta) = SystemTime::now().duration_since(UNIX_EPOCH) else {
        return String::new();
    };
    let secs = i64::try_from(delta.as_secs()).unwrap_or(0);
    let micros = i64::from(delta.subsec_micros());
    let (year, month, day, hour, minute, second) = civil_from_epoch(secs);
    if micros == 0 {
        format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}+00:00")
    } else {
        format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{micros:06}+00:00")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_stub_is_four_keys_in_the_literals_order() {
        let body = JsonBody::ok(Value::Object(this_device_stub(false)));
        assert_eq!(
            body.render(),
            r#"{"scope":"this-device","merged":false,"sync_enabled":false,"hint":"pass ?scope=all-devices to union pulled peers"}"#
        );
    }

    #[test]
    fn the_stub_reports_sync_enabled_without_merging() {
        // The flag is the store's, the `merged` bool is a constant: an enabled
        // store still gets the un-merged stub until the caller opts in.
        let body = JsonBody::ok(Value::Object(this_device_stub(true)));
        assert!(
            body.render()
                .contains(r#""merged":false,"sync_enabled":true"#)
        );
    }

    #[test]
    fn an_empty_daily_union_sums_to_the_integer_zero() {
        // DIV-057, in the shape this module meets it: `sum([])` is `int` 0, so
        // an empty store renders `"cost_usd":0` and NOT `0.0`.
        assert_eq!(Neumaier::default().finish_pynum(), PyNum::Int(0));
        assert_eq!(Neumaier::default().finish_pynum().to_json(), Value::from(0));
        // …while a `by_day` bucket starts at a literal `0.0` and stays a float.
        assert_eq!(PyNum::Float(0.0).to_json().to_string(), "0.0");
    }

    #[test]
    fn the_totals_cost_is_compensated_and_the_by_day_cost_is_not() {
        // The two accumulations are four lines apart in `merged_overview` and
        // they disagree by design on a list long enough to lose bits.
        let values = [1e16, 1.0, -1e16, 1.0];
        let mut acc = Neumaier::default();
        for v in values {
            acc.add(v);
        }
        let mut plain = 0.0_f64;
        for v in values {
            plain += v;
        }
        assert!((acc.finish() - 2.0).abs() < f64::EPSILON, "sum() is exact");
        assert!(
            (plain - 2.0).abs() > f64::EPSILON,
            "+= drifts, and the port must drift with it"
        );
    }

    #[test]
    fn sqlite_storage_classes_survive_into_the_payload() {
        // An `INTEGER` token count must not acquire a `.0`, and a `REAL` cost
        // must not lose one.
        assert_eq!(sqlite_value(ValueRef::Integer(7)), Value::from(7));
        assert_eq!(sqlite_value(ValueRef::Real(0.0)).to_string(), "0.0");
        assert_eq!(sqlite_value(ValueRef::Null), Value::Null);
    }

    #[test]
    fn now_iso_has_pythons_isoformat_shape() {
        let stamp = now_iso();
        assert!(stamp.ends_with("+00:00"), "{stamp}");
        assert_eq!(stamp.as_bytes()[10], b'T', "{stamp}");
        // `2026-07-31T13:45:12+00:00` (25) or with microseconds (32).
        assert!(stamp.len() == 25 || stamp.len() == 32, "{stamp}");
    }

    #[test]
    fn the_epoch_renders_as_pythons_epoch() {
        // Same two dates the file-local `civil_from_days` was pinned on before
        // the dedup pass, now expressed in epoch SECONDS against the shared
        // routine — the drift alarm for the crate boundary.
        assert_eq!(civil_from_epoch(0), (1970, 1, 1, 0, 0, 0));
        assert_eq!(
            civil_from_epoch(20_665 * 86_400 + 13 * 3600 + 45 * 60 + 12),
            (2026, 7, 31, 13, 45, 12)
        );
    }
}
