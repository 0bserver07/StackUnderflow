//! `routes/sessions.py` — 3 endpoints, wave 5.
//!
//! | Item | Method | FastAPI path | axum path | State |
//! |---|---|---|---|---|
//! | `RS-5-104` | `GET` | `/api/jsonl-files` | `/api/jsonl-files` | **ported** |
//! | `RS-5-105` | `GET` | `/api/sessions/compare` | `/api/sessions/compare` | **ported** — batch E |
//! | `RS-5-106` | `GET` | `/api/jsonl-content` | `/api/jsonl-content` | **ported** |
//!
//! # `/api/jsonl-files`
//!
//! The Sessions tab's index. Its shape is two grouped queries, not an N+1: the
//! July campaign replaced `2 queries + a compute_cost` *per session* (~3.7 K
//! statements for ~1.8 K sessions) with one `GROUP BY session_fk` aggregate and
//! one `ROW_NUMBER()` window for titles. Both drive off
//! `session_fk IN (SELECT id FROM sessions WHERE project_id IN (…))` — §6b's
//! list-subquery shape, transliterated rather than "simplified", because
//! `messages` is a UNION-ALL view over monthly partitions and only that shape
//! lets each arm seek its `(session_fk, seq)` index.
//!
//! # `/api/jsonl-content`
//!
//! One session's raw transcript, replayed out of `messages.raw_json`. The
//! base64-media elision is the whole reason this endpoint is cheap: a
//! screenshot-heavy session is ~94% inline image bytes (one measured session:
//! 110 MiB of 117 MiB) and the tab never reads `source.data`, so the payload
//! ships a `<elided: image/png base64, N bytes>` stub unless `raw_media=1`.
//!
//! # `/api/sessions/compare` — DIV-070 closed, one new divergence in its place
//!
//! `_session_costs_for_sessions` reconstructs `RawEntry` objects from two
//! sessions' `raw_json`, then runs `classifier.tag` → `enricher.build` →
//! `aggregator.summarise_session_costs`. The first two were already in
//! `stax_etl::stats`; the third was on that module's deliberately-unported list,
//! which is why wave 5 filed DIV-070 rather than reaching across the crate fence
//! for one function. Batch E was granted that reach: the function is now
//! `stax_etl::stats::aggregator::summarise_session_costs`, ported with tests
//! beside the collector it drives, and `stats/mod.rs`'s scope paragraph says so.
//! The endpoint's own logic lives in [`crate::services::session_compare`].
//!
//! **The 200 body still cannot be byte-matched, for a different reason.** The
//! `diff.tokens` object is built by iterating
//! `set(sa["tokens"]) | set(sb["tokens"])`, and CPython randomises `str` hashing
//! per process. `endpoint-parity.sh` does not pin `PYTHONHASHSEED` (only
//! `parity-cli.sh` does), so the reference emits a different key order on every
//! boot — measured three times over the harness store, three orders, every other
//! byte identical. `!J-compare` therefore stays known-open on a payload-level
//! nondeterminism of the same class as DIV-085, and the rows that CAN be pinned
//! (the 422s, the 404s, the 405s) are pinned. See `parity/DIV-e-compare.md`.

use std::collections::HashMap;

use axum::Router;
use axum::extract::{RawQuery, State};
use axum::http::StatusCode;
use axum::routing::get;
use rusqlite::Connection;
use serde_json::{Map, Value};
use stax_etl::pricing::RawTokens;
use stax_etl::stats::aggregator::round_py;
use stax_etl::stats::pydatetime::{PyDateTime, parse_ts};

use crate::currency::active_currency_payload;
use crate::json::{
    HandlerResult, HttpError, JsonBody, join_failure, missing_query_param, validation_detail,
};
use crate::pyops::{char_prefix, path_name};
use crate::qs::Query;
use crate::services::session_compare::{self, CompareError};
use crate::state::AppState;

/// `title_text[:150]` — a CPython `str` slice, so 150 **code points**.
const TITLE_CHARS: usize = 150;

/// Mount this module's endpoints onto `router`.
///
/// In `router.include_router` order, which is the order `routes/sessions.py`
/// declares them.
pub fn register(router: Router<AppState>) -> Router<AppState> {
    router
        .route("/api/jsonl-files", get(get_jsonl_files))
        .route("/api/sessions/compare", get(compare_sessions))
        .route("/api/jsonl-content", get(get_jsonl_content))
}

// ── shared store reads ───────────────────────────────────────────────────────

/// `store/types.py::ProjectRow`, narrowed to what this module reads.
#[derive(Debug, Clone)]
struct ProjectRow {
    id: i64,
    provider: String,
}

/// `queries.get_projects_by_slug` — **every** row for the slug, in row order.
///
/// One slug can name several projects (the schema's `UNIQUE(provider, slug)`
/// means one per provider), which is why this returns a list and every caller
/// binds an `IN (…)`. The full column list is kept so the emitted SQL matches
/// the reference statement byte for byte in a query log.
fn get_projects_by_slug(conn: &Connection, slug: &str) -> rusqlite::Result<Vec<ProjectRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, provider, slug, path, display_name, first_seen, last_modified \
         FROM projects WHERE slug = ?",
    )?;
    stmt.query_map([slug], |row| {
        Ok(ProjectRow {
            id: row.get(0)?,
            provider: row.get(1)?,
        })
    })?
    .collect()
}

/// `store/types.py::SessionRow`.
#[derive(Debug, Clone)]
struct SessionRow {
    id: i64,
    project_id: i64,
    session_id: String,
    first_ts: Option<String>,
    last_ts: Option<String>,
    message_count: i64,
}

/// `queries.list_sessions(conn, project_id=[…])` — `ORDER BY last_ts DESC`.
///
/// An empty id list returns `[]` **without touching the DB**; the Python guard
/// is `if not project_id: return []` and promoting it to "all sessions" would
/// turn an unmatched provider filter into a full listing.
fn list_sessions(conn: &Connection, project_ids: &[i64]) -> rusqlite::Result<Vec<SessionRow>> {
    if project_ids.is_empty() {
        return Ok(Vec::new());
    }
    let sql = format!(
        "SELECT id, project_id, session_id, first_ts, last_ts, message_count \
         FROM sessions WHERE project_id IN ({}) ORDER BY last_ts DESC",
        placeholders(project_ids.len())
    );
    let mut stmt = conn.prepare(&sql)?;
    let params: Vec<&dyn rusqlite::ToSql> = project_ids
        .iter()
        .map(|id| id as &dyn rusqlite::ToSql)
        .collect();
    stmt.query_map(params.as_slice(), |row| {
        Ok(SessionRow {
            id: row.get(0)?,
            project_id: row.get(1)?,
            session_id: row.get(2)?,
            first_ts: row.get(3)?,
            last_ts: row.get(4)?,
            message_count: row.get(5)?,
        })
    })?
    .collect()
}

/// `",".join("?" for _ in xs)`.
fn placeholders(count: usize) -> String {
    let mut out = String::with_capacity(count.saturating_mul(2));
    for index in 0..count {
        if index > 0 {
            out.push(',');
        }
        out.push('?');
    }
    out
}

