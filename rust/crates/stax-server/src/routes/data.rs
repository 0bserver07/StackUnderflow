//! `routes/data.py` — 5 endpoints, wave 5.
//!
//! | Item | Method | FastAPI path | axum path | State |
//! |---|---|---|---|---|
//! | `RS-5-066` | `GET`  | `/api/stats`            | `/api/stats`            | **ported** |
//! | `RS-5-067` | `GET`  | `/api/dashboard-data`   | `/api/dashboard-data`   | open (batch A) |
//! | `RS-5-068` | `GET`  | `/api/messages`         | `/api/messages`         | open (batch A) |
//! | `RS-5-069` | `GET`  | `/api/messages/summary` | `/api/messages/summary` | open (batch A) |
//! | `RS-5-070` | `POST` | `/api/refresh`          | `/api/refresh`          | open (batch A) |
//!
//! # `/api/stats`
//!
//! Four lines of handler over the deepest call in the tree:
//! `queries.get_project_stats` is `build_enriched_dataset` (reconstruct every
//! `RawEntry` from `messages.raw_json`) → dedup → classifier → enricher →
//! `aggregator.summarise`, 1,518 lines of collectors producing eighteen
//! top-level blocks. That is RS-3-062, landed separately as
//! `stax_etl::stats::dataset::get_project_stats` and proven byte-identical on
//! 298 projects / 518,677 messages.
//!
//! Everything this module adds is the trimming Python does *after* that call,
//! and all of it is in-place mutation of the payload, so the order is the
//! contract: cap `daily_stats` → strip the heavy blocks → currency → `include`
//! filter. `currency` is stamped **before** the filter and always survives it.
//!
//! The memo (`routes/cost.py::_project_stats_cached`, an LRU keyed on
//! `(store, slug, tz, ids)` with an ingest signature) is **not** ported. It is a
//! pure latency device: it returns a deep copy of the same payload the pipeline
//! produces, and its signature moves the moment ingest writes, so it can never
//! serve a different answer. Recorded as DIV-055 rather than reproduced,
//! because reproducing it would mean holding multi-megabyte payloads in a
//! process the campaign has not yet measured for memory.

use axum::Router;
use axum::extract::{RawQuery, State};
use axum::http::StatusCode;
use axum::routing::get;
use rusqlite::Connection;
use serde_json::{Map, Value};

use crate::currency::active_currency_payload;
use crate::json::{HandlerResult, HttpError, JsonBody, join_failure};
use crate::qs::Query;
use crate::state::AppState;

/// `routes/cost.py::_TZ_OFFSET_MIN` / `_MAX` — minutes EAST of UTC.
const TZ_OFFSET_MIN: i64 = -720;
/// See [`TZ_OFFSET_MIN`].
const TZ_OFFSET_MAX: i64 = 840;

/// `_HEAVY_NESTED_LISTS` — `(parent, child)` pairs emptied unless `details=1`.
const HEAVY_NESTED_LISTS: [(&str, &str); 4] = [
    ("errors", "assistant_details"),
    ("errors", "error_details"),
    ("user_interactions", "command_details"),
    ("user_interactions", "tool_count_distribution"),
];

/// `_HEAVY_TOP_LEVEL_LISTS`.
const HEAVY_TOP_LEVEL_LISTS: [&str; 4] = [
    "session_costs",
    "command_costs",
    "session_efficiency",
    "retry_signals",
];

/// `_strip_heavy_blocks`'s outlier cap.
const OUTLIER_CAP: usize = 10;

/// Mount this module's endpoints onto `router`.
///
/// `routes/mod.rs` already calls this at `data`'s `include_router` position,
/// second of the 34; batch A adds the other four routes here and touches
/// nothing else.
pub fn register(router: Router<AppState>) -> Router<AppState> {
    router.route("/api/stats", get(get_stats))
}

