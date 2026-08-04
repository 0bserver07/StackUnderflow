//! `routes/data.py` — 5 endpoints, wave 5.
//!
//! | Item | Method | FastAPI path | axum path | State |
//! |---|---|---|---|---|
//! | `RS-5-066` | `GET`  | `/api/stats`            | `/api/stats`            | **ported** (batch A) |
//! | `RS-5-067` | `GET`  | `/api/dashboard-data`   | `/api/dashboard-data`   | **ported** (batch C) |
//! | `RS-5-068` | `GET`  | `/api/messages`         | `/api/messages`         | **ported** (batch C) |
//! | `RS-5-069` | `GET`  | `/api/messages/summary` | `/api/messages/summary` | **ported** (batch C) |
//! | `RS-5-070` | `POST` | `/api/refresh`          | `/api/refresh`          | **ported** (batch C) |
//!
//! # Two batches, one file
//!
//! Batch A ported `/api/stats` and the price-book seam behind it (DIV-056);
//! batch C added the other four and touched none of A's code. The differ's
//! eight `D-stats*` rows guard that boundary.
//!
//! # `/api/refresh` is the only WRITER in the wave-5 surface
//!
//! It re-runs the full ingest pass. That makes it unfit for the shared case
//! matrix on two counts, both of which are DIV-059's lesson stated in the
//! concrete: (1) it mutates the store every other row reads, and a `!`
//! known-open row would still ISSUE the request; (2) its `refresh_time_ms`
//! field is a wall-clock measurement, so the two servers can never agree on it
//! byte for byte. It therefore gets its own procedure on a throwaway state copy
//! — `rust/REFRESH-DIFFER.md` carries the exact commands — and **no row in
//! `rust/parity/endpoint-cases.txt`, ever**.
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

use std::collections::BTreeSet;

