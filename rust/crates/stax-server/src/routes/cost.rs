//! `routes/cost.py` — 4 endpoints, wave 5 (batch A).
//!
//! | Item | Method | FastAPI path | axum path | State |
//! |---|---|---|---|---|
//! | `RS-5-062` | `GET` | `/api/cost-data`                    | `/api/cost-data`             | ported |
//! | `RS-5-063` | `GET` | `/api/cost-data/by-provider`        | `/api/cost-data/by-provider` | ported |
//! | `RS-5-064` | `GET` | `/api/interaction/{interaction_id}` | same                         | ported |
//! | `RS-5-065` | `GET` | `/api/cost-data/by-model`           | `/api/cost-data/by-model`    | ported |
//!
//! # The three rollup paths, and why the gates on them are not optional
//!
//! Two of these endpoints can answer from a pre-aggregated mart *or* by
//! re-pricing the `messages` table, and the two do not agree. `daily_mart.cost_usd`
//! is what the **normalizer** stored, and the normalizer writes `0.0` for any
//! event whose `cost_source` is not a real rate-card match, while the raw path
//! re-prices those same messages through `compute_cost`'s default-family
//! fallback and invents a number. Python measured the gap at -65.4% over a week
//! window on a slug with 183 non-`rate_card` events, with individual (day,
//! model) cells going from $9.26 raw to $0.00 mart. `_by_model_mart_eligible` is
//! the guard, and it is ported gate for gate:
//!
//! 1. every project id is materialised (`project_mart` row present) — an
//!    un-materialised store has an EMPTY `usage_events`, which passes gate 2
//!    vacuously and would serve an empty chart for a project full of messages;
//! 2. zero `usage_events` rows with `cost_source != 'rate_card'`;
//! 3. and the period must be day-aligned. `week` is a rolling `now - 7d`
//!    *instant*, so truncating it to `YYYY-MM-DD` pulls in the whole boundary
//!    day (+8-29% depending on the hour). `week` therefore never takes the mart.
//!
//! # Accumulation shape (this module is the counter-example to the last one)
//!
//! `routes/commands.py` uses `sum()`, which is Neumaier-compensated. Everything
//! here uses `bucket["cost_usd"] += cost` — a plain `+=` chain — so the port
//! must **not** compensate. Matching the operation is the rule; "more accurate"
//! is a divergence.

use std::collections::HashSet;

use axum::Router;
use axum::extract::{Path as PathParam, RawQuery, State};
use axum::http::StatusCode;
use axum::routing::get;
use rusqlite::Connection;
use serde_json::{Map, Value};
use stax_etl::pricing::RawTokens;
use stax_etl::pricing::costs::PricingEngine;
use stax_etl::stats::enricher::{EnrichedDataset, Interaction, Record};

use crate::currency::active_currency_payload;
use crate::json::{HandlerResult, HttpError, JsonBody, join_failure};
use crate::qs::Query;
use crate::state::AppState;

/// `COST_KEYS` — the 9 analytics sections split off `/api/dashboard-data` (§A3).
const COST_KEYS: [&str; 9] = [
    "session_costs",
    "command_costs",
    "tool_costs",
    "token_composition",
    "outliers",
    "retry_signals",
    "session_efficiency",
    "error_cost",
    "trends",
];

/// The `COST_KEYS` members whose missing-value default is `{}` rather than `[]`.
const DICT_SHAPED_KEYS: [&str; 5] = [
    "tool_costs",
    "token_composition",
    "outliers",
    "error_cost",
    "trends",
];

/// `_TZ_OFFSET_MIN` / `_MAX` — minutes EAST of UTC.
const TZ_OFFSET_MIN: i64 = -720;
const TZ_OFFSET_MAX: i64 = 840;

/// `_BY_MODEL_MART_PERIODS` — the periods whose bounds sit on day boundaries.
const BY_MODEL_MART_PERIODS: [&str; 3] = ["today", "month", "all"];

/// Mount this module's endpoints onto `router`.
pub fn register(router: Router<AppState>) -> Router<AppState> {
    router
        .route("/api/cost-data", get(get_cost_data))
        .route("/api/cost-data/by-provider", get(get_cost_by_provider))
        .route("/api/cost-data/by-model", get(get_cost_by_model))
        .route("/api/interaction/{interaction_id}", get(get_interaction))
}

// ── shared helpers ───────────────────────────────────────────────────────────

/// `_resolve_log_path` — the query param, else `deps.current_log_path`, else 400.
fn resolve_log_path(query: &Query, state: &AppState) -> Result<String, HttpError> {
    match optional_log_path(query, state) {
        path if path.is_empty() => Err(HttpError::bad_request(
            "No project selected or log_path provided",
        )),
        path => Ok(path),
    }
}

/// `log_path or deps.current_log_path` WITHOUT the 400 — the by-provider and
/// by-model routes treat "no project at all" as the legitimate global view.
fn optional_log_path(query: &Query, state: &AppState) -> String {
    let from_query = query.get("log_path").unwrap_or_default();
    if !from_query.is_empty() {
        return from_query.to_owned();
    }
    state.current_project().log_path.unwrap_or_default()
}

fn path_name(path: &str) -> String {
    std::path::Path::new(path)
        .file_name()
        .map_or_else(String::new, |name| name.to_string_lossy().into_owned())
}

/// `_project_ids_for` — the 404 string, em-dash included.
fn project_ids_for(conn: &Connection, path: &str) -> Result<Vec<i64>, HttpError> {
    let slug = path_name(path);
    let mut stmt = conn
        .prepare("SELECT id FROM projects WHERE slug = ?")
        .map_err(sql_500)?;
    let ids: Vec<i64> = stmt
        .query_map([&slug], |row| row.get(0))
        .map_err(sql_500)?
        .collect::<rusqlite::Result<_>>()
        .map_err(sql_500)?;
    if ids.is_empty() {
        return Err(HttpError::not_found(format!(
            "Project '{slug}' not found in store — try /api/refresh first"
        )));
    }
    Ok(ids)
}

fn sql_500(err: rusqlite::Error) -> HttpError {
    HttpError::new(StatusCode::INTERNAL_SERVER_ERROR, err.to_string())
}

fn any_500(err: &anyhow::Error) -> HttpError {
    HttpError::new(StatusCode::INTERNAL_SERVER_ERROR, err.to_string())
}

fn normalise_filter(raw: Option<&[String]>) -> Option<HashSet<String>> {
    let raw = raw?;
    let normed: HashSet<String> = raw
        .iter()
        .filter(|value| !value.trim().is_empty())
        .map(|value| value.trim().to_lowercase())
        .collect();
    (!normed.is_empty()).then_some(normed)
}

fn table_exists(conn: &Connection, name: &str) -> rusqlite::Result<bool> {
    let mut stmt = conn.prepare("SELECT 1 FROM sqlite_master WHERE type='table' AND name=?")?;
    let mut rows = stmt.query([name])?;
    Ok(rows.next()?.is_some())
}

// ── GET /api/cost-data ───────────────────────────────────────────────────────

/// `_COST_DATA_RANGE_DAYS` — `all` (and absent) mean "no window".
fn range_days(range: &str) -> Option<Option<i64>> {
    match range {
        "all" => Some(None),
        "7d" => Some(Some(7)),
        "30d" => Some(Some(30)),
        _ => None,
    }
}

