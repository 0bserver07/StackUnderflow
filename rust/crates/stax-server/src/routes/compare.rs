//! `routes/compare.py` — 1 endpoint, wave 5.
//!
//! | Item | Method | FastAPI path | axum path | State |
//! |---|---|---|---|---|
//! | `RS-5-059` | `GET` | `/api/compare` | `/api/compare` | **ported** |
//!
//! The Compare tab's data source: one row per model the user touched in a
//! window, with the metrics the side-by-side card renders (sessions, calls,
//! one-shot %, retry rate, cache hit rate, $/call, $/session, total cost, total
//! tokens). Every line of that lives in [`crate::services::compare`], because
//! the `stax compare` CLI verb calls the *same* function — this module
//! is the twenty lines of parameter handling around it, exactly as Python's is.
//!
//! # What this thin module still has to get exactly right
//!
//! * **The 400's text is a tuple join, not a sorted one.** `_VALID_PERIODS` is
//!   `("today", "week", "month", "all")` and the detail is
//!   `', '.join(_VALID_PERIODS)`, so it reads `today, week, month, all`. The
//!   service layer's own `ValueError` for the same four aliases joins
//!   `sorted(PERIOD_MAP)` and reads `all, month, today, week`. Two orderings,
//!   one feature; the HTTP one is [`unknown_period_detail`].
//! * **An unknown period is a 400, not a fallback.** The docstring says why:
//!   "Returns 400 on an unknown period rather than silently falling back so the
//!   frontend surfaces typos."
//! * **`project` repeats, `provider` does not.** `list[str] | None` against
//!   `str | None`, so `?project=a&project=b` is two filters while
//!   `?provider=a&provider=b` is just `b` (starlette's `QueryParams` keeps the
//!   last). And an absent `project` is `None`, not `[]` — the service branches
//!   on `is None` to choose its codepath, so the distinction is load-bearing.
//! * **No currency stamp.** Most ported routes end with
//!   `active_currency_payload`; this one does not, and adding it would invent a
//!   key. The dollar figures go out raw.
//!
//! # `schema.apply` is not ported
//!
//! Python opens its own connection and runs `schema.apply(conn)` per request.
//! `apply` reads `PRAGMA user_version` and returns immediately when the store is
//! current, which it always is by the time a request arrives — `server.py`'s
//! lifespan applied it at startup. The port never migrates a store it is only
//! reading (DIV-085).

use axum::Router;
use axum::extract::{RawQuery, State};
use axum::http::StatusCode;
use axum::routing::get;

use crate::json::{HandlerResult, HttpError, JsonBody, join_failure};
use crate::qs::Query;
use crate::services::compare::{build_compare_payload, now_unix_seconds};
use crate::services::scope::Instant;
use crate::state::AppState;

/// `_VALID_PERIODS` — the route's own allow-list, in its own order.
const VALID_PERIODS: [&str; 4] = ["today", "week", "month", "all"];

/// `_PERIOD_Q = Query("month", …)`.
const DEFAULT_PERIOD: &str = "month";

/// The `detail` of the 400, byte for byte.
///
/// `', '.join(_VALID_PERIODS)` over a **tuple**, so the order is the literal's
/// and not alphabetical. Public so the service module's test can assert that the
/// two error strings in this feature really do differ.
#[must_use]
pub fn unknown_period_detail(period: &str) -> String {
    format!(
        "Unknown period '{period}'. Valid: {}",
        VALID_PERIODS.join(", ")
    )
}

/// Mount this module's endpoints onto `router`.
///
/// Called once, from [`super::register_all`], at this module's `include_router`
/// position (fourteenth of the 34).
pub fn register(router: Router<AppState>) -> Router<AppState> {
    router.route("/api/compare", get(get_compare))
}