use axum::Router;
use axum::extract::{RawQuery, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use rusqlite::Connection;
use serde_json::{Map, Value};
use stax_etl::pricing::RawTokens;
use stax_etl::pricing::costs::PricingEngine;
use stax_etl::stats::aggregator::{Neumaier, PyNum, round_py};

use crate::currency::active_currency_payload;
use crate::json::{HandlerResult, HttpError, JsonBody, join_failure, validation_422};
use crate::pyops::COST_KEYS;
use crate::qs::Query;
use crate::services::mart_queries::table_exists;
use crate::services::messages as messages_api;
use crate::services::stats_memo;
use crate::state::AppState;

// `_TZ_OFFSET_MIN` / `_MAX` moved with the memo they belong to —
// `crate::services::stats_memo`, DIV-055. They were duplicated here, in
// `routes/cost.rs` and in `routes/commands.rs` for the same reason Python
// duplicates the clamp *call* at three sites: the constant lives with
// `_clamp_tz_offset`, which lives with `_project_stats_cached`.

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
/// second of the 34. Order inside the module is `data.py`'s decorator order.
pub fn register(router: Router<AppState>) -> Router<AppState> {
    router
        .route("/api/stats", get(get_stats))
        .route("/api/dashboard-data", get(get_dashboard_data))
        .route("/api/messages", get(get_messages))
        .route("/api/messages/summary", get(get_messages_summary_endpoint))
        .route("/api/refresh", post(refresh_data))
}

/// `GET /api/stats`.
///
/// Declared blocking and run on `spawn_blocking`, for the same reason Python
/// declares it `def` rather than `async def`: the body is sqlite plus the
/// collector sweep, and it must not sit on the event loop.
async fn get_stats(State(state): State<AppState>, RawQuery(raw): RawQuery) -> HandlerResult {
    let query = Query::parse(raw.as_deref().unwrap_or_default());
    // NOTE (batch C): the three `map_err`s below used to render
    // `HttpError::new(422, err.field)`, i.e. `{"detail":"days"}`. That is the
    // wrong shape — FastAPI renders a LIST — and it was latent because no
    // `D-stats*` case row sends an uncoercible value. The sibling rows
    // `DD-bad-int` / `MSG-tz-bad` caught the identical bug in this batch's own
    // handlers on the first gate run, so it is fixed here in the same pass and
    // `D-stats-bad-int` / `D-stats-bad-bool` now cover it. **Nothing else in
    // `get_stats` was touched** — the pipeline call, the price-book seam
    // (DIV-056), the trim order and the include filter are batch A's, unchanged.
    let timezone_offset = match query.int_or("timezone_offset", 0) {
        Ok(value) => value,
        Err(err) => return Ok(validation_422(&err)),
    };
    let days = match query.opt_int("days") {
        Ok(value) => value,
        Err(err) => return Ok(validation_422(&err)),
    };
    let details = match query.bool_or("details", false) {
        Ok(value) => value,
        Err(err) => return Ok(validation_422(&err)),
    };
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
    // `stats = _project_stats_cached(conn, project_ids=…, slug=slug,
    //  tz_offset=timezone_offset)` — `data.py` line 245, with `keys=None`
    // because `/api/stats` needs every top-level key. DIV-055: this is the one
    // endpoint where the reference was faster, and the memo is why. The clamp
    // lives inside `project_stats_cached`, exactly as `_clamp_tz_offset` lives
    // inside `_project_stats_cached`, so it reaches the key AND the call.
    stats_memo::project_stats_cached(
        state.stats_memo(),
        &conn,
        &stats_memo::StatsRequest {
            store_path: state.store_path(),
            slug: &slug,
            project_ids: &project_ids,
            tz_offset,
            keys: None,
        },
        |err| HttpError::new(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()),
        |tz_offset| {
            stax_etl::stats::dataset::get_project_stats_with(
                &conn,
                &project_ids,
                tz_offset,
                &engine,
            )
            .map(|(_messages, stats)| stats)
            .map_err(|err| HttpError::new(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))
        },
    )
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

// ═══ batch C — the other four endpoints ══════════════════════════════════════

/// `MESSAGES_DEFAULT_PER_PAGE` / `MESSAGES_MAX_PER_PAGE`.
const MESSAGES_DEFAULT_PER_PAGE: i64 = 100;
/// See [`MESSAGES_DEFAULT_PER_PAGE`].
const MESSAGES_MAX_PER_PAGE: i64 = 500;

/// `settings.messages_initial_load = _Opt(500, "MESSAGES_INITIAL_LOAD")`.
const MESSAGES_INITIAL_LOAD_DEFAULT: i64 = 500;

/// `aggregator._CACHE_COST_BASE_UNIT_SCALE` — real USD × 1e6.
const CACHE_COST_BASE_UNIT_SCALE: f64 = 1_000_000.0;

/// `_PROJECT_MART_ADDITIVE` — the columns that SUM across a slug's providers.
///
/// `errors_by_category` is deliberately absent: it is a JSON map and is merged
/// key-wise by [`errors_block_from_marts`] off the *unmerged* rows.
const PROJECT_MART_ADDITIVE: [&str; 19] = [
    "total_messages",
    "total_sessions",
    "total_input_tokens",
    "total_output_tokens",
    "total_cache_read",
    "total_cache_create",
    "total_cost_usd",
    "total_user_messages",
    "total_assistant_messages",
    "total_tool_use_messages",
    "total_tool_result_messages",
    "total_commands",
    "total_records",
    "total_errors",
    "total_cache_read_messages",
    "total_commands_followed_by_interruption",
    "total_command_tools",
    "total_command_steps",
    // NOT in Python's tuple — see `merge_project_mart_rows`, which carries the
    // identity columns separately. Kept out of the numeric loop.
    "",
];

// ── shared helpers ───────────────────────────────────────────────────────────

/// `_require_project()` — `if not deps.current_log_path: raise HTTPException(400)`.
///
/// The truthiness matters: an EMPTY log path is falsy in Python and 400s
/// exactly like an unset one. A provider with no on-disk log directory
/// legitimately stores `""`.
fn require_project(state: &AppState) -> Result<String, HttpError> {
    match state.current_project().log_path {
        Some(path) if !path.is_empty() => Ok(path),
        _ => Err(HttpError::bad_request("No project selected")),
    }
}

/// One `projects` row, as much of it as this module reads.
struct ProjectRow {
    id: i64,
    provider: Option<String>,
}

/// `_get_project_rows` — `queries.get_projects_by_slug`, 404 when empty.
///
/// The 404 string carries an em-dash, and the HTTP writer is
/// `ensure_ascii=False`, so it reaches the wire as three raw UTF-8 bytes.
fn project_rows(conn: &Connection, log_path: &str) -> Result<Vec<ProjectRow>, HttpError> {
    let slug = std::path::Path::new(log_path)
        .file_name()
        .map_or_else(String::new, |name| name.to_string_lossy().into_owned());
    let mut stmt = conn
        .prepare("SELECT id, provider FROM projects WHERE slug = ?")
        .map_err(sql_500)?;
    let rows: Vec<ProjectRow> = stmt
        .query_map([&slug], |row| {
            Ok(ProjectRow {
                id: row.get(0)?,
                provider: row.get(1)?,
            })
        })
        .map_err(sql_500)?
        .collect::<rusqlite::Result<_>>()
        .map_err(sql_500)?;
    if rows.is_empty() {
        return Err(HttpError::not_found(format!(
            "Project '{slug}' not found in store — try /api/refresh first"
        )));
    }
    Ok(rows)
}

/// `_filtered_project_ids` — the ids for the slug, narrowed to `provider_filter`.
///
/// A slug maps to one row PER PROVIDER (`UNIQUE(provider, slug)`), so the filter
/// must test EVERY row. The predecessor did `get_project(slug)` — a single
/// `fetchone` — and tested that one arbitrary row, so a multi-provider project
/// returned empty whenever an earlier-listed provider was excluded. Returns an
/// empty vec (not a 404) when the filter excludes every provider, so callers can
/// serve a shape-stable empty body.
fn filtered_project_ids(
    conn: &Connection,
    log_path: &str,
    provider_filter: Option<&BTreeSet<String>>,
) -> Result<Vec<i64>, HttpError> {
    let rows = project_rows(conn, log_path)?;
    Ok(match provider_filter {
        None => rows.iter().map(|row| row.id).collect(),
        Some(filter) => rows
            .iter()
            .filter(|row| {
                // `(r.provider or "").lower() in provider_filter`.
                filter.contains(&row.provider.clone().unwrap_or_default().to_lowercase())
            })
            .map(|row| row.id)
            .collect(),
    })
}

fn sql_500(err: rusqlite::Error) -> HttpError {
    HttpError::new(StatusCode::INTERNAL_SERVER_ERROR, err.to_string())
}

fn any_500(err: &anyhow::Error) -> HttpError {
    HttpError::new(StatusCode::INTERNAL_SERVER_ERROR, err.to_string())
}

/// `{p.strip().lower() for p in values if p and p.strip()} or None`.
///
/// A `set` on the Python side; a [`BTreeSet`] here, because the only operations
/// are membership and (for the SQL `IN` clause) a deterministic iteration order.
fn normalise_filter(raw: Option<Vec<String>>) -> Option<BTreeSet<String>> {
    let raw = raw?;
    let normed: BTreeSet<String> = raw
        .iter()
        .filter(|value| !value.trim().is_empty())
        .map(|value| value.trim().to_lowercase())
        .collect();
    // `if normed:` — an all-blank filter list leaves the filter UNSET, which is
    // not the same as an empty one (unset means "no filter", empty would mean
    // "match nothing").
    (!normed.is_empty()).then_some(normed)
}

/// `deps.config.get("messages_initial_load")`.
///
/// Resolved per request rather than at startup, which is *closer* to Python than
/// [`crate::state::Config`] is: `settings._Opt.__get__` re-reads `env → file →
/// default` on every attribute access. It is not on `Config` because `state.rs`
/// is shared wave-5 foundation that batch C's charter does not extend to, and
/// one setting does not justify editing a file two other batches were mid-flight
/// in. `MAX_DATE_RANGE_DAYS` *is* on `Config`, so the two fields in the same
/// `config` block resolve by different routes — recorded as DIV-123 rather than
/// smoothed over.
fn messages_initial_load(state: &AppState) -> i64 {
    if let Ok(raw) = std::env::var("MESSAGES_INITIAL_LOAD") {
        // `_cast` swallows the ValueError into the DEFAULT — it does not fall
        // through to the file.
        return raw.parse().unwrap_or(MESSAGES_INITIAL_LOAD_DEFAULT);
    }
    let home = state
        .store_path()
        .parent()
        .map(std::path::Path::to_path_buf);
    home.and_then(|dir| std::fs::read_to_string(dir.join("config.json")).ok())
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|json| json.get("messages_initial_load").and_then(Value::as_i64))
        .unwrap_or(MESSAGES_INITIAL_LOAD_DEFAULT)
}

/// The `config` sub-block every `/api/dashboard-data` body carries.
fn config_block(state: &AppState) -> Value {
    let mut block = Map::new();
    block.insert(
        "messages_initial_load".to_owned(),
        Value::from(messages_initial_load(state)),
    );
    block.insert(
        "max_date_range_days".to_owned(),
        Value::from(state.config().max_date_range_days),
    );
    Value::Object(block)
}

// ── store/mart_queries.py, the subset these endpoints read ───────────────────
//
// FLAGGED FOR DEDUP: `routes/cost.rs` (batch A) carries private copies of
// `table_exists`, `mart_has_project_row`, `daily_for_project` and
// `tool_mart_for_project`, and `services/mart_queries.rs` (batch C, optimize)
// carries a third set. Three copies is two too many; the merge is a
// post-landing task, not a mid-flight cross-file edit while the other two are
// uncommitted.

/// `mart_queries.mart_has_project_row` — the "is this project materialised?" gate.
fn mart_has_project_row(conn: &Connection, project_id: i64) -> rusqlite::Result<bool> {
    if !table_exists(conn, "project_mart")? {
        return Ok(false);
    }
    let mut stmt = conn.prepare("SELECT 1 FROM project_mart WHERE project_id = ? LIMIT 1")?;
    let mut rows = stmt.query([project_id])?;
    Ok(rows.next()?.is_some())
}

/// `mart_queries.get_project_mart_row` — the row as a dict, or `None`.
///
/// Modelled as a `Map` rather than a struct because Python's consumers use
/// `.get(k) or 0` on it and one of them ([`merge_project_mart_rows`]) builds a
/// *different* dict with the same keys; a struct would force a second type and
/// a conversion between them for no gain.
fn get_project_mart_row(
    conn: &Connection,
    project_id: i64,
) -> rusqlite::Result<Option<Map<String, Value>>> {
    if !table_exists(conn, "project_mart")? {
        return Ok(None);
    }
    let mut stmt = conn.prepare(
        "SELECT project_id, provider, slug, display_name, first_ts, last_ts, \
                total_messages, total_sessions, total_input_tokens, \
                total_output_tokens, total_cache_read, total_cache_create, \
                total_cost_usd, \
                total_user_messages, total_assistant_messages, \
                total_tool_use_messages, total_tool_result_messages, \
                total_commands, \
                total_records, total_errors, errors_by_category, \
                total_cache_read_messages, total_commands_followed_by_interruption, \
                total_command_tools, total_command_steps \
         FROM project_mart WHERE project_id = ?",
    )?;
    let names: Vec<String> = stmt.column_names().into_iter().map(str::to_owned).collect();
    let mut rows = stmt.query([project_id])?;
    let Some(row) = rows.next()? else {
        return Ok(None);
    };
    let mut out = Map::new();
    for (index, name) in names.iter().enumerate() {
        out.insert(name.clone(), sqlite_to_json(row, index)?);
    }
    Ok(Some(out))
}

/// One SQLite cell as the JSON value `sqlite3.Row` would hand Python.
fn sqlite_to_json(row: &rusqlite::Row<'_>, index: usize) -> rusqlite::Result<Value> {
    Ok(match row.get_ref(index)? {
        rusqlite::types::ValueRef::Null => Value::Null,
        rusqlite::types::ValueRef::Integer(i) => Value::from(i),
        rusqlite::types::ValueRef::Real(f) => {
            serde_json::Number::from_f64(f).map_or(Value::Null, Value::Number)
        }
        rusqlite::types::ValueRef::Text(t) => {
            Value::String(String::from_utf8_lossy(t).into_owned())
        }
        rusqlite::types::ValueRef::Blob(_) => Value::Null,
    })
}

/// One `daily_mart` row.
#[derive(Debug, Clone)]
struct DailyRow {
    day: Option<String>,
    provider: Option<String>,
    model: Option<String>,
    speed: Option<String>,
    input_tokens: i64,
    output_tokens: i64,
    cache_read: i64,
    cache_create: i64,
    message_count: i64,
    session_count: i64,
    cost_usd: f64,
}

/// `mart_queries.daily_for_project`.
///
/// The filters are folded into the SQL exactly as Python builds them —
/// `LOWER(provider) IN (…)` / `LOWER(model) IN (…)` — and the trailing
/// `ORDER BY day` is what makes the caller's `setdefault` insertion order
/// deterministic.
fn daily_for_project(
    conn: &Connection,
    project_id: i64,
    provider_filter: Option<&BTreeSet<String>>,
    model_filter: Option<&BTreeSet<String>>,
) -> rusqlite::Result<Vec<DailyRow>> {
    if !table_exists(conn, "daily_mart")? {
        return Ok(Vec::new());
    }
    let mut sql = String::from(
        "SELECT day, project_id, provider, model, speed, \
                input_tokens, output_tokens, cache_read, cache_create, \
                message_count, session_count, cost_usd \
         FROM daily_mart WHERE project_id = ?",
    );
    let mut params: Vec<rusqlite::types::Value> = vec![project_id.into()];
    // `if provider_filter:` — an EMPTY set is falsy and adds no clause at all,
    // which is why the callers pass `None` rather than an empty set.
    if let Some(filter) = provider_filter.filter(|f| !f.is_empty()) {
        sql.push_str(&in_clause(" AND LOWER(provider) IN ", filter.len()));
        params.extend(filter.iter().map(|v| v.to_lowercase().into()));
    }
    if let Some(filter) = model_filter.filter(|f| !f.is_empty()) {
        sql.push_str(&in_clause(" AND LOWER(model) IN ", filter.len()));
        params.extend(filter.iter().map(|v| v.to_lowercase().into()));
    }
    sql.push_str(" ORDER BY day");

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(rusqlite::params_from_iter(params), |row| {
        Ok(DailyRow {
            day: row.get(0)?,
            provider: row.get(2)?,
            model: row.get(3)?,
            speed: row.get(4)?,
            input_tokens: row.get::<_, Option<i64>>(5)?.unwrap_or(0),
            output_tokens: row.get::<_, Option<i64>>(6)?.unwrap_or(0),
            cache_read: row.get::<_, Option<i64>>(7)?.unwrap_or(0),
            cache_create: row.get::<_, Option<i64>>(8)?.unwrap_or(0),
            message_count: row.get::<_, Option<i64>>(9)?.unwrap_or(0),
            session_count: row.get::<_, Option<i64>>(10)?.unwrap_or(0),
            cost_usd: row.get::<_, Option<f64>>(11)?.unwrap_or(0.0),
        })
    })?;
    rows.collect()
}

/// `f" AND col IN ({','.join('?' * n)})"`.
fn in_clause(prefix: &str, count: usize) -> String {
    let placeholders = std::iter::repeat_n("?", count)
        .collect::<Vec<_>>()
        .join(",");
    format!("{prefix}({placeholders})")
}

/// `mart_queries.tool_mart_for_project`, narrowed to the one field
/// `_tools_usage_from_marts` reads.
///
/// The full helper returns eight fields per tool; `/api/dashboard-data` sums
/// only `calls` (`SUM(event_count)`, the 1/N attribution unit). Returned in
/// `GROUP BY tool_name` order so the caller's insertion order is SQLite's, as
/// Python's dict iteration is.
fn tool_mart_calls_for_project(
    conn: &Connection,
    project_id: i64,
) -> rusqlite::Result<Vec<(String, i64)>> {
    if !table_exists(conn, "tool_mart")? {
        return Ok(Vec::new());
    }
    let mut stmt = conn.prepare(
        "SELECT tool_name, SUM(event_count) AS calls \
         FROM tool_mart WHERE project_id = ? GROUP BY tool_name",
    )?;
    let rows = stmt.query_map([project_id], |row| {
        Ok((
            row.get::<_, Option<String>>(0)?.unwrap_or_default(),
            row.get::<_, Option<i64>>(1)?.unwrap_or(0),
        ))
    })?;
    // `if not name: continue` — an empty tool name contributes nothing.
    Ok(rows
        .collect::<rusqlite::Result<Vec<_>>>()?
        .into_iter()
        .filter(|(name, _)| !name.is_empty())
        .collect())
}

/// `mart_queries.mart_has_command_day_rows`.
fn mart_has_command_day_rows(conn: &Connection) -> rusqlite::Result<bool> {
    if !table_exists(conn, "command_day_mart")? {
        return Ok(false);
    }
    let mut stmt = conn.prepare("SELECT 1 FROM command_day_mart LIMIT 1")?;
    let mut rows = stmt.query([])?;
    Ok(rows.next()?.is_some())
}

/// `mart_queries.command_count_in_window`.
fn command_count_in_window(
    conn: &Connection,
    project_ids: &[i64],
    day_from: Option<&str>,
    day_to: Option<&str>,
) -> rusqlite::Result<i64> {
    if project_ids.is_empty() || !table_exists(conn, "command_day_mart")? {
        return Ok(0);
    }
    let mut sql = format!(
        "SELECT COALESCE(SUM(command_count), 0) AS c FROM command_day_mart WHERE project_id IN ({})",
        std::iter::repeat_n("?", project_ids.len())
            .collect::<Vec<_>>()
            .join(",")
    );
    let mut params: Vec<rusqlite::types::Value> =
        project_ids.iter().map(|id| (*id).into()).collect();
    if let Some(day) = day_from.filter(|d| !d.is_empty()) {
        sql.push_str(" AND day >= ?");
        params.push(day.to_owned().into());
    }
    if let Some(day) = day_to.filter(|d| !d.is_empty()) {
        sql.push_str(" AND day <= ?");
        params.push(day.to_owned().into());
    }
    let mut stmt = conn.prepare(&sql)?;
    stmt.query_row(rusqlite::params_from_iter(params), |row| {
        Ok(row.get::<_, Option<i64>>(0)?.unwrap_or(0))
    })
}

// ── GET /api/dashboard-data ──────────────────────────────────────────────────

async fn get_dashboard_data(
    State(state): State<AppState>,
    RawQuery(raw): RawQuery,
) -> HandlerResult {
    let query = Query::parse(raw.as_deref().unwrap_or_default());
    let timezone_offset = match query.int_or("timezone_offset", 0) {
        Ok(value) => value,
        Err(err) => return Ok(validation_422(&err)),
    };
    let provider_filter = normalise_filter(query.opt_list("provider"));
    let model_filter = normalise_filter(query.opt_list("model"));

    let log_path = require_project(&state)?;

    let worker = state.clone();
    let outcome = tokio::task::spawn_blocking(move || {
        dashboard_statistics(
            &worker,
            &log_path,
            timezone_offset,
            provider_filter.as_ref(),
            model_filter.as_ref(),
        )
    })
    .await
    .map_err(|err| join_failure(&err))??;

    let currency = active_currency_payload(&state.config().currency)
        .map_err(|err| HttpError::new(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))?;

    // The provider-excluded early return is a DIFFERENT literal from the normal
    // body: its `messages_page` is a four-key dict (no `total_pages` /
    // `start_index` / `end_index`), it carries `"filtered": true`, and its key
    // order puts `currency` before `filtered`. Reproduced literally.
    let Dashboard::Payload {
        messages,
        statistics,
    } = outcome
    else {
        let mut page = Map::new();
        page.insert("messages".to_owned(), Value::Array(Vec::new()));
        page.insert("page".to_owned(), Value::from(1));
        page.insert("per_page".to_owned(), Value::from(50));
        page.insert("total".to_owned(), Value::from(0));

        let mut body = Map::new();
        body.insert("statistics".to_owned(), Value::Object(Map::new()));
        body.insert("messages_page".to_owned(), Value::Object(page));
        body.insert("message_count".to_owned(), Value::from(0));
        body.insert(
            "is_reindexing".to_owned(),
            Value::Bool(state.is_reindexing()),
        );
        body.insert("config".to_owned(), config_block(&state));
        body.insert("currency".to_owned(), currency);
        body.insert("filtered".to_owned(), Value::Bool(true));
        return Ok(JsonBody::ok(Value::Object(body)));
    };

    // `first_page = get_paginated_messages(messages, page=1, per_page=50)`.
    // On the mart path `messages` is `[]` by construction (§A3: dashboard-data
    // only ever exposed the first 50 and the marts do not carry message rows),
    // so this is the empty envelope — including the page-0 / negative
    // start_index shape `services::messages` documents.
    let message_count = i64::try_from(messages.len()).unwrap_or(i64::MAX);
    let first_page = messages_api::get_paginated_messages(messages, 1, 50);

    let mut body = Map::new();
    body.insert("statistics".to_owned(), statistics);
    body.insert("messages_page".to_owned(), first_page);
    body.insert("message_count".to_owned(), Value::from(message_count));
    body.insert(
        "is_reindexing".to_owned(),
        Value::Bool(state.is_reindexing()),
    );
    body.insert("config".to_owned(), config_block(&state));
    // `_apply_currency_to_stats` returns the payload UNCHANGED at rate 1.0, and
    // DIV-052 records that the non-USD branch is unreachable — so the deep-copy
    // + `_convert_in_place` walk is recorded, not ported.
    body.insert("currency".to_owned(), currency);
    Ok(JsonBody::ok(Value::Object(body)))
}

/// What the blocking body of `/api/dashboard-data` produced.
///
/// The provider-excluded early return is a wholly different body shape — a
/// four-key `messages_page`, an empty `statistics`, a `filtered` flag — so it is
/// signalled as its own variant rather than smuggled through as an empty
/// payload the caller has to detect.
enum Dashboard {
    /// The normal body's two moving parts.
    Payload {
        messages: Vec<Value>,
        statistics: Value,
    },
    /// `if provider_filter is not None and not project_ids:`.
    ProviderExcluded,
}

/// The blocking body of `/api/dashboard-data`.
fn dashboard_statistics(
    state: &AppState,
    log_path: &str,
    timezone_offset: i64,
    provider_filter: Option<&BTreeSet<String>>,
    model_filter: Option<&BTreeSet<String>>,
) -> Result<Dashboard, HttpError> {
    let conn = state.connect().map_err(|err| any_500(&err))?;
    let project_ids = filtered_project_ids(&conn, log_path, provider_filter)?;
    if provider_filter.is_some() && project_ids.is_empty() {
        return Ok(Dashboard::ProviderExcluded);
    }

    // The memo (`_DASHBOARD_CACHE`, an 8-entry LRU keyed `(slug, tz)` against a
    // `(MAX(last_ts), SUM(message_count))` sessions signature) is NOT ported —
    // the same call DIV-055 recorded for `/api/stats`. It is a pure latency
    // device: the hit path recomputes `is_reindexing`, `config`, the currency
    // stamp and the model filter from scratch, so a hit and a miss produce the
    // same bytes, and the signature moves the instant ingest writes. Recorded as
    // DIV-122 rather than reproduced, because reproducing it means holding
    // multi-megabyte payloads in a process nothing has measured for memory.
    let mart_ready = !project_ids.is_empty()
        && project_ids
            .iter()
            .try_fold(true, |acc, pid| {
                mart_has_project_row(&conn, *pid).map(|ok| acc && ok)
            })
            .map_err(sql_500)?;

    let engine = crate::pricing::engine(&conn, state.package_dir()).map_err(|err| any_500(&err))?;
    let (messages, mut stats) = if mart_ready {
        // `model_filter=None` — Python passes None here explicitly "for parity"
        // and applies the model filter to the finished `models` map below, so a
        // cached and an uncached payload filter identically.
        let stats = stats_from_marts(&conn, &project_ids, provider_filter, None, None, &engine)
            .map_err(sql_500)?;
        // `messages = []` — §A3: dashboard-data only ever exposed the first 50,
        // and the marts carry no message rows.
        (Vec::new(), stats)
    } else {
        // NOTE: `queries.get_project_stats` is called WITHOUT the tz clamp here
        // — `_project_stats_cached` is not in this path, so a `timezone_offset`
        // of 99999 reaches the aggregator raw. `/api/stats` and `/api/cost-data`
        // both clamp. Bug-for-bug; filed as DIV-124.
        stax_etl::stats::dataset::get_project_stats_with(
            &conn,
            &project_ids,
            timezone_offset,
            &engine,
        )
        .map_err(|err| any_500(&err))?
    };

    // §A3 + §D1 + §D2 — the lean-payload trims, in Python's order.
    let mut lean = Map::new();
    if let Value::Object(map) = &mut stats {
        for (key, value) in std::mem::take(map) {
            if COST_KEYS.contains(&key.as_str()) {
                continue;
            }
            lean.insert(key, value);
        }
    }
    if let Some(Value::Object(ui)) = lean.get("user_interactions") {
        let trimmed: Map<String, Value> = ui
            .iter()
            .filter(|(key, _)| {
                key.as_str() != "command_details" && key.as_str() != "tool_count_distribution"
            })
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        lean.insert("user_interactions".to_owned(), Value::Object(trimmed));
    }
    if let Some(filter) = model_filter
        && let Some(Value::Object(models)) = lean.get("models")
    {
        let kept: Map<String, Value> = models
            .iter()
            .filter(|(key, _)| filter.contains(&key.to_lowercase()))
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        lean.insert("models".to_owned(), Value::Object(kept));
    }

    Ok(Dashboard::Payload {
        messages,
        statistics: Value::Object(lean),
    })
}

// ── the mart-backed statistics block ─────────────────────────────────────────

/// `_merge_project_mart_rows` — per-provider rows into one lifetime total.
///
/// `if len(present) == 1: return present[0]` is not an optimisation: the
/// single-row result carries EVERY column (including `errors_by_category` and
/// `project_id`), while the merged result carries only the additive ones plus
/// `first_ts` / `last_ts` / `provider` / `slug` / `display_name`. Consumers only
/// read the intersection, so the difference is invisible — but reproducing the
/// short-circuit keeps it that way rather than betting on it.
fn merge_project_mart_rows(rows: &[Option<Map<String, Value>>]) -> Option<Map<String, Value>> {
    let present: Vec<&Map<String, Value>> = rows.iter().filter_map(Option::as_ref).collect();
    let first = *present.first()?;
    if present.len() == 1 {
        return Some(first.clone());
    }
    let mut merged = Map::new();
    for key in PROJECT_MART_ADDITIVE {
        if key.is_empty() {
            continue;
        }
        // `merged = {k: 0 for k in ...}` then `merged[k] += r.get(k) or 0`. The
        // seed is an int, so a column that is all-integer stays an int on the
        // wire and one carrying a float (`total_cost_usd`) becomes a float the
        // moment the first non-zero lands — Python's `0 + 1.5` is `1.5`.
        let mut acc = PyNum::Int(0);
        for row in &present {
            acc = py_add(acc, row.get(key));
        }
        merged.insert(key.to_owned(), acc.to_json());
    }
    // `min(first_seen) if first_seen else None` over the TRUTHY values only —
    // `if r.get("first_ts")` skips NULL and the empty string alike.
    merged.insert(
        "first_ts".to_owned(),
        truthy_strings(&present, "first_ts")
            .min()
            .map_or(Value::Null, Value::String),
    );
    merged.insert(
        "last_ts".to_owned(),
        truthy_strings(&present, "last_ts")
            .max()
            .map_or(Value::Null, Value::String),
    );
    for key in ["provider", "slug", "display_name"] {
        merged.insert(
            key.to_owned(),
            first.get(key).cloned().unwrap_or(Value::Null),
        );
    }
    Some(merged)
}

/// The truthy string values of `key` across `rows` — Python's `if r.get(k)`.
fn truthy_strings<'a>(
    rows: &'a [&'a Map<String, Value>],
    key: &'a str,
) -> impl Iterator<Item = String> + 'a {
    rows.iter().filter_map(move |row| {
        row.get(key)
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
    })
}

