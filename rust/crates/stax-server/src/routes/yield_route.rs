//! `routes/yield_route.py` — 1 endpoint, wave 5 (batch C).
//!
//! | Item | Method | FastAPI path | axum path | State |
//! |---|---|---|---|---|
//! | `RS-5-116` | `GET` | `/api/yield` | `/api/yield` | ported |
//!
//! # What the endpoint answers
//!
//! "Of the money spent on AI sessions in this window, how much bought commits
//! that are still in the tree?" Sessions are classified `productive` /
//! `reverted` / `abandoned` / `no_repo` by correlating each session's start
//! time against the git history of the directory it ran in. All of that lives
//! in [`crate::services::yield_tracker`]; this module is what Python's is —
//! parse, validate, call, stamp currency, return.
//!
//! **The payload is a function of the machine's git working trees**, not only of
//! the store. The full read-only audit and the determinism caveats are DIV-095;
//! the short version is that nothing here writes anything, so the endpoint is
//! cleared for parity case rows under LAW 7, but a `follow_commit_*` difference
//! on the newest sessions may be a repo that moved between the two requests
//! rather than a port defect.
//!
//! # The four details that decide the bytes
//!
//! 1. **The allow-list is a six-tuple and the 400 joins it in TUPLE order** —
//!    `today, week, month, all, 7days, 30days`. `routes/cost.py`'s equivalent
//!    message *is* sorted; these two are not copy-pasteable.
//! 2. **`week` is not a `reports/scope.py` period.** It is accepted here and
//!    rewritten to `7days` inside the tracker.
//! 3. **The cost sort is `sorted(..., reverse=True)`, which is STABLE.** CPython
//!    reverses the list, sorts stably, and reverses again, so equal costs keep
//!    `compute_yield`'s chronological order. [`Vec::sort_by`] is stable and
//!    `sort_unstable_by` is not; the comparator also has to survive a `NaN`
//!    without panicking, which `partial_cmp` + `Ordering::Equal` does.
//! 4. **`yield_summary` runs on the UNSORTED entries** while `to_dicts` runs on
//!    the sorted copy. Since the summary accumulates with `+=`, the addition
//!    order is a real last-bits property and not a cosmetic one.
//!
//! And one shape worth naming: `get_outcomes_for_session` is called once **per
//! entry**, inside the connection block. That is N+1 by construction, and it is
//! ported as written — batching it would reorder the rows inside `pr` and
//! `ci_runs`, and the order is the payload.

use axum::Router;
use axum::extract::{RawQuery, State};
use axum::http::StatusCode;
use axum::routing::get;
use serde_json::{Map, Value};

use crate::currency::active_currency_payload;
use crate::json::{HandlerResult, HttpError, JsonBody, join_failure};
use crate::qs::Query;
use crate::services::outcome_attribution::get_outcomes_for_session;
use crate::services::scope::Instant;
use crate::services::yield_tracker::{
    self, SystemGit, YieldEntry, max_sessions_per_project, to_dicts, yield_summary,
};
use crate::state::AppState;

/// `_VALID_PERIODS` — a friendly superset of `reports/scope.py`'s specs.
///
/// Order matters twice over: it is the order the 400's `detail` joins, and
/// `week` / `all` sit in the MIDDLE of it rather than where a sorted list would
/// put them.
const VALID_PERIODS: [&str; 6] = ["today", "week", "month", "all", "7days", "30days"];

/// `_WARNING` — shipped on every response so no consumer can render the
/// breakdown without the caveat.
const WARNING: &str = "Yield is correlated by time, not by content. A commit that lands within \
                       24h of a session is credited to that session even if it was about \
                       something else. Treat the breakdown as a smoke signal, not a verdict.";

/// Mount this module's endpoint onto `router`.
///
/// Called once, from [`super::register_all`], at this module's `include_router`
/// position.
pub fn register(router: Router<AppState>) -> Router<AppState> {
    router.route("/api/yield", get(get_yield))
}