/// `_session_fk_subquery` — §6b's list subquery, not a join predicate.
///
/// Measured on the July campaign: joining `sessions` to the partitioned
/// `messages` view forces the planner to materialise the whole UNION-ALL;
/// constraining `session_fk` to a subquery lets each monthly arm seek its
/// `(session_fk, seq)` index. Port the shape.
fn session_fk_subquery(project_count: usize) -> String {
    format!(
        "session_fk IN (SELECT id FROM sessions WHERE project_id IN ({}))",
        placeholders(project_count)
    )
}

// ── GET /api/jsonl-files ─────────────────────────────────────────────────────

/// `_bulk_session_aggregates`' row.
#[derive(Debug, Clone, Default)]
struct SessionAggregate {
    user_messages: i64,
    assistant_messages: i64,
    input_tokens: i64,
    output_tokens: i64,
    model: Option<String>,
    tool_calls: i64,
}

/// `_bulk_session_aggregates` — one grouped pass over the project's messages.
fn bulk_session_aggregates(
    conn: &Connection,
    project_ids: &[i64],
) -> rusqlite::Result<HashMap<i64, SessionAggregate>> {
    if project_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let sql = format!(
        "SELECT session_fk, \
         SUM(CASE WHEN role = 'user' THEN 1 ELSE 0 END) AS user_messages, \
         SUM(CASE WHEN role = 'assistant' THEN 1 ELSE 0 END) AS assistant_messages, \
         COALESCE(SUM(input_tokens), 0) AS input_tokens, \
         COALESCE(SUM(output_tokens), 0) AS output_tokens, \
         MAX(CASE WHEN model IS NOT NULL AND model != '' THEN model END) AS model, \
         COALESCE(SUM(json_array_length(tools_json)), 0) AS tool_calls \
         FROM messages \
         WHERE {} \
         GROUP BY session_fk",
        session_fk_subquery(project_ids.len())
    );
    let mut stmt = conn.prepare(&sql)?;
    let params: Vec<&dyn rusqlite::ToSql> = project_ids
        .iter()
        .map(|id| id as &dyn rusqlite::ToSql)
        .collect();
    let rows = stmt.query_map(params.as_slice(), |row| {
        Ok((
            row.get::<_, i64>(0)?,
            SessionAggregate {
                // `SUM` over zero rows is NULL, and the handler's `or 0`
                // catches it. A group always has rows here, but the shape is
                // reproduced rather than assumed.
                user_messages: row.get::<_, Option<i64>>(1)?.unwrap_or(0),
                assistant_messages: row.get::<_, Option<i64>>(2)?.unwrap_or(0),
                input_tokens: row.get::<_, Option<i64>>(3)?.unwrap_or(0),
                output_tokens: row.get::<_, Option<i64>>(4)?.unwrap_or(0),
                model: row.get(5)?,
                tool_calls: row.get::<_, Option<i64>>(6)?.unwrap_or(0),
            },
        ))
    })?;
    let mut out = HashMap::new();
    for row in rows {
        let (fk, aggregate) = row?;
        out.insert(fk, aggregate);
    }
    Ok(out)
}

/// `_bulk_session_titles` — the first non-empty user message per session.
fn bulk_session_titles(
    conn: &Connection,
    project_ids: &[i64],
) -> rusqlite::Result<HashMap<i64, String>> {
    if project_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let sql = format!(
        "SELECT session_fk, content_text FROM (\
         SELECT session_fk, content_text, \
         ROW_NUMBER() OVER (PARTITION BY session_fk ORDER BY seq) AS rn \
         FROM messages \
         WHERE {} \
         AND role = 'user' AND content_text IS NOT NULL AND content_text != '' \
         ) WHERE rn = 1",
        session_fk_subquery(project_ids.len())
    );
    let mut stmt = conn.prepare(&sql)?;
    let params: Vec<&dyn rusqlite::ToSql> = project_ids
        .iter()
        .map(|id| id as &dyn rusqlite::ToSql)
        .collect();
    let rows = stmt.query_map(params.as_slice(), |row| {
        Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
    })?;
    let mut out = HashMap::new();
    for row in rows {
        let (fk, title) = row?;
        out.insert(fk, title);
    }
    Ok(out)
}

async fn get_jsonl_files(State(state): State<AppState>, RawQuery(raw): RawQuery) -> HandlerResult {
    let query = Query::parse(raw.as_deref().unwrap_or_default());
    let project = query.get("project").map(str::to_owned);
    let provider = query.opt_list("provider");

    // `if project: … elif log_path: … else: 400`. Both legs are *truthiness*
    // checks, so `?project=` (empty) falls through to the log path and an empty
    // log path 400s exactly like an unset one.
    let slug = match project.filter(|value| !value.is_empty()) {
        Some(project) => project,
        None => match state.current_project().log_path {
            Some(path) if !path.is_empty() => path_name(&path),
            _ => return Err(HttpError::bad_request("No project selected")),
        },
    };

    let provider_filter = normalise_provider_filter(provider.as_deref());
    let worker = state.clone();
    let payload = tokio::task::spawn_blocking(move || {
        jsonl_files_payload(&worker, &slug, provider_filter.as_deref())
    })
    .await
    .map_err(|err| join_failure(&err))?;

    match payload {
        Ok(payload) => Ok(JsonBody::ok(payload)),
        Err(FilesError::Http(err)) => Err(err),
        // `except Exception as e: raise HTTPException(500, f"Error reading log
        // files: {str(e)}")`. The message is the exception's `str`, so the port
        // has to surface the SQLite text and not a wrapper of its own.
        Err(FilesError::Other(message)) => Err(HttpError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Error reading log files: {message}"),
        )),
    }
}

/// The two ways a blocking body ends: a re-raised `HTTPException`, or the
/// `except Exception` funnel.
enum FilesError {
    Http(HttpError),
    Other(String),
}

impl From<rusqlite::Error> for FilesError {
    fn from(err: rusqlite::Error) -> Self {
        Self::Other(err.to_string())
    }
}