/// `acc += row.get(key) or 0`, preserving Python's int/float distinction.
fn py_add(acc: PyNum, value: Option<&Value>) -> PyNum {
    // `or 0` — a NULL, a zero and a missing key are all the int 0.
    let addend = match value {
        Some(Value::Number(n)) if n.is_f64() && n.as_f64() != Some(0.0) => {
            PyNum::Float(n.as_f64().unwrap_or(0.0))
        }
        Some(Value::Number(n)) => n.as_i64().map_or(PyNum::Int(0), PyNum::Int),
        _ => PyNum::Int(0),
    };
    match (acc, addend) {
        (PyNum::Int(a), PyNum::Int(b)) => PyNum::Int(a + b),
        (a, b) => PyNum::Float(a.as_f64() + b.as_f64()),
    }
}

/// `row.get(key, 0) or 0` cast to `int` — the shape most consumers use.
fn mart_int(row: Option<&Map<String, Value>>, key: &str) -> i64 {
    row.and_then(|r| r.get(key))
        .and_then(|v| {
            v.as_i64().or_else(|| {
                #[allow(clippy::cast_possible_truncation)]
                v.as_f64().map(|f| f as i64)
            })
        })
        .unwrap_or(0)
}

/// `_cache_block_from_mart`.
///
/// `hit_rate` is `round(w_read / asst * 100, 1)` guarded by `if asst`, and the
/// guard's fallback is the **float** `0.0` — so an assistant-less project emits
/// `0.0`, not `0`. `round` is CPython's ties-to-even, not `f64::round`.
fn cache_block_from_mart(merged: Option<&Map<String, Value>>, cost_saved_units: f64) -> Value {
    let mut block = Map::new();
    // `if not merged_row:` — the whole block collapses to one key.
    if merged.is_none() {
        block.insert("hit_rate".to_owned(), Value::from(0.0));
        return Value::Object(block);
    }
    let created = mart_int(merged, "total_cache_create");
    let read = mart_int(merged, "total_cache_read");
    let asst = mart_int(merged, "total_assistant_messages");
    let w_read = mart_int(merged, "total_cache_read_messages");
    #[allow(clippy::cast_precision_loss)]
    let hit_rate = if asst == 0 {
        0.0
    } else {
        round_py(w_read as f64 / asst as f64 * 100.0, 1)
    };
    block.insert("total_created".to_owned(), Value::from(created));
    block.insert("total_read".to_owned(), Value::from(read));
    block.insert("tokens_saved".to_owned(), Value::from(read - created));
    block.insert(
        "cost_saved_base_units".to_owned(),
        Value::from(round_py(cost_saved_units, 2)),
    );
    block.insert(
        "break_even_achieved".to_owned(),
        Value::Bool(read > created),
    );
    block.insert("hit_rate".to_owned(), Value::from(hit_rate));
    Value::Object(block)
}

/// `_cache_cost_saved_units_from_marts` + `aggregator.cache_cost_saved_base_units`.
///
/// Lifetime and UNFILTERED — `daily_for_project` is called with no provider or
/// model narrowing, to stay consistent with the merged `project_mart` totals the
/// rest of the cache block reports. The accumulation is `total_usd += …`, a
/// plain `+=` and not `sum()`, so it is NOT compensated here.
fn cache_cost_saved_units_from_marts(
    conn: &Connection,
    project_ids: &[i64],
    engine: &PricingEngine,
) -> rusqlite::Result<f64> {
    let mut total_usd = 0.0_f64;
    for pid in project_ids {
        for row in daily_for_project(conn, *pid, None, None)? {
            let model = row.model.clone().unwrap_or_default();
            let (read, created) = (row.cache_read, row.cache_create);
            // `if not model or (not read and not created): continue` in the
            // caller, then `if … or not model or model == "N/A": continue` in
            // the pricer. Both are ported; the second also catches "N/A".
            if model.is_empty() || (read == 0 && created == 0) || model == "N/A" {
                continue;
            }
            let provider = match row.provider.as_deref() {
                Some(p) if !p.is_empty() => p,
                _ => "anthropic",
            };
            let speed = match row.speed.as_deref() {
                Some(s) if !s.is_empty() => s,
                _ => "standard",
            };
            let breakdown = engine.compute_cost(
                &RawTokens::canonical(read + created, 0, created, read),
                &model,
                provider,
                speed,
                None,
            );
            total_usd +=
                breakdown.input_cost - breakdown.cache_read_cost - breakdown.cache_creation_cost;
        }
    }
    Ok(round_py(total_usd * CACHE_COST_BASE_UNIT_SCALE, 2))
}