async fn get_cost_data(State(state): State<AppState>, RawQuery(raw): RawQuery) -> HandlerResult {
    let query = Query::parse(raw.as_deref().unwrap_or_default());
    let path = resolve_log_path(&query, &state)?;
    let timezone_offset = query
        .int_or("timezone_offset", 0)
        .map_err(|err| HttpError::new(StatusCode::UNPROCESSABLE_ENTITY, err.field))?;
    let model_filter = normalise_filter(query.opt_list("model").as_deref());

    // `if range_ is not None and range_ not in _COST_DATA_RANGE_DAYS` — note the
    // sorted join, which puts `30d` first: "Valid: 30d, 7d, all".
    let range = query.get("range").map(str::to_owned);
    let window_days = match &range {
        None => None,
        Some(value) => match range_days(value) {
            Some(days) => days,
            None => {
                return Err(HttpError::bad_request(format!(
                    "Unknown range '{value}'. Valid: 30d, 7d, all"
                )));
            }
        },
    };

    let worker = state.clone();
    let (mut stats, tool_costs_windowed) = tokio::task::spawn_blocking(move || {
        cost_data_stats(
            &worker,
            &path,
            timezone_offset,
            model_filter.as_ref(),
            window_days,
        )
    })
    .await
    .map_err(|err| join_failure(&err))??;

    let mut payload = Map::new();
    for key in COST_KEYS {
        // `val = stats.get(key)`; `if val is None:` — a present-but-null value
        // takes the default too, and the default's SHAPE is per-key.
        let value = match stats.remove(key) {
            Some(Value::Null) | None => {
                if DICT_SHAPED_KEYS.contains(&key) {
                    Value::Object(Map::new())
                } else {
                    Value::Array(Vec::new())
                }
            }
            Some(value) => value,
        };
        payload.insert(key.to_owned(), value);
    }

    let currency = active_currency_payload(&state.config().currency)
        .map_err(|err| HttpError::new(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))?;
    // `if currency["rate_from_usd"] != 1.0:` — DIV-052 keeps `_convert_in_place`
    // unreachable, so the whole cost-field walk is recorded, not ported.
    payload.insert("currency".to_owned(), currency);
    payload.insert(
        "tool_costs_windowed".to_owned(),
        Value::Bool(tool_costs_windowed),
    );
    Ok(JsonBody::ok(Value::Object(payload)))
}

/// The blocking body: the pipeline sweep, narrowed to `COST_KEYS`, then overlaid.
fn cost_data_stats(
    state: &AppState,
    path: &str,
    timezone_offset: i64,
    model_filter: Option<&HashSet<String>>,
    window_days: Option<i64>,
) -> Result<(Map<String, Value>, bool), HttpError> {
    let conn = state.connect().map_err(|err| any_500(&err))?;
    let project_ids = project_ids_for(&conn, path)?;
    let engine = crate::pricing::engine(&conn, state.package_dir()).map_err(|err| any_500(&err))?;
    // `_project_stats_cached` clamps before both the cache key and the call.
    let tz_offset = timezone_offset.clamp(TZ_OFFSET_MIN, TZ_OFFSET_MAX);
    let (_messages, stats) =
        stax_etl::stats::dataset::get_project_stats_with(&conn, &project_ids, tz_offset, &engine)
            .map_err(|err| any_500(&err))?;

    // `keys=COST_KEYS` — the memo's narrowed copy. Not a perf device here (there
    // is no memo to copy out of), but the *set* is load-bearing: an unrequested
    // key must be OMITTED from the working dict, not carried and dropped later.
    let mut stats: Map<String, Value> = match stats {
        Value::Object(map) => COST_KEYS
            .iter()
            .filter_map(|key| {
                map.get(*key)
                    .map(|value| ((*key).to_owned(), value.clone()))
            })
            .collect(),
        _ => Map::new(),
    };

    // A slug maps to one `projects` row PER PROVIDER, so a multi-provider
    // project legitimately resolves to several ids; gating on `len == 1` used to
    // drop the overlay for exactly the busiest projects.
    let mart_pids: Vec<i64> = project_ids
        .iter()
        .copied()
        .filter(|pid| mart_has_project_row(&conn, *pid).unwrap_or(false))
        .collect();

    let mut tool_costs_windowed = false;
    if !mart_pids.is_empty() {
        tool_costs_windowed =
            overlay_mart_rollups(&conn, &mart_pids, &mut stats, model_filter, window_days)
                .map_err(sql_500)?;
    }
    Ok((stats, tool_costs_windowed))
}

/// `_overlay_mart_rollups` — returns the `_tool_costs_windowed` marker the route
/// pops, rather than round-tripping it through the payload dict.
fn overlay_mart_rollups(
    conn: &Connection,
    project_ids: &[i64],
    stats: &mut Map<String, Value>,
    model_filter: Option<&HashSet<String>>,
    window_days: Option<i64>,
) -> rusqlite::Result<bool> {
    let mut daily_rows: Vec<DailyMartRow> = Vec::new();
    for pid in project_ids {
        daily_rows.extend(daily_for_project(conn, *pid, None, None, model_filter)?);
    }

    if daily_rows.is_empty() {
        // A model filter that excludes every row leaves the blocks EMPTY but
        // shape-stable, so the donut renders its zero state instead of
        // all-model totals. With no filter, an empty mart changes nothing.
        if model_filter.is_some() {
            let tc = ensure_token_composition(stats);
            tc.insert("daily".to_owned(), Value::Object(Map::new()));
            tc.insert("totals".to_owned(), zero_totals());
            tc.remove("reasoning_share");
        }
        return Ok(false);
    }

    // `daily.setdefault(day, …)` on a plain dict: insertion-ordered by first
    // appearance across the per-project queries, each of which is `ORDER BY day`.
    let mut day_order: Vec<String> = Vec::new();
    let mut day_buckets: Vec<[i64; 4]> = Vec::new();
    let mut totals = [0_i64; 4];
    for row in &daily_rows {
        // `if not day: continue` — a NULL or empty day contributes to NEITHER
        // the per-day buckets NOR the totals.
        let Some(day) = row.day.as_ref().filter(|day| !day.is_empty()) else {
            continue;
        };
        let idx = match day_order.iter().position(|seen| seen == day) {
            Some(idx) => idx,
            None => {
                day_order.push(day.clone());
                day_buckets.push([0; 4]);
                day_buckets.len() - 1
            }
        };
        let measures = [
            row.input_tokens,
            row.output_tokens,
            row.cache_read,
            row.cache_create,
        ];
        for (slot, value) in measures.into_iter().enumerate() {
            day_buckets[idx][slot] += value;
            totals[slot] += value;
        }
    }

    // The aggregator's authoritative all-model reasoning total is read BEFORE
    // `totals` is rebound, and carried onto the mart-derived totals only when no
    // model filter is active — `daily_mart` cannot attribute reasoning to a
    // model, and an all-model reasoning figure against model-scoped output would
    // misattribute (the same reason `tool_costs` is skipped when filtered).
    let reasoning_total = stats
        .get("token_composition")
        .and_then(|tc| tc.get("totals"))
        .filter(|value| value.is_object())
        .and_then(|totals| totals.get("reasoning"))
        .and_then(Value::as_i64)
        .unwrap_or(0);

    let tc = ensure_token_composition(stats);
    let mut totals_obj = Map::new();
    totals_obj.insert("input".to_owned(), Value::from(totals[0]));
    totals_obj.insert("output".to_owned(), Value::from(totals[1]));
    totals_obj.insert("cache_read".to_owned(), Value::from(totals[2]));
    totals_obj.insert("cache_creation".to_owned(), Value::from(totals[3]));
    if model_filter.is_none() && reasoning_total > 0 {
        totals_obj.insert("reasoning".to_owned(), Value::from(reasoning_total));
        let out_tok = totals[1];
        let share = if out_tok > 0 {
            // Python's `int / int` is true division, so this is a float even
            // when it divides exactly.
            reasoning_total as f64 / out_tok as f64
        } else {
            0.0
        };
        tc.insert("reasoning_share".to_owned(), Value::from(share));
    } else {
        tc.remove("reasoning_share");
    }

    let mut daily_obj = Map::new();
    for (day, bucket) in day_order.iter().zip(day_buckets.iter()) {
        let mut entry = Map::new();
        entry.insert("input".to_owned(), Value::from(bucket[0]));
        entry.insert("output".to_owned(), Value::from(bucket[1]));
        entry.insert("cache_read".to_owned(), Value::from(bucket[2]));
        entry.insert("cache_creation".to_owned(), Value::from(bucket[3]));
        daily_obj.insert(day.clone(), Value::Object(entry));
    }
    // Assignment order is `daily` then `totals`, and both keys already exist on
    // the aggregator's block, so `insert` overwrites in place and the key order
    // is the aggregator's — not this function's.
    tc.insert("daily".to_owned(), Value::Object(daily_obj));
    tc.insert("totals".to_owned(), Value::Object(totals_obj));

    // `tool_mart` has NO model dimension, so it cannot be narrowed; leaving the
    // aggregator's all-model figures (badged in the UI) beats silently showing
    // all-model numbers that look model-scoped.
    if model_filter.is_some() || !mart_has_tool_rows(conn)? {
        return Ok(false);
    }

    let mut day_from: Option<String> = None;
    if let Some(days) = window_days {
        // `max((r.get("day") or "") for r in daily_rows)` — over the FULL,
        // unfiltered series (the filter branch cannot reach here).
        let anchor = daily_rows
            .iter()
            .map(|row| row.day.clone().unwrap_or_default())
            .max()
            .unwrap_or_default();
        // `date.fromisoformat` then `- (days - 1)`: the anchor day counts as day
        // 1, so `7d` is the last 7 calendar days. An unparseable anchor falls
        // back to the UNWINDOWED rollup rather than 500ing.
        day_from = iso_date_minus_days(&anchor, days - 1);
    }

    // Merge per-provider rollups by tool name, summing every numeric column.
    let mut tool_order: Vec<String> = Vec::new();
    let mut tool_rows: Vec<ToolMartRow> = Vec::new();
    for pid in project_ids {
        for (name, row) in tool_mart_for_project(conn, *pid, day_from.as_deref())? {
            match tool_order.iter().position(|seen| *seen == name) {
                Some(idx) => tool_rows[idx].add(&row),
                None => {
                    tool_order.push(name);
                    tool_rows.push(row);
                }
            }
        }
    }

    let reshaped = || {
        let mut out = Map::new();
        for (name, row) in tool_order.iter().zip(tool_rows.iter()) {
            out.insert(name.clone(), row.to_aggregator_shape());
        }
        Value::Object(out)
    };

    if day_from.is_some() {
        // A windowed request with an EMPTY in-window rollup means "no tool
        // activity in range" and must replace the aggregator's all-time block —
        // an all-time chart under a 7d filter is the #24 bug.
        stats.insert("tool_costs".to_owned(), reshaped());
        return Ok(true);
    }
    if !tool_order.is_empty() {
        stats.insert("tool_costs".to_owned(), reshaped());
    }
    Ok(false)
}