/// `GET /api/yield` → `{period, summary, entries, currency, warning}`.
async fn get_yield(State(state): State<AppState>, RawQuery(raw): RawQuery) -> HandlerResult {
    let query = Query::parse(raw.as_deref().unwrap_or_default());
    // `period: str = Query("month")`. starlette takes the LAST occurrence of a
    // repeated scalar, and `?period=` is the empty string — which is not in the
    // allow-list and therefore a 400, not a fallback to the default.
    let period = query.str_or("period", "month").to_owned();
    if !VALID_PERIODS.contains(&period.as_str()) {
        return Err(HttpError::bad_request(format!(
            "Invalid period '{period}'. Valid: {}",
            VALID_PERIODS.join(", ")
        )));
    }
    // `project_filter = list(project) if isinstance(project, list) else None` —
    // the FastAPI-sentinel guard. `opt_list` already yields `None` for an absent
    // repeated parameter, which is the same distinction.
    let project_filter = query.opt_list("project");

    // Resolved once, in the handler, so the blocking body is a pure function of
    // its arguments (campaign finding 5). `parse_period` reads the clock inside
    // `compute_yield`; pinning the instant here also means the two SQL bounds
    // and the git window come from one reading rather than several.
    let now = Instant::now_utc();
    let cap = max_sessions_per_project(&|key| std::env::var(key).ok());

    let worker = state.clone();
    let period_for_worker = period.clone();
    let (summary, entries) = tokio::task::spawn_blocking(move || {
        compute(
            &worker,
            &period_for_worker,
            project_filter.as_deref(),
            cap,
            now,
        )
    })
    .await
    .map_err(|err| join_failure(&err))??;

    let currency = active_currency_payload(&state.config().currency)
        .map_err(|err| HttpError::new(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))?;
    // `if rate != 1.0:` then a walk over `_ENTRY_COST_FIELDS` and
    // `_SUMMARY_COST_FIELDS`. DIV-052 makes `active_currency_payload` USD-only,
    // so the branch cannot fire; recorded rather than ported blind, exactly as
    // `routes/data.rs` and `routes/cost.rs` already do.

    let mut payload = Map::new();
    payload.insert("period".to_owned(), Value::String(period));
    payload.insert("summary".to_owned(), summary);
    payload.insert("entries".to_owned(), Value::Array(entries));
    payload.insert("currency".to_owned(), currency);
    payload.insert("warning".to_owned(), Value::String(WARNING.to_owned()));
    Ok(JsonBody::ok(Value::Object(payload)))
}