/// `_parse_category_map` — `project_mart.errors_by_category` to a count map.
///
/// The column is a JSON object *string*; a merged multi-provider row may have
/// already parsed it. Malformed input yields an empty map rather than raising,
/// so one poison row cannot break the payload.
fn parse_category_map(value: Option<&Value>) -> Map<String, Value> {
    let parsed = match value {
        Some(Value::Object(map)) => Some(map.clone()),
        Some(Value::String(text)) if !text.is_empty() => serde_json::from_str::<Value>(text)
            .ok()
            .and_then(|v| v.as_object().cloned()),
        _ => None,
    };
    parsed.map_or_else(Map::new, |map| {
        map.into_iter()
            .map(|(key, value)| {
                // `int(v or 0)` — a NULL or a float both become an int.
                let count = value.as_i64().or_else(|| {
                    #[allow(clippy::cast_possible_truncation)]
                    value.as_f64().map(|f| f as i64)
                });
                (key, Value::from(count.unwrap_or(0)))
            })
            .collect()
    })
}

/// `_errors_block_from_marts` — built from the UNMERGED rows.
///
/// `rate` divides `total_errors` by `total_records` — the ALL-KINDS record
/// count the aggregator divides by, **not** the billable `total_messages`. The
/// zero guard yields the float `0.0`.
fn errors_block_from_marts(rows: &[Option<Map<String, Value>>]) -> Value {
    let mut total_errors = 0_i64;
    let mut total_records = 0_i64;
    let mut by_category: Map<String, Value> = Map::new();
    for row in rows.iter().flatten() {
        total_errors += mart_int(Some(row), "total_errors");
        total_records += mart_int(Some(row), "total_records");
        for (cat, count) in parse_category_map(row.get("errors_by_category")) {
            let next = by_category.get(&cat).and_then(Value::as_i64).unwrap_or(0)
                + count.as_i64().unwrap_or(0);
            by_category.insert(cat, Value::from(next));
        }
    }
    let mut block = Map::new();
    block.insert("total".to_owned(), Value::from(total_errors));
    #[allow(clippy::cast_precision_loss)]
    let rate = if total_records == 0 {
        0.0
    } else {
        total_errors as f64 / total_records as f64
    };
    block.insert("rate".to_owned(), Value::from(rate));
    block.insert("by_category".to_owned(), Value::Object(by_category));
    Value::Object(block)
}

/// `_user_interactions_from_mart`.
///
/// The `if not merged_row:` branch is ONE key, and the value of that key depends
/// on whether a window was active — `0 if windowed_commands is None else
/// windowed_commands`. The rates all guard on `if commands`, and each guard's
/// fallback is a float (`0.0`), while the counts stay ints.
fn user_interactions_from_mart(
    merged: Option<&Map<String, Value>>,
    windowed_commands: Option<i64>,
) -> Value {
    let mut block = Map::new();
    if merged.is_none() {
        block.insert(
            "user_commands_analyzed".to_owned(),
            Value::from(windowed_commands.unwrap_or(0)),
        );
        return Value::Object(block);
    }
    let commands = mart_int(merged, "total_commands");
    let int_followed = mart_int(merged, "total_commands_followed_by_interruption");
    let cmd_tools = mart_int(merged, "total_command_tools");
    let cmd_steps = mart_int(merged, "total_command_steps");
    #[allow(clippy::cast_precision_loss)]
    let ratio = |numerator: i64, scale: f64, digits: usize| -> f64 {
        if commands == 0 {
            0.0
        } else {
            round_py(numerator as f64 / commands as f64 * scale, digits)
        }
    };
    block.insert(
        "user_commands_analyzed".to_owned(),
        Value::from(windowed_commands.unwrap_or(commands)),
    );
    block.insert(
        "commands_followed_by_interruption".to_owned(),
        Value::from(int_followed),
    );
    block.insert("total_tools_used".to_owned(), Value::from(cmd_tools));
    block.insert("total_assistant_steps".to_owned(), Value::from(cmd_steps));
    block.insert(
        "interruption_rate".to_owned(),
        Value::from(ratio(int_followed, 100.0, 1)),
    );
    block.insert(
        "avg_tools_per_command".to_owned(),
        Value::from(ratio(cmd_tools, 1.0, 2)),
    );
    block.insert(
        "avg_steps_per_command".to_owned(),
        Value::from(ratio(cmd_steps, 1.0, 2)),
    );
    Value::Object(block)
}

/// `mart_queries.daily_mart_to_overview`.
///
/// Two branches. With a `project_mart` row the lifetime totals are trusted
/// wholesale; without one, everything is summed off the daily rows — and there
/// `cost = sum(float(...) for r in rows)` is a **`sum()`**, so it is
/// Neumaier-compensated AND it returns the *int* `0` on an empty sequence
/// (DIV-057). The `/api/dashboard-data` gate guarantees a mart row, so the
/// fallback is unreachable from this route; it is ported because the function is
/// shared in Python and a missing branch is a hole waiting for the next caller.
fn daily_mart_to_overview(
    rows: &[DailyRow],
    project_mart_row: Option<&Map<String, Value>>,
) -> Value {
    let mut out = Map::new();
    if let Some(row) = project_mart_row {
        let mut tokens = Map::new();
        tokens.insert(
            "input".to_owned(),
            Value::from(mart_int(Some(row), "total_input_tokens")),
        );
        tokens.insert(
            "output".to_owned(),
            Value::from(mart_int(Some(row), "total_output_tokens")),
        );
        tokens.insert(
            "cache_read".to_owned(),
            Value::from(mart_int(Some(row), "total_cache_read")),
        );
        tokens.insert(
            "cache_creation".to_owned(),
            Value::from(mart_int(Some(row), "total_cache_create")),
        );
        let mut range = Map::new();
        // `.get("first_ts")` with NO `or` — a NULL stays `None`, i.e. JSON null.
        range.insert(
            "start".to_owned(),
            row.get("first_ts").cloned().unwrap_or(Value::Null),
        );
        range.insert(
            "end".to_owned(),
            row.get("last_ts").cloned().unwrap_or(Value::Null),
        );
        let mut types = Map::new();
        for (json_key, column) in [
            ("user", "total_user_messages"),
            ("assistant", "total_assistant_messages"),
            ("tool_use", "total_tool_use_messages"),
            ("tool_result", "total_tool_result_messages"),
        ] {
            types.insert(
                json_key.to_owned(),
                Value::from(mart_int(Some(row), column)),
            );
        }
        out.insert("total_tokens".to_owned(), Value::Object(tokens));
        // `float(... or 0.0)` — always a float, even when the column is an int 0.
        out.insert(
            "total_cost".to_owned(),
            Value::from(
                row.get("total_cost_usd")
                    .and_then(Value::as_f64)
                    .unwrap_or(0.0),
            ),
        );
        out.insert("date_range".to_owned(), Value::Object(range));
        out.insert(
            "total_messages".to_owned(),
            Value::from(mart_int(Some(row), "total_messages")),
        );
        out.insert(
            "total_sessions".to_owned(),
            Value::from(mart_int(Some(row), "total_sessions")),
        );
        out.insert("message_types".to_owned(), Value::Object(types));
        return Value::Object(out);
    }

    let mut tokens = Map::new();
    tokens.insert(
        "input".to_owned(),
        Value::from(rows.iter().map(|r| r.input_tokens).sum::<i64>()),
    );
    tokens.insert(
        "output".to_owned(),
        Value::from(rows.iter().map(|r| r.output_tokens).sum::<i64>()),
    );
    tokens.insert(
        "cache_read".to_owned(),
        Value::from(rows.iter().map(|r| r.cache_read).sum::<i64>()),
    );
    tokens.insert(
        "cache_creation".to_owned(),
        Value::from(rows.iter().map(|r| r.cache_create).sum::<i64>()),
    );
    let mut cost = Neumaier::default();
    for row in rows {
        cost.add(row.cost_usd);
    }
    // `sorted({r["day"] for r in rows if r.get("day")})` — a SET, so duplicate
    // days collapse before the sort.
    let days: BTreeSet<&str> = rows
        .iter()
        .filter_map(|r| r.day.as_deref())
        .filter(|d| !d.is_empty())
        .collect();
    let mut range = Map::new();
    range.insert(
        "start".to_owned(),
        days.iter().next().map_or(Value::Null, |d| Value::from(*d)),
    );
    range.insert(
        "end".to_owned(),
        days.iter()
            .next_back()
            .map_or(Value::Null, |d| Value::from(*d)),
    );
    out.insert("total_tokens".to_owned(), Value::Object(tokens));
    // `sum()` over an EMPTY generator is the int 0, so this key is `0` and not
    // `0.0` on a project with no daily rows. DIV-057, visible in the JSON.
    out.insert("total_cost".to_owned(), cost.finish_pynum().to_json());
    out.insert("date_range".to_owned(), Value::Object(range));
    out.insert(
        "total_messages".to_owned(),
        Value::from(rows.iter().map(|r| r.message_count).sum::<i64>()),
    );
    out.insert("total_sessions".to_owned(), Value::from(0));
    out.insert("message_types".to_owned(), Value::Object(Map::new()));
    Value::Object(out)
}

/// `mart_queries.daily_mart_by_day` — the date-keyed `daily_stats` block.
///
/// Every accumulation here is an explicit `+=`, not `sum()`, so none of it is
/// compensated. Bucket insertion order is first-appearance across the
/// concatenated per-project queries, each of which is `ORDER BY day`.
fn daily_mart_by_day(rows: &[DailyRow]) -> Value {
    let mut order: Vec<String> = Vec::new();
    let mut buckets: Vec<DayBucket> = Vec::new();
    for row in rows {
        let Some(day) = row.day.as_deref().filter(|d| !d.is_empty()) else {
            continue;
        };
        let index = match order.iter().position(|seen| seen == day) {
            Some(index) => index,
            None => {
                order.push(day.to_owned());
                buckets.push(DayBucket::default());
                buckets.len() - 1
            }
        };
        let bucket = &mut buckets[index];
        bucket.cost_total += row.cost_usd;
        bucket.input += row.input_tokens;
        bucket.output += row.output_tokens;
        bucket.cache_read += row.cache_read;
        bucket.cache_creation += row.cache_create;
        bucket.messages += row.message_count;
        bucket.sessions += row.session_count;
        // `model = r.get("model") or ""` / `if model:` — a NULL model
        // contributes to the day's TOTAL but not to its by_model breakdown.
        if let Some(model) = row.model.as_deref().filter(|m| !m.is_empty()) {
            match bucket.by_model.iter_mut().find(|(name, _)| name == model) {
                Some(entry) => entry.1 += row.cost_usd,
                None => bucket.by_model.push((model.to_owned(), row.cost_usd)),
            }
        }
    }

    let mut out = Map::new();
    for (day, bucket) in order.into_iter().zip(buckets) {
        out.insert(day, bucket.to_json());
    }
    Value::Object(out)
}

/// One `daily_stats` entry, in `daily_mart_by_day`'s literal key order.
#[derive(Default)]
struct DayBucket {
    messages: i64,
    sessions: i64,
    input: i64,
    output: i64,
    cache_creation: i64,
    cache_read: i64,
    cost_total: f64,
    by_model: Vec<(String, f64)>,
}

impl DayBucket {
    fn to_json(&self) -> Value {
        let mut tokens = Map::new();
        tokens.insert("input".to_owned(), Value::from(self.input));
        tokens.insert("output".to_owned(), Value::from(self.output));
        tokens.insert(
            "cache_creation".to_owned(),
            Value::from(self.cache_creation),
        );
        tokens.insert("cache_read".to_owned(), Value::from(self.cache_read));

        let mut by_model = Map::new();
        for (name, cost) in &self.by_model {
            by_model.insert(name.clone(), Value::from(*cost));
        }
        let mut cost = Map::new();
        // The seed is the FLOAT 0.0 in Python's `setdefault` literal, so a day
        // whose rows all priced at zero still emits `0.0`, not `0`.
        cost.insert("total".to_owned(), Value::from(self.cost_total));
        cost.insert("by_model".to_owned(), Value::Object(by_model));

        let mut out = Map::new();
        out.insert("messages".to_owned(), Value::from(self.messages));
        out.insert("sessions".to_owned(), Value::from(self.sessions));
        out.insert("tokens".to_owned(), Value::Object(tokens));
        out.insert("cost".to_owned(), Value::Object(cost));
        // The remaining keys are seeded and never written by this path; they are
        // part of the frontend's `Record<string, DailyData>` contract.
        out.insert("user_commands".to_owned(), Value::from(0));
        out.insert("interrupted_commands".to_owned(), Value::from(0));
        out.insert("interruption_rate".to_owned(), Value::from(0.0));
        out.insert("errors".to_owned(), Value::from(0));
        out.insert("assistant_messages".to_owned(), Value::from(0));
        out.insert("error_rate".to_owned(), Value::from(0.0));
        Value::Object(out)
    }
}