/// `GET /api/compare`.
///
/// Python declares this `async def` and then does blocking SQLite inside it —
/// which pins the event loop for the duration of the query. The port runs the
/// body on `spawn_blocking` instead: same answer, and the difference is
/// invisible to a byte differ that issues one request at a time.
async fn get_compare(State(state): State<AppState>, RawQuery(raw): RawQuery) -> HandlerResult {
    let query = Query::parse(raw.as_deref().unwrap_or_default());
    let period = query.str_or("period", DEFAULT_PERIOD).to_owned();
    if !VALID_PERIODS.contains(&period.as_str()) {
        return Err(HttpError::bad_request(unknown_period_detail(&period)));
    }

    // `list(project) if isinstance(project, list) else None` — the Query-sentinel
    // coercion. Over HTTP there is no sentinel; the shape that matters is that
    // an ABSENT `project` is `None` rather than an empty list, which is what
    // `opt_list` already gives.
    let project_filter = query.opt_list("project");
    // `provider if isinstance(provider, str) else None`. Note that `?provider=`
    // arrives as `Some("")` and NOT as `None` — FastAPI would hand the handler
    // the empty string too, and the service treats the two differently.
    let provider_filter = query.get("provider").map(str::to_owned);

    // `parse_period` reads the clock inside `compare_models`; injected here so
    // the service stays a pure function of its inputs (campaign finding 5).
    let now = Instant::now_utc();

    let worker = state.clone();
    let payload = tokio::task::spawn_blocking(move || {
        let conn = worker
            .connect()
            .map_err(|err| HttpError::new(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))?;
        // LAW 2 / DIV-056: the engine comes from THIS store's `price_book`, the
        // same source `server.py`'s lifespan primes `infra.costs` with. A
        // `default_engine()` here would price the fallback path off the manifest
        // and be 2% wrong on a backfilled store, invisibly.
        let engine = crate::pricing::engine(&conn, worker.package_dir())
            .map_err(|err| HttpError::new(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))?;
        build_compare_payload(
            &conn,
            &engine,
            &period,
            project_filter.as_deref(),
            provider_filter.as_deref(),
            now,
            // `time.time()`, evaluated in the dict literal — i.e. AFTER the
            // query. Passing the closure rather than a value keeps that order.
            now_unix_seconds,
        )
        .map_err(|err| HttpError::new(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))
    })
    .await
    .map_err(|err| join_failure(&err))??;

    Ok(JsonBody::ok(payload))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_four_hundred_joins_the_tuple_and_does_not_sort_it() {
        assert_eq!(
            unknown_period_detail("7days"),
            "Unknown period '7days'. Valid: today, week, month, all"
        );
        // The rendered body is FastAPI's one-key envelope.
        assert_eq!(
            HttpError::bad_request(unknown_period_detail(""))
                .body()
                .render(),
            r#"{"detail":"Unknown period ''. Valid: today, week, month, all"}"#
        );
    }

    #[test]
    fn the_services_error_message_sorts_the_aliases_and_the_routes_does_not() {
        // RELOCATED by the tranche-3 crate split. `services/compare.rs` moved to
        // `stax-reports`, which must not depend on this crate, so the half of
        // that module's assertion which reaches for the HTTP string lives here
        // now. Both halves still run every `cargo test`; neither string is
        // allowed to quietly become the other.
        assert_eq!(
            stax_reports::compare::unknown_period_message("nope"),
            "Unknown period 'nope'. Valid: all, month, today, week",
            "the service layer sorts `PERIOD_MAP`"
        );
        assert_eq!(
            unknown_period_detail("nope"),
            "Unknown period 'nope'. Valid: today, week, month, all",
            "`routes/compare.py` joins the tuple in declaration order"
        );
    }

    #[test]
    fn the_allow_list_is_exactly_the_service_layers_alias_table() {
        // If these ever drift, the 400 stops guarding `_resolve_scope` and its
        // ValueError becomes reachable as a 500.
        let mut route = VALID_PERIODS.to_vec();
        let mut service: Vec<&str> = crate::services::compare::PERIOD_MAP
            .iter()
            .map(|(alias, _)| *alias)
            .collect();
        route.sort_unstable();
        service.sort_unstable();
        assert_eq!(route, service);
    }

    #[test]
    fn an_absent_project_is_none_while_a_repeated_one_is_every_value() {
        let query = Query::parse("period=week");
        assert_eq!(query.opt_list("project"), None);
        assert_eq!(query.str_or("period", DEFAULT_PERIOD), "week");

        let query = Query::parse("project=a&project=b&provider=x&provider=y");
        assert_eq!(
            query.opt_list("project"),
            Some(vec!["a".to_owned(), "b".to_owned()])
        );
        // `provider` is a scalar: starlette keeps the LAST occurrence.
        assert_eq!(query.get("provider"), Some("y"));
    }

    #[test]
    fn an_empty_provider_value_is_the_empty_string_and_not_an_absent_one() {
        // The distinction the mart path's `is not None` test turns on.
        let query = Query::parse("provider=");
        assert_eq!(query.get("provider"), Some(""));
        assert_eq!(Query::parse("").get("provider"), None);
    }

    #[test]
    fn the_default_period_is_month_and_the_last_occurrence_wins() {
        assert_eq!(Query::parse("").str_or("period", DEFAULT_PERIOD), "month");
        assert_eq!(
            Query::parse("period=today&period=all").str_or("period", DEFAULT_PERIOD),
            "all"
        );
    }
}
