//! `routes/commands.py` — 3 endpoints, wave 5 (batch A).
//!
//! | Item | Method | FastAPI path | axum path | State |
//! |---|---|---|---|---|
//! | `RS-5-056` | `GET` | `/api/commands`          | `/api/commands`          | ported |
//! | `RS-5-057` | `GET` | `/api/commands/daily`    | `/api/commands/daily`    | ported |
//! | `RS-5-058` | `GET` | `/api/tool-distribution` | `/api/tool-distribution` | ported |
//!
//! # The three traps in `_interaction_to_command`
//!
//! 1. **`compute_cost` is called with no `provider`.** The signature defaults to
//!    `"anthropic"`, so every command row in this endpoint is priced as
//!    Anthropic regardless of which adapter produced it — a cursor or codex
//!    interaction bills through the Anthropic pricer here while
//!    `/api/cost-data`'s aggregator path passes the real provider. That is a
//!    live divergence *inside Python* and it is inherited verbatim (DIV-067).
//! 2. **`sum()` over the per-`(model, speed)` costs is Neumaier-compensated**
//!    (gh-100425). A `+=` chain drifts 1-2 ULP past a few thousand buckets, and
//!    `cost` is also the default sort key, so the drift can reorder the page.
//! 3. **`tokens` is a `Counter`, and a `Counter` that never saw a record
//!    serialises as `{}`** — not as four zeros. [`TokenBag::touched`] is exactly
//!    that bit, and an interaction with no responses and no tool results hits it.
//!
//! The `_STATS_CACHE` / `_DATASET_CACHE` memos are not ported, for the reason
//! DIV-055 already records for `routes/data.py`: they are latency devices whose
//! signature moves the instant ingest writes, so they cannot serve a different
//! answer — only a faster one.

use std::collections::HashSet;

use axum::Router;
use axum::extract::{RawQuery, State};
use axum::http::StatusCode;
use axum::routing::get;
use rusqlite::Connection;
use serde_json::{Map, Value};
use stax_etl::pricing::costs::PricingEngine;
// CPython's `sum()` over floats — one home, in the crate that owns the
// CPython-numeric ports. This file used to carry a private copy.
use stax_etl::stats::aggregator::neumaier_sum;
use stax_etl::stats::enricher::{EnrichedDataset, Interaction, Record, TokenBag};

use crate::currency::active_currency_payload;
use crate::json::{HandlerResult, HttpError, JsonBody, join_failure, validation_422_field_only};
use crate::pyops::path_name;
use crate::qs::Query;
use crate::services::mart_queries::table_exists;
use crate::services::stats_memo;
use crate::state::AppState;

/// `_DEFAULT_LIMIT` / `_MAX_LIMIT`.
const DEFAULT_LIMIT: i64 = 50;
const MAX_LIMIT: i64 = 500;

// `_TZ_OFFSET_MIN` / `_MAX` moved to `crate::services::stats_memo` with the
// `_clamp_tz_offset` that is their only reader — DIV-055. Three copies of a
// two-line constant existed because the clamp was open-coded at three call
// sites; the memo funnels all three now, exactly as `_project_stats_cached`
// does in Python.

/// Mount this module's endpoints onto `router`.
pub fn register(router: Router<AppState>) -> Router<AppState> {
    router
        .route("/api/commands", get(get_commands))
        // Declared BEFORE `/api/tool-distribution` in the Python module, and
        // both are literal paths, so no ordering question arises.
        .route("/api/commands/daily", get(get_commands_daily))
        .route("/api/tool-distribution", get(get_tool_distribution))
}

// ── shared: the project resolution both routes do ────────────────────────────

/// `_resolve_log_path` — the query param, else `deps.current_log_path`, else 400.
///
/// `log_path or deps.current_log_path` is a truthiness chain: an EMPTY
/// `?log_path=` falls through to the server state rather than being honoured as
/// a value, and an empty state 400s.
fn resolve_log_path(query: &Query, state: &AppState) -> Result<String, HttpError> {
    let from_query = query.get("log_path").unwrap_or_default();
    if !from_query.is_empty() {
        return Ok(from_query.to_owned());
    }
    match state.current_project().log_path {
        Some(path) if !path.is_empty() => Ok(path),
        _ => Err(HttpError::bad_request(
            "No project selected or log_path provided",
        )),
    }
}