/// `tc = stats.get("token_composition")`; a non-dict is REPLACED with the
/// three-key skeleton, in that literal's order.
fn ensure_token_composition(stats: &mut Map<String, Value>) -> &mut Map<String, Value> {
    if !stats.get("token_composition").is_some_and(Value::is_object) {
        let mut skeleton = Map::new();
        skeleton.insert("daily".to_owned(), Value::Object(Map::new()));
        skeleton.insert("totals".to_owned(), Value::Object(Map::new()));
        skeleton.insert("per_session".to_owned(), Value::Object(Map::new()));
        stats.insert("token_composition".to_owned(), Value::Object(skeleton));
    }
    match stats.get_mut("token_composition") {
        Some(Value::Object(map)) => map,
        _ => unreachable!("just inserted an object"),
    }
}

fn zero_totals() -> Value {
    let mut obj = Map::new();
    obj.insert("input".to_owned(), Value::from(0));
    obj.insert("output".to_owned(), Value::from(0));
    obj.insert("cache_read".to_owned(), Value::from(0));
    obj.insert("cache_creation".to_owned(), Value::from(0));
    Value::Object(obj)
}

/// `(date.fromisoformat(anchor) - timedelta(days=n)).isoformat()`, or `None` on
/// a `ValueError` — an unparseable anchor day is the unwindowed fallback.
fn iso_date_minus_days(anchor: &str, days: i64) -> Option<String> {
    let parts: Vec<&str> = anchor.split('-').collect();
    if parts.len() != 3 {
        return None;
    }
    let year: i64 = parts[0].parse().ok()?;
    let month: i64 = parts[1].parse().ok()?;
    let day: i64 = parts[2].parse().ok()?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let (y, m, d) = civil_from_days(days_from_civil(year, month, day) - days);
    Some(format!("{y:04}-{m:02}-{d:02}"))
}

// ── mart readers (`store/mart_queries.py`) ───────────────────────────────────
//
// FLAGGED FOR THE ARCHITECT'S DEDUP LIST: every reader below is a
// `store/mart_queries.py` function and belongs in `stax-etl` beside the mart
// BUILDERS that write these same tables. They are file-local here only because
// batch A may not edit crates outside `stax-server`.

/// `mart_has_project_row` — keyed on `project_mart`, not `daily_mart`.
///
/// A project with zero billable activity still gets a `project_mart` row (totals
/// all zero), so the "is this materialised?" gate does not misfire on a project
/// that exists but has accrued no usage events.
fn mart_has_project_row(conn: &Connection, project_id: i64) -> rusqlite::Result<bool> {
    if !table_exists(conn, "project_mart")? {
        return Ok(false);
    }
    let mut stmt = conn.prepare("SELECT 1 FROM project_mart WHERE project_id = ? LIMIT 1")?;
    let mut rows = stmt.query([project_id])?;
    Ok(rows.next()?.is_some())
}

fn mart_has_tool_rows(conn: &Connection) -> rusqlite::Result<bool> {
    if !table_exists(conn, "tool_mart")? {
        return Ok(false);
    }
    let mut stmt = conn.prepare("SELECT 1 FROM tool_mart LIMIT 1")?;
    let mut rows = stmt.query([])?;
    Ok(rows.next()?.is_some())
}

/// The `daily_mart` columns these routes read.
struct DailyMartRow {
    day: Option<String>,
    model: Option<String>,
    input_tokens: i64,
    output_tokens: i64,
    cache_read: i64,
    cache_create: i64,
    message_count: i64,
    cost_usd: f64,
}

/// `daily_for_project` — one project, optional day window and model filter.
fn daily_for_project(
    conn: &Connection,
    project_id: i64,
    day_from: Option<&str>,
    day_to: Option<&str>,
    model_filter: Option<&HashSet<String>>,
) -> rusqlite::Result<Vec<DailyMartRow>> {
    if !table_exists(conn, "daily_mart")? {
        return Ok(Vec::new());
    }
    let mut sql = "SELECT day, model, input_tokens, output_tokens, cache_read, cache_create, \
                          message_count, cost_usd \
                   FROM daily_mart WHERE project_id = ?"
        .to_owned();
    let mut params: Vec<Box<dyn rusqlite::ToSql>> = vec![Box::new(project_id)];
    if let Some(from) = day_from.filter(|value| !value.is_empty()) {
        sql.push_str(" AND day >= ?");
        params.push(Box::new(from.to_owned()));
    }
    if let Some(to) = day_to.filter(|value| !value.is_empty()) {
        sql.push_str(" AND day <= ?");
        params.push(Box::new(to.to_owned()));
    }
    if let Some(filter) = model_filter.filter(|filter| !filter.is_empty()) {
        // A Python `set` has no order, so the bound-parameter order cannot
        // affect the result; sorted here so the emitted SQL is reproducible.
        let mut models: Vec<String> = filter.iter().map(|m| m.to_lowercase()).collect();
        models.sort();
        sql.push_str(&format!(
            " AND LOWER(model) IN ({})",
            vec!["?"; models.len()].join(",")
        ));
        for model in models {
            params.push(Box::new(model));
        }
    }
    sql.push_str(" ORDER BY day");
    let mut stmt = conn.prepare(&sql)?;
    let refs: Vec<&dyn rusqlite::ToSql> = params.iter().map(std::convert::AsRef::as_ref).collect();
    let rows = stmt.query_map(refs.as_slice(), |row| {
        Ok(DailyMartRow {
            day: row.get(0)?,
            model: row.get(1)?,
            input_tokens: row.get::<_, Option<i64>>(2)?.unwrap_or(0),
            output_tokens: row.get::<_, Option<i64>>(3)?.unwrap_or(0),
            cache_read: row.get::<_, Option<i64>>(4)?.unwrap_or(0),
            cache_create: row.get::<_, Option<i64>>(5)?.unwrap_or(0),
            message_count: row.get::<_, Option<i64>>(6)?.unwrap_or(0),
            cost_usd: row.get::<_, Option<f64>>(7)?.unwrap_or(0.0),
        })
    })?;
    rows.collect()
}