/// `mart_queries.daily_mart_by_model` — the `models` map.
fn daily_mart_by_model(rows: &[DailyRow]) -> Value {
    let mut order: Vec<String> = Vec::new();
    let mut buckets: Vec<ModelBucket> = Vec::new();
    for row in rows {
        let Some(model) = row.model.as_deref().filter(|m| !m.is_empty()) else {
            continue;
        };
        let index = match order.iter().position(|seen| seen == model) {
            Some(index) => index,
            None => {
                order.push(model.to_owned());
                buckets.push(ModelBucket::default());
                buckets.len() - 1
            }
        };
        let bucket = &mut buckets[index];
        bucket.count += row.message_count;
        bucket.cost += row.cost_usd;
        bucket.input_tokens += row.input_tokens;
        bucket.output_tokens += row.output_tokens;
        bucket.cache_read += row.cache_read;
        bucket.cache_creation += row.cache_create;
    }
    let mut out = Map::new();
    for (model, bucket) in order.into_iter().zip(buckets) {
        let mut entry = Map::new();
        entry.insert("count".to_owned(), Value::from(bucket.count));
        entry.insert("cost".to_owned(), Value::from(bucket.cost));
        entry.insert("input_tokens".to_owned(), Value::from(bucket.input_tokens));
        entry.insert(
            "output_tokens".to_owned(),
            Value::from(bucket.output_tokens),
        );
        entry.insert("cache_read".to_owned(), Value::from(bucket.cache_read));
        entry.insert(
            "cache_creation".to_owned(),
            Value::from(bucket.cache_creation),
        );
        out.insert(model, Value::Object(entry));
    }
    Value::Object(out)
}

/// One `models` entry, in `daily_mart_by_model`'s literal key order.
#[derive(Default)]
struct ModelBucket {
    count: i64,
    cost: f64,
    input_tokens: i64,
    output_tokens: i64,
    cache_read: i64,
    cache_creation: i64,
}

/// `_stats_from_marts` — the whole `statistics` block from mart reads only.
///
/// `day_from` / `day_to` are Python parameters that `/api/dashboard-data` never
/// passes, so the windowed-Commands branch below is unreachable from this route
/// today. It is ported, and the two mart helpers it needs with it, because the
/// function is shared and the branch is one `if` away from being live.
///
/// The keys that no mart carries — hour-of-day `hourly_pattern`, the per-tool
/// error flags — are emitted shape-stable rather than omitted. `hourly_pattern`
/// is specifically `{"messages": {}, "tokens": {}}` and NOT a bare `[]`: an
/// empty list is truthy in JS, so it would dodge the frontend's
/// `?? {messages, tokens}` fallback and render a blank chart.
fn stats_from_marts(
    conn: &Connection,
    project_ids: &[i64],
    provider_filter: Option<&BTreeSet<String>>,
    day_from: Option<&str>,
    day_to: Option<&str>,
    engine: &PricingEngine,
) -> rusqlite::Result<Value> {
    let proj_rows: Vec<Option<Map<String, Value>>> = project_ids
        .iter()
        .map(|pid| get_project_mart_row(conn, *pid))
        .collect::<rusqlite::Result<_>>()?;
    let merged = merge_project_mart_rows(&proj_rows);

    let mut daily_rows: Vec<DailyRow> = Vec::new();
    for pid in project_ids {
        daily_rows.extend(daily_for_project(conn, *pid, provider_filter, None)?);
    }

    let overview = daily_mart_to_overview(&daily_rows, merged.as_ref());
    let daily_stats = daily_mart_by_day(&daily_rows);
    let models = daily_mart_by_model(&daily_rows);

    // `_tools_usage_from_marts` — summed across every provider id.
    let mut tool_order: Vec<String> = Vec::new();
    let mut tool_calls: Vec<i64> = Vec::new();
    for pid in project_ids {
        for (name, calls) in tool_mart_calls_for_project(conn, *pid)? {
            match tool_order.iter().position(|seen| *seen == name) {
                Some(index) => tool_calls[index] += calls,
                None => {
                    tool_order.push(name);
                    tool_calls.push(calls);
                }
            }
        }
    }
    let usage_counts: Map<String, Value> = tool_order
        .into_iter()
        .zip(tool_calls)
        .map(|(name, calls)| (name, Value::from(calls)))
        .collect();

    // `if (day_from or day_to) and mart_has_command_day_rows(conn)` — with no
    // window this is `None`, which keeps the LIFETIME command count rather than
    // dropping the KPI to zero.
    let windowed = day_from.is_some_and(|d| !d.is_empty()) || day_to.is_some_and(|d| !d.is_empty());
    let windowed_commands: Option<i64> = if windowed && mart_has_command_day_rows(conn)? {
        Some(command_count_in_window(
            conn,
            project_ids,
            day_from,
            day_to,
        )?)
    } else {
        None
    };

    let mut tools = Map::new();
    tools.insert("usage_counts".to_owned(), Value::Object(usage_counts));
    tools.insert("error_counts".to_owned(), Value::Object(Map::new()));
    tools.insert("error_rates".to_owned(), Value::Object(Map::new()));

    let mut sessions = Map::new();
    sessions.insert(
        "count".to_owned(),
        Value::from(
            merged
                .as_ref()
                .map_or(0, |row| mart_int(Some(row), "total_sessions")),
        ),
    );

    let mut hourly = Map::new();
    hourly.insert("messages".to_owned(), Value::Object(Map::new()));
    hourly.insert("tokens".to_owned(), Value::Object(Map::new()));

    let cost_saved = cache_cost_saved_units_from_marts(conn, project_ids, engine)?;

    let mut out = Map::new();
    out.insert("overview".to_owned(), overview);
    out.insert("tools".to_owned(), Value::Object(tools));
    out.insert("sessions".to_owned(), Value::Object(sessions));
    out.insert("daily_stats".to_owned(), daily_stats);
    out.insert("hourly_pattern".to_owned(), Value::Object(hourly));
    out.insert("errors".to_owned(), errors_block_from_marts(&proj_rows));
    out.insert("models".to_owned(), models);
    out.insert(
        "user_interactions".to_owned(),
        user_interactions_from_mart(merged.as_ref(), windowed_commands),
    );
    out.insert(
        "cache".to_owned(),
        cache_block_from_mart(merged.as_ref(), cost_saved),
    );
    Ok(Value::Object(out))
}

// ── GET /api/messages ────────────────────────────────────────────────────────

async fn get_messages(State(state): State<AppState>, RawQuery(raw): RawQuery) -> HandlerResult {
    let query = Query::parse(raw.as_deref().unwrap_or_default());
    // Python declares these in signature order, and FastAPI validates in that
    // same order — so a request with TWO bad parameters reports only the first,
    // and which one that is depends on the declaration order, not the query
    // string's. `?per_page=x&page=y` reports `page`.
    let mut page = match query.int_or("page", 1) {
        Ok(value) => value,
        Err(err) => return Ok(validation_422(&err)),
    };
    let mut per_page = match query.int_or("per_page", MESSAGES_DEFAULT_PER_PAGE) {
        Ok(value) => value,
        Err(err) => return Ok(validation_422(&err)),
    };
    let limit = match query.opt_int("limit") {
        Ok(value) => value,
        Err(err) => return Ok(validation_422(&err)),
    };
    // `timezone_offset` is declared, coerced (so `?timezone_offset=abc` is a
    // 422) and then never read. Ported as a parse-only parameter, because the
    // 422 is observable and dropping the parameter would turn it into a 200.
    let _timezone_offset = match query.int_or("timezone_offset", 0) {
        Ok(value) => value,
        Err(err) => return Ok(validation_422(&err)),
    };
    let provider_filter = normalise_filter(query.opt_list("provider"));
    let model_filter = normalise_filter(query.opt_list("model"));

    let log_path = require_project(&state)?;

    if page < 1 {
        page = 1;
    }
    // The legacy alias: `?limit=N` becomes `per_page` ONLY when the caller did
    // not set their own `per_page`. The test is against the DEFAULT value, so
    // `?limit=5&per_page=100` keeps 100 — an explicit 100 is indistinguishable
    // from no per_page at all. Bug-for-bug.
    if let Some(limit) = limit
        && per_page == MESSAGES_DEFAULT_PER_PAGE
    {
        per_page = limit;
    }
    per_page = per_page.clamp(1, MESSAGES_MAX_PER_PAGE);

    let worker = state.clone();
    let payload = tokio::task::spawn_blocking(move || {
        messages_page(
            &worker,
            &log_path,
            page,
            per_page,
            provider_filter.as_ref(),
            model_filter.as_ref(),
        )
    })
    .await
    .map_err(|err| join_failure(&err))??;

    Ok(JsonBody::ok(payload))
}

/// `_empty_messages_page` — the shape-stable envelope a filter-excluded project
/// gets. Note it is NOT [`messages_api::build_messages_page`]'s output: it
/// hard-codes `total_pages`, `start_index` and `end_index` to `0`, where the
/// real builder would have produced page `0` and a negative start.
fn empty_messages_page(page: i64, per_page: i64) -> Value {
    let mut out = Map::new();
    out.insert("messages".to_owned(), Value::Array(Vec::new()));
    out.insert("total".to_owned(), Value::from(0));
    out.insert("page".to_owned(), Value::from(page));
    out.insert("per_page".to_owned(), Value::from(per_page));
    out.insert("total_pages".to_owned(), Value::from(0));
    out.insert("start_index".to_owned(), Value::from(0));
    out.insert("end_index".to_owned(), Value::from(0));
    Value::Object(out)
}

fn messages_page(
    state: &AppState,
    log_path: &str,
    page: i64,
    per_page: i64,
    provider_filter: Option<&BTreeSet<String>>,
    model_filter: Option<&BTreeSet<String>>,
) -> Result<Value, HttpError> {
    let conn = state.connect().map_err(|err| any_500(&err))?;
    let project_ids = filtered_project_ids(&conn, log_path, provider_filter)?;
    if provider_filter.is_some() && project_ids.is_empty() {
        return Ok(empty_messages_page(page, per_page));
    }
    let total = count_project_messages(&conn, &project_ids, model_filter).map_err(sql_500)?;
    // The clamp is applied to compute the SQL OFFSET, and the ORIGINAL `page` is
    // handed to the envelope builder, which clamps it again. Same math twice, so
    // the OFFSET and the reported indices cannot disagree.
    let (_page, _pages, start_index, _end) = messages_api::page_bounds(total, page, per_page);
    let page_messages =
        project_messages_page(&conn, &project_ids, start_index, per_page, model_filter)
            .map_err(|err| any_500(&err))?;
    Ok(messages_api::build_messages_page(
        page_messages,
        total,
        page,
        per_page,
    ))
}

/// `queries.count_project_messages`.
///
/// The list subquery is §6b law: against the partitioned `messages` VIEW a JOIN
/// to `sessions` makes the planner materialise the whole view and build a
/// transient index (~3.6 s on a 44K-message project); the subquery lets each
/// partition use its `(session_fk, seq)` index instead (~5 ms). Only the project
/// ids are bound, so a project with thousands of sessions never approaches the
/// SQL variable limit.
fn count_project_messages(
    conn: &Connection,
    project_ids: &[i64],
    model_filter: Option<&BTreeSet<String>>,
) -> rusqlite::Result<i64> {
    if project_ids.is_empty() {
        return Ok(0);
    }
    let placeholders = std::iter::repeat_n("?", project_ids.len())
        .collect::<Vec<_>>()
        .join(",");
    let mut sql = format!(
        "SELECT COUNT(*) FROM messages m \
         WHERE m.session_fk IN (SELECT id FROM sessions WHERE project_id IN ({placeholders}))"
    );
    let mut params: Vec<rusqlite::types::Value> =
        project_ids.iter().map(|id| (*id).into()).collect();
    if let Some(filter) = model_filter.filter(|f| !f.is_empty()) {
        sql.push_str(&in_clause(
            " AND lower(COALESCE(m.model, '')) IN ",
            filter.len(),
        ));
        // `params.extend(sorted(model_filter))` — a BTreeSet already iterates
        // sorted, which is the same bound order.
        params.extend(filter.iter().map(|v| v.clone().into()));
    }
    let mut stmt = conn.prepare(&sql)?;
    stmt.query_row(rusqlite::params_from_iter(params), |row| row.get(0))
}