fn jsonl_files_payload(
    state: &AppState,
    slug: &str,
    provider_filter: Option<&[String]>,
) -> Result<Value, FilesError> {
    let conn = state
        .connect()
        .map_err(|err| FilesError::Other(err.to_string()))?;

    let mut project_rows = get_projects_by_slug(&conn, slug)?;
    if project_rows.is_empty() {
        // `return JSONResponse([])` — a bare ARRAY, not `{"files": []}`. This
        // and the empty-after-filter case below are two different bodies and
        // the Sessions tab distinguishes them.
        return Ok(Value::Array(Vec::new()));
    }
    if let Some(filter) = provider_filter {
        project_rows.retain(|row| filter.iter().any(|p| *p == row.provider.to_lowercase()));
    }
    let currency = active_currency_payload(&state.config().currency)
        .map_err(|err| FilesError::Other(err.to_string()))?;
    if project_rows.is_empty() {
        let mut obj = Map::new();
        obj.insert("files".to_owned(), Value::Array(Vec::new()));
        obj.insert("currency".to_owned(), currency);
        return Ok(Value::Object(obj));
    }

    let project_ids: Vec<i64> = project_rows.iter().map(|row| row.id).collect();
    let provider_map: HashMap<i64, String> = project_rows
        .iter()
        .map(|row| {
            (
                row.id,
                // `r.provider or "anthropic"` — an EMPTY provider is falsy too.
                if row.provider.is_empty() {
                    "anthropic".to_owned()
                } else {
                    row.provider.clone()
                },
            )
        })
        .collect();

    let sessions = list_sessions(&conn, &project_ids)?;
    let aggregates = bulk_session_aggregates(&conn, &project_ids)?;
    let titles = bulk_session_titles(&conn, &project_ids)?;

    let engine = crate::pricing::engine(&conn, state.package_dir())
        .map_err(|err| FilesError::Other(err.to_string()))?;

    let mut files: Vec<(f64, Value)> = Vec::with_capacity(sessions.len());
    for session in &sessions {
        // A session row with zero message rows takes the all-zero shape the
        // old per-session `get_session_stats` returned.
        let aggregate = aggregates.get(&session.id).cloned().unwrap_or_default();
        let title = titles
            .get(&session.id)
            // `title_text[:150] if title_text else None` — an empty title is
            // falsy, so it becomes `null`, not `""`.
            .filter(|text| !text.is_empty())
            .map(|text| char_prefix(text, TITLE_CHARS));

        // `if model and (input_tokens or output_tokens)` — an empty model
        // string is falsy, and both token counts zero skips pricing entirely.
        let mut estimated_cost = 0.0_f64;
        if let Some(model) = aggregate.model.as_deref().filter(|m| !m.is_empty())
            && (aggregate.input_tokens != 0 || aggregate.output_tokens != 0)
        {
            // `compute_cost({"input": …, "output": …}, model)` — a TWO-key
            // token dict, so cache buckets are absent (not zero) and the
            // provider defaults to `"anthropic"` regardless of the project's.
            let mut tokens = RawTokens::empty();
            tokens.set("input", aggregate.input_tokens);
            tokens.set("output", aggregate.output_tokens);
            estimated_cost = engine
                .compute_cost(&tokens, model, "anthropic", "standard", None)
                .total_cost;
        }

        let created = iso_to_ts(session.first_ts.as_deref());
        let mut obj = Map::new();
        obj.insert(
            "name".to_owned(),
            Value::from(format!("{}.jsonl", session.session_id)),
        );
        obj.insert(
            "path".to_owned(),
            Value::from(format!("{}.jsonl", session.session_id)),
        );
        obj.insert(
            "is_subagent".to_owned(),
            Value::Bool(session.session_id.starts_with("agent-")),
        );
        obj.insert("created".to_owned(), Value::from(created));
        obj.insert(
            "modified".to_owned(),
            Value::from(iso_to_ts(session.last_ts.as_deref())),
        );
        // "not tracked in the store" — a literal 0, not a file stat.
        obj.insert("size".to_owned(), Value::from(0));
        obj.insert("messages".to_owned(), Value::from(session.message_count));
        obj.insert(
            "user_messages".to_owned(),
            Value::from(aggregate.user_messages),
        );
        obj.insert(
            "assistant_messages".to_owned(),
            Value::from(aggregate.assistant_messages),
        );
        obj.insert(
            "input_tokens".to_owned(),
            Value::from(aggregate.input_tokens),
        );
        obj.insert(
            "output_tokens".to_owned(),
            Value::from(aggregate.output_tokens),
        );
        obj.insert(
            "model".to_owned(),
            aggregate.model.clone().map_or(Value::Null, Value::from),
        );
        obj.insert("title".to_owned(), title.map_or(Value::Null, Value::from));
        obj.insert("tool_calls".to_owned(), Value::from(aggregate.tool_calls));
        obj.insert(
            "estimated_cost".to_owned(),
            Value::from(round_py(estimated_cost, 4)),
        );
        obj.insert(
            "provider".to_owned(),
            Value::from(
                provider_map
                    .get(&session.project_id)
                    .cloned()
                    .unwrap_or_else(|| "anthropic".to_owned()),
            ),
        );
        files.push((created, Value::Object(obj)));
    }
    drop(conn);

    // `files.sort(key=lambda x: x["created"])` — ASCENDING, and Python's sort
    // is stable, so sessions sharing a `created` keep their `last_ts DESC`
    // order. `sort_by` is stable too; `sort_unstable_by` would not be.
    files.sort_by(|left, right| {
        left.0
            .partial_cmp(&right.0)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let mut obj = Map::new();
    obj.insert(
        "files".to_owned(),
        Value::Array(files.into_iter().map(|(_, value)| value).collect()),
    );
    obj.insert("currency".to_owned(), currency);
    Ok(Value::Object(obj))
}

/// The handler's inline provider normalisation.
///
/// `{p.strip().lower() for p in provider if p and p.strip()}`, then
/// "empty set stays `None`". A Python `set` has no order, so the result is
/// sorted here purely so the `retain` below is reproducible.
fn normalise_provider_filter(provider: Option<&[String]>) -> Option<Vec<String>> {
    let provider = provider?;
    if provider.is_empty() {
        return None;
    }
    let mut normed: Vec<String> = provider
        .iter()
        .map(|value| value.trim().to_lowercase())
        .filter(|value| !value.is_empty())
        .collect();
    normed.sort();
    normed.dedup();
    (!normed.is_empty()).then_some(normed)
}

// ── GET /api/sessions/compare ────────────────────────────────────────────────

/// FastAPI's 422 when SEVERAL required query parameters are absent at once.
///
/// pydantic validates every field and the handler renders the whole error list,
/// in the order the endpoint DECLARES its parameters (`a`, then `b`) — not the
/// order in which the query string happens to omit them. Measured against the
/// harness interpreter's FastAPI 0.141 / pydantic 2.13, not transcribed
/// (`endpoint-cases-e-compare.txt` carries all three shapes as rows).
///
/// The per-field entry is [`missing_query_param`]'s, concatenated rather than
/// re-spelled, so there is exactly one place that knows what a `missing` error
/// looks like. Law 8's `{"detail":"<field>"}` shape is **not** what FastAPI
/// answers here and is not used.
fn missing_query_params(fields: &[&str]) -> Value {
    let mut detail: Vec<Value> = Vec::with_capacity(fields.len());
    for field in fields {
        let mut one = missing_query_param(field);
        if let Some(Value::Array(items)) = one.get_mut("detail") {
            detail.append(items);
        }
    }
    let mut obj = Map::new();
    obj.insert("detail".to_owned(), Value::Array(detail));
    Value::Object(obj)
}

/// `GET /api/sessions/compare` — the cost/token/duration diff of two sessions.
///
/// Python declares this `async def` and then blocks on SQLite inside it; the
/// port runs the store work on `spawn_blocking`, as every other ported handler
/// in this module does.
async fn compare_sessions(State(state): State<AppState>, RawQuery(raw): RawQuery) -> HandlerResult {
    let query = Query::parse(raw.as_deref().unwrap_or_default());
    // `a: str` / `b: str` have no defaults, so FastAPI 422s before the handler
    // runs. An EMPTY `?a=` is a perfectly valid `str` and DOES reach the
    // handler — it simply names no session and 404s there. A repeated `?a=`
    // keeps the LAST value, which is `Query::get`'s starlette semantics.
    let a = query.get("a").map(str::to_owned);
    let b = query.get("b").map(str::to_owned);
    let (a, b) = match (a, b) {
        (Some(a), Some(b)) => (a, b),
        (a, b) => {
            let mut absent: Vec<&str> = Vec::with_capacity(2);
            if a.is_none() {
                absent.push("a");
            }
            if b.is_none() {
                absent.push("b");
            }
            return Ok(JsonBody::with_status(
                StatusCode::UNPROCESSABLE_ENTITY,
                missing_query_params(&absent),
            ));
        }
    };

    // `path = log_path or deps.current_log_path` — truthiness on both, so
    // `?log_path=` (empty) falls through to the selected project and an empty
    // selection 400s exactly like an unset one.
    let path = query
        .get("log_path")
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .or_else(|| {
            state
                .current_project()
                .log_path
                .filter(|value| !value.is_empty())
        });
    let Some(path) = path else {
        return Err(HttpError::bad_request(
            "No project selected or log_path provided",
        ));
    };
    let slug = path_name(&path);

    let worker = state.clone();
    let payload = tokio::task::spawn_blocking(move || compare_payload(&worker, &slug, &a, &b))
        .await
        .map_err(|err| join_failure(&err))??;

    // `currency = active_currency_payload()` runs AFTER the try/except that
    // produces the 500, so a currency failure is not "Failed to load stats".
    let currency = active_currency_payload(&state.config().currency)
        .map_err(|err| HttpError::new(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))?;
    // `if rate != 1.0:` rewrites `a.cost`, `b.cost` and `diff.cost` in place
    // (`{**sa, "cost": …}` keeps `cost` where it already was). DIV-052 makes
    // the non-USD leg unreachable, so — as in `routes/commands.rs` and
    // `routes/data.rs` — the conversion is not ported blind.

    let Value::Object(mut obj) = payload else {
        return Err(HttpError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to load stats: comparison payload was not an object",
        ));
    };
    obj.insert("currency".to_owned(), currency);
    Ok(JsonBody::ok(Value::Object(obj)))
}

/// The blocking body: open, resolve the slug, run the comparison.
///
/// The two SQLite-error funnels are Python's two, and they are NOT the same
/// message: everything inside the handler's `try` becomes
/// `500 "Failed to load stats: {e}"`, while `db.connect` itself is inside it
/// too — so a failure to open is that message as well.
fn compare_payload(state: &AppState, slug: &str, a: &str, b: &str) -> Result<Value, HttpError> {
    let failed = |message: String| {
        HttpError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Failed to load stats: {message}"),
        )
    };

    let conn = state.connect().map_err(|err| failed(err.to_string()))?;
    let project_rows = get_projects_by_slug(&conn, slug).map_err(|err| failed(err.to_string()))?;
    if project_rows.is_empty() {
        // The slug is interpolated into the detail here and NOT in
        // `/api/jsonl-content`'s "Project not found in store" — two neighbouring
        // handlers, two different messages.
        return Err(HttpError::not_found(format!(
            "Project '{slug}' not found in store"
        )));
    }
    let project_ids: Vec<i64> = project_rows.iter().map(|row| row.id).collect();
    let provider_map: HashMap<i64, String> = project_rows
        .iter()
        .map(|row| {
            (
                row.id,
                // `r.provider or "anthropic"` — an EMPTY provider is falsy too.
                if row.provider.is_empty() {
                    "anthropic".to_owned()
                } else {
                    row.provider.clone()
                },
            )
        })
        .collect();

    // LAW 2 / DIV-056: the engine comes from THIS store's `price_book`, which is
    // what `server.py`'s lifespan primes `infra.costs` with. `default_engine()`
    // would price off the manifest and be quietly ~2% wrong.
    let engine = crate::pricing::engine(&conn, state.package_dir())
        .map_err(|err| failed(err.to_string()))?;

    match session_compare::compare_payload(&conn, &engine, &project_ids, &provider_map, a, b) {
        Ok(payload) => Ok(payload),
        Err(CompareError::NotFound(detail)) => Err(HttpError::not_found(detail)),
        Err(CompareError::Failed(message)) => Err(failed(message)),
    }
}