/// The `tool_mart_for_project` value shape.
#[derive(Clone)]
struct ToolMartRow {
    calls: i64,
    calls_total: i64,
    cost: f64,
    tokens_in: i64,
    tokens_out: i64,
    cache_read_tokens: i64,
    cache_creation_tokens: i64,
    sessions: i64,
}

impl ToolMartRow {
    /// The merge loop's `merged[k] = (merged.get(k) or 0) + v` for every numeric
    /// column. `sessions` is summed here too — Python does not special-case it,
    /// even though the SQL built it with `MAX`.
    fn add(&mut self, other: &Self) {
        self.calls += other.calls;
        self.calls_total += other.calls_total;
        self.cost += other.cost;
        self.tokens_in += other.tokens_in;
        self.tokens_out += other.tokens_out;
        self.cache_read_tokens += other.cache_read_tokens;
        self.cache_creation_tokens += other.cache_creation_tokens;
        self.sessions += other.sessions;
    }

    /// `_tool_mart_to_aggregator_shape` — the column names renamed to
    /// `_ToolCostCollector`'s field names, in the dict-literal's order.
    ///
    /// `sessions` is deliberately NOT in the output: the reshape lists seven
    /// keys and that is not one of them.
    fn to_aggregator_shape(&self) -> Value {
        let mut obj = Map::new();
        obj.insert("calls".to_owned(), Value::from(self.calls));
        obj.insert("calls_total".to_owned(), Value::from(self.calls_total));
        obj.insert("input_tokens".to_owned(), Value::from(self.tokens_in));
        obj.insert("output_tokens".to_owned(), Value::from(self.tokens_out));
        obj.insert(
            "cache_read_tokens".to_owned(),
            Value::from(self.cache_read_tokens),
        );
        obj.insert(
            "cache_creation_tokens".to_owned(),
            Value::from(self.cache_creation_tokens),
        );
        obj.insert("cost".to_owned(), Value::from(self.cost));
        Value::Object(obj)
    }
}