/// The blocking body: sqlite, then a git fan-out. Both belong off the event
/// loop, which is why Python declares its handler `async def` but does every
/// bit of this work synchronously inside it.
fn compute(
    state: &AppState,
    period: &str,
    project_filter: Option<&[String]>,
    cap: Option<usize>,
    now: Instant,
) -> Result<(Value, Vec<Value>), HttpError> {
    let conn = state.connect().map_err(|err| any_500(&err))?;
    // LAW 2: the engine is injected from the store's `price_book`, never
    // `default_engine()`. It only reaches the empty-mart fallback path, but that
    // is the path that re-prices `messages` — the one DIV-056 mispriced by 2%.
    let engine = crate::pricing::engine(&conn, state.package_dir()).map_err(|err| any_500(&err))?;

    let entries =
        yield_tracker::compute_yield(&conn, period, project_filter, cap, now, &SystemGit, &engine)
            .map_err(|err| HttpError::new(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))?;

    // `sorted(entries, key=lambda e: e.cost_usd, reverse=True)` — a COPY, so
    // `entries` (and therefore the summary below) keeps start-time order.
    let sorted = sort_by_cost_desc(&entries);
    let mut body_entries = to_dicts(&sorted);

    for entry in &mut body_entries {
        let session_id = entry
            .get("session_id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        // One round trip per entry, plus two per linked commit inside. N+1 by
        // construction and kept that way — see the module docs.
        let outcomes = get_outcomes_for_session(&conn, &session_id)
            .map_err(|err| HttpError::new(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))?;
        if let Value::Object(map) = entry {
            // `e["pr"] = outcomes["prs"]` — the key is SINGULAR and the value is
            // the PR *list*. Both are new keys, so they land after
            // `follow_commit_age_hours` in the rendered object.
            map.insert("pr".to_owned(), Value::Array(outcomes.prs));
            map.insert("ci_runs".to_owned(), Value::Array(outcomes.ci_runs));
        }
    }

    // Computed from the UNSORTED list. Python runs this after `conn.close()`;
    // the position is immaterial, the argument is not.
    let summary = yield_summary(&entries);
    Ok((summary, body_entries))
}

/// `sorted(entries, key=lambda e: e.cost_usd, reverse=True)`.
///
/// Stable descending: CPython's `reverse=True` reverses the list, runs the same
/// stable Timsort, and reverses back, so ties come out in the input's order.
/// `sort_by` (stable) with an inverted comparator is the same guarantee;
/// `sort_unstable_by` is not, and would scramble every equal-cost run — of which
/// a real store has many, because `cost_usd` is `0.0` for any session the ETL
/// could not price.
fn sort_by_cost_desc(entries: &[YieldEntry]) -> Vec<YieldEntry> {
    let mut sorted = entries.to_vec();
    sorted.sort_by(|left, right| {
        // `partial_cmp` is `None` only for a `NaN` cost; treating that as `Equal`
        // leaves the entry where it was rather than panicking, which is the
        // closest a total order gets to Timsort's behaviour on an incomparable.
        right
            .cost_usd
            .partial_cmp(&left.cost_usd)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    sorted
}

fn any_500(err: &anyhow::Error) -> HttpError {
    HttpError::new(StatusCode::INTERNAL_SERVER_ERROR, err.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::yield_tracker::Classification;

    fn entry(session: &str, cost: f64) -> YieldEntry {
        YieldEntry {
            session_id: session.to_owned(),
            project_slug: "p".to_owned(),
            cwd: "/repo".to_owned(),
            started_at: "2026-07-01T00:00:00+00:00".to_owned(),
            cost_usd: cost,
            classification: Classification::NoRepo,
            follow_commit_sha: None,
            follow_commit_msg: None,
            follow_commit_age_hours: None,
        }
    }

    fn ids(entries: &[YieldEntry]) -> Vec<&str> {
        entries.iter().map(|e| e.session_id.as_str()).collect()
    }

    #[test]
    fn equal_costs_keep_compute_yields_order_because_the_sort_is_stable() {
        // Every one of these is $0 — the single most common shape on a real
        // store, since an unpriceable session is `0.0`. `sort_unstable_by` is
        // free to permute them and would diverge on the whole `entries` array.
        let entries = vec![
            entry("a", 0.0),
            entry("b", 0.0),
            entry("c", 0.0),
            entry("d", 0.0),
        ];
        assert_eq!(ids(&sort_by_cost_desc(&entries)), vec!["a", "b", "c", "d"]);
    }

    #[test]
    fn the_sort_is_descending_and_ties_within_it_stay_put() {
        let entries = vec![
            entry("cheap", 0.5),
            entry("tie-first", 2.0),
            entry("dear", 9.0),
            entry("tie-second", 2.0),
        ];
        assert_eq!(
            ids(&sort_by_cost_desc(&entries)),
            vec!["dear", "tie-first", "tie-second", "cheap"]
        );
    }

    #[test]
    fn a_nan_cost_does_not_panic_the_comparator() {
        let entries = vec![entry("a", 1.0), entry("nan", f64::NAN), entry("b", 2.0)];
        let sorted = sort_by_cost_desc(&entries);
        assert_eq!(sorted.len(), 3);
        assert!(sorted.iter().any(|e| e.session_id == "nan"));
    }

    #[test]
    fn the_period_400_joins_the_allow_list_in_tuple_order() {
        // NOT sorted: `week` and `all` sit in the middle. `routes/cost.py`'s
        // sibling message IS sorted, which is exactly the trap.
        let err = HttpError::bad_request(format!(
            "Invalid period '{}'. Valid: {}",
            "nonsense",
            VALID_PERIODS.join(", ")
        ));
        assert_eq!(
            err.body().render(),
            r#"{"detail":"Invalid period 'nonsense'. Valid: today, week, month, all, 7days, 30days"}"#
        );
    }

    #[test]
    fn the_warning_is_one_line_with_no_double_spaces_at_the_seams() {
        // Python builds it from three adjacent string literals; a mis-joined
        // continuation is a silent one-byte difference on EVERY response.
        assert_eq!(
            WARNING,
            "Yield is correlated by time, not by content. A commit that lands within 24h of a \
             session is credited to that session even if it was about something else. Treat the \
             breakdown as a smoke signal, not a verdict."
        );
        assert!(!WARNING.contains("  "));
    }

    #[test]
    fn every_accepted_period_is_one_the_tracker_can_normalise() {
        for period in VALID_PERIODS {
            let normalised = yield_tracker::normalize_period(period);
            assert!(
                crate::services::scope::parse_period(
                    normalised,
                    Instant::from_parts(2026, 7, 31, 0, 0, 0, 0)
                )
                .is_ok(),
                "{period} -> {normalised}"
            );
        }
    }
}