// ── GET /api/jsonl-content ───────────────────────────────────────────────────

async fn get_jsonl_content(
    State(state): State<AppState>,
    RawQuery(raw): RawQuery,
) -> HandlerResult {
    let query = Query::parse(raw.as_deref().unwrap_or_default());
    // `file: str` has no default, so FastAPI 422s before the handler runs.
    let Some(file) = query.get("file").map(str::to_owned) else {
        return Ok(JsonBody::with_status(
            StatusCode::UNPROCESSABLE_ENTITY,
            missing_query_param("file"),
        ));
    };
    let project = query.get("project").map(str::to_owned);
    let raw_media = match query.bool_or("raw_media", false) {
        Ok(value) => value,
        // pydantic's own error list, not a one-line detail: measured against
        // the reference and byte-identical, which is one case DIV-053's
        // "approximate" caveat does not have to cover.
        Err(err) => {
            return Ok(JsonBody::with_status(
                StatusCode::UNPROCESSABLE_ENTITY,
                validation_detail(&err),
            ));
        }
    };

    let slug = match project.filter(|value| !value.is_empty()) {
        Some(project) => project,
        None => match state.current_project().log_path {
            Some(path) if !path.is_empty() => path_name(&path),
            _ => return Err(HttpError::bad_request("No project selected")),
        },
    };

    // `Path(file).stem` — the last component with one extension removed.
    let session_id = path_stem(&file);
    if session_id.is_empty() {
        return Err(HttpError::bad_request("Invalid file parameter"));
    }

    let worker = state.clone();
    let payload = tokio::task::spawn_blocking(move || {
        jsonl_content_payload(&worker, &slug, &session_id, raw_media)
    })
    .await
    .map_err(|err| join_failure(&err))?;

    match payload {
        Ok(payload) => Ok(JsonBody::ok(payload)),
        Err(FilesError::Http(err)) => Err(err),
        Err(FilesError::Other(message)) => Err(HttpError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Error reading file: {message}"),
        )),
    }
}

/// One `messages` row, narrowed to the columns this endpoint reads.
struct ContentRow {
    timestamp: Option<String>,
    role: String,
    raw_json: Option<String>,
}