/// `tool_mart_for_project` — `(tool_name, row)` in `GROUP BY` order.
fn tool_mart_for_project(
    conn: &Connection,
    project_id: i64,
    day_from: Option<&str>,
) -> rusqlite::Result<Vec<(String, ToolMartRow)>> {
    if !table_exists(conn, "tool_mart")? {
        return Ok(Vec::new());
    }
    let mut sql = "SELECT tool_name, \
                          SUM(event_count) AS calls, \
                          SUM(calls_total) AS calls_total, \
                          SUM(cost_usd) AS cost, \
                          SUM(tokens_in) AS tokens_in, \
                          SUM(tokens_out) AS tokens_out, \
                          SUM(cache_read) AS cache_read_tokens, \
                          SUM(cache_create) AS cache_creation_tokens, \
                          MAX(session_count) AS sessions \
                   FROM tool_mart WHERE project_id = ?"
        .to_owned();
    let mut params: Vec<Box<dyn rusqlite::ToSql>> = vec![Box::new(project_id)];
    if let Some(from) = day_from.filter(|value| !value.is_empty()) {
        sql.push_str(" AND day >= ?");
        params.push(Box::new(from.to_owned()));
    }
    sql.push_str(" GROUP BY tool_name");
    let mut stmt = conn.prepare(&sql)?;
    let refs: Vec<&dyn rusqlite::ToSql> = params.iter().map(std::convert::AsRef::as_ref).collect();
    let rows = stmt.query_map(refs.as_slice(), |row| {
        Ok((
            row.get::<_, Option<String>>(0)?.unwrap_or_default(),
            ToolMartRow {
                calls: row.get::<_, Option<i64>>(1)?.unwrap_or(0),
                calls_total: row.get::<_, Option<i64>>(2)?.unwrap_or(0),
                cost: row.get::<_, Option<f64>>(3)?.unwrap_or(0.0),
                tokens_in: row.get::<_, Option<i64>>(4)?.unwrap_or(0),
                tokens_out: row.get::<_, Option<i64>>(5)?.unwrap_or(0),
                cache_read_tokens: row.get::<_, Option<i64>>(6)?.unwrap_or(0),
                cache_creation_tokens: row.get::<_, Option<i64>>(7)?.unwrap_or(0),
                sessions: row.get::<_, Option<i64>>(8)?.unwrap_or(0),
            },
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (name, value) = row?;
        // `if not name: continue`.
        if !name.is_empty() {
            out.push((name, value));
        }
    }
    Ok(out)
}

// ── periods (`reports/scope.py::parse_period`) ───────────────────────────────

/// `_BY_PROVIDER_PERIOD_MAP` — the HTTP alias → the `parse_period` spec.
fn period_spec(period: &str) -> Option<&'static str> {
    match period {
        "today" => Some("today"),
        "week" => Some("7days"),
        "month" => Some("month"),
        "all" => Some("all"),
        _ => None,
    }
}

/// `Scope` — both bounds as UTC ISO-8601 strings, or `None` for unbounded.
struct Scope {
    since: Option<String>,
    until: Option<String>,
}

/// `parse_period(spec)` with `now = datetime.now(UTC)`.
///
/// The `7days` leg carries MICROSECONDS (it is `now - timedelta(days=7)`, not a
/// day boundary), so the two servers compute bounds a few milliseconds apart. It
/// can only change the answer for a message whose timestamp falls inside that
/// gap; the mart fast-path refuses `week` for the related reason that
/// day-truncating a rolling instant is lossy.
fn parse_period(spec: &str) -> Scope {
    let now_us = i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_micros(),
    )
    .unwrap_or(0);
    let (year, month, day) = civil_from_micros(now_us);
    let midnight = micros_from_civil(year, month, day);
    match spec {
        "today" => Scope {
            since: Some(isoformat_utc(midnight)),
            // `.replace(hour=23, minute=59, second=59, microsecond=0)`.
            until: Some(isoformat_utc(midnight + 86_399 * 1_000_000)),
        },
        "7days" => Scope {
            since: Some(isoformat_utc(now_us - 7 * 86_400 * 1_000_000)),
            until: Some(isoformat_utc(now_us)),
        },
        "month" => {
            let first = micros_from_civil(year, month, 1);
            let last_day = days_in_month(year, month);
            let last = micros_from_civil(year, month, last_day) + 86_399 * 1_000_000;
            Scope {
                since: Some(isoformat_utc(first)),
                until: Some(isoformat_utc(last)),
            }
        }
        // "all", and nothing else reaches here — the alias map gates it.
        _ => Scope {
            since: None,
            until: None,
        },
    }
}

/// `scope.since[:10]` — the mart's `day` column is `YYYY-MM-DD`.
fn iso_day(value: Option<&String>) -> Option<String> {
    value.map(|value| value.chars().take(10).collect())
}

// ── GET /api/cost-data/by-provider ───────────────────────────────────────────

async fn get_cost_by_provider(
    State(state): State<AppState>,
    RawQuery(raw): RawQuery,
) -> HandlerResult {
    let query = Query::parse(raw.as_deref().unwrap_or_default());
    let period = query.str_or("period", "month").to_owned();
    let Some(spec) = period_spec(&period) else {
        // `', '.join(sorted(_BY_PROVIDER_PERIOD_MAP))`.
        return Err(HttpError::bad_request(format!(
            "Unknown period '{period}'. Valid: all, month, today, week"
        )));
    };
    let provider_filter = normalise_filter(query.opt_list("provider").as_deref());
    let path = optional_log_path(&query, &state);

    let worker = state.clone();
    let mut rows = tokio::task::spawn_blocking(move || by_provider_rows(&worker, spec, &path))
        .await
        .map_err(|err| join_failure(&err))??;

    let currency = active_currency_payload(&state.config().currency)
        .map_err(|err| HttpError::new(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))?;
    // `if rate != 1.0:` — DIV-052.
    // `out_rows.sort(key=..., reverse=True)` — stable, so ties keep the
    // first-appearance order the rollup produced.
    rows.sort_by(|a, b| {
        b.cost_usd
            .partial_cmp(&a.cost_usd)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    // The provider filter runs AFTER the sort and after the FX conversion —
    // order matters only for the reader, but it is the order Python wrote.
    if let Some(filter) = &provider_filter {
        rows.retain(|row| filter.contains(&row.provider.to_lowercase()));
    }

    let rendered: Vec<Value> = rows
        .iter()
        .map(|row| {
            let mut obj = Map::new();
            obj.insert("provider".to_owned(), Value::from(row.provider.clone()));
            obj.insert("cost_usd".to_owned(), Value::from(row.cost_usd));
            obj.insert("message_count".to_owned(), Value::from(row.message_count));
            obj.insert("session_count".to_owned(), Value::from(row.session_count));
            Value::Object(obj)
        })
        .collect();

    let mut payload = Map::new();
    payload.insert("period".to_owned(), Value::from(period));
    payload.insert("rows".to_owned(), Value::Array(rendered));
    payload.insert("currency".to_owned(), currency);
    Ok(JsonBody::ok(Value::Object(payload)))
}

struct ProviderRow {
    provider: String,
    cost_usd: f64,
    message_count: i64,
    session_count: i64,
}

fn by_provider_rows(
    state: &AppState,
    spec: &str,
    path: &str,
) -> Result<Vec<ProviderRow>, HttpError> {
    let scope = parse_period(spec);
    let conn = state.connect().map_err(|err| any_500(&err))?;
    if !path.is_empty() {
        // RANK 19: `provider_day_mart` is keyed (day, provider) with NO
        // project_id, so it can only answer the all-projects question. A
        // project-scoped request therefore BYPASSES the mart entirely rather
        // than leaking the whole store's spend onto one project's card.
        let project_ids = project_ids_for(&conn, path)?;
        let engine =
            crate::pricing::engine(&conn, state.package_dir()).map_err(|err| any_500(&err))?;
        return by_provider_from_messages(&conn, &scope, &engine, Some(&project_ids))
            .map_err(sql_500);
    }
    let day_from = iso_day(scope.since.as_ref());
    let day_to = iso_day(scope.until.as_ref());
    let mart =
        provider_day_rollup(&conn, day_from.as_deref(), day_to.as_deref()).map_err(sql_500)?;
    if !mart.is_empty() {
        return Ok(mart);
    }
    // A half-finished backfill keeps working: an empty mart falls back to the
    // messages sweep rather than reporting no spend.
    let engine = crate::pricing::engine(&conn, state.package_dir()).map_err(|err| any_500(&err))?;
    by_provider_from_messages(&conn, &scope, &engine, None).map_err(sql_500)
}

/// `provider_day_rollup` → `_build_by_provider_rows_from_mart`.
fn provider_day_rollup(
    conn: &Connection,
    day_from: Option<&str>,
    day_to: Option<&str>,
) -> rusqlite::Result<Vec<ProviderRow>> {
    if !table_exists(conn, "provider_day_mart")? {
        return Ok(Vec::new());
    }
    let mut sql = "SELECT provider, \
                          SUM(cost_usd) AS cost_usd, \
                          SUM(message_count) AS message_count, \
                          SUM(session_count) AS session_count, \
                          SUM(project_count) AS project_count \
                   FROM provider_day_mart WHERE 1=1"
        .to_owned();
    let mut params: Vec<String> = Vec::new();
    if let Some(from) = day_from.filter(|value| !value.is_empty()) {
        sql.push_str(" AND day >= ?");
        params.push(from.to_owned());
    }
    if let Some(to) = day_to.filter(|value| !value.is_empty()) {
        sql.push_str(" AND day <= ?");
        params.push(to.to_owned());
    }
    sql.push_str(" GROUP BY provider ORDER BY SUM(cost_usd) DESC");
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(rusqlite::params_from_iter(params.iter()), |row| {
        Ok(ProviderRow {
            // `(r.get("provider") or "unknown").lower()`.
            provider: row
                .get::<_, Option<String>>(0)?
                .filter(|value| !value.is_empty())
                .unwrap_or_else(|| "unknown".to_owned())
                .to_lowercase(),
            cost_usd: row.get::<_, Option<f64>>(1)?.unwrap_or(0.0),
            message_count: row.get::<_, Option<i64>>(2)?.unwrap_or(0),
            session_count: row.get::<_, Option<i64>>(3)?.unwrap_or(0),
        })
    })?;
    rows.collect()
}

/// `_build_by_provider_rows_from_messages` — the raw sweep.
///
/// NOTE the asymmetry with the mart path: the provider key here is NOT
/// lowercased (`prov = r["provider"] or "unknown"`), so a store with a
/// mixed-case provider yields a differently-cased `provider` field depending on
/// which path answered. Bug-for-bug.
fn by_provider_from_messages(
    conn: &Connection,
    scope: &Scope,
    engine: &PricingEngine,
    project_ids: Option<&[i64]>,
) -> rusqlite::Result<Vec<ProviderRow>> {
    let mut sql = "SELECT projects.provider AS provider, \
                          sessions.id AS session_id, \
                          COALESCE(messages.model, '') AS model, \
                          COALESCE(messages.input_tokens, 0) AS input_tokens, \
                          COALESCE(messages.output_tokens, 0) AS output_tokens, \
                          COALESCE(messages.cache_create_tokens, 0) AS cache_create_tokens, \
                          COALESCE(messages.cache_read_tokens, 0) AS cache_read_tokens, \
                          COALESCE(messages.speed, 'standard') AS speed, \
                          messages.role AS role \
                   FROM messages \
                   JOIN sessions ON sessions.id = messages.session_fk \
                   JOIN projects ON projects.id = sessions.project_id \
                   WHERE 1=1 "
        .to_owned();
    let mut params: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
    // `if project_ids:` — an EMPTY list is falsy and silently means "all". That
    // is Python's behaviour and it is unreachable here (the caller 404s first).
    if let Some(ids) = project_ids.filter(|ids| !ids.is_empty()) {
        sql.push_str(&format!(
            "AND sessions.project_id IN ({}) ",
            vec!["?"; ids.len()].join(",")
        ));
        for id in ids {
            params.push(Box::new(*id));
        }
    }
    if let Some(since) = &scope.since {
        sql.push_str("AND messages.timestamp >= ? ");
        params.push(Box::new(since.clone()));
    }
    if let Some(until) = &scope.until {
        sql.push_str("AND messages.timestamp <= ? ");
        params.push(Box::new(until.clone()));
    }

    let mut stmt = conn.prepare(&sql)?;
    let refs: Vec<&dyn rusqlite::ToSql> = params.iter().map(std::convert::AsRef::as_ref).collect();
    let mut rows = stmt.query(refs.as_slice())?;

    // `setdefault` on a plain dict: insertion-ordered by first appearance.
    let mut order: Vec<String> = Vec::new();
    let mut buckets: Vec<(f64, i64, Vec<i64>)> = Vec::new();
    while let Some(row) = rows.next()? {
        let provider: Option<String> = row.get(0)?;
        let session_id: i64 = row.get(1)?;
        let model: String = row.get(2)?;
        let input: i64 = row.get(3)?;
        let output: i64 = row.get(4)?;
        let cache_create: i64 = row.get(5)?;
        let cache_read: i64 = row.get(6)?;
        let speed: Option<String> = row.get(7)?;
        let role: Option<String> = row.get(8)?;

        // `prov = r["provider"] or "unknown"` — truthiness, so `""` too.
        let prov = provider
            .clone()
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| "unknown".to_owned());
        let idx = match order.iter().position(|seen| *seen == prov) {
            Some(idx) => idx,
            None => {
                order.push(prov.clone());
                buckets.push((0.0, 0, Vec::new()));
                buckets.len() - 1
            }
        };
        buckets[idx].1 += 1;
        // A `set` of session ids — only the COUNT reaches the payload, so a
        // sorted-unique vec is the same answer with a deterministic cost.
        if !buckets[idx].2.contains(&session_id) {
            buckets[idx].2.push(session_id);
        }
        if role.as_deref() == Some("assistant") && !model.is_empty() {
            // `provider=prov or "anthropic"` — `prov` is already non-empty by
            // construction, so the `or` is dead. Ported as written.
            let cost = engine
                .compute_cost(
                    &RawTokens::canonical(input, output, cache_create, cache_read),
                    &model,
                    &prov,
                    speed
                        .as_deref()
                        .filter(|s| !s.is_empty())
                        .unwrap_or("standard"),
                    None,
                )
                .total_cost;
            // A `+=` chain, NOT `sum()` — do not compensate.
            buckets[idx].0 += cost;
        }
    }

    Ok(order
        .into_iter()
        .zip(buckets)
        .map(
            |(provider, (cost_usd, message_count, sessions))| ProviderRow {
                provider,
                cost_usd,
                message_count,
                session_count: i64::try_from(sessions.len()).unwrap_or(i64::MAX),
            },
        )
        .collect())
}

// ── GET /api/cost-data/by-model ──────────────────────────────────────────────

async fn get_cost_by_model(
    State(state): State<AppState>,
    RawQuery(raw): RawQuery,
) -> HandlerResult {
    let query = Query::parse(raw.as_deref().unwrap_or_default());
    let period = query.str_or("period", "month").to_owned();
    let Some(spec) = period_spec(&period) else {
        return Err(HttpError::bad_request(format!(
            "Unknown period '{period}'. Valid: all, month, today, week"
        )));
    };
    let path = optional_log_path(&query, &state);

    let worker = state.clone();
    let worker_period = period.clone();
    let rows =
        tokio::task::spawn_blocking(move || by_model_rows(&worker, spec, &worker_period, &path))
            .await
            .map_err(|err| join_failure(&err))??;

    let currency = active_currency_payload(&state.config().currency)
        .map_err(|err| HttpError::new(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))?;
    // `rate` is 1.0 (DIV-052), but the MULTIPLICATION is still performed: it is
    // what produces the float, and `cost * 1.0` of an int-shaped mart value is
    // still a float. Kept explicit rather than optimised away.
    let rate = currency
        .get("rate_from_usd")
        .and_then(Value::as_f64)
        .unwrap_or(1.0);

    // `models.setdefault(...)` — insertion-ordered by first appearance in the
    // day-sorted row list.
    let mut order: Vec<String> = Vec::new();
    let mut totals: Vec<f64> = Vec::new();
    let mut daily: Vec<Vec<Value>> = Vec::new();
    for row in &rows {
        let idx = match order.iter().position(|seen| *seen == row.model) {
            Some(idx) => idx,
            None => {
                order.push(row.model.clone());
                totals.push(0.0);
                daily.push(Vec::new());
                order.len() - 1
            }
        };
        let cost = row.cost_usd * rate;
        // `+=`, not `sum()`.
        totals[idx] += cost;
        let mut entry = Map::new();
        entry.insert("date".to_owned(), Value::from(row.day.clone()));
        entry.insert("cost_usd".to_owned(), Value::from(cost));
        entry.insert("message_count".to_owned(), Value::from(row.message_count));
        daily[idx].push(Value::Object(entry));
    }

    // `sorted(models.values(), key=..., reverse=True)` — stable.
    let mut indices: Vec<usize> = (0..order.len()).collect();
    indices.sort_by(|a, b| {
        totals[*b]
            .partial_cmp(&totals[*a])
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let out_models: Vec<Value> = indices
        .into_iter()
        .map(|idx| {
            let mut obj = Map::new();
            obj.insert("model".to_owned(), Value::from(order[idx].clone()));
            obj.insert("total_cost".to_owned(), Value::from(totals[idx]));
            obj.insert("daily".to_owned(), Value::Array(daily[idx].clone()));
            Value::Object(obj)
        })
        .collect();

    let mut payload = Map::new();
    payload.insert("period".to_owned(), Value::from(period));
    payload.insert("models".to_owned(), Value::Array(out_models));
    payload.insert("currency".to_owned(), currency);
    Ok(JsonBody::ok(Value::Object(payload)))
}

struct ModelDayRow {
    day: String,
    model: String,
    cost_usd: f64,
    message_count: i64,
}

fn by_model_rows(
    state: &AppState,
    spec: &str,
    period: &str,
    path: &str,
) -> Result<Vec<ModelDayRow>, HttpError> {
    let scope = parse_period(spec);
    let conn = state.connect().map_err(|err| any_500(&err))?;
    if path.is_empty() {
        return model_day_series(&conn, scope.since.as_deref(), scope.until.as_deref())
            .map_err(sql_500);
    }
    let project_ids = project_ids_for(&conn, path)?;
    if BY_MODEL_MART_PERIODS.contains(&period)
        && by_model_mart_eligible(&conn, &project_ids).map_err(sql_500)?
    {
        return by_model_from_mart(
            &conn,
            &project_ids,
            iso_day(scope.since.as_ref()).as_deref(),
            iso_day(scope.until.as_ref()).as_deref(),
        )
        .map_err(sql_500);
    }
    let engine = crate::pricing::engine(&conn, state.package_dir()).map_err(|err| any_500(&err))?;
    by_model_from_messages(&conn, &scope, &engine, &project_ids).map_err(sql_500)
}

/// `_by_model_mart_eligible` — the two gates, cheapest first.
fn by_model_mart_eligible(conn: &Connection, project_ids: &[i64]) -> rusqlite::Result<bool> {
    if project_ids.is_empty() {
        return Ok(false);
    }
    for pid in project_ids {
        if !mart_has_project_row(conn, *pid)? {
            return Ok(false);
        }
    }
    let mut stmt = conn.prepare(&format!(
        "SELECT 1 FROM usage_events WHERE project_id IN ({}) AND cost_source != 'rate_card' LIMIT 1",
        vec!["?"; project_ids.len()].join(",")
    ))?;
    let mut rows = stmt.query(rusqlite::params_from_iter(project_ids.iter()))?;
    Ok(rows.next()?.is_none())
}

/// `_build_by_model_rows_from_mart`.
fn by_model_from_mart(
    conn: &Connection,
    project_ids: &[i64],
    day_from: Option<&str>,
    day_to: Option<&str>,
) -> rusqlite::Result<Vec<ModelDayRow>> {
    let mut order: Vec<(String, String)> = Vec::new();
    let mut rows: Vec<ModelDayRow> = Vec::new();
    for pid in project_ids {
        for row in daily_for_project(conn, *pid, day_from, day_to, None)? {
            let model = row.model.unwrap_or_default();
            // Mirrors the raw path, which only counts assistant messages
            // carrying a real model id.
            if model.is_empty() || model == "N/A" {
                continue;
            }
            let day = row.day.unwrap_or_default();
            let key = (day.clone(), model.clone());
            match order.iter().position(|seen| *seen == key) {
                Some(idx) => {
                    rows[idx].cost_usd += row.cost_usd;
                    rows[idx].message_count += row.message_count;
                }
                None => {
                    order.push(key);
                    rows.push(ModelDayRow {
                        day,
                        model,
                        cost_usd: row.cost_usd,
                        message_count: row.message_count,
                    });
                }
            }
        }
    }
    sort_by_day(&mut rows);
    Ok(rows)
}

/// `_build_by_model_rows_from_messages` — the project-scoped raw rollup.
fn by_model_from_messages(
    conn: &Connection,
    scope: &Scope,
    engine: &PricingEngine,
    project_ids: &[i64],
) -> rusqlite::Result<Vec<ModelDayRow>> {
    let mut sql = "SELECT projects.provider AS provider, \
                          substr(messages.timestamp, 1, 10) AS day, \
                          COALESCE(messages.model, '') AS model, \
                          COALESCE(messages.input_tokens, 0) AS input_tokens, \
                          COALESCE(messages.output_tokens, 0) AS output_tokens, \
                          COALESCE(messages.cache_create_tokens, 0) AS cache_create_tokens, \
                          COALESCE(messages.cache_read_tokens, 0) AS cache_read_tokens, \
                          COALESCE(messages.speed, 'standard') AS speed, \
                          messages.role AS role \
                   FROM messages \
                   JOIN sessions ON sessions.id = messages.session_fk \
                   JOIN projects ON projects.id = sessions.project_id \
                   WHERE 1=1 "
        .to_owned();
    let mut params: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
    if !project_ids.is_empty() {
        sql.push_str(&format!(
            "AND sessions.project_id IN ({}) ",
            vec!["?"; project_ids.len()].join(",")
        ));
        for id in project_ids {
            params.push(Box::new(*id));
        }
    }
    if let Some(since) = &scope.since {
        sql.push_str("AND messages.timestamp >= ? ");
        params.push(Box::new(since.clone()));
    }
    if let Some(until) = &scope.until {
        sql.push_str("AND messages.timestamp <= ? ");
        params.push(Box::new(until.clone()));
    }

    let mut stmt = conn.prepare(&sql)?;
    let refs: Vec<&dyn rusqlite::ToSql> = params.iter().map(std::convert::AsRef::as_ref).collect();
    let mut sql_rows = stmt.query(refs.as_slice())?;

    let mut order: Vec<(String, String)> = Vec::new();
    let mut rows: Vec<ModelDayRow> = Vec::new();
    while let Some(row) = sql_rows.next()? {
        let provider: Option<String> = row.get(0)?;
        let day: Option<String> = row.get(1)?;
        let model: String = row.get(2)?;
        let input: i64 = row.get(3)?;
        let output: i64 = row.get(4)?;
        let cache_create: i64 = row.get(5)?;
        let cache_read: i64 = row.get(6)?;
        let speed: Option<String> = row.get(7)?;
        let role: Option<String> = row.get(8)?;

        if role.as_deref() != Some("assistant") || model.is_empty() || model == "N/A" {
            continue;
        }
        let day = day.unwrap_or_default();
        let key = (day.clone(), model.clone());
        let idx = match order.iter().position(|seen| *seen == key) {
            Some(idx) => idx,
            None => {
                order.push(key);
                rows.push(ModelDayRow {
                    day,
                    model: model.clone(),
                    cost_usd: 0.0,
                    message_count: 0,
                });
                rows.len() - 1
            }
        };
        rows[idx].message_count += 1;
        rows[idx].cost_usd += engine
            .compute_cost(
                &RawTokens::canonical(input, output, cache_create, cache_read),
                &model,
                provider
                    .as_deref()
                    .filter(|value| !value.is_empty())
                    .unwrap_or("anthropic"),
                speed
                    .as_deref()
                    .filter(|s| !s.is_empty())
                    .unwrap_or("standard"),
                None,
            )
            .total_cost;
    }
    sort_by_day(&mut rows);
    Ok(rows)
}

/// `sorted(per_key.values(), key=lambda b: b["day"])` — STABLE, so equal days
/// keep the insertion order the rollup produced.
fn sort_by_day(rows: &mut [ModelDayRow]) {
    rows.sort_by(|a, b| a.day.cmp(&b.day));
}

/// `mart_queries.model_day_series` — the global (no project) path.
fn model_day_series(
    conn: &Connection,
    since_iso: Option<&str>,
    until_iso: Option<&str>,
) -> rusqlite::Result<Vec<ModelDayRow>> {
    if !table_exists(conn, "model_day_mart")? {
        return Ok(Vec::new());
    }
    let mut sql = "SELECT day, model, \
                          SUM(cost_usd) AS cost_usd, \
                          SUM(message_count) AS message_count \
                   FROM model_day_mart WHERE 1=1"
        .to_owned();
    let mut params: Vec<String> = Vec::new();
    // `_iso_to_day` then `if day_from:` — a truthiness gate, so an empty slice
    // (an empty `since_iso`) adds no clause at all.
    if let Some(from) = since_iso
        .map(|value| value.chars().take(10).collect::<String>())
        .filter(|from| !from.is_empty())
    {
        sql.push_str(" AND day >= ?");
        params.push(from);
    }
    if let Some(to) = until_iso
        .map(|value| value.chars().take(10).collect::<String>())
        .filter(|to| !to.is_empty())
    {
        sql.push_str(" AND day <= ?");
        params.push(to);
    }
    sql.push_str(" GROUP BY day, model ORDER BY day");
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(rusqlite::params_from_iter(params.iter()), |row| {
        Ok(ModelDayRow {
            day: row.get::<_, Option<String>>(0)?.unwrap_or_default(),
            model: row.get::<_, Option<String>>(1)?.unwrap_or_default(),
            cost_usd: row.get::<_, Option<f64>>(2)?.unwrap_or(0.0),
            message_count: row.get::<_, Option<i64>>(3)?.unwrap_or(0),
        })
    })?;
    let mut out = Vec::new();
    for row in rows {
        let row = row?;
        // `if not model: continue` — dropped AFTER the GROUP BY.
        if !row.model.is_empty() {
            out.push(row);
        }
    }
    Ok(out)
}

// ── GET /api/interaction/{interaction_id} ────────────────────────────────────

async fn get_interaction(
    State(state): State<AppState>,
    PathParam(interaction_id): PathParam<String>,
    RawQuery(raw): RawQuery,
) -> HandlerResult {
    let query = Query::parse(raw.as_deref().unwrap_or_default());
    let path = resolve_log_path(&query, &state)?;

    tokio::task::spawn_blocking(move || {
        let conn = state.connect().map_err(|err| any_500(&err))?;
        let project_ids = project_ids_for(&conn, &path)?;
        let built = stax_etl::stats::dataset::build_enriched_dataset(&conn, &project_ids)
            .map_err(|err| any_500(&err))?;
        drop(conn);
        // `if dataset is None: raise HTTPException(404, "Project has no data")`.
        let Some((dataset, _log_dir)) = built else {
            return Err(HttpError::not_found("Project has no data"));
        };
        // A LINEAR scan, and deliberately so: the Python memo exists because the
        // rebuild is ~2.5 s / ~740 MB and the scan is 0.2 ms, not because the
        // scan is slow. The FIRST match wins.
        for ix in &dataset.interactions {
            if ix.interaction_id == interaction_id {
                return Ok(JsonBody::ok(serialise_interaction(&dataset, ix)));
            }
        }
        Err(HttpError::not_found(format!(
            "Interaction '{interaction_id}' not found"
        )))
    })
    .await
    .map_err(|err| join_failure(&err))?
}

/// `_serialise_interaction` — thirteen keys, in the dict literal's order.
fn serialise_interaction(dataset: &EnrichedDataset, ix: &Interaction) -> Value {
    let record = |idx: &usize| {
        dataset
            .records
            .get(*idx)
            .map_or(Value::Null, serialise_record)
    };
    let mut obj = Map::new();
    obj.insert(
        "interaction_id".to_owned(),
        Value::from(ix.interaction_id.clone()),
    );
    obj.insert("session_id".to_owned(), Value::from(ix.session_id.clone()));
    obj.insert("start_time".to_owned(), Value::from(ix.start_time.clone()));
    obj.insert("end_time".to_owned(), Value::from(ix.end_time.clone()));
    obj.insert("model".to_owned(), ix.model.clone());
    obj.insert("tool_count".to_owned(), Value::from(ix.tool_count));
    obj.insert(
        "assistant_steps".to_owned(),
        Value::from(ix.assistant_steps),
    );
    obj.insert(
        "is_continuation".to_owned(),
        Value::Bool(ix.is_continuation),
    );
    obj.insert(
        "tools_used".to_owned(),
        Value::Array(ix.tools_used.iter().map(tool_to_json).collect()),
    );
    obj.insert("has_task_tool".to_owned(), Value::Bool(ix.has_task_tool));
    obj.insert("command".to_owned(), record(&ix.command));
    obj.insert(
        "responses".to_owned(),
        Value::Array(ix.responses.iter().map(record).collect()),
    );
    obj.insert(
        "tool_results".to_owned(),
        Value::Array(ix.tool_results.iter().map(record).collect()),
    );
    Value::Object(obj)
}

/// `_serialise_record` — fields listed explicitly, and `raw_data` is NOT one of
/// them (it holds non-JSON-native fragments from some adapters).
fn serialise_record(rec: &Record) -> Value {
    let mut obj = Map::new();
    obj.insert("session_id".to_owned(), Value::from(rec.session_id.clone()));
    obj.insert("kind".to_owned(), Value::from(rec.kind.clone()));
    obj.insert("timestamp".to_owned(), Value::from(rec.timestamp.clone()));
    obj.insert("model".to_owned(), rec.model.clone());
    obj.insert("content".to_owned(), Value::from(rec.content.clone()));
    obj.insert("tokens".to_owned(), rec.tokens.to_json());
    obj.insert(
        "tools".to_owned(),
        Value::Array(rec.tools.iter().map(tool_to_json).collect()),
    );
    obj.insert("is_error".to_owned(), Value::Bool(rec.is_error));
    obj.insert(
        "error_category".to_owned(),
        rec.error_category.clone().map_or(Value::Null, Value::from),
    );
    obj.insert(
        "is_interruption".to_owned(),
        Value::Bool(rec.is_interruption),
    );
    obj.insert(
        "has_tool_result".to_owned(),
        Value::Bool(rec.has_tool_result),
    );
    obj.insert("uuid".to_owned(), rec.uuid.clone());
    obj.insert("parent_uuid".to_owned(), rec.parent_uuid.clone());
    obj.insert("is_sidechain".to_owned(), rec.is_sidechain.clone());
    obj.insert("message_id".to_owned(), rec.message_id.clone());
    obj.insert("cwd".to_owned(), rec.cwd.clone());
    Value::Object(obj)
}

/// `_tools_from`'s `{"name", "id", "input"}` dict, in that key order.
///
/// A `ToolRef` with no block cannot occur on this path — `build_enriched_dataset`
/// builds at `Detail::Full` — but the empty object is the honest answer for the
/// lean builds where it can.
fn tool_to_json(tool: &stax_etl::stats::enricher::ToolRef) -> Value {
    let mut obj = Map::new();
    if let Some(block) = &tool.block {
        obj.insert("name".to_owned(), block.name.clone());
        obj.insert("id".to_owned(), block.id.clone());
        obj.insert("input".to_owned(), block.input.clone());
    }
    Value::Object(obj)
}

// ── civil calendar ───────────────────────────────────────────────────────────
//
// FLAGGED FOR THE ARCHITECT'S DEDUP LIST: identical to `routes/budgets.rs`'s
// copy, which is itself a re-derivation of the private helpers in
// `stax_etl::stats::pydatetime`. Three copies want one home in that module.

fn civil_from_micros(micros: i64) -> (i64, i64, i64) {
    civil_from_days(micros.div_euclid(86_400_000_000))
}

fn micros_from_civil(year: i64, month: i64, day: i64) -> i64 {
    days_from_civil(year, month, day) * 86_400_000_000
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = if month > 2 { month - 3 } else { month + 9 };
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn days_in_month(year: i64, month: i64) -> i64 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if (year % 4 == 0 && year % 100 != 0) || year % 400 == 0 => 29,
        2 => 28,
        _ => 30,
    }
}

/// `datetime.isoformat()` for a UTC-aware value — with the microsecond field
/// only when it is non-zero, which is CPython's rule and the reason the `7days`
/// bounds look different from every other period's.
fn isoformat_utc(micros: i64) -> String {
    let (year, month, day) = civil_from_micros(micros);
    let secs_of_day = micros.div_euclid(1_000_000).rem_euclid(86_400);
    let (hour, minute, second) = (
        secs_of_day / 3600,
        (secs_of_day / 60) % 60,
        secs_of_day % 60,
    );
    let sub = micros.rem_euclid(1_000_000);
    let stamp = format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}");
    if sub == 0 {
        format!("{stamp}+00:00")
    } else {
        format!("{stamp}.{sub:06}+00:00")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unknown_range_names_the_valid_ones_in_sorted_order() {
        // `', '.join(sorted({"all", "7d", "30d"}))` puts the DIGIT first.
        assert!(range_days("7d").is_some());
        assert_eq!(range_days("nope"), None);
        let message = format!("Unknown range 'nope'. Valid: {}", "30d, 7d, all");
        assert_eq!(message, "Unknown range 'nope'. Valid: 30d, 7d, all");
    }

    #[test]
    fn the_window_anchor_counts_as_day_one() {
        // "the anchor day counts as day 1, so 7d == the last 7 calendar days".
        assert_eq!(
            iso_date_minus_days("2026-07-31", 6).as_deref(),
            Some("2026-07-25")
        );
        // Across a month boundary, and across a leap day.
        assert_eq!(
            iso_date_minus_days("2026-03-01", 29).as_deref(),
            Some("2026-01-31")
        );
        assert_eq!(
            iso_date_minus_days("2024-03-01", 1).as_deref(),
            Some("2024-02-29")
        );
        // An unparseable anchor is the UNWINDOWED fallback, not a 500.
        assert_eq!(iso_date_minus_days("not-a-date", 6), None);
        assert_eq!(iso_date_minus_days("", 6), None);
    }

    #[test]
    fn only_day_aligned_periods_may_reach_the_mart() {
        // `week` is a rolling instant; truncating it to 10 chars pulls in the
        // whole boundary day (+8-29%), which is why it is absent here.
        assert!(BY_MODEL_MART_PERIODS.contains(&"today"));
        assert!(BY_MODEL_MART_PERIODS.contains(&"month"));
        assert!(BY_MODEL_MART_PERIODS.contains(&"all"));
        assert!(!BY_MODEL_MART_PERIODS.contains(&"week"));
    }

    #[test]
    fn a_rolling_period_carries_microseconds_and_a_day_boundary_does_not() {
        // CPython omits `.ffffff` only when it is zero, and that is exactly the
        // difference between `today`/`month` and `7days`.
        assert_eq!(isoformat_utc(0), "1970-01-01T00:00:00+00:00");
        assert_eq!(isoformat_utc(1), "1970-01-01T00:00:00.000001+00:00");
        assert_eq!(
            isoformat_utc(micros_from_civil(2026, 7, 31) + 86_399 * 1_000_000),
            "2026-07-31T23:59:59+00:00"
        );
    }

    #[test]
    fn the_reshape_drops_sessions_and_keeps_seven_keys() {
        let row = ToolMartRow {
            calls: 3,
            calls_total: 7,
            cost: 1.5,
            tokens_in: 10,
            tokens_out: 20,
            cache_read_tokens: 30,
            cache_creation_tokens: 40,
            sessions: 99,
        };
        assert_eq!(
            stax_memory::pyjson::dumps_http(&row.to_aggregator_shape()),
            r#"{"calls":3,"calls_total":7,"input_tokens":10,"output_tokens":20,"cache_read_tokens":30,"cache_creation_tokens":40,"cost":1.5}"#
        );
    }

    #[test]
    fn the_missing_key_default_is_shaped_per_key() {
        for key in [
            "tool_costs",
            "token_composition",
            "outliers",
            "error_cost",
            "trends",
        ] {
            assert!(
                DICT_SHAPED_KEYS.contains(&key),
                "{key} should default to {{}}"
            );
        }
        for key in [
            "session_costs",
            "command_costs",
            "retry_signals",
            "session_efficiency",
        ] {
            assert!(
                !DICT_SHAPED_KEYS.contains(&key),
                "{key} should default to []"
            );
        }
        assert_eq!(COST_KEYS.len(), 9);
    }

    #[test]
    fn a_non_dict_token_composition_is_replaced_with_the_three_key_skeleton() {
        let mut stats = Map::new();
        stats.insert("token_composition".to_owned(), Value::from(7));
        ensure_token_composition(&mut stats);
        assert_eq!(
            stax_memory::pyjson::dumps_http(stats.get("token_composition").expect("present")),
            r#"{"daily":{},"totals":{},"per_session":{}}"#
        );
    }

    #[test]
    fn the_day_sort_is_stable_across_equal_days() {
        let mut rows = vec![
            ModelDayRow {
                day: "2026-07-02".into(),
                model: "b".into(),
                cost_usd: 0.0,
                message_count: 0,
            },
            ModelDayRow {
                day: "2026-07-01".into(),
                model: "z".into(),
                cost_usd: 0.0,
                message_count: 0,
            },
            ModelDayRow {
                day: "2026-07-01".into(),
                model: "a".into(),
                cost_usd: 0.0,
                message_count: 0,
            },
        ];
        sort_by_day(&mut rows);
        let models: Vec<&str> = rows.iter().map(|r| r.model.as_str()).collect();
        // `z` before `a`: the key is the DAY only, so insertion order breaks ties.
        assert_eq!(models, vec!["z", "a", "b"]);
    }
}