/// `GET /api/stats`.
///
/// Declared blocking and run on `spawn_blocking`, for the same reason Python
/// declares it `def` rather than `async def`: the body is sqlite plus the
/// collector sweep, and it must not sit on the event loop.
async fn get_stats(State(state): State<AppState>, RawQuery(raw): RawQuery) -> HandlerResult {
    let query = Query::parse(raw.as_deref().unwrap_or_default());
    let timezone_offset = query
        .int_or("timezone_offset", 0)
        .map_err(|err| HttpError::new(StatusCode::UNPROCESSABLE_ENTITY, err.field))?;
    let days = query
        .opt_int("days")
        .map_err(|err| HttpError::new(StatusCode::UNPROCESSABLE_ENTITY, err.field))?;
    let details = query
        .bool_or("details", false)
        .map_err(|err| HttpError::new(StatusCode::UNPROCESSABLE_ENTITY, err.field))?;
    let include = query.opt_list("include");

    // `_require_project()` — note the truthiness: an EMPTY log path is falsy in
    // Python and 400s exactly like an unset one.
    let log_path = match state.current_project().log_path {
        Some(path) if !path.is_empty() => path,
        _ => return Err(HttpError::bad_request("No project selected")),
    };

    let worker = state.clone();
    let payload =
        tokio::task::spawn_blocking(move || compute_stats(&worker, &log_path, timezone_offset))
            .await
            .map_err(|err| join_failure(&err))??;

    let mut stats = payload;

    // `if isinstance(stats, dict):` — always true here; an empty project yields
    // an empty object, and the trims below are no-ops on it rather than skips.
    if let Value::Object(map) = &mut stats {
        // `cap_days = 90 if days is None else max(0, days)`.
        let cap_days = days.map_or(90, |value| value.max(0));
        if cap_days > 0 {
            cap_daily_stats(map, cap_days);
        }
        if !details {
            strip_heavy_blocks(map);
        }
    }

    let currency = active_currency_payload(&state.config().currency)
        .map_err(|err| HttpError::new(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))?;
    // `if currency["rate_from_usd"] != 1.0: _convert_in_place(...)` — DIV-052
    // makes the non-USD branch unreachable, so it is not ported rather than
    // ported blind.
    if let Value::Object(map) = &mut stats {
        map.insert("currency".to_owned(), currency);
    }

    // `if include:` then `if wanted and isinstance(stats, dict)`. Stamped AFTER
    // currency, and `currency` always passes the filter.
    if let Some(include) = include {
        let wanted: Vec<String> = include
            .iter()
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
            .collect();
        if !wanted.is_empty()
            && let Value::Object(map) = &stats
        {
            stats = filter_includes(map, &wanted);
        }
    }

    Ok(JsonBody::ok(stats))
}

/// The blocking body: resolve the slug, then run the pipeline.
fn compute_stats(state: &AppState, log_path: &str, tz_offset: i64) -> Result<Value, HttpError> {
    let conn = state
        .connect()
        .map_err(|err| HttpError::new(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))?;
    let slug = std::path::Path::new(log_path)
        .file_name()
        .map_or_else(String::new, |name| name.to_string_lossy().into_owned());
    let project_ids = project_ids_for_slug(&conn, &slug)
        .map_err(|err| HttpError::new(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))?;
    if project_ids.is_empty() {
        // `_get_project_rows` raises this exact string, em-dash included.
        return Err(HttpError::not_found(format!(
            "Project '{slug}' not found in store — try /api/refresh first"
        )));
    }
    // `_clamp_tz_offset` lives in the memo on the Python side, so the clamp
    // reaches `get_project_stats` there too. Applied here for the same reason.
    let tz_offset = tz_offset.clamp(TZ_OFFSET_MIN, TZ_OFFSET_MAX);
    // RS-3-082's seam, and it is not theoretical: `get_project_stats` builds the
    // *manifest* engine (`default_engine`), while `server.py`'s lifespan flips
    // `infra.costs` onto the primed `price_book` table before it serves a byte.
    // On a store whose book has been backfilled the two rate sources disagree,
    // and the differ measured it — `overview.total_cost` 568.59588725 (python,
    // book) vs 557.33358795 (rust, manifest), a 2.0% gap on eight D-stats cases
    // that were green when the harness store's `price_book` was still empty.
    // `crate::pricing::engine` is the same source `routes/pricing.rs` and
    // `routes/commands.rs` price with; injecting it is the whole fix.
    let engine = crate::pricing::engine(&conn, state.package_dir())
        .map_err(|err| HttpError::new(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))?;
    let (_messages, stats) =
        stax_etl::stats::dataset::get_project_stats_with(&conn, &project_ids, tz_offset, &engine)
            .map_err(|err| HttpError::new(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))?;
    Ok(stats)
}