fn jsonl_content_payload(
    state: &AppState,
    slug: &str,
    session_id: &str,
    raw_media: bool,
) -> Result<Value, FilesError> {
    let conn = state
        .connect()
        .map_err(|err| FilesError::Other(err.to_string()))?;

    let project_rows = get_projects_by_slug(&conn, slug)?;
    if project_rows.is_empty() {
        return Err(FilesError::Http(HttpError::not_found(
            "Project not found in store",
        )));
    }
    let project_ids: Vec<i64> = project_rows.iter().map(|row| row.id).collect();

    let sql = format!(
        "SELECT id FROM sessions WHERE project_id IN ({}) AND session_id = ?",
        placeholders(project_ids.len())
    );
    let session_fk: Option<i64> = {
        let mut stmt = conn.prepare(&sql)?;
        let mut params: Vec<&dyn rusqlite::ToSql> = project_ids
            .iter()
            .map(|id| id as &dyn rusqlite::ToSql)
            .collect();
        params.push(&session_id);
        let mut rows = stmt.query(params.as_slice())?;
        // `.fetchone()` — the FIRST row, and `None` when there is none.
        match rows.next()? {
            Some(row) => Some(row.get(0)?),
            None => None,
        }
    };
    let Some(session_fk) = session_fk else {
        return Err(FilesError::Http(HttpError::not_found("File not found")));
    };

    // `queries.get_session_messages` — every row, `ORDER BY seq`.
    let messages: Vec<ContentRow> = {
        let mut stmt = conn.prepare(
            "SELECT id, session_fk, seq, timestamp, role, model, \
             input_tokens, output_tokens, cache_create_tokens, cache_read_tokens, \
             content_text, tools_json, raw_json, is_sidechain, uuid, parent_uuid, \
             speed \
             FROM messages WHERE session_fk = ? ORDER BY seq",
        )?;
        stmt.query_map([session_fk], |row| {
            Ok(ContentRow {
                timestamp: row.get(3)?,
                role: row.get(4)?,
                raw_json: row.get(12)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?
    };
    drop(conn);

    let mut lines: Vec<Value> = Vec::with_capacity(messages.len());
    let mut user_count = 0_i64;
    let mut assistant_count = 0_i64;
    let mut cwd = Value::Null;

    for (index, message) in messages.iter().enumerate() {
        let line_number = i64::try_from(index).unwrap_or(i64::MAX).saturating_add(1);
        // `except (json.JSONDecodeError, TypeError)` — the `TypeError` leg is a
        // NULL `raw_json`, which `json.loads` rejects by type, not by syntax.
        let mut parsed = match message.raw_json.as_deref() {
            Some(text) => serde_json::from_str::<Value>(text)
                .unwrap_or_else(|_| parse_error_line(line_number)),
            None => parse_error_line(line_number),
        };
        if !raw_media {
            elide_base64_media(&mut parsed);
        }
        if index == 0 {
            // `raw.get("cwd", "")`. A non-object `raw` makes CPython raise
            // `AttributeError`, which the handler's `except Exception` turns
            // into a 500 with a message this port cannot reproduce verbatim —
            // recorded as DIV-072 rather than guessed at; the default `""`
            // is taken here so a scalar top-level record still answers.
            cwd = match &parsed {
                Value::Object(map) => map.get("cwd").cloned().unwrap_or_else(|| Value::from("")),
                _ => Value::from(""),
            };
        }
        lines.push(parsed);
        if message.role == "user" {
            user_count += 1;
        } else if message.role == "assistant" {
            assistant_count += 1;
        }
    }

    let first_ts = messages.first().and_then(|row| row.timestamp.clone());
    let last_ts = messages.last().and_then(|row| row.timestamp.clone());

    let total_lines = i64::try_from(lines.len()).unwrap_or(i64::MAX);
    let mut metadata = Map::new();
    metadata.insert("session_id".to_owned(), Value::from(session_id));
    metadata.insert("file_size".to_owned(), Value::from(0));
    metadata.insert(
        "created".to_owned(),
        Value::from(iso_to_ts(first_ts.as_deref())),
    );
    metadata.insert(
        "modified".to_owned(),
        Value::from(iso_to_ts(last_ts.as_deref())),
    );
    metadata.insert(
        "first_timestamp".to_owned(),
        first_ts.clone().map_or(Value::Null, Value::from),
    );
    metadata.insert(
        "last_timestamp".to_owned(),
        last_ts.clone().map_or(Value::Null, Value::from),
    );
    metadata.insert(
        "duration_minutes".to_owned(),
        duration_minutes(first_ts.as_deref(), last_ts.as_deref()).map_or(Value::Null, Value::from),
    );
    metadata.insert("cwd".to_owned(), cwd);

    let mut obj = Map::new();
    obj.insert("lines".to_owned(), Value::Array(lines));
    obj.insert("total_lines".to_owned(), Value::from(total_lines));
    obj.insert("user_count".to_owned(), Value::from(user_count));
    obj.insert("assistant_count".to_owned(), Value::from(assistant_count));
    obj.insert("metadata".to_owned(), Value::Object(metadata));
    Ok(Value::Object(obj))
}

/// `{"error": "parse error", "line_number": i + 1}`.
fn parse_error_line(line_number: i64) -> Value {
    let mut obj = Map::new();
    obj.insert("error".to_owned(), Value::from("parse error"));
    obj.insert("line_number".to_owned(), Value::from(line_number));
    Value::Object(obj)
}

// ── base64 media elision ─────────────────────────────────────────────────────

/// `_is_base64_media` — the block declares `type == "base64"`, or it hangs off
/// a `source` key and carries an `image/*` media type.
fn is_base64_media(map: &Map<String, Value>, parent_key: Option<&str>) -> bool {
    if map.get("type").and_then(Value::as_str) == Some("base64") {
        return true;
    }
    if parent_key == Some("source") {
        return map
            .get("media_type")
            .and_then(Value::as_str)
            .is_some_and(|value| value.starts_with("image/"));
    }
    false
}

/// `_media_stub` — `<elided: image/png base64, 1519180 bytes>`.
///
/// `len(data)` on a CPython `str` counts **code points**, not bytes. Base64 is
/// ASCII so the two agree for a real payload, but a fixture with a non-ASCII
/// `data` would diverge on `.len()`, so this counts chars.
fn media_stub(map: &Map<String, Value>, char_count: usize) -> String {
    let label = match map.get("media_type").and_then(Value::as_str) {
        Some(media_type) if !media_type.is_empty() => format!("{media_type} "),
        _ => String::new(),
    };
    format!("<elided: {label}base64, {char_count} bytes>")
}

/// `_elide_base64_media` — replace inline base64 payloads with a size stub.
///
/// Python walks an explicit stack of `(node, parent_key)`; the traversal order
/// cannot matter (a parsed JSON document is a tree, each node is visited once,
/// and the mutation is local to the node) so this recurses instead. Depth is
/// bounded by the parser: `serde_json`'s recursion limit rejects a pathological
/// document before this function ever sees it.
fn elide_base64_media(node: &mut Value) {
    elide_inner(node, None);
}

fn elide_inner(node: &mut Value, parent_key: Option<&str>) {
    match node {
        Value::Object(map) => {
            // `data` is read, tested and replaced before the children are
            // walked, exactly as Python does it — and the replacement is a
            // short ASCII string, so re-walking it could not match anyway.
            let stub = match map.get("data") {
                Some(Value::String(data))
                    if !data.is_empty() && is_base64_media(map, parent_key) =>
                {
                    Some(media_stub(map, data.chars().count()))
                }
                _ => None,
            };
            if let Some(stub) = stub {
                map.insert("data".to_owned(), Value::from(stub));
            }
            let keys: Vec<String> = map.keys().cloned().collect();
            for key in keys {
                if let Some(value) = map.get_mut(&key)
                    && (value.is_object() || value.is_array())
                {
                    elide_inner(value, Some(&key));
                }
            }
        }
        Value::Array(items) => {
            // A list element inherits the list's OWN parent key, which is what
            // makes `source: [{…}]` behave like `source: {…}`.
            for item in items.iter_mut() {
                if item.is_object() || item.is_array() {
                    elide_inner(item, parent_key);
                }
            }
        }
        _ => {}
    }
}

// ── small ports ──────────────────────────────────────────────────────────────

/// `_iso_to_ts` — `datetime.fromisoformat(iso.replace("Z", "+00:00")).timestamp()`.
///
/// `0.0` for a falsy input and for the `(ValueError, AttributeError)` leg.
///
/// **DIV-071.** `.timestamp()` on a *naive* datetime is
/// `mktime`-in-the-process-local-zone; on an aware one it is the instant. Only
/// the aware leg is ported — every adapter writes `datetime.isoformat()` with
/// an offset, and inventing a local zone in a `forbid(unsafe_code)` crate with
/// no tz database would be guessing. A naive timestamp therefore reads as UTC
/// here; the ledger carries the case.
fn iso_to_ts(iso: Option<&str>) -> f64 {
    let Some(iso) = iso.filter(|value| !value.is_empty()) else {
        return 0.0;
    };
    let Some(parsed) = parse_ts(iso) else {
        return 0.0;
    };
    epoch_seconds(parsed)
}

/// `(dt - datetime(1970, 1, 1, tzinfo=utc)).total_seconds()`.
fn epoch_seconds(value: PyDateTime) -> f64 {
    // A naive value is read as UTC — see DIV-071 on [`iso_to_ts`].
    let aware = PyDateTime {
        wall_us: value.wall_us,
        offset_s: Some(value.offset_s.unwrap_or(0)),
    };
    let epoch = PyDateTime {
        wall_us: 0,
        offset_s: Some(0),
    };
    aware.sub_total_seconds(epoch).unwrap_or(0.0)
}

/// `_duration_minutes` — `(end - start).total_seconds() / 60`, or `None`.
fn duration_minutes(first: Option<&str>, last: Option<&str>) -> Option<f64> {
    let first = first.filter(|value| !value.is_empty())?;
    let last = last.filter(|value| !value.is_empty())?;
    let start = parse_ts(first)?;
    let end = parse_ts(last)?;
    // `aware - naive` is a `TypeError` in CPython, which the handler catches
    // and turns into `None`. `sub_total_seconds` returns `None` for exactly
    // that mix, so the branch ports as a `?`.
    Some(end.sub_total_seconds(start)? / 60.0)
}

/// `pathlib.PurePath(p).stem` — the name with one trailing suffix removed.
///
/// `PurePath(".bashrc").stem` is `".bashrc"`, not `""`: a leading dot does not
/// start a suffix. `Path::file_stem` agrees on that, and on `"a.b.c" -> "a.b"`.
fn path_stem(path: &str) -> String {
    std::path::Path::new(path)
        .file_stem()
        .map_or_else(String::new, |stem| stem.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_list_subquery_shape_is_the_one_6b_pins() {
        // Not a join predicate. §6b measured 912ms vs 9ms on the difference.
        assert_eq!(
            session_fk_subquery(2),
            "session_fk IN (SELECT id FROM sessions WHERE project_id IN (?,?))"
        );
    }

    #[test]
    fn an_aware_timestamp_is_its_instant_and_a_missing_one_is_zero() {
        assert_eq!(iso_to_ts(Some("1970-01-01T00:00:00+00:00")), 0.0);
        assert_eq!(iso_to_ts(Some("1970-01-01T01:00:00+00:00")), 3600.0);
        // The `Z` spelling goes through `.replace("Z", "+00:00")` first.
        assert_eq!(iso_to_ts(Some("1970-01-01T01:00:00Z")), 3600.0);
        // An offset is applied, not ignored: 01:00+01:00 IS the epoch.
        assert_eq!(iso_to_ts(Some("1970-01-01T01:00:00+01:00")), 0.0);
        assert_eq!(iso_to_ts(None), 0.0);
        assert_eq!(iso_to_ts(Some("")), 0.0);
        assert_eq!(iso_to_ts(Some("not a date")), 0.0);
    }

    #[test]
    fn duration_is_minutes_and_survives_a_missing_end() {
        assert_eq!(
            duration_minutes(
                Some("2026-01-01T00:00:00+00:00"),
                Some("2026-01-01T00:30:00+00:00")
            ),
            Some(30.0)
        );
        assert_eq!(
            duration_minutes(Some("2026-01-01T00:00:00+00:00"), None),
            None
        );
        // Mixed awareness is a CPython TypeError, caught into `None`.
        assert_eq!(
            duration_minutes(
                Some("2026-01-01T00:00:00"),
                Some("2026-01-01T00:30:00+00:00")
            ),
            None
        );
    }

    #[test]
    fn the_media_stub_names_the_type_and_the_size() {
        let mut node: Value = serde_json::json!({
            "type": "image",
            "source": {"type": "base64", "media_type": "image/png", "data": "AAAA"},
        });
        elide_base64_media(&mut node);
        assert_eq!(
            stax_memory::pyjson::dumps_http(&node),
            r#"{"type":"image","source":{"type":"base64","media_type":"image/png","data":"<elided: image/png base64, 4 bytes>"}}"#
        );
    }

    #[test]
    fn a_source_block_without_the_discriminator_still_elides() {
        // The defensive leg: an adapter that omits `type` but says `image/*`.
        let mut node: Value = serde_json::json!({
            "source": {"media_type": "image/jpeg", "data": "QUJD"},
        });
        elide_base64_media(&mut node);
        assert_eq!(
            stax_memory::pyjson::dumps_http(&node),
            r#"{"source":{"media_type":"image/jpeg","data":"<elided: image/jpeg base64, 4 bytes>"}}"#
        );
    }

    #[test]
    fn a_plain_data_key_is_left_alone() {
        // No `type: base64`, no `source` parent — a tool result that happens to
        // have a `data` field keeps every byte.
        let mut node: Value = serde_json::json!({"result": {"data": "hello"}});
        elide_base64_media(&mut node);
        assert_eq!(
            stax_memory::pyjson::dumps_http(&node),
            r#"{"result":{"data":"hello"}}"#
        );
    }

    #[test]
    fn a_list_element_inherits_the_lists_parent_key() {
        let mut node: Value = serde_json::json!({
            "source": [{"media_type": "image/png", "data": "AA"}],
        });
        elide_base64_media(&mut node);
        assert_eq!(
            stax_memory::pyjson::dumps_http(&node),
            r#"{"source":[{"media_type":"image/png","data":"<elided: image/png base64, 2 bytes>"}]}"#
        );
    }

    #[test]
    fn an_empty_data_string_is_not_stubbed() {
        // `isinstance(data, str) and data` — the empty string is falsy.
        let mut node: Value = serde_json::json!({"type": "base64", "data": ""});
        elide_base64_media(&mut node);
        assert_eq!(
            stax_memory::pyjson::dumps_http(&node),
            r#"{"type":"base64","data":""}"#
        );
    }

    #[test]
    fn the_stem_is_pathlibs_not_a_split_on_dot() {
        assert_eq!(path_stem("abc.jsonl"), "abc");
        assert_eq!(path_stem("a/b/c.jsonl"), "c");
        assert_eq!(path_stem("a.b.c"), "a.b");
        assert_eq!(path_stem(".bashrc"), ".bashrc");
        assert_eq!(path_stem(""), "");
    }

    #[test]
    fn the_title_slice_counts_code_points() {
        let text: String = "é".repeat(200);
        assert_eq!(char_prefix(&text, TITLE_CHARS).chars().count(), 150);
    }

    #[test]
    fn the_provider_filter_drops_blanks_and_collapses_to_none() {
        assert_eq!(
            normalise_provider_filter(Some(&["  Claude ".to_owned(), "codex".to_owned()])),
            Some(vec!["claude".to_owned(), "codex".to_owned()])
        );
        // `?provider=` — truthy list, empty after normalisation, so `None`.
        assert_eq!(normalise_provider_filter(Some(&[String::new()])), None);
        assert_eq!(normalise_provider_filter(None), None);
    }

    // ── GET /api/sessions/compare ───────────────────────────────────────────

    #[test]
    fn several_absent_parameters_come_out_as_one_list_in_declaration_order() {
        // Measured on the harness interpreter (fastapi 0.141.1 / pydantic
        // 2.13.4), not transcribed: `a` before `b`, both entries, one `detail`.
        assert_eq!(
            JsonBody::with_status(
                StatusCode::UNPROCESSABLE_ENTITY,
                missing_query_params(&["a", "b"])
            )
            .render(),
            r#"{"detail":[{"type":"missing","loc":["query","a"],"msg":"Field required","input":null},{"type":"missing","loc":["query","b"],"msg":"Field required","input":null}]}"#
        );
        assert_eq!(
            JsonBody::with_status(
                StatusCode::UNPROCESSABLE_ENTITY,
                missing_query_params(&["b"])
            )
            .render(),
            r#"{"detail":[{"type":"missing","loc":["query","b"],"msg":"Field required","input":null}]}"#
        );
    }
}

/// In-process exercises of `/api/sessions/compare` against a seeded store.
///
/// Separate module because these are the only tests in the file that need a
/// router, a scratch home and the real package tree; the block above is pure
/// functions.
#[cfg(test)]
mod compare_tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request as HttpRequest;
    use tower::ServiceExt as _;

    /// A scratch `STACKUNDERFLOW_HOME` that cleans itself up.
    struct Scratch(std::path::PathBuf);

    impl Drop for Scratch {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).ok();
        }
    }

    /// Two projects; `-p-one` holds a cheap session, a priced one and a session
    /// with no messages at all, `-p-two` holds one the first project must not
    /// see.
    ///
    /// `package_dir` is the REAL package tree, because `crate::pricing::engine`
    /// reads `data/models.toml` out of it — LAW 2, and the reason the priced
    /// row below is a real number rather than a fixture's.
    fn seeded(tag: &str) -> (AppState, Scratch) {
        let dir = std::env::temp_dir().join(format!(
            "stax-sesscmp-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |delta| delta.as_nanos())
        ));
        std::fs::create_dir_all(&dir).expect("scratch dir");
        let store = dir.join("store.db");
        let conn = Connection::open(&store).expect("open");
        conn.execute_batch(
            "CREATE TABLE projects (id INTEGER PRIMARY KEY, provider TEXT, slug TEXT, \
                 path TEXT, display_name TEXT, first_seen TEXT, last_modified TEXT);
             CREATE TABLE sessions (id INTEGER PRIMARY KEY, project_id INTEGER NOT NULL, \
                 session_id TEXT NOT NULL, first_ts TEXT, last_ts TEXT, \
                 message_count INTEGER NOT NULL DEFAULT 0);
             CREATE TABLE messages (id INTEGER PRIMARY KEY, session_fk INTEGER NOT NULL, \
                 seq INTEGER NOT NULL, timestamp TEXT, raw_json TEXT);
             INSERT INTO projects (id, provider, slug) VALUES
                 (1, 'claude', '-p-one'), (2, 'claude', '-p-two');
             INSERT INTO sessions (id, project_id, session_id) VALUES
                 (10, 1, 'sess-a'), (11, 1, 'sess-b'), (12, 1, 'sess-empty'),
                 (13, 2, 'sess-other');",
        )
        .expect("schema");

        let rows: [(i64, i64, &str, &str); 4] = [
            (
                10,
                1,
                "2026-03-04T10:00:00+00:00",
                r#"{"type":"human","uuid":"a1","message":{"content":"just asking"}}"#,
            ),
            (
                11,
                1,
                "2026-03-04T11:00:00+00:00",
                r#"{"type":"human","uuid":"b1","message":{"content":"do the work"}}"#,
            ),
            (
                11,
                2,
                "2026-03-04T11:00:30+00:00",
                r#"{"type":"assistant","uuid":"b2","message":{"id":"m1",
                    "model":"claude-opus-4-8",
                    "usage":{"input_tokens":100,"output_tokens":200,
                             "cache_creation_input_tokens":300,
                             "cache_read_input_tokens":400},
                    "content":"done"}}"#,
            ),
            (
                13,
                1,
                "2026-03-04T12:00:00+00:00",
                r#"{"type":"human","uuid":"o1","message":{"content":"other project"}}"#,
            ),
        ];
        for (fk, seq, ts, raw) in rows {
            conn.execute(
                "INSERT INTO messages (session_fk, seq, timestamp, raw_json) \
                 VALUES (?, ?, ?, ?)",
                rusqlite::params![fk, seq, ts, raw],
            )
            .expect("seed message");
        }
        drop(conn);

        let package =
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../stackunderflow");
        let state = AppState::new(store, package, crate::state::Config::default());
        (state, Scratch(dir))
    }

    /// Drive the mounted route in-process — no port, so nothing can collide
    /// with the harness's `:8096` / `:8097` or the maintainer's `:8095`.
    ///
    /// The `method_not_allowed_fallback` mirrors `lib.rs:76`, because the 405
    /// body is stamped by the crate root and a bare `register(Router::new())`
    /// would answer axum's native empty 405 instead of starlette's.
    async fn call(state: &AppState, method: &str, target: &str) -> (StatusCode, String) {
        let app = register(Router::new())
            .method_not_allowed_fallback(|| async { crate::json::method_not_allowed() })
            .with_state(state.clone());
        let response = app
            .oneshot(
                HttpRequest::builder()
                    .method(method)
                    .uri(target)
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 22)
            .await
            .expect("body");
        (status, String::from_utf8(bytes.to_vec()).expect("utf-8"))
    }

    #[tokio::test]
    async fn both_required_parameters_are_reported_together() {
        let (state, _scratch) = seeded("missing");
        let (status, body) = call(&state, "GET", "/api/sessions/compare").await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(
            body,
            r#"{"detail":[{"type":"missing","loc":["query","a"],"msg":"Field required","input":null},{"type":"missing","loc":["query","b"],"msg":"Field required","input":null}]}"#
        );

        // One present, one absent — only the absent one is named.
        let (status, body) = call(&state, "GET", "/api/sessions/compare?a=sess-a").await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(
            body,
            r#"{"detail":[{"type":"missing","loc":["query","b"],"msg":"Field required","input":null}]}"#
        );
    }

    #[tokio::test]
    async fn no_project_and_no_log_path_is_the_four_hundred() {
        // The case matrix cannot reach this: `P-by-dir-known` selects a project
        // long before the `J-*` rows and nothing deselects. Pinned here instead.
        let (state, _scratch) = seeded("noproject");
        let (status, body) = call(&state, "GET", "/api/sessions/compare?a=x&b=y").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(
            body,
            r#"{"detail":"No project selected or log_path provided"}"#
        );
    }

    #[tokio::test]
    async fn an_empty_log_path_falls_back_to_the_selected_project() {
        // `log_path or deps.current_log_path` is a TRUTHINESS test on both, so
        // `?log_path=` is not "the empty project", it is "no override".
        let (state, _scratch) = seeded("fallback");
        state.set_current_project(crate::state::CurrentProject {
            project_path: None,
            log_path: Some("/home/u/.claude/projects/-p-one".to_owned()),
        });
        let (status, body) =
            call(&state, "GET", "/api/sessions/compare?a=zzz&b=zzz&log_path=").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        // …and the id is named ONCE PER POSITION, not once per value.
        assert_eq!(body, r#"{"detail":"Session(s) not found: zzz, zzz"}"#);
    }

    #[tokio::test]
    async fn an_unknown_slug_is_spelled_out_in_the_detail() {
        // This handler interpolates the slug; its neighbour
        // `/api/jsonl-content` says only "Project not found in store".
        let (state, _scratch) = seeded("noslug");
        let (status, body) = call(
            &state,
            "GET",
            "/api/sessions/compare?a=x&b=y&log_path=/home/u/.claude/projects/-nope",
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body, r#"{"detail":"Project '-nope' not found in store"}"#);
    }

    #[tokio::test]
    async fn the_session_lookup_is_scoped_to_the_resolved_project() {
        // `sess-other` is a real session — of the OTHER project. A handler that
        // dropped the `project_id IN (…)` predicate would answer 200.
        let (state, _scratch) = seeded("scope");
        let (status, body) = call(
            &state,
            "GET",
            "/api/sessions/compare?a=sess-a&b=sess-other&log_path=/h/-p-one",
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body, r#"{"detail":"Session(s) not found: sess-other"}"#);
    }

    #[tokio::test]
    async fn a_repeated_parameter_keeps_the_last_value() {
        // starlette's `QueryParams.get`. The 404 names `zzz`, not `sess-a`.
        let (state, _scratch) = seeded("repeat");
        let (status, body) = call(
            &state,
            "GET",
            "/api/sessions/compare?a=sess-a&a=zzz&b=sess-b&log_path=/h/-p-one",
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body, r#"{"detail":"Session(s) not found: zzz"}"#);
    }

    #[tokio::test]
    async fn an_empty_id_passes_validation_and_fails_the_lookup() {
        // `?a=` is a valid `str` to pydantic, so it reaches the handler and
        // names no session — the detail ends in a bare space.
        let (state, _scratch) = seeded("emptyid");
        let (status, body) = call(
            &state,
            "GET",
            "/api/sessions/compare?a=&b=sess-b&log_path=/h/-p-one",
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body, r#"{"detail":"Session(s) not found: "}"#);
    }

    #[tokio::test]
    async fn a_session_with_no_messages_reaches_the_second_four_oh_four() {
        // `sess-empty` clears the id check — it IS a `sessions` row — and then
        // produces no `session_costs` entry, so `sa is None`. Without this the
        // second 404 would be an unported branch wearing a green tick.
        let (state, _scratch) = seeded("nocosts");
        let (status, body) = call(
            &state,
            "GET",
            "/api/sessions/compare?a=sess-empty&b=sess-b&log_path=/h/-p-one",
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body, r#"{"detail":"Session(s) not found: sess-empty"}"#);
    }

    #[tokio::test]
    async fn the_happy_path_is_the_whole_payload_byte_for_byte() {
        // `cost` is the reference's own answer, not this port's: the same token
        // bag through the reference `infra.costs.compute_cost` for
        // `claude-opus-4-8` gives `0.007574999999999999`, trailing bits and all
        // — which is what makes the float a parity assertion rather than a
        // snapshot. The `diff.tokens` key order here is the port's chosen one
        // (see `services::session_compare::token_diff`); Python's is a
        // hash-randomised set and is the reason `!J-compare` stays open.
        let (state, _scratch) = seeded("happy");
        let (status, body) = call(
            &state,
            "GET",
            "/api/sessions/compare?a=sess-a&b=sess-b&log_path=/h/-p-one",
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body,
            r#"{"a":{"session_id":"sess-a","started_at":"2026-03-04T10:00:00+00:00","ended_at":"2026-03-04T10:00:00+00:00","duration_s":0.0,"cost":0.0,"tokens":{"input":0,"output":0,"cache_creation":0,"cache_read":0},"messages":1,"commands":1,"errors":0,"first_prompt_preview":"just asking","models_used":[]},"b":{"session_id":"sess-b","started_at":"2026-03-04T11:00:00+00:00","ended_at":"2026-03-04T11:00:30+00:00","duration_s":30.0,"cost":0.007574999999999999,"tokens":{"input":100,"output":200,"cache_creation":300,"cache_read":400},"messages":2,"commands":1,"errors":0,"first_prompt_preview":"do the work","models_used":["claude-opus-4-8"]},"diff":{"cost":0.007574999999999999,"tokens":{"input":100,"output":200,"cache_creation":300,"cache_read":400},"commands":0,"errors":0,"duration_s":30.0},"currency":{"code":"USD","symbol":"$","rate_from_usd":1.0,"warning":null}}"#
        );
    }

    #[tokio::test]
    async fn comparing_a_session_with_itself_is_a_zero_diff_and_not_a_four_oh_four() {
        // `session_id IN (?, ?)` binds the same value twice and SQLite returns
        // one row, so this is the branch that proves `cost` / `duration_s`
        // still render `0.0` where `commands` / `errors` render `0`.
        let (state, _scratch) = seeded("same");
        let (status, body) = call(
            &state,
            "GET",
            "/api/sessions/compare?a=sess-b&b=sess-b&log_path=/h/-p-one",
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            body.contains(
                r#""diff":{"cost":0.0,"tokens":{"input":0,"output":0,"cache_creation":0,"cache_read":0},"commands":0,"errors":0,"duration_s":0.0}"#
            ),
            "{body}"
        );
    }

    #[tokio::test]
    async fn the_unclaimed_methods_answer_starlettes_405() {
        let (state, _scratch) = seeded("methods");
        for method in ["POST", "PUT", "DELETE"] {
            let (status, body) = call(&state, method, "/api/sessions/compare?a=x&b=y").await;
            assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED, "{method}");
            assert_eq!(body, r#"{"detail":"Method Not Allowed"}"#, "{method}");
        }
    }
}