/// One row of the hydration pass in [`project_messages_page`].
///
/// A named struct rather than the four-tuple the SQL naturally yields: the
/// fields are three `Option<String>`s and a `String`, and a tuple of those is
/// exactly the shape where a transposed destructuring compiles and silently
/// swaps `session_id` for `provider`.
struct HydratedMessage {
    raw_json: Option<String>,
    session_id: String,
    timestamp: Option<String>,
    provider: Option<String>,
}

/// `queries.get_project_messages_page` — reconstruct ONE page.
///
/// Two cheap steps rather than building the whole-project dataset and slicing:
/// page the row ids over indexed columns (`raw_json` untouched, so the
/// sort/offset cost is proportional to lightweight columns), then hydrate
/// `raw_json` for just those ids. The page then runs through the SAME classifier
/// + enricher + formatter the full path uses.
///
/// The dataset is built with **no interactions**, which is what drops the three
/// `interaction_*` stamps from the output — they need whole-project grouping and
/// no `/api/messages` consumer reads them. Reproduced by emptying the
/// interaction list after the build, since the grouping is not separable in this
/// port; the records themselves are bit-identical either way.
fn project_messages_page(
    conn: &Connection,
    project_ids: &[i64],
    offset: i64,
    limit: i64,
    model_filter: Option<&BTreeSet<String>>,
) -> anyhow::Result<Vec<Value>> {
    if project_ids.is_empty() || limit <= 0 {
        return Ok(Vec::new());
    }
    let offset = offset.max(0);
    let placeholders = std::iter::repeat_n("?", project_ids.len())
        .collect::<Vec<_>>()
        .join(",");

    let mut id_sql = format!(
        "SELECT m.id FROM messages m \
         WHERE m.session_fk IN (SELECT id FROM sessions WHERE project_id IN ({placeholders}))"
    );
    let mut id_params: Vec<rusqlite::types::Value> =
        project_ids.iter().map(|id| (*id).into()).collect();
    if let Some(filter) = model_filter.filter(|f| !f.is_empty()) {
        id_sql.push_str(&in_clause(
            " AND lower(COALESCE(m.model, '')) IN ",
            filter.len(),
        ));
        id_params.extend(filter.iter().map(|v| v.clone().into()));
    }
    // `(timestamp, id)` — `id` is a stable, globally-unique tiebreaker, so pages
    // never overlap or skip a row when timestamps collide.
    id_sql.push_str(" ORDER BY m.timestamp, m.id LIMIT ? OFFSET ?");
    id_params.push(limit.into());
    id_params.push(offset.into());

    let page_ids: Vec<i64> = {
        let mut stmt = conn.prepare(&id_sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(id_params), |row| row.get(0))?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    if page_ids.is_empty() {
        return Ok(Vec::new());
    }

    let id_placeholders = std::iter::repeat_n("?", page_ids.len())
        .collect::<Vec<_>>()
        .join(",");
    let hydrate_sql = format!(
        "SELECT m.id AS id, m.raw_json AS raw_json, s.session_id AS session_id, \
                m.timestamp AS timestamp, p.provider AS provider \
         FROM messages m \
         JOIN sessions s ON s.id = m.session_fk \
         JOIN projects p ON s.project_id = p.id \
         WHERE m.id IN ({id_placeholders})"
    );
    let mut by_id: std::collections::HashMap<i64, HydratedMessage> =
        std::collections::HashMap::with_capacity(page_ids.len());
    {
        let mut stmt = conn.prepare(&hydrate_sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(page_ids.iter()), |row| {
            Ok((
                row.get::<_, i64>(0)?,
                HydratedMessage {
                    raw_json: row.get(1)?,
                    session_id: row.get(2)?,
                    timestamp: row.get(3)?,
                    provider: row.get(4)?,
                },
            ))
        })?;
        for row in rows {
            let (id, hydrated) = row?;
            by_id.insert(id, hydrated);
        }
    }

    let mut raw_entries: Vec<stax_etl::stats::classifier::RawEntry> =
        Vec::with_capacity(page_ids.len());
    // Restore `(timestamp, id)` order — `IN ()` does not preserve it.
    for mid in &page_ids {
        let Some(hydrated) = by_id.get(mid) else {
            continue;
        };
        let HydratedMessage {
            raw_json,
            session_id,
            timestamp,
            provider,
        } = hydrated;
        // `json.loads` with NO try — a poison blob raises and the request 500s
        // on the Python side. `build_enriched_dataset` (DIV-064) skips and
        // counts instead; the same choice is made here, for the same reason: a
        // store with one is a store Python cannot serve at all.
        let Some(mut payload) = raw_json
            .as_deref()
            .and_then(|text| serde_json::from_str::<Value>(text).ok())
        else {
            continue;
        };
        if let Some(ts) = timestamp.as_deref().filter(|ts| !ts.is_empty())
            && let Some(obj) = payload.as_object_mut()
        {
            obj.insert("timestamp".to_owned(), Value::String(ts.to_owned()));
        }
        raw_entries.push(stax_etl::stats::classifier::RawEntry {
            payload,
            session_id: session_id.clone(),
            provider: match provider.as_deref() {
                Some(p) if !p.is_empty() => p.to_owned(),
                _ => "anthropic".to_owned(),
            },
        });
    }

    let tagged = stax_etl::stats::classifier::tag(raw_entries);
    let mut dataset = stax_etl::stats::enricher::build_detailed(tagged);
    // `EnrichedDataset(records=records, interactions=[], sessions={})`.
    dataset.interactions.clear();
    Ok(stax_etl::stats::formatter::to_dicts(&dataset, None))
}

// ── GET /api/messages/summary ────────────────────────────────────────────────

async fn get_messages_summary_endpoint(State(state): State<AppState>) -> HandlerResult {
    let log_path = require_project(&state)?;
    let worker = state.clone();
    let payload = tokio::task::spawn_blocking(move || summarise_messages(&worker, &log_path))
        .await
        .map_err(|err| join_failure(&err))??;
    Ok(JsonBody::ok(payload))
}

fn summarise_messages(state: &AppState, log_path: &str) -> Result<Value, HttpError> {
    let conn = state.connect().map_err(|err| any_500(&err))?;
    let project_ids: Vec<i64> = project_rows(&conn, log_path)?
        .into_iter()
        .map(|row| row.id)
        .collect();

    let mart_ready = !project_ids.is_empty()
        && project_ids
            .iter()
            .try_fold(true, |acc, pid| {
                mart_has_project_row(&conn, *pid).map(|ok| acc && ok)
            })
            .map_err(sql_500)?;
    if mart_ready {
        return messages_summary_from_marts(&conn, &project_ids).map_err(sql_500);
    }

    // The slow path runs the whole pipeline — including an `aggregator.summarise`
    // whose result this route then DISCARDS. Ported as written: the discard is
    // free here, and narrowing it would change which rows the classifier sees.
    let engine = crate::pricing::engine(&conn, state.package_dir()).map_err(|err| any_500(&err))?;
    let (messages, _stats) =
        stax_etl::stats::dataset::get_project_stats_with(&conn, &project_ids, 0, &engine)
            .map_err(|err| any_500(&err))?;
    Ok(messages_api::get_messages_summary(&messages))
}

/// `_messages_summary_from_marts`.
///
/// `total` is the summed `project_mart.total_records` — every stored record,
/// which is what `len(messages)` meant on the legacy path. It is deliberately
/// NOT `total_messages`: that column counts BILLABLE EVENTS, so the predecessor
/// returned a total that contradicted its own `by_type`.
///
/// `by_type` carries a key only when its count is non-zero (`if users:`), so an
/// assistant-only project emits a ONE-key `by_type`. The tool-use / tool-result
/// columns are deliberately absent: in the legacy classifier they are
/// overlapping flags rather than a partition, and adding them would break
/// `sum(by_type) == total`.
fn messages_summary_from_marts(conn: &Connection, project_ids: &[i64]) -> rusqlite::Result<Value> {
    let (mut total, mut users, mut assistants, mut sessions) = (0_i64, 0_i64, 0_i64, 0_i64);
    for pid in project_ids {
        let Some(row) = get_project_mart_row(conn, *pid)? else {
            continue;
        };
        total += mart_int(Some(&row), "total_records");
        users += mart_int(Some(&row), "total_user_messages");
        assistants += mart_int(Some(&row), "total_assistant_messages");
        sessions += mart_int(Some(&row), "total_sessions");
    }

    let mut by_type = Map::new();
    if users != 0 {
        by_type.insert("user".to_owned(), Value::from(users));
    }
    if assistants != 0 {
        by_type.insert("assistant".to_owned(), Value::from(assistants));
    }

    let (by_model, total_tokens) = summary_by_model_and_tokens(conn, project_ids)?;

    let mut out = Map::new();
    out.insert("total".to_owned(), Value::from(total));
    out.insert("by_type".to_owned(), Value::Object(by_type));
    out.insert("by_model".to_owned(), Value::Object(by_model));
    out.insert("total_tokens".to_owned(), Value::from(total_tokens));
    out.insert("total_sessions".to_owned(), Value::from(sessions));
    Ok(Value::Object(out))
}

/// `_summary_by_model_and_tokens` — one scoped `GROUP BY` over `messages`.
///
/// Same list-subquery idiom as [`count_project_messages`], and for the same
/// reason. A row with no model is keyed `"N/A"`, because that is the
/// `Record.model` default the enricher stamps and therefore the key the legacy
/// pass produced.
///
/// One known, bounded divergence Python already records: Claude Code's
/// `"<synthetic>"` sentinel is stripped to NULL by the ingest adapter, so those
/// rows land in `"N/A"` here while the legacy pass — which re-parsed `raw_json`
/// — gave them their own bucket. Measured at 16 of 31,893 rows (0.05%) on the
/// largest local project. Inherited, not fixed.
fn summary_by_model_and_tokens(
    conn: &Connection,
    project_ids: &[i64],
) -> rusqlite::Result<(Map<String, Value>, i64)> {
    if project_ids.is_empty() {
        return Ok((Map::new(), 0));
    }
    let placeholders = std::iter::repeat_n("?", project_ids.len())
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!(
        "SELECT COALESCE(NULLIF(m.model, ''), 'N/A') AS model, COUNT(*) AS n, \
                SUM(COALESCE(m.input_tokens, 0) + COALESCE(m.output_tokens, 0)) AS tok \
         FROM messages m \
         WHERE m.session_fk IN (SELECT id FROM sessions WHERE project_id IN ({placeholders})) \
         GROUP BY 1"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(rusqlite::params_from_iter(project_ids.iter()), |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, Option<i64>>(1)?.unwrap_or(0),
            row.get::<_, Option<i64>>(2)?.unwrap_or(0),
        ))
    })?;
    let mut by_model = Map::new();
    let mut total_tokens = 0_i64;
    for row in rows {
        let (model, count, tokens) = row?;
        by_model.insert(model, Value::from(count));
        total_tokens += tokens;
    }
    Ok((by_model, total_tokens))
}

// ── POST /api/refresh ────────────────────────────────────────────────────────

/// `POST /api/refresh` — RS-5-070. **The only writer in the wave-5 surface.**
///
/// The body parameter is `request: dict`, so FastAPI requires a JSON *object*:
/// no body, or a body that is not an object, is a 422 before the handler runs.
/// The value is then never read by either branch — it exists only to make the
/// endpoint a POST with a body.
///
/// Which branch runs depends on `deps.current_log_path`, and the two produce
/// DIFFERENT bodies: the per-project one has `message_count`, the all-projects
/// one has `projects_refreshed` and `total_projects`. Both carry
/// `refresh_time_ms`, a wall-clock measurement — which is why this endpoint has
/// its own differ procedure (`rust/REFRESH-DIFFER.md`) and no case row.
async fn refresh_data(State(state): State<AppState>, body: axum::body::Bytes) -> HandlerResult {
    // The body is taken as raw bytes rather than through `axum::Json`, which
    // would need the `json` feature and would still not produce pydantic's
    // error shape. FastAPI validates `request: dict` BEFORE the handler runs, so
    // a rejection here never reaches the ingest pass — which is what makes a 422
    // probe safe to issue against a live server.
    // Returned as `Ok` rather than `Err` on purpose: the wire bytes are a 422
    // either way, and `HttpError` models FastAPI's *single-string* `detail`,
    // while a validation error's `detail` is a LIST.
    if let Err(rejection) = crate::json::dict_body(&body) {
        return Ok(rejection);
    }

    let per_project = state
        .current_project()
        .log_path
        .is_some_and(|p| !p.is_empty());
    let worker = state.clone();
    let payload = tokio::task::spawn_blocking(move || run_refresh(&worker, per_project))
        .await
        .map_err(|err| join_failure(&err))??;
    Ok(JsonBody::ok(payload))
}