/// `queries.get_projects_by_slug` → the id list, in row order.
fn project_ids_for_slug(conn: &Connection, slug: &str) -> rusqlite::Result<Vec<i64>> {
    let mut stmt = conn.prepare("SELECT id FROM projects WHERE slug = ?")?;
    stmt.query_map([slug], |row| row.get(0))?.collect()
}

/// `_cap_daily_stats` — keep the most recent `days` date keys.
///
/// `daily_stats` is a date-keyed object; Python sorts the keys, slices the tail
/// and rebuilds the dict, so the surviving entries come out in **sorted** order
/// regardless of the order the aggregator inserted them. Reproduced exactly —
/// rebuilding in the original insertion order would be a key-order divergence
/// on every capped response.
fn cap_daily_stats(stats: &mut Map<String, Value>, days: i64) {
    let Some(Value::Object(daily)) = stats.get("daily_stats") else {
        return;
    };
    let days = usize::try_from(days).unwrap_or(usize::MAX);
    if daily.len() <= days {
        return;
    }
    let mut keys: Vec<String> = daily.keys().cloned().collect();
    keys.sort();
    let keep = &keys[keys.len() - days..];
    let mut capped = Map::new();
    for key in keep {
        if let Some(value) = daily.get(key) {
            capped.insert(key.clone(), value.clone());
        }
    }
    stats.insert("daily_stats".to_owned(), Value::Object(capped));
}

/// `_strip_heavy_blocks` — empty the heavy lists **in place**, keep the keys.
///
/// The keys staying is the contract ("clients can still introspect
/// `stats["errors"]["assistant_details"]`"), and so is the *type* of the
/// emptied value: a list becomes `[]`, anything else becomes `{}`.
fn strip_heavy_blocks(stats: &mut Map<String, Value>) {
    for (parent, child) in HEAVY_NESTED_LISTS {
        if let Some(Value::Object(section)) = stats.get_mut(parent)
            && let Some(current) = section.get_mut(child)
        {
            *current = if current.is_array() {
                Value::Array(Vec::new())
            } else {
                Value::Object(Map::new())
            };
        }
    }
    for key in HEAVY_TOP_LEVEL_LISTS {
        if let Some(current) = stats.get_mut(key)
            && current.is_array()
        {
            *current = Value::Array(Vec::new());
        }
    }
    if let Some(Value::Object(outliers)) = stats.get_mut("outliers") {
        for key in ["high_tool_commands", "high_step_commands"] {
            if let Some(Value::Array(list)) = outliers.get_mut(key)
                && list.len() > OUTLIER_CAP
            {
                list.truncate(OUTLIER_CAP);
            }
        }
    }
}