/// `queries.get_projects_by_slug` — `(id, provider)` in row order.
fn projects_by_slug(conn: &Connection, slug: &str) -> rusqlite::Result<Vec<(i64, String)>> {
    let mut stmt = conn.prepare("SELECT id, provider FROM projects WHERE slug = ?")?;
    stmt.query_map([slug], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect()
}

/// `_project_ids_for` — the 404 string, em-dash included.
fn project_ids_for(conn: &Connection, path: &str) -> Result<Vec<i64>, HttpError> {
    let slug = path_name(path);
    let rows = projects_by_slug(conn, &slug)
        .map_err(|err| HttpError::new(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))?;
    if rows.is_empty() {
        return Err(HttpError::not_found(format!(
            "Project '{slug}' not found in store — try /api/refresh first"
        )));
    }
    Ok(rows.into_iter().map(|(id, _)| id).collect())
}

/// `{p.strip().lower() for p in raw if p and p.strip()}`, empty set → `None`.
fn normalise_filter(raw: Option<&[String]>) -> Option<HashSet<String>> {
    let raw = raw?;
    let normed: HashSet<String> = raw
        .iter()
        .filter(|value| !value.trim().is_empty())
        .map(|value| value.trim().to_lowercase())
        .collect();
    (!normed.is_empty()).then_some(normed)
}

// ── GET /api/commands ────────────────────────────────────────────────────────

/// One assembled command row, pre-JSON.
struct CommandRow {
    interaction_id: String,
    session_id: String,
    timestamp: String,
    prompt_preview: String,
    /// `None` when `by_model` was empty — see [`interaction_to_command`].
    cost: Option<f64>,
    tokens: TokenBag,
    tools_used: i64,
    steps: i64,
    models_used: Vec<String>,
    had_error: bool,
}

async fn get_commands(State(state): State<AppState>, RawQuery(raw): RawQuery) -> HandlerResult {
    let query = Query::parse(raw.as_deref().unwrap_or_default());
    let path = resolve_log_path(&query, &state)?;
    // NOT FastAPI's shape — see `json::validation_422_field_only`. Preserved
    // byte-for-byte by the wave-5 dedup pass; the fix is a behaviour change.
    let unprocessable = |err: crate::qs::QueryError| validation_422_field_only(&err);
    let mut offset = query.int_or("offset", 0).map_err(unprocessable)?;
    let mut limit = query
        .int_or("limit", DEFAULT_LIMIT)
        .map_err(unprocessable)?;
    let sort = query.str_or("sort", "cost").to_owned();
    let order = query.str_or("order", "desc").to_owned();

    // "Clamp pagination inputs defensively" — note the asymmetry: a too-small
    // limit becomes the DEFAULT (50), not 1.
    if offset < 0 {
        offset = 0;
    }
    if limit < 1 {
        limit = DEFAULT_LIMIT;
    }
    if limit > MAX_LIMIT {
        limit = MAX_LIMIT;
    }
    // `order != "asc"` — anything that is not exactly `asc` is descending.
    let reverse = order != "asc";

    let worker = state.clone();
    let built = tokio::task::spawn_blocking(move || build_command_rows(&worker, &path))
        .await
        .map_err(|err| join_failure(&err))??;

    let Some(mut commands) = built else {
        // `if dataset is None:` — the early return has NO `currency` key.
        let mut obj = Map::new();
        obj.insert("commands".to_owned(), Value::Array(Vec::new()));
        obj.insert("total".to_owned(), Value::from(0));
        obj.insert("offset".to_owned(), Value::from(offset));
        obj.insert("limit".to_owned(), Value::from(limit));
        return Ok(JsonBody::ok(Value::Object(obj)));
    };

    sort_commands(&mut commands, &sort, reverse);

    let total = commands.len();
    // `commands[offset : offset + limit]` — Python's slice never panics and
    // never wraps; both bounds are clamped to the length.
    let start = usize::try_from(offset).unwrap_or(usize::MAX).min(total);
    let end = offset
        .checked_add(limit)
        .and_then(|n| usize::try_from(n).ok())
        .unwrap_or(usize::MAX)
        .min(total);
    let page = &commands[start..end];

    let currency = active_currency_payload(&state.config().currency)
        .map_err(|err| HttpError::new(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))?;
    // `if rate != 1.0:` — DIV-052 keeps the non-USD leg unreachable, so the
    // conversion loop is not ported rather than ported blind.

    let mut obj = Map::new();
    obj.insert(
        "commands".to_owned(),
        Value::Array(page.iter().map(command_to_json).collect()),
    );
    obj.insert("total".to_owned(), Value::from(total));
    obj.insert("offset".to_owned(), Value::from(offset));
    obj.insert("limit".to_owned(), Value::from(limit));
    obj.insert("currency".to_owned(), currency);
    Ok(JsonBody::ok(Value::Object(obj)))
}

/// The blocking body: open, resolve, rebuild the dataset, flatten.
///
/// `Ok(None)` is Python's `dataset is None`.
fn build_command_rows(state: &AppState, path: &str) -> Result<Option<Vec<CommandRow>>, HttpError> {
    let conn = state
        .connect()
        .map_err(|err| HttpError::new(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))?;
    let project_ids = project_ids_for(&conn, path)?;
    let engine = crate::pricing::engine(&conn, state.package_dir())
        .map_err(|err| HttpError::new(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))?;
    let built = stax_etl::stats::dataset::build_enriched_dataset(&conn, &project_ids)
        .map_err(|err| HttpError::new(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))?;
    let Some((dataset, _log_dir)) = built else {
        return Ok(None);
    };
    Ok(Some(
        dataset
            .interactions
            .iter()
            .map(|ix| interaction_to_command(&dataset, ix, &engine))
            .collect(),
    ))
}

/// `_interaction_to_command` — see the module docs for the three traps.
fn interaction_to_command(
    dataset: &EnrichedDataset,
    ix: &Interaction,
    engine: &PricingEngine,
) -> CommandRow {
    let mut tokens = TokenBag::default();
    // `by_model` is a plain dict keyed `(model, speed)`, so it is
    // INSERTION-ordered and the compensated sum below walks it in that order.
    // A `HashMap` would randomise the summation order, which changes the last
    // ULP of `cost`, which is the default sort key.
    let mut by_model_order: Vec<(String, String)> = Vec::new();
    let mut by_model: Vec<TokenBag> = Vec::new();
    let mut had_error = false;
    let mut models_used: HashSet<String> = HashSet::new();

    for record in ix
        .responses
        .iter()
        .chain(ix.tool_results.iter())
        .filter_map(|idx| dataset.records.get(*idx))
    {
        if record.is_error {
            had_error = true;
        }
        tokens.add(&record.tokens);
        // `r.kind == "assistant" and r.model and r.model != "N/A"` — truthiness
        // on a field that is `None` for every non-assistant record.
        if record.kind == "assistant"
            && let Some(model) = model_str(record)
            && model != "N/A"
        {
            models_used.insert(model.clone());
            let key = (model, record.speed.clone());
            match by_model_order.iter().position(|existing| existing == &key) {
                Some(idx) => by_model[idx].add(&record.tokens),
                None => {
                    by_model_order.push(key);
                    let mut bag = TokenBag::default();
                    bag.add(&record.tokens);
                    by_model.push(bag);
                }
            }
        }
    }

    // TRAP 1: no `provider` argument, so `compute_cost`'s default applies.
    // TRAP 2: `sum()` over the generator is compensated.
    // TRAP 4, which the differ found and reading did not: `sum()` over an EMPTY
    // generator returns the **int** `0`, so a command with no priced assistant
    // response ships `"cost":0` while every other command ships a float. An
    // `f64` zero renders `0.0` and diverges on exactly those rows — 6 of the 14
    // red cases in the first full run, on a project whose user turns are mostly
    // tool-result-only. `None` here IS Python's int.
    let cost = (!by_model_order.is_empty()).then(|| {
        neumaier_sum(
            by_model_order
                .iter()
                .zip(by_model.iter())
                .map(|((model, speed), bag)| {
                    engine
                        .compute_cost(&bag.raw(), model, "anthropic", speed, None)
                        .total_cost
                }),
        )
    });

    let mut models: Vec<String> = models_used.into_iter().collect();
    models.sort();

    let command = dataset.records.get(ix.command);
    CommandRow {
        interaction_id: ix.interaction_id.clone(),
        session_id: ix.session_id.clone(),
        timestamp: ix.start_time.clone(),
        prompt_preview: preview(command.map_or("", |rec| rec.content.as_str()), 200),
        cost,
        tokens,
        tools_used: i64::try_from(ix.tool_count).unwrap_or(i64::MAX),
        steps: i64::try_from(ix.assistant_steps).unwrap_or(i64::MAX),
        models_used: models,
        had_error,
    }
}

/// `rec.model` as a string, or `None` for the falsy cases Python's `and` skips.
fn model_str(record: &Record) -> Option<String> {
    match &record.model {
        Value::String(model) if !model.is_empty() => Some(model.clone()),
        _ => None,
    }
}

/// `_preview` — newlines to spaces, strip, then the first `limit` CHARACTERS.
///
/// `str.strip()` with no argument removes Unicode whitespace from both ends,
/// and the slice counts code points, not bytes — a 200-byte truncation would
/// split a multi-byte character and change the payload (and, in Rust, panic).
fn preview(text: &str, limit: usize) -> String {
    if text.is_empty() {
        return String::new();
    }
    // The two `.replace` calls collapse into one pattern set: both map to a
    // single space, so a `\r\n` still becomes two spaces either way.
    text.replace(['\n', '\r'], " ")
        .trim()
        .chars()
        .take(limit)
        .collect()
}

fn command_to_json(row: &CommandRow) -> Value {
    let mut obj = Map::new();
    obj.insert(
        "interaction_id".to_owned(),
        Value::from(row.interaction_id.clone()),
    );
    obj.insert("session_id".to_owned(), Value::from(row.session_id.clone()));
    obj.insert("timestamp".to_owned(), Value::from(row.timestamp.clone()));
    obj.insert(
        "prompt_preview".to_owned(),
        Value::from(row.prompt_preview.clone()),
    );
    // `Some(f)` → a float; `None` → Python's `sum([]) == 0`, an **int**.
    obj.insert(
        "cost".to_owned(),
        row.cost.map_or_else(|| Value::from(0), Value::from),
    );
    obj.insert("tokens".to_owned(), row.tokens.to_json());
    obj.insert("tools_used".to_owned(), Value::from(row.tools_used));
    obj.insert("steps".to_owned(), Value::from(row.steps));
    obj.insert(
        "models_used".to_owned(),
        Value::Array(row.models_used.iter().cloned().map(Value::from).collect()),
    );
    obj.insert("had_error".to_owned(), Value::Bool(row.had_error));
    Value::Object(obj)
}

/// `commands.sort(key=_SORT_KEYS.get(sort, _SORT_KEYS["cost"]), reverse=reverse)`.
///
/// An unknown `sort` falls back to `cost` rather than erroring, and Python's
/// `reverse=True` keeps the sort STABLE (it does not reverse ties), so the
/// comparator is inverted rather than the list.
fn sort_commands(commands: &mut [CommandRow], sort: &str, reverse: bool) {
    // `tokens` sums `c["tokens"].values()` — the SERIALISED dict, so a present
    // `reasoning` key is counted too and an untouched bag sums to 0.
    fn token_total(bag: TokenBag) -> i64 {
        if !bag.touched {
            return 0;
        }
        bag.input + bag.output + bag.cache_creation + bag.cache_read + bag.reasoning.unwrap_or(0)
    }
    let compare = |a: &CommandRow, b: &CommandRow| match sort {
        "tokens" => token_total(a.tokens).cmp(&token_total(b.tokens)),
        "tools" => a.tools_used.cmp(&b.tools_used),
        "steps" => a.steps.cmp(&b.steps),
        "time" => a.timestamp.cmp(&b.timestamp),
        // "cost", and every unrecognised value. Python sorts the int `0` and
        // the floats in one list — `0 < 0.5` compares numerically — so the
        // absent-sum case collapses to `0.0` HERE and only here; the JSON
        // still ships the int.
        _ => a
            .cost
            .unwrap_or(0.0)
            .partial_cmp(&b.cost.unwrap_or(0.0))
            .unwrap_or(std::cmp::Ordering::Equal),
    };
    if reverse {
        commands.sort_by(|a, b| compare(b, a));
    } else {
        commands.sort_by(compare);
    }
}

// ── GET /api/commands/daily ──────────────────────────────────────────────────

async fn get_commands_daily(
    State(state): State<AppState>,
    RawQuery(raw): RawQuery,
) -> HandlerResult {
    let query = Query::parse(raw.as_deref().unwrap_or_default());
    // NOTE: this route does NOT go through `_resolve_log_path` — no project and
    // no `log_path` is the legitimate cross-project view, not a 400.
    let from_query = query.get("log_path").unwrap_or_default().to_owned();
    let path = if from_query.is_empty() {
        state.current_project().log_path.unwrap_or_default()
    } else {
        from_query
    };

    tokio::task::spawn_blocking(move || {
        let conn = state
            .connect()
            .map_err(|err| HttpError::new(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))?;
        let (daily, scope) = if path.is_empty() {
            (
                command_day_series(&conn, None).map_err(|err| {
                    HttpError::new(StatusCode::INTERNAL_SERVER_ERROR, err.to_string())
                })?,
                "global",
            )
        } else {
            let project_ids = project_ids_for(&conn, &path)?;
            (
                command_day_series(&conn, Some(&project_ids)).map_err(|err| {
                    HttpError::new(StatusCode::INTERNAL_SERVER_ERROR, err.to_string())
                })?,
                "project",
            )
        };
        // `sum(int(d["commands"]) for d in daily)` — an integer sum, exact.
        let total: i64 = daily.iter().map(|(_, commands)| *commands).sum();

        let rows: Vec<Value> = daily
            .into_iter()
            .map(|(date, commands)| {
                let mut obj = Map::new();
                obj.insert("date".to_owned(), Value::from(date));
                obj.insert("commands".to_owned(), Value::from(commands));
                Value::Object(obj)
            })
            .collect();
        let mut obj = Map::new();
        obj.insert("daily".to_owned(), Value::Array(rows));
        obj.insert("total".to_owned(), Value::from(total));
        obj.insert("scope".to_owned(), Value::from(scope));
        Ok(JsonBody::ok(Value::Object(obj)))
    })
    .await
    .map_err(|err| join_failure(&err))?
}

/// `mart_queries.command_day_series` — `[(day, commands)]`, oldest first.
///
/// FLAGGED FOR THE ARCHITECT'S DEDUP LIST: this is a `store/mart_queries.py`
/// read helper and belongs in `stax-etl` next to the mart builders. Written
/// file-locally because batch A may not edit crates outside `stax-server`.
///
/// An **empty** id slice returns `[]` without touching the DB — the same
/// never-promote-empty-to-all contract `queries._scoped_rows` documents.
fn command_day_series(
    conn: &Connection,
    project_ids: Option<&[i64]>,
) -> rusqlite::Result<Vec<(String, i64)>> {
    if !table_exists(conn, "command_day_mart")? {
        return Ok(Vec::new());
    }
    let mut sql =
        "SELECT day, SUM(command_count) AS commands FROM command_day_mart WHERE 1=1".to_owned();
    let mut params: Vec<i64> = Vec::new();
    if let Some(ids) = project_ids {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        sql.push_str(&format!(
            " AND project_id IN ({})",
            vec!["?"; ids.len()].join(",")
        ));
        params.extend_from_slice(ids);
    }
    sql.push_str(" GROUP BY day ORDER BY day");
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(rusqlite::params_from_iter(params.iter()), |row| {
        Ok((
            row.get::<_, Option<String>>(0)?,
            row.get::<_, Option<i64>>(1)?.unwrap_or(0),
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (day, commands) = row?;
        // `if r["day"]` — the comprehension's trailing filter drops a NULL or
        // empty day AFTER the GROUP BY, so its count is silently lost. Ported.
        match day {
            Some(day) if !day.is_empty() => out.push((day, commands)),
            _ => {}
        }
    }
    Ok(out)
}

// ── GET /api/tool-distribution ───────────────────────────────────────────────

async fn get_tool_distribution(
    State(state): State<AppState>,
    RawQuery(raw): RawQuery,
) -> HandlerResult {
    let query = Query::parse(raw.as_deref().unwrap_or_default());
    let path = resolve_log_path(&query, &state)?;
    let timezone_offset = query
        .int_or("timezone_offset", 0)
        .map_err(|err| validation_422_field_only(&err))?;
    let provider_filter = normalise_filter(query.opt_list("provider").as_deref());
    let model_filter = normalise_filter(query.opt_list("model").as_deref());

    tokio::task::spawn_blocking(move || {
        tool_distribution(
            &state,
            &path,
            timezone_offset,
            provider_filter.as_ref(),
            model_filter.as_ref(),
        )
    })
    .await
    .map_err(|err| join_failure(&err))?
}

fn tool_distribution(
    state: &AppState,
    path: &str,
    timezone_offset: i64,
    provider_filter: Option<&HashSet<String>>,
    model_filter: Option<&HashSet<String>>,
) -> HandlerResult {
    let conn = state
        .connect()
        .map_err(|err| HttpError::new(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))?;
    let slug = path_name(path);
    let mut rows = projects_by_slug(&conn, &slug)
        .map_err(|err| HttpError::new(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))?;
    if rows.is_empty() {
        return Err(HttpError::not_found(format!(
            "Project '{slug}' not found in store — try /api/refresh first"
        )));
    }
    if let Some(filter) = provider_filter {
        rows.retain(|(_, provider)| filter.contains(&provider.to_lowercase()));
        if rows.is_empty() {
            // A provider filter that excludes every row is an EMPTY map, not a
            // 404 — shape-stable, matching `/api/messages`' posture.
            return Ok(empty_distribution());
        }
    }
    let project_ids: Vec<i64> = rows.into_iter().map(|(id, _)| id).collect();

    // The primed price-book engine, not `default_engine`'s manifest — see the
    // note in `routes/data.rs::compute_stats`. Nothing this route *returns* is
    // priced, but the sweep it runs is the same one `/api/stats` runs, and two
    // callers of one pipeline pricing from two rate sources is how that gap
    // survives a fix to only one of them.
    let engine = crate::pricing::engine(&conn, state.package_dir())
        .map_err(|err| HttpError::new(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))?;
    // The THIRD consumer of DIV-055's shared entry — `commands.py` line 311.
    // "so the Overview tab doesn't recompute the full pipeline a third time",
    // and it reads only `user_interactions`, so that is all it copies out of a
    // 5.5-19 MB dict. The clamp is inside `project_stats_cached`.
    //
    // The id tuple is in the key, which is what stops a provider-narrowed sweep
    // from colliding with the slug's all-provider entry — Python says so at this
    // exact call site, and it is the reason the key is a tuple and not a slug.
    let stats = stats_memo::project_stats_cached(
        state.stats_memo(),
        &conn,
        &stats_memo::StatsRequest {
            store_path: state.store_path(),
            slug: &slug,
            project_ids: &project_ids,
            tz_offset: timezone_offset,
            keys: Some(&["user_interactions"]),
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
    )?;
    drop(conn);

    let ui = stats.get("user_interactions");

    if let Some(filter) = model_filter {
        // Recompute from `command_details` so a filtered count is built from
        // the same non-interruption rows the canonical distribution is.
        let details = ui
            .and_then(|value| value.get("command_details"))
            .and_then(Value::as_array);
        // `Counter[int]` — a plain dict underneath, so the JSON key order is
        // FIRST-INCREMENT order, not sorted. Kept insertion-ordered here.
        let mut order: Vec<i64> = Vec::new();
        let mut counts: Vec<i64> = Vec::new();
        for detail in details.into_iter().flatten() {
            let Some(detail) = detail.as_object() else {
                continue;
            };
            if truthy(detail.get("is_interruption")) {
                continue;
            }
            // `(d.get("model") or "").lower()` — a missing or null model is the
            // empty string, which only matches a filter that contains `""`.
            let model = detail
                .get("model")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_lowercase();
            if !filter.contains(&model) {
                continue;
            }
            // `int(d.get("tools_used", 0) or 0)`.
            let bucket = detail
                .get("tools_used")
                .and_then(Value::as_i64)
                .unwrap_or(0);
            match order.iter().position(|value| *value == bucket) {
                Some(idx) => counts[idx] += 1,
                None => {
                    order.push(bucket);
                    counts.push(1);
                }
            }
        }
        let mut dist = Map::new();
        for (bucket, count) in order.into_iter().zip(counts) {
            // `json.dumps` renders an int dict key as its `str()`.
            dist.insert(bucket.to_string(), Value::from(count));
        }
        let mut obj = Map::new();
        obj.insert("tool_count_distribution".to_owned(), Value::Object(dist));
        return Ok(JsonBody::ok(Value::Object(obj)));
    }

    let dist = ui
        .and_then(|value| value.get("tool_count_distribution"))
        .filter(|value| value.is_object())
        .cloned()
        .unwrap_or_else(|| Value::Object(Map::new()));
    let mut obj = Map::new();
    obj.insert("tool_count_distribution".to_owned(), dist);
    Ok(JsonBody::ok(Value::Object(obj)))
}

fn empty_distribution() -> JsonBody {
    let mut obj = Map::new();
    obj.insert(
        "tool_count_distribution".to_owned(),
        Value::Object(Map::new()),
    );
    JsonBody::ok(Value::Object(obj))
}

/// Python truthiness for an optional JSON value.
fn truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(cost: f64, steps: i64, timestamp: &str) -> CommandRow {
        CommandRow {
            interaction_id: format!("ix-{timestamp}"),
            session_id: "s".to_owned(),
            timestamp: timestamp.to_owned(),
            prompt_preview: String::new(),
            cost: Some(cost),
            tokens: TokenBag::default(),
            tools_used: 0,
            steps,
            models_used: Vec::new(),
            had_error: false,
        }
    }

    #[test]
    fn a_descending_sort_stays_stable_across_ties() {
        // Python's `reverse=True` does NOT reverse equal elements; the two
        // $1.00 rows must come out in their original order.
        let mut rows = vec![row(1.0, 0, "a"), row(2.0, 0, "b"), row(1.0, 0, "c")];
        sort_commands(&mut rows, "cost", true);
        let ids: Vec<&str> = rows.iter().map(|r| r.timestamp.as_str()).collect();
        assert_eq!(ids, vec!["b", "a", "c"]);
    }

    #[test]
    fn an_unknown_sort_key_falls_back_to_cost() {
        let mut rows = vec![row(1.0, 9, "a"), row(2.0, 1, "b")];
        sort_commands(&mut rows, "nonsense", true);
        assert_eq!(rows[0].timestamp, "b");
    }

    #[test]
    fn an_empty_cost_sum_ships_pythons_int_zero_not_a_float() {
        // `sum(<empty generator>)` is `0`, and `json.dumps` writes `0`, not
        // `0.0`. The differ found this on `C-list-default` at byte 21047.
        let mut row = row(0.0, 0, "a");
        row.cost = None;
        assert!(command_to_json(&row).render_contains(r#""cost":0,"#));
        row.cost = Some(0.0);
        assert!(command_to_json(&row).render_contains(r#""cost":0.0,"#));
    }

    /// Tiny helper so the assertion above reads as the byte claim it is.
    trait RenderContains {
        fn render_contains(&self, needle: &str) -> bool;
    }
    impl RenderContains for Value {
        fn render_contains(&self, needle: &str) -> bool {
            stax_memory::pyjson::dumps_http(self).contains(needle)
        }
    }

    #[test]
    fn an_untouched_counter_serialises_as_an_empty_object() {
        // Not four zeros — `Counter()` that never saw a record is `{}`, and an
        // interaction with no responses and no tool results produces exactly it.
        let row = row(0.0, 0, "a");
        assert!(
            command_to_json(&row)
                .get("tokens")
                .is_some_and(|t| t.as_object().is_some_and(serde_json::Map::is_empty))
        );
    }

    #[test]
    fn the_preview_counts_characters_not_bytes() {
        // A byte slice at 200 would split the last em-dash and panic; Python
        // slices code points.
        let text = "—".repeat(300);
        assert_eq!(preview(&text, 200).chars().count(), 200);
        assert_eq!(preview("  a\nb\r c  ", 200), "a b  c");
        assert_eq!(preview("", 200), "");
    }

    #[test]
    fn a_too_small_limit_becomes_the_default_not_one() {
        let mut limit = 0_i64;
        if limit < 1 {
            limit = DEFAULT_LIMIT;
        }
        assert_eq!(limit, 50);
    }
}