// DIV-127 IS CLOSED, and DIV-367 is why.
//
// `dict_body_required` used to live here: FastAPI's three rejections for a
// `request: dict` parameter (`missing`, `json_invalid`, `dict_type`),
// **transcribed from pydantic's error catalogue and never measured**, because
// `/api/refresh` has no case row and never will (it re-runs ingest). The
// closing pass measured the whole class on endpoints that CAN be rowed — nine
// other handlers carry the same annotation — and the three shapes came back
// byte-for-byte as written, with one exception no transcription had caught:
// a body of the literal `null` is `missing`, not `dict_type`.
//
// The check is now [`crate::json::dict_body`], measured every gate run by the
// `V-*` rows. This endpoint inherits it without owning a copy, which is the
// only way an unrowable endpoint can be right about a shape.

/// The blocking body shared by `_refresh_current_project_impl` and
/// `_refresh_all_projects_impl` — the ingest pass itself is identical in both;
/// only the reporting differs.
///
/// `run_ingest` returns PROVIDER-keyed counts, so the predecessor's
/// `counts.get(slug, 0)` was structurally always `0` and `files_changed` /
/// `message_count` reported "no changes" no matter what was ingested. Both
/// branches sum the values instead.
///
/// The cache invalidations Python performs afterwards
/// (`invalidate_dashboard_cache`, `_invalidate_stats_cache`,
/// `invalidate_optimize_cache`) are no-ops here for the dashboard and stats
/// memos, which are not ported (DIV-055 / DIV-122) — there is nothing to
/// invalidate. `/api/optimize`'s cache IS ported (its `cache` field is on the
/// wire), and it is keyed on `store.db`'s mtime, which this pass moves; the
/// eager drop is a race-avoidance nicety on an unflushed filesystem, and it is
/// not reachable across module boundaries here. Recorded as DIV-125.
fn run_refresh(state: &AppState, per_project: bool) -> Result<Value, HttpError> {
    let started = std::time::Instant::now();
    let conn = state.connect().map_err(|err| any_500(&err))?;
    let engine = crate::pricing::engine(&conn, state.package_dir()).map_err(|err| any_500(&err))?;
    let ctx = stax_etl::normalize::NormalizeContext::new(engine);
    let adapters = stax_adapters::registry::registered();
    let report = stax_etl::ingest::run_ingest(
        &conn,
        &adapters,
        &ctx,
        &stax_etl::ingest::SystemClock,
        &stax_etl::ingest::ReindexConfig::default(),
    )
    .map_err(|err| any_500(&err))?;
    drop(conn);

    // `sum(counts.values())`.
    let total_new: i64 = report.counts.iter().map(|(_, added)| *added).sum();

    // `_invalidate_stats_cache(...)` — DIV-055's memo, dropped at two of the
    // four sites Python drops it (the other two are `routes/cfg.rs`'s alias
    // writers). The memo self-invalidates on the sessions signature anyway, so
    // this is the same DEFENSIVE posture Python's own comment describes — but
    // the scope is not: the per-project branch clears only this slug
    // (`data.py` line 1077) and the all-projects branch clears everything
    // (`data.py` line 1128, "every slug may have moved — full clear").
    if total_new != 0 {
        let slug = if per_project {
            state
                .current_project()
                .log_path
                .as_deref()
                .map(crate::pyops::path_name)
        } else {
            None
        };
        state.stats_memo().invalidate(slug.as_deref());
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
    let ms = (started.elapsed().as_secs_f64() * 1000.0) as i64;

    let mut out = Map::new();
    out.insert("status".to_owned(), Value::from("success"));
    if per_project {
        out.insert(
            "message".to_owned(),
            Value::from(if total_new != 0 {
                "Files changed - data refreshed successfully"
            } else {
                "No changes detected - using cached data"
            }),
        );
        out.insert("files_changed".to_owned(), Value::Bool(total_new > 0));
        out.insert("message_count".to_owned(), Value::from(total_new));
        out.insert("refresh_time_ms".to_owned(), Value::from(ms));
    } else {
        out.insert(
            "message".to_owned(),
            Value::from(if total_new != 0 {
                format!("Ingested {total_new} new records")
            } else {
                "No changes detected".to_owned()
            }),
        );
        out.insert("files_changed".to_owned(), Value::Bool(total_new > 0));
        out.insert("refresh_time_ms".to_owned(), Value::from(ms));
        out.insert("projects_refreshed".to_owned(), Value::from(total_new));
        // Yes, the same number twice — `total_projects` is `total_new`, not a
        // project count. Bug-for-bug; the field name is a lie the UI does not
        // read. DIV-126.
        out.insert("total_projects".to_owned(), Value::from(total_new));
    }
    Ok(Value::Object(out))
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
        assert_eq!(stats_memo::clamp_tz_offset(-999), -720);
        assert_eq!(stats_memo::clamp_tz_offset(9999), 840);
        // §6b divergence 5: the React callers send raw `getTimezoneOffset()`,
        // which is minutes WEST, where the backend wants minutes east. Both
        // signs are inside the clamp, so the port inherits the wrong bucketing
        // faithfully — as it must until the frontend fix lands.
        assert_eq!(stats_memo::clamp_tz_offset(480), 480);
        assert_eq!(stats_memo::clamp_tz_offset(-480), -480);
    }
}

#[cfg(test)]
mod tests_batch_c {
    use super::*;
    use serde_json::json;

    fn mart_row(fields: Value) -> Map<String, Value> {
        fields.as_object().expect("object").clone()
    }

    fn daily_row(day: &str, model: Option<&str>, cost: f64, messages: i64) -> DailyRow {
        DailyRow {
            day: Some(day.to_owned()),
            provider: Some("claude".to_owned()),
            model: model.map(str::to_owned),
            speed: Some("standard".to_owned()),
            input_tokens: 10,
            output_tokens: 5,
            cache_read: 3,
            cache_create: 1,
            message_count: messages,
            session_count: 1,
            cost_usd: cost,
        }
    }

    #[test]
    fn a_single_provider_row_is_returned_whole_not_rebuilt() {
        // `if len(present) == 1: return present[0]` — the short-circuit keeps
        // columns the merged path drops (`project_id`, `errors_by_category`).
        let row = mart_row(json!({
            "project_id": 7, "errors_by_category": "{\"api\":2}",
            "total_messages": 3, "total_cost_usd": 1.5,
        }));
        let merged = merge_project_mart_rows(&[Some(row.clone()), None]).expect("some");
        assert_eq!(merged, row);
        assert!(merged.contains_key("project_id"));
    }

    #[test]
    fn merging_two_providers_sums_the_additive_columns_and_drops_the_rest() {
        let a = mart_row(json!({
            "project_id": 1, "provider": "claude", "slug": "s", "display_name": "S",
            "first_ts": "2026-02-01", "last_ts": "2026-06-01",
            "total_messages": 3, "total_sessions": 1, "total_cost_usd": 1.5,
            "errors_by_category": "{\"api\":2}",
        }));
        let b = mart_row(json!({
            "project_id": 2, "provider": "codex", "slug": "s", "display_name": "S2",
            "first_ts": "2026-01-01", "last_ts": "2026-05-01",
            "total_messages": 4, "total_sessions": 2, "total_cost_usd": 0.25,
            "errors_by_category": "{\"api\":1}",
        }));
        let merged = merge_project_mart_rows(&[Some(a), Some(b)]).expect("some");
        assert_eq!(merged["total_messages"], json!(7));
        assert_eq!(merged["total_sessions"], json!(3));
        assert_eq!(merged["total_cost_usd"], json!(1.75));
        // earliest first_ts, latest last_ts — ISO strings sort chronologically,
        // and each bound is taken across ALL rows independently, so the pair
        // need not come from the same provider. Here it does not: the earliest
        // start is codex's and the latest end is claude's.
        assert_eq!(merged["first_ts"], json!("2026-01-01"));
        assert_eq!(merged["last_ts"], json!("2026-06-01"));
        // Identity columns come from the FIRST row.
        assert_eq!(merged["provider"], json!("claude"));
        assert_eq!(merged["display_name"], json!("S"));
        // The non-additive JSON map is NOT merged here — that is
        // `errors_block_from_marts`'s job, off the unmerged rows.
        assert!(!merged.contains_key("errors_by_category"));
        assert!(!merged.contains_key("project_id"));
    }

    #[test]
    fn an_all_integer_merged_column_stays_an_int_on_the_wire() {
        // The seed is `{k: 0}`, a Python int. `0 + 3 + 4` is an int; the moment
        // a float lands the column becomes a float. Both spellings reach JSON.
        let a = mart_row(json!({"total_messages": 3, "total_cost_usd": 0}));
        let b = mart_row(json!({"total_messages": 4, "total_cost_usd": 0}));
        let merged = merge_project_mart_rows(&[Some(a), Some(b)]).expect("some");
        assert_eq!(
            stax_memory::pyjson::dumps_http(&merged["total_messages"]),
            "7"
        );
        assert_eq!(
            stax_memory::pyjson::dumps_http(&merged["total_cost_usd"]),
            "0"
        );
    }

    #[test]
    fn an_empty_or_null_timestamp_is_skipped_by_the_min_max() {
        // `if r.get("first_ts")` — falsy in Python, so "" and NULL both skip.
        let a = mart_row(json!({"first_ts": "", "last_ts": null}));
        let b = mart_row(json!({"first_ts": "2026-03-01", "last_ts": "2026-03-02"}));
        let merged = merge_project_mart_rows(&[Some(a), Some(b)]).expect("some");
        assert_eq!(merged["first_ts"], json!("2026-03-01"));
        assert_eq!(merged["last_ts"], json!("2026-03-02"));
    }

    #[test]
    fn no_mart_row_collapses_the_cache_block_to_one_key() {
        assert_eq!(
            stax_memory::pyjson::dumps_http(&cache_block_from_mart(None, 99.0)),
            r#"{"hit_rate":0.0}"#
        );
    }

    #[test]
    fn the_cache_hit_rate_guard_yields_a_float_zero_not_an_int() {
        let row = mart_row(json!({
            "total_cache_create": 10, "total_cache_read": 4,
            "total_assistant_messages": 0, "total_cache_read_messages": 0,
        }));
        // tokens_saved goes NEGATIVE below break-even, and that is the point of
        // the field; break_even_achieved reports the same fact as a bool.
        assert_eq!(
            stax_memory::pyjson::dumps_http(&cache_block_from_mart(Some(&row), 0.0)),
            r#"{"total_created":10,"total_read":4,"tokens_saved":-6,"cost_saved_base_units":0.0,"break_even_achieved":false,"hit_rate":0.0}"#
        );
    }

    #[test]
    fn the_cache_hit_rate_rounds_the_way_cpython_does() {
        let row = mart_row(json!({
            "total_cache_create": 1, "total_cache_read": 2,
            "total_assistant_messages": 3, "total_cache_read_messages": 1,
        }));
        let block = cache_block_from_mart(Some(&row), 1.0);
        // 1/3*100 = 33.333… → 33.3 at one decimal, ties-to-even.
        assert_eq!(block["hit_rate"], json!(33.3));
        assert_eq!(block["break_even_achieved"], json!(true));
    }

    #[test]
    fn the_errors_block_divides_by_records_not_messages_and_merges_categories() {
        let a = mart_row(json!({
            "total_errors": 2, "total_records": 10, "total_messages": 4,
            "errors_by_category": "{\"api\": 1, \"tool\": 1}",
        }));
        let b = mart_row(json!({
            "total_errors": 1, "total_records": 30, "total_messages": 2,
            "errors_by_category": "{\"api\": 1}",
        }));
        // 3/40 = 0.075 — NOT 3/6, which is what dividing by total_messages gives.
        assert_eq!(
            stax_memory::pyjson::dumps_http(&errors_block_from_marts(&[Some(a), Some(b)])),
            r#"{"total":3,"rate":0.075,"by_category":{"api":2,"tool":1}}"#
        );
    }