/// `_filter_includes` — a dict comprehension over `stats.items()`, so the
/// surviving keys keep the payload's order, not the `include` list's.
fn filter_includes(stats: &Map<String, Value>, wanted: &[String]) -> Value {
    let mut out = Map::new();
    for (key, value) in stats {
        if key == "currency" || wanted.iter().any(|w| w == key) {
            out.insert(key.clone(), value.clone());
        }
    }
    Value::Object(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn daily(keys: &[&str]) -> Map<String, Value> {
        let mut daily = Map::new();
        for key in keys {
            daily.insert((*key).to_owned(), Value::from(1));
        }
        let mut stats = Map::new();
        stats.insert("daily_stats".to_owned(), Value::Object(daily));
        stats
    }

    #[test]
    fn the_daily_cap_keeps_the_newest_and_sorts_them() {
        // Insertion order here is deliberately scrambled: Python's
        // `sorted(ds.keys())[-days:]` rebuild means the OUTPUT is sorted, and
        // preserving the aggregator's order instead would be a byte divergence.
        let mut stats = daily(&["2026-03-01", "2026-01-01", "2026-02-01"]);
        cap_daily_stats(&mut stats, 2);
        let Some(Value::Object(capped)) = stats.get("daily_stats") else {
            panic!("daily_stats survives")
        };
        let keys: Vec<&String> = capped.keys().collect();
        assert_eq!(keys, vec!["2026-02-01", "2026-03-01"]);
    }

    #[test]
    fn a_cap_at_or_above_the_length_is_a_no_op_and_keeps_the_order() {
        let mut stats = daily(&["2026-03-01", "2026-01-01"]);
        cap_daily_stats(&mut stats, 5);
        let Some(Value::Object(kept)) = stats.get("daily_stats") else {
            panic!("daily_stats survives")
        };
        // NOT sorted — the early return means the original object is untouched.
        assert_eq!(
            kept.keys().collect::<Vec<_>>(),
            vec!["2026-03-01", "2026-01-01"]
        );
    }

    #[test]
    fn stripping_keeps_the_keys_and_the_value_type() {
        let mut stats: Map<String, Value> = serde_json::from_value(serde_json::json!({
            "errors": {"assistant_details": [1, 2, 3], "error_details": {"a": 1}, "total": 7},
            "user_interactions": {"command_details": [1], "tool_count_distribution": [1, 2]},
            "session_costs": [1, 2, 3],
            "command_costs": [1],
            "outliers": {"high_tool_commands": (0..25).collect::<Vec<_>>()},
        }))
        .expect("object");
        strip_heavy_blocks(&mut stats);
        assert_eq!(
            stax_memory::pyjson::dumps_http(&Value::Object(stats.clone())),
            r#"{"errors":{"assistant_details":[],"error_details":{},"total":7},"user_interactions":{"command_details":[],"tool_count_distribution":[]},"session_costs":[],"command_costs":[],"outliers":{"high_tool_commands":[0,1,2,3,4,5,6,7,8,9]}}"#
        );
    }

    #[test]
    fn the_include_filter_keeps_payload_order_and_always_currency() {
        let stats: Map<String, Value> = serde_json::from_value(serde_json::json!({
            "overview": 1, "tools": 2, "models": 3, "currency": {"code": "USD"},
        }))
        .expect("object");
        // `include=models&include=overview` — the OUTPUT order is the payload's
        // (overview before models), not the query's.
        let filtered = filter_includes(&stats, &["models".to_owned(), "overview".to_owned()]);
        assert_eq!(
            stax_memory::pyjson::dumps_http(&filtered),
            r#"{"overview":1,"models":3,"currency":{"code":"USD"}}"#
        );
    }

    #[test]
    fn an_unknown_include_name_is_ignored_not_an_error() {
        let stats: Map<String, Value> =
            serde_json::from_value(serde_json::json!({"overview": 1, "currency": {}}))
                .expect("object");
        let filtered = filter_includes(&stats, &["nope".to_owned()]);
        assert_eq!(
            stax_memory::pyjson::dumps_http(&filtered),
            r#"{"currency":{}}"#
        );
    }

    #[test]
    fn the_tz_clamp_is_a_real_world_utc_offset() {
        assert_eq!((-999_i64).clamp(TZ_OFFSET_MIN, TZ_OFFSET_MAX), -720);
        assert_eq!(9999_i64.clamp(TZ_OFFSET_MIN, TZ_OFFSET_MAX), 840);
        // §6b divergence 5: the React callers send raw `getTimezoneOffset()`,
        // which is minutes WEST, where the backend wants minutes east. Both
        // signs are inside the clamp, so the port inherits the wrong bucketing
        // faithfully — as it must until the frontend fix lands.
        assert_eq!(480_i64.clamp(TZ_OFFSET_MIN, TZ_OFFSET_MAX), 480);
        assert_eq!((-480_i64).clamp(TZ_OFFSET_MIN, TZ_OFFSET_MAX), -480);
    }
}