    #[test]
    fn a_record_less_project_gets_a_float_zero_error_rate() {
        let row = mart_row(json!({"total_errors": 0, "total_records": 0}));
        let block = errors_block_from_marts(&[Some(row)]);
        assert_eq!(stax_memory::pyjson::dumps_http(&block["rate"]), "0.0");
    }

    #[test]
    fn a_poison_category_column_yields_an_empty_map_rather_than_raising() {
        assert_eq!(parse_category_map(Some(&json!("not json"))).len(), 0);
        assert_eq!(parse_category_map(Some(&json!("[1,2]"))).len(), 0);
        assert_eq!(parse_category_map(Some(&Value::Null)).len(), 0);
        assert_eq!(parse_category_map(None).len(), 0);
        // Already-parsed and string forms agree, and `int(v or 0)` coerces.
        assert_eq!(
            parse_category_map(Some(&json!({"api": null, "tool": 2.0}))),
            parse_category_map(Some(&json!(r#"{"api": 0, "tool": 2}"#)))
        );
    }

    #[test]
    fn no_mart_row_collapses_user_interactions_to_one_key() {
        assert_eq!(
            stax_memory::pyjson::dumps_http(&user_interactions_from_mart(None, None)),
            r#"{"user_commands_analyzed":0}"#
        );
        // …and the windowed count wins even in the collapsed shape.
        assert_eq!(
            stax_memory::pyjson::dumps_http(&user_interactions_from_mart(None, Some(4))),
            r#"{"user_commands_analyzed":4}"#
        );
    }

    #[test]
    fn the_interaction_rates_round_at_their_own_precisions() {
        let row = mart_row(json!({
            "total_commands": 3,
            "total_commands_followed_by_interruption": 1,
            "total_command_tools": 7,
            "total_command_steps": 10,
        }));
        // interruption_rate is 1 decimal, the two averages are 2.
        assert_eq!(
            stax_memory::pyjson::dumps_http(&user_interactions_from_mart(Some(&row), None)),
            r#"{"user_commands_analyzed":3,"commands_followed_by_interruption":1,"total_tools_used":7,"total_assistant_steps":10,"interruption_rate":33.3,"avg_tools_per_command":2.33,"avg_steps_per_command":3.33}"#
        );
    }

    #[test]
    fn a_window_overrides_only_the_command_count_not_the_denominators() {
        let row = mart_row(json!({
            "total_commands": 4,
            "total_commands_followed_by_interruption": 1,
            "total_command_tools": 4,
            "total_command_steps": 4,
        }));
        let block = user_interactions_from_mart(Some(&row), Some(1));
        assert_eq!(block["user_commands_analyzed"], json!(1));
        // The rates still divide by the LIFETIME 4, not by the windowed 1 —
        // windowing only the numerator would skew them, so Python leaves them.
        assert_eq!(block["interruption_rate"], json!(25.0));
        assert_eq!(block["avg_tools_per_command"], json!(1.0));
    }

    #[test]
    fn the_overview_trusts_the_project_mart_row_over_the_daily_rows() {
        let row = mart_row(json!({
            "total_input_tokens": 100, "total_output_tokens": 50,
            "total_cache_read": 7, "total_cache_create": 2,
            "total_cost_usd": 1.25, "first_ts": "2026-01-01", "last_ts": "2026-02-01",
            "total_messages": 9, "total_sessions": 2,
            "total_user_messages": 4, "total_assistant_messages": 5,
            "total_tool_use_messages": 1, "total_tool_result_messages": 1,
        }));
        // The daily rows say something completely different; they are ignored.
        let rows = vec![daily_row("2026-05-05", Some("m"), 99.0, 999)];
        assert_eq!(
            stax_memory::pyjson::dumps_http(&daily_mart_to_overview(&rows, Some(&row))),
            r#"{"total_tokens":{"input":100,"output":50,"cache_read":7,"cache_creation":2},"total_cost":1.25,"date_range":{"start":"2026-01-01","end":"2026-02-01"},"total_messages":9,"total_sessions":2,"message_types":{"user":4,"assistant":5,"tool_use":1,"tool_result":1}}"#
        );
    }

    #[test]
    fn the_daily_only_overview_reports_an_int_zero_cost_on_no_rows() {
        // DIV-057: `sum(<empty generator>)` is the int 0, so this key is `0`.
        // The mart gate makes this branch unreachable from /api/dashboard-data;
        // it is pinned so the next caller inherits Python's answer.
        assert_eq!(
            stax_memory::pyjson::dumps_http(&daily_mart_to_overview(&[], None)),
            r#"{"total_tokens":{"input":0,"output":0,"cache_read":0,"cache_creation":0},"total_cost":0,"date_range":{"start":null,"end":null},"total_messages":0,"total_sessions":0,"message_types":{}}"#
        );
    }

    #[test]
    fn the_daily_only_overview_takes_its_range_from_a_sorted_day_set() {
        let rows = vec![
            daily_row("2026-03-01", Some("m"), 1.0, 2),
            daily_row("2026-01-01", Some("m"), 2.0, 3),
            daily_row("2026-03-01", Some("n"), 4.0, 1),
        ];
        let overview = daily_mart_to_overview(&rows, None);
        assert_eq!(overview["date_range"]["start"], json!("2026-01-01"));
        assert_eq!(overview["date_range"]["end"], json!("2026-03-01"));
        assert_eq!(overview["total_messages"], json!(6));
        assert_eq!(overview["total_cost"], json!(7.0));
    }

    #[test]
    fn daily_buckets_keep_first_appearance_order_and_a_null_model_skips_by_model() {
        let rows = vec![
            daily_row("2026-03-01", Some("m"), 1.0, 2),
            daily_row("2026-01-01", None, 0.5, 1),
            daily_row("2026-03-01", Some("m"), 2.0, 1),
        ];
        let by_day = daily_mart_by_day(&rows);
        let obj = by_day.as_object().expect("object");
        // Insertion order is first appearance, NOT sorted — the caller's
        // `ORDER BY day` per project is what makes it deterministic.
        assert_eq!(
            obj.keys().collect::<Vec<_>>(),
            vec!["2026-03-01", "2026-01-01"]
        );
        assert_eq!(obj["2026-03-01"]["cost"]["total"], json!(3.0));
        assert_eq!(obj["2026-03-01"]["cost"]["by_model"]["m"], json!(3.0));
        // The model-less row contributes to the day total but NOT to by_model.
        assert_eq!(obj["2026-01-01"]["cost"]["total"], json!(0.5));
        assert_eq!(
            stax_memory::pyjson::dumps_http(&obj["2026-01-01"]["cost"]["by_model"]),
            "{}"
        );
    }

    #[test]
    fn a_daily_bucket_carries_every_key_the_frontend_contract_names() {
        let by_day = daily_mart_by_day(&[daily_row("2026-03-01", Some("m"), 1.0, 2)]);
        assert_eq!(
            stax_memory::pyjson::dumps_http(&by_day["2026-03-01"]),
            r#"{"messages":2,"sessions":1,"tokens":{"input":10,"output":5,"cache_creation":1,"cache_read":3},"cost":{"total":1.0,"by_model":{"m":1.0}},"user_commands":0,"interrupted_commands":0,"interruption_rate":0.0,"errors":0,"assistant_messages":0,"error_rate":0.0}"#
        );
    }

    #[test]
    fn a_day_with_no_key_is_dropped_entirely() {
        let mut orphan = daily_row("2026-03-01", Some("m"), 1.0, 2);
        orphan.day = None;
        let mut blank = daily_row("2026-03-01", Some("m"), 1.0, 2);
        blank.day = Some(String::new());
        let by_day = daily_mart_by_day(&[orphan, blank]);
        assert_eq!(stax_memory::pyjson::dumps_http(&by_day), "{}");
    }

    #[test]
    fn the_models_map_skips_model_less_rows_and_keeps_first_appearance_order() {
        let rows = vec![
            daily_row("2026-03-01", Some("z"), 1.0, 2),
            daily_row("2026-03-01", None, 9.0, 9),
            daily_row("2026-03-02", Some("a"), 0.5, 1),
            daily_row("2026-03-03", Some("z"), 0.25, 1),
        ];
        let models = daily_mart_by_model(&rows);
        let obj = models.as_object().expect("object");
        assert_eq!(obj.keys().collect::<Vec<_>>(), vec!["z", "a"]);
        assert_eq!(obj["z"]["count"], json!(3));
        assert_eq!(obj["z"]["cost"], json!(1.25));
        assert_eq!(
            stax_memory::pyjson::dumps_http(&obj["a"]),
            r#"{"count":1,"cost":0.5,"input_tokens":10,"output_tokens":5,"cache_read":3,"cache_creation":1}"#
        );
    }

    #[test]
    fn the_filter_excluded_message_page_is_not_the_normal_empty_envelope() {
        // `_empty_messages_page` hard-codes the three index fields to 0, where
        // `build_messages_page` would have produced page 0 and start -100
        // (DIV-121). The same endpoint answers "empty" two different ways
        // depending on WHY it is empty, and both are ported as written.
        assert_eq!(
            stax_memory::pyjson::dumps_http(&empty_messages_page(1, 100)),
            r#"{"messages":[],"total":0,"page":1,"per_page":100,"total_pages":0,"start_index":0,"end_index":0}"#
        );
        assert_ne!(
            empty_messages_page(1, 100),
            messages_api::build_messages_page(Vec::new(), 0, 1, 100)
        );
    }

    #[test]
    fn an_all_blank_filter_list_leaves_the_filter_unset() {
        // `if normed:` — unset means "no filter"; an EMPTY set would mean
        // "match nothing", and the two take different branches upstream.
        assert_eq!(normalise_filter(Some(vec!["  ".to_owned()])), None);
        assert_eq!(normalise_filter(Some(Vec::new())), None);
        assert_eq!(normalise_filter(None), None);
        let filter =
            normalise_filter(Some(vec![" CLAUDE ".to_owned(), "codex".to_owned()])).expect("some");
        assert!(filter.contains("claude"));
        assert!(filter.contains("codex"));
    }

    #[test]
    fn the_refresh_validation_body_is_a_detail_list_not_a_detail_string() {
        // Every other error in this module renders `{"detail":"..."}`; a
        // pydantic validation failure renders a LIST. **DIV-127 is closed**:
        // these bytes were transcribed when this endpoint owned its own copy of
        // the check, and the DIV-367 pass MEASURED every one of them against the
        // reference on rowable siblings that carry the identical annotation.
        // The assertions stayed at this address because `/api/refresh` has no
        // case row and never will — this test is the only thing standing
        // between the shared helper and an endpoint nobody can probe.
        let reject = |raw: &[u8]| {
            crate::json::dict_body(raw)
                .expect_err("a rejection")
                .render()
        };
        assert_eq!(
            reject(b""),
            r#"{"detail":[{"type":"missing","loc":["body"],"msg":"Field required","input":null}]}"#
        );
        // A literal `null` is `missing` too, NOT `dict_type` — pydantic never
        // reaches the container check, because `None` is "no value supplied".
        // The one shape in the class that a transcription got wrong.
        assert_eq!(
            reject(b"null"),
            r#"{"detail":[{"type":"missing","loc":["body"],"msg":"Field required","input":null}]}"#
        );
        // The `loc` offset, the empty-object `input` and the `ctx.error`
        // wording are all FastAPI's own hand-built shape, CPython's decoder
        // underneath.
        assert_eq!(
            reject(b"nope"),
            r#"{"detail":[{"type":"json_invalid","loc":["body",0],"msg":"JSON decode error","input":{},"ctx":{"error":"Expecting value"}}]}"#
        );
        assert_eq!(
            reject(b"{\"a\""),
            r#"{"detail":[{"type":"json_invalid","loc":["body",4],"msg":"JSON decode error","input":{},"ctx":{"error":"Expecting ':' delimiter"}}]}"#
        );
        assert_eq!(
            reject(b"[]"),
            r#"{"detail":[{"type":"dict_type","loc":["body"],"msg":"Input should be a valid dictionary","input":[]}]}"#
        );
        // A bare `dict` constrains the container only: the values are `Any`,
        // so this reaches the handler.
        assert!(crate::json::dict_body(br#"{"x": 3}"#).is_ok());
    }

    #[test]
    fn the_in_clause_binds_one_placeholder_per_member() {
        assert_eq!(in_clause(" AND x IN ", 3), " AND x IN (?,?,?)");
        assert_eq!(in_clause(" AND x IN ", 1), " AND x IN (?)");
    }
}
