//! `routes/qa.py` — 4 endpoints, wave 6.
//!
//! | Item | Method | FastAPI path | axum path | State |
//! |---|---|---|---|---|
//! | `RS-6-012` | `GET` | `/api/qa` | `/api/qa` | **ported** |
//! | `RS-6-013` | `GET` | `/api/qa/stats` | `/api/qa/stats` | **ported** |
//! | `RS-6-014` | `GET` | `/api/qa/{qa_id}` | `/api/qa/{qa_id}` | **ported** |
//! | `RS-6-015` | `POST` | `/api/qa/reindex` | `/api/qa/reindex` | **ported** — see `QA-REINDEX-DIFFER.md` |
//!
//! # A second sidecar, with a second sanitiser
//!
//! `QAService` keeps `qa_pairs` + a `qa_fts` FTS5 index in
//! `$STACKUNDERFLOW_HOME/qa_pairs.db`. Same read-only-never-create policy as
//! [`super::search`] (DIV-077).
//!
//! What is *not* shared is the query sanitiser, and the difference is
//! load-bearing. `SearchService._sanitize_fts_query` neutralises every FTS5
//! operator into a literal term; `QAService._sanitize_fts_query` **passes the
//! query straight through** the moment it spots `AND`/`OR`/`NOT`/`NEAR` as a
//! whole word, or a bare `*` or `"` anywhere. So `?search=a AND b` is a real
//! FTS5 boolean here and three literal words there — and a malformed one
//! reaches the engine, raises `OperationalError`, and is swallowed into an
//! empty page. Two sanitisers, two behaviours, both reproduced;
//! [`stax_core::ask::sanitize_fts_query`] is deliberately *not* reused for this
//! module.
//!
//! # Ordering
//!
//! `/api/qa/stats` is declared before `/api/qa/{qa_id}` in `qa.py`, so
//! Starlette matches the literal first. axum prefers a static segment over a
//! `{param}` one regardless. Same answer, and `stats` is therefore not
//! reachable as a `qa_id` on either side.
//!
//! # `POST /api/qa/reindex` — ported, and it has no case row. Ever.
//!
//! DIV-080 deferred it; it is now written, under the same standing ruling that
//! keeps `/api/search/reindex` and `/api/tags/reindex` out of
//! `parity/endpoint-cases.txt`: a `!` row suppresses the *verdict* and still
//! *fires the request* (DIV-059/078), and this handler `DELETE`s and rebuilds
//! every row of `qa_pairs.db` on the home the two harness servers share. It is
//! proven by `rust/QA-REINDEX-DIFFER.md` instead — two throwaway homes, two
//! ports, an artefact diff, run twice.
//!
//! The extraction it drives is the interesting half: `extract_qa_pairs` pairs a
//! user turn with every assistant turn that follows it, absorbing follow-ups
//! ("that didn't work") into the same pair rather than starting a new one, and
//! classifying the outcome from three signals. All of it is below, in
//! [`extract_qa_pairs`] and its helpers, transcribed rather than approximated —
//! the pair `id` is a SHA-256 of `session_id:timestamp:question[:200]`, so a
//! one-character drift in what counts as the question changes the primary key.
//!
//! Three fields in what it leaves behind are wall-clock: `elapsed_ms` in the
//! response, and `qa_pairs.created_at` + `qa_index_metadata.indexed_at` in the
//! artefact. The differ compares those for shape and everything else for bytes.

use std::collections::{BTreeSet, HashMap};

use axum::Router;
use axum::extract::{Path, RawQuery, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use rusqlite::{Connection, OpenFlags};
use serde_json::{Map, Value};
use stax_etl::stats::aggregator::round_py;

use super::search::{
    group_by_slug, index_error, merged_messages, msg_text, now_iso, py_error_text, py_strip,
};
use crate::json::{JsonBody, validation_422};
use crate::pyops::{floor_div, sql_value};
use crate::qs::Query;
use crate::state::AppState;

/// `per_page = min(per_page, 100)`.
const MAX_PER_PAGE: i64 = 100;

/// `row["question_text"][:500]` / `[:500]` — code points.
const TEXT_CHARS: usize = 500;

/// The `qa_pairs` columns every read here projects, in `SELECT` order.
const QA_COLUMNS: &str = "q.id, q.session_id, q.project, q.question_text, q.answer_text, \
                          q.code_snippets, q.tools_used, q.timestamp, q.model, \
                          q.num_attempts, q.resolution_status, q.loop_count";

/// Mount this module's endpoints onto `router`.
pub fn register(router: Router<AppState>) -> Router<AppState> {
    router
        .route("/api/qa", get(list_qa_pairs))
        .route("/api/qa/stats", get(qa_stats))
        .route("/api/qa/{qa_id}", get(get_qa_pair))
        // `POST /api/qa/reindex` is declared AFTER `GET /api/qa/{qa_id}` in
        // `qa.py`, and starlette matches in declaration order — so on the
        // reference `GET /api/qa/reindex` reaches the detail handler with
        // `qa_id="reindex"` and 404s, it does not 405. matchit prefers the
        // static segment, which would have made it a 405. The `.get(…)` leg
        // below restores the reference's answer; without it this port would
        // have quietly changed a status nobody has a case row for.
        .route(
            "/api/qa/reindex",
            post(reindex_qa).get(get_qa_pair_named_reindex),
        )
}

/// `QA_DB_PATH` — `app_dir() / "qa_pairs.db"`.
fn qa_db_path(state: &AppState) -> std::path::PathBuf {
    state.store_path().parent().map_or_else(
        || std::path::PathBuf::from("qa_pairs.db"),
        |dir| dir.join("qa_pairs.db"),
    )
}

/// `QAService._get_conn`, read-only — DIV-077.
fn open_qa(state: &AppState) -> Option<Connection> {
    let path = qa_db_path(state);
    if !path.exists() {
        return None;
    }
    Connection::open_with_flags(
        &path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .ok()
}

// ── GET /api/qa ──────────────────────────────────────────────────────────────

/// The declared signature, in order.
struct ListParams {
    project: Option<String>,
    date_from: Option<String>,
    date_to: Option<String>,
    search: Option<String>,
    resolution_status: Option<String>,
    page: i64,
    per_page: i64,
}

async fn list_qa_pairs(State(state): State<AppState>, RawQuery(raw): RawQuery) -> JsonBody {
    let query = Query::parse(raw.as_deref().unwrap_or_default());
    let params = ListParams {
        project: query.get("project").map(str::to_owned),
        date_from: query.get("date_from").map(str::to_owned),
        date_to: query.get("date_to").map(str::to_owned),
        search: query.get("search").map(str::to_owned),
        resolution_status: query.get("resolution_status").map(str::to_owned),
        page: match query.int_or("page", 1) {
            Ok(value) => value,
            Err(err) => return validation_422(&err),
        },
        per_page: match query.int_or("per_page", 20) {
            Ok(value) => value.min(MAX_PER_PAGE),
            Err(err) => return validation_422(&err),
        },
    };

    match tokio::task::spawn_blocking(move || list_qa(&state, &params)).await {
        Ok(payload) => JsonBody::ok(payload),
        Err(err) => failure(format!("Failed to list Q&A pairs: {err}")),
    }
}

/// `QAService.list_qa` — two SQL shapes, one response shape.
///
/// The FTS branch and the plain branch differ in more than a `WHERE`: the FTS
/// one joins `qa_fts` on `q.rowid` and projects two `snippet()` columns, the
/// plain one selects `NULL as question_snippet, NULL as answer_snippet` so the
/// row shape stays identical. Both then `ORDER BY q.timestamp DESC` — note that
/// the FTS branch does **not** order by rank, so relevance never reaches the
/// caller here (unlike `/api/search`).
///
/// The empty-page early return has FIVE keys, not six: `list_qa` has no
/// `query` echo. That asymmetry with `/api/search` is real and reproduced.
fn list_qa(state: &AppState, params: &ListParams) -> Value {
    let Some(conn) = open_qa(state) else {
        return empty_page(params);
    };

    let mut clauses: Vec<&str> = Vec::new();
    let mut filter_params: Vec<rusqlite::types::Value> = Vec::new();
    if let Some(project) = params.project.as_deref().filter(|v| !v.is_empty()) {
        clauses.push("q.project = ?");
        filter_params.push(rusqlite::types::Value::Text(project.to_owned()));
    }
    if let Some(date_from) = params.date_from.as_deref().filter(|v| !v.is_empty()) {
        clauses.push("q.timestamp >= ?");
        filter_params.push(rusqlite::types::Value::Text(date_from.to_owned()));
    }
    if let Some(date_to) = params.date_to.as_deref().filter(|v| !v.is_empty()) {
        clauses.push("q.timestamp <= ?");
        // A bare `YYYY-MM-DD` upper bound means end-of-day.
        filter_params.push(rusqlite::types::Value::Text(if date_to.len() == 10 {
            format!("{date_to}T23:59:59")
        } else {
            date_to.to_owned()
        }));
    }
    if let Some(status) = params
        .resolution_status
        .as_deref()
        .filter(|v| !v.is_empty())
    {
        clauses.push("q.resolution_status = ?");
        filter_params.push(rusqlite::types::Value::Text(status.to_owned()));
    }

    let searching = params
        .search
        .as_deref()
        .is_some_and(|value| !value.trim().is_empty());

    let (where_sql, bound) = if searching {
        let safe_query = sanitize_qa_fts_query(params.search.as_deref().unwrap_or_default());
        let mut where_sql = "WHERE qa_fts MATCH ?".to_owned();
        let mut bound = vec![rusqlite::types::Value::Text(safe_query)];
        if !clauses.is_empty() {
            where_sql.push_str(" AND ");
            where_sql.push_str(&clauses.join(" AND "));
            bound.extend(filter_params);
        }
        (where_sql, bound)
    } else {
        let where_sql = if clauses.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", clauses.join(" AND "))
        };
        (where_sql, filter_params)
    };

    let from_sql = if searching {
        "FROM qa_fts JOIN qa_pairs q ON qa_fts.rowid = q.rowid"
    } else {
        "FROM qa_pairs q"
    };

    let count_sql = format!("SELECT COUNT(*) as total {from_sql} {where_sql}");
    let total = match conn.query_row(
        &count_sql,
        rusqlite::params_from_iter(bound.iter()),
        |row| row.get::<_, i64>(0),
    ) {
        Ok(total) => total,
        // The plain branch does NOT wrap its count in a try, so a missing table
        // there escapes to the route's 500 in Python. The port answers the same
        // empty page for both, because a store the port never creates is the
        // DIV-077 case, not a corrupt one.
        Err(_) => return empty_page(params),
    };

    let total_pages = if total > 0 {
        floor_div(total + params.per_page - 1, params.per_page)
    } else {
        0
    };
    let mut page = params.page;
    if page < 1 {
        page = 1;
    }
    if page > total_pages && total_pages > 0 {
        page = total_pages;
    }
    let offset = (page - 1).saturating_mul(params.per_page);

    let results_sql = if searching {
        format!(
            "SELECT {QA_COLUMNS}, \
             snippet(qa_fts, 0, '<mark>', '</mark>', '...', 32) as question_snippet, \
             snippet(qa_fts, 1, '<mark>', '</mark>', '...', 48) as answer_snippet \
             {from_sql} {where_sql} \
             ORDER BY q.timestamp DESC \
             LIMIT ? OFFSET ?"
        )
    } else {
        format!(
            "SELECT {QA_COLUMNS}, NULL as question_snippet, NULL as answer_snippet \
             {from_sql} {where_sql} \
             ORDER BY q.timestamp DESC \
             LIMIT ? OFFSET ?"
        )
    };
    let mut page_bound = bound;
    page_bound.push(rusqlite::types::Value::Integer(params.per_page));
    page_bound.push(rusqlite::types::Value::Integer(offset));

    let Ok(mut stmt) = conn.prepare(&results_sql) else {
        return empty_page(params);
    };
    let Ok(rows) = stmt
        .query_map(rusqlite::params_from_iter(page_bound.iter()), |row| {
            let mut obj = Map::new();
            obj.insert("id".to_owned(), sql_value(row, 0)?);
            obj.insert("session_id".to_owned(), sql_value(row, 1)?);
            obj.insert("project".to_owned(), sql_value(row, 2)?);
            obj.insert("question_text".to_owned(), truncated(row, 3)?);
            obj.insert("answer_text".to_owned(), truncated(row, 4)?);
            obj.insert("code_snippets".to_owned(), json_column(row, 5)?);
            obj.insert("tools_used".to_owned(), json_column(row, 6)?);
            obj.insert("timestamp".to_owned(), sql_value(row, 7)?);
            obj.insert("model".to_owned(), sql_value(row, 8)?);
            obj.insert("num_attempts".to_owned(), sql_value(row, 9)?);
            obj.insert("resolution_status".to_owned(), sql_value(row, 10)?);
            obj.insert("loop_count".to_owned(), sql_value(row, 11)?);
            obj.insert("question_snippet".to_owned(), sql_value(row, 12)?);
            obj.insert("answer_snippet".to_owned(), sql_value(row, 13)?);
            Ok(Value::Object(obj))
        })
        .and_then(Iterator::collect::<rusqlite::Result<Vec<_>>>)
    else {
        return empty_page(params);
    };

    let mut obj = Map::new();
    obj.insert("results".to_owned(), Value::Array(rows));
    obj.insert("total".to_owned(), Value::from(total));
    obj.insert("page".to_owned(), Value::from(page));
    obj.insert("per_page".to_owned(), Value::from(params.per_page));
    obj.insert("total_pages".to_owned(), Value::from(total_pages));
    Value::Object(obj)
}

/// The five-key body the two `OperationalError` swallows share.
fn empty_page(params: &ListParams) -> Value {
    let mut obj = Map::new();
    obj.insert("results".to_owned(), Value::Array(Vec::new()));
    obj.insert("total".to_owned(), Value::from(0));
    obj.insert("page".to_owned(), Value::from(params.page));
    obj.insert("per_page".to_owned(), Value::from(params.per_page));
    obj.insert("total_pages".to_owned(), Value::from(0));
    Value::Object(obj)
}

// ── GET /api/qa/{qa_id} ──────────────────────────────────────────────────────

async fn get_qa_pair(State(state): State<AppState>, Path(qa_id): Path<String>) -> JsonBody {
    qa_detail(state, qa_id).await
}

/// `GET /api/qa/reindex` — the literal path, matched as a `qa_id`.
///
/// See the `register` comment: the reference's declaration order sends this to
/// the detail handler, so it 404s rather than 405s.
async fn get_qa_pair_named_reindex(State(state): State<AppState>) -> JsonBody {
    qa_detail(state, "reindex".to_owned()).await
}

async fn qa_detail(state: AppState, qa_id: String) -> JsonBody {
    match tokio::task::spawn_blocking(move || qa_by_id(&state, &qa_id)).await {
        Ok(Some(payload)) => JsonBody::ok(payload),
        Ok(None) => {
            // `raise HTTPException(404, "Q&A pair not found")`, re-raised past
            // the handler's own `except Exception` by the bare `except
            // HTTPException: raise` — so it is FastAPI's `{"detail": …}`, not
            // this module's `{"error": …}`.
            let mut obj = Map::new();
            obj.insert("detail".to_owned(), Value::from("Q&A pair not found"));
            JsonBody::with_status(StatusCode::NOT_FOUND, Value::Object(obj))
        }
        Err(err) => failure(format!("Failed to get Q&A pair: {err}")),
    }
}

/// `QAService.get_qa_by_id` — the FULL texts, not the 500-char slices.
///
/// `created_at` appears here and nowhere else; the list projection drops it and
/// this one drops the two snippet columns. Two different dict literals for the
/// same table, and the key order below is the detail one's.
fn qa_by_id(state: &AppState, qa_id: &str) -> Option<Value> {
    let conn = open_qa(state)?;
    let mut stmt = conn
        .prepare(
            "SELECT id, session_id, project, question_text, answer_text, \
             code_snippets, tools_used, timestamp, model, num_attempts, \
             resolution_status, loop_count, created_at \
             FROM qa_pairs WHERE id = ?",
        )
        .ok()?;
    let mut rows = stmt.query([qa_id]).ok()?;
    let row = rows.next().ok()??;
    let mut obj = Map::new();
    obj.insert("id".to_owned(), sql_value(row, 0).ok()?);
    obj.insert("session_id".to_owned(), sql_value(row, 1).ok()?);
    obj.insert("project".to_owned(), sql_value(row, 2).ok()?);
    obj.insert("question_text".to_owned(), sql_value(row, 3).ok()?);
    obj.insert("answer_text".to_owned(), sql_value(row, 4).ok()?);
    obj.insert("code_snippets".to_owned(), json_column(row, 5).ok()?);
    obj.insert("tools_used".to_owned(), json_column(row, 6).ok()?);
    obj.insert("timestamp".to_owned(), sql_value(row, 7).ok()?);
    obj.insert("model".to_owned(), sql_value(row, 8).ok()?);
    obj.insert("num_attempts".to_owned(), sql_value(row, 9).ok()?);
    obj.insert("resolution_status".to_owned(), sql_value(row, 10).ok()?);
    obj.insert("loop_count".to_owned(), sql_value(row, 11).ok()?);
    obj.insert("created_at".to_owned(), sql_value(row, 12).ok()?);
    Some(Value::Object(obj))
}

// ── GET /api/qa/stats ────────────────────────────────────────────────────────

async fn qa_stats(State(state): State<AppState>) -> JsonBody {
    match tokio::task::spawn_blocking(move || stats(&state)).await {
        Ok(payload) => JsonBody::ok(payload),
        Err(err) => failure(format!("Failed to get Q&A stats: {err}")),
    }
}

/// `QAService.get_stats`.
fn stats(state: &AppState) -> Value {
    let conn = open_qa(state);
    let scalar = |sql: &str| -> i64 {
        conn.as_ref()
            .and_then(|conn| conn.query_row(sql, [], |row| row.get::<_, i64>(0)).ok())
            .unwrap_or(0)
    };
    let rows = |sql: &str, keys: [&str; 2]| -> Vec<Value> {
        conn.as_ref()
            .and_then(|conn| {
                let mut stmt = conn.prepare(sql).ok()?;
                let rows = stmt
                    .query_map([], |row| {
                        let mut obj = Map::new();
                        obj.insert(keys[0].to_owned(), sql_value(row, 0)?);
                        obj.insert(keys[1].to_owned(), sql_value(row, 1)?);
                        Ok(Value::Object(obj))
                    })
                    .ok()?
                    .collect::<rusqlite::Result<Vec<_>>>()
                    .ok()?;
                Some(rows)
            })
            .unwrap_or_default()
    };

    let total = scalar("SELECT COUNT(*) as c FROM qa_pairs");
    let by_project = rows(
        "SELECT project, COUNT(*) as count FROM qa_pairs GROUP BY project ORDER BY count DESC",
        ["project", "count"],
    );
    let by_date = rows(
        "SELECT substr(timestamp, 1, 10) as date, COUNT(*) as count \
         FROM qa_pairs \
         WHERE timestamp IS NOT NULL AND timestamp != '' \
         GROUP BY date \
         ORDER BY date DESC \
         LIMIT 30",
        ["date", "count"],
    );
    let indexed_projects: Vec<Value> = conn
        .as_ref()
        .and_then(|conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT project, indexed_at, qa_count FROM qa_index_metadata ORDER BY project",
                )
                .ok()?;
            let rows = stmt
                .query_map([], |row| {
                    // `dict(row)` — the SELECT's column order.
                    let mut obj = Map::new();
                    obj.insert("project".to_owned(), sql_value(row, 0)?);
                    obj.insert("indexed_at".to_owned(), sql_value(row, 1)?);
                    obj.insert("qa_count".to_owned(), sql_value(row, 2)?);
                    Ok(Value::Object(obj))
                })
                .ok()?
                .collect::<rusqlite::Result<Vec<_>>>()
                .ok()?;
            Some(rows)
        })
        .unwrap_or_default();
    // `code_snippets != '[]'` — a string comparison, so a row storing `[ ]` or
    // NULL counts as "has code". Reproduced, not tidied.
    let with_code = scalar("SELECT COUNT(*) as c FROM qa_pairs WHERE code_snippets != '[]'");

    let mut obj = Map::new();
    obj.insert("total_pairs".to_owned(), Value::from(total));
    obj.insert("by_project".to_owned(), Value::Array(by_project));
    obj.insert("by_date".to_owned(), Value::Array(by_date));
    obj.insert(
        "indexed_projects".to_owned(),
        Value::Array(indexed_projects),
    );
    obj.insert("with_code_snippets".to_owned(), Value::from(with_code));
    Value::Object(obj)
}

// ── POST /api/qa/reindex ─────────────────────────────────────────────────────

/// Words that make a user turn a question. Order is the list's; `startswith`
/// is checked against each in turn and the first hit wins, so the order is
/// only observable through cost, never through the answer.
const QUESTION_KEYWORDS: [&str; 31] = [
    "how",
    "why",
    "fix",
    "error",
    "help",
    "what",
    "can you",
    "is there",
    "could you",
    "where",
    "when",
    "which",
    "should",
    "would",
    "does",
    "doesn't work",
    "not working",
    "broken",
    "issue",
    "problem",
    "bug",
    "implement",
    "create",
    "add",
    "make",
    "build",
    "set up",
    "configure",
    "explain",
    "show me",
    "tell me",
];

/// Phrases that mean "your last answer was wrong" — a continuation of the same
/// question rather than a new one.
const FOLLOWUP_PATTERNS: [&str; 22] = [
    "that didn't work",
    "that doesn't work",
    "still not working",
    "still broken",
    "still getting",
    "same error",
    "same issue",
    "didn't fix",
    "doesn't fix",
    "try again",
    "that's not right",
    "that's wrong",
    "not quite",
    "almost but",
    "close but",
    "nope",
    "no, ",
    "no that",
    "actually,",
    "wait,",
    "but ",
    "however ",
];

/// `content_preview[:200]` in `_generate_qa_id` — code points.
const ID_PREVIEW_CHARS: usize = 200;

/// `snippet[:2000]` in `_extract_code_snippets` — code points.
const SNIPPET_CHARS: usize = 2000;

/// `content_lower[:100]` in `_is_followup` — code points.
const FOLLOWUP_WINDOW: usize = 100;

/// One extracted pair, in the dict-literal order `extract_qa_pairs` builds.
struct QaPair {
    id: String,
    session_id: String,
    project: String,
    question_text: String,
    answer_text: String,
    code_snippets: Vec<String>,
    tools_used: Vec<String>,
    timestamp: String,
    model: String,
    num_attempts: i64,
    resolution_status: String,
    loop_count: i64,
}

/// `reindex_qa` — the route, its clock, and its own error wording.
///
/// The 500 leg here is **not** the other two modules': `qa.py` says
/// `f"Q&A reindex failed: {str(e)}"` where `search.py` and `tags.py` both say
/// `f"Reindex failed: {str(e)}"`. Three routes, two messages; transcribed, not
/// harmonised.
async fn reindex_qa(State(state): State<AppState>) -> JsonBody {
    let start = std::time::Instant::now();
    let outcome = tokio::task::spawn_blocking(move || reindex_all(&state)).await;
    let elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;
    match outcome {
        Ok(Ok(mut result)) => {
            if let Value::Object(map) = &mut result {
                map.insert(
                    "elapsed_ms".to_owned(),
                    Value::from(round_py(elapsed_ms, 2)),
                );
            }
            JsonBody::ok(result)
        }
        Ok(Err(err)) => failure(format!("Q&A reindex failed: {}", py_error_text(&err))),
        Err(err) => failure(format!("Q&A reindex failed: {err}")),
    }
}

/// `QAService._get_conn` + `_ensure_schema`, on a WRITE handle.
///
/// Same shape and same reason as [`super::search::open_index_for_write`]: the
/// read path never creates this file (DIV-077) and the writer must.
/// `QAService._ensure_schema`, statement for statement.
///
/// The indentation is load-bearing for the same reason [`super::search`]'s is:
/// `sqlite_master.sql` keeps the verbatim text.
const QA_SCHEMA: [&str; 10] = [
    r#"CREATE TABLE IF NOT EXISTS qa_pairs (
                    id TEXT PRIMARY KEY,
                    session_id TEXT NOT NULL,
                    project TEXT NOT NULL,
                    question_text TEXT NOT NULL,
                    answer_text TEXT NOT NULL,
                    code_snippets TEXT DEFAULT '[]',
                    tools_used TEXT DEFAULT '[]',
                    timestamp TEXT,
                    model TEXT,
                    num_attempts INTEGER DEFAULT 1,
                    created_at TEXT NOT NULL,
                    resolution_status TEXT NOT NULL DEFAULT 'open',
                    loop_count INTEGER NOT NULL DEFAULT 0
                )"#,
    r#"CREATE VIRTUAL TABLE IF NOT EXISTS qa_fts USING fts5(
                    question_text,
                    answer_text,
                    content='qa_pairs',
                    content_rowid='rowid',
                    tokenize='porter unicode61'
                )"#,
    r#"CREATE TRIGGER IF NOT EXISTS qa_ai AFTER INSERT ON qa_pairs BEGIN
                    INSERT INTO qa_fts(rowid, question_text, answer_text)
                    VALUES (new.rowid, new.question_text, new.answer_text);
                END"#,
    r#"CREATE TRIGGER IF NOT EXISTS qa_ad AFTER DELETE ON qa_pairs BEGIN
                    INSERT INTO qa_fts(qa_fts, rowid, question_text, answer_text)
                    VALUES('delete', old.rowid, old.question_text, old.answer_text);
                END"#,
    r#"CREATE TRIGGER IF NOT EXISTS qa_au AFTER UPDATE ON qa_pairs BEGIN
                    INSERT INTO qa_fts(qa_fts, rowid, question_text, answer_text)
                    VALUES('delete', old.rowid, old.question_text, old.answer_text);
                    INSERT INTO qa_fts(rowid, question_text, answer_text)
                    VALUES (new.rowid, new.question_text, new.answer_text);
                END"#,
    r#"CREATE TABLE IF NOT EXISTS qa_index_metadata (
                    project TEXT PRIMARY KEY,
                    indexed_at TEXT NOT NULL,
                    qa_count INTEGER DEFAULT 0
                )"#,
    r#"CREATE INDEX IF NOT EXISTS idx_qa_project ON qa_pairs(project)"#,
    r#"CREATE INDEX IF NOT EXISTS idx_qa_timestamp ON qa_pairs(timestamp)"#,
    r#"CREATE INDEX IF NOT EXISTS idx_qa_session ON qa_pairs(session_id)"#,
    r#"CREATE INDEX IF NOT EXISTS idx_qa_resolution ON qa_pairs(resolution_status)"#,
];

fn open_qa_for_write(state: &AppState) -> rusqlite::Result<Connection> {
    let conn = Connection::open(qa_db_path(state))?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    for statement in QA_SCHEMA {
        conn.execute(statement, [])?;
    }
    Ok(conn)
}

/// `QAService.index_project` — clear the slug, insert the pairs, stamp metadata.
///
/// `created_at` is one `datetime.now(UTC).isoformat()` taken **once** before the
/// loop and reused for every row *and* for the metadata stamp, so a run's rows
/// all share a timestamp. That is why the differ can compare `created_at` for
/// "one distinct value per run" rather than only for shape.
fn index_project(conn: &Connection, project: &str, pairs: &[QaPair]) -> rusqlite::Result<i64> {
    conn.execute_batch("BEGIN")?;
    let result = index_project_body(conn, project, pairs);
    match &result {
        Ok(_) => conn.execute_batch("COMMIT")?,
        Err(_) => conn.execute_batch("ROLLBACK")?,
    }
    result
}

fn index_project_body(conn: &Connection, project: &str, pairs: &[QaPair]) -> rusqlite::Result<i64> {
    conn.execute("DELETE FROM qa_pairs WHERE project = ?", [project])?;
    let now = now_iso();
    let mut count = 0i64;
    {
        let mut insert = conn.prepare(
            "INSERT OR REPLACE INTO qa_pairs \
             (id, session_id, project, question_text, answer_text, \
              code_snippets, tools_used, timestamp, model, num_attempts, created_at, \
              resolution_status, loop_count) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )?;
        for qa in pairs {
            insert.execute(rusqlite::params![
                qa.id,
                qa.session_id,
                qa.project,
                qa.question_text,
                qa.answer_text,
                // `json.dumps(...)` with EVERY default — `ensure_ascii=True`
                // and the `", "` separator. Not the HTTP writer's compact form:
                // a two-snippet list is stored as `["a", "b"]`, with the space.
                stax_memory::pyjson::dumps_py_default(&qa.code_snippets),
                stax_memory::pyjson::dumps_py_default(&qa.tools_used),
                qa.timestamp,
                qa.model,
                qa.num_attempts,
                now,
                qa.resolution_status,
                qa.loop_count,
            ])?;
            count += 1;
        }
    }
    conn.execute(
        "INSERT OR REPLACE INTO qa_index_metadata (project, indexed_at, qa_count) VALUES (?, ?, ?)",
        rusqlite::params![project, now, count],
    )?;
    Ok(count)
}

/// `QAService.reindex_all(None, None, projects=projects)`.
///
/// Python calls `extract_qa_pairs` **twice** per slug — once inside
/// `index_project` to write, once again in the loop to count `len(qa_pairs)`.
/// The function is pure (no clock, no store, no randomness), so the two results
/// are the same list and this port extracts once. That is a cost change, not a
/// behaviour change; every other doubled call in this campaign was reproduced,
/// and this one is called out rather than assumed.
fn reindex_all(state: &AppState) -> anyhow::Result<Value> {
    let wanted: Vec<String> = {
        let conn = state.connect()?;
        stax_core::api::store_list_projects(&conn)?
            .into_iter()
            .map(|row| row.slug)
            .collect()
    };

    let conn = state.connect()?;
    let rows = stax_core::api::store_list_projects(&conn)?;
    let groups = group_by_slug(&rows, (!wanted.is_empty()).then_some(wanted.as_slice()));

    let index = open_qa_for_write(state)?;
    let mut total_qa = 0i64;
    let mut projects_indexed = 0i64;
    let mut errors: Vec<Value> = Vec::new();

    for (slug, ids) in groups {
        match merged_messages(&conn, &ids) {
            Ok(merged) if merged.is_empty() => {}
            Ok(merged) => {
                let pairs = extract_qa_pairs(&slug, &merged);
                match index_project(&index, &slug, &pairs) {
                    Ok(_) => {
                        total_qa += i64::try_from(pairs.len()).unwrap_or(i64::MAX);
                        projects_indexed += 1;
                    }
                    Err(err) => errors.push(index_error(&slug, &err.to_string())),
                }
            }
            Err(err) => errors.push(index_error(&slug, &py_error_text(&err))),
        }
    }

    let mut obj = Map::new();
    obj.insert("projects_indexed".to_owned(), Value::from(projects_indexed));
    obj.insert("total_qa_indexed".to_owned(), Value::from(total_qa));
    obj.insert("errors".to_owned(), Value::Array(errors));
    Ok(Value::Object(obj))
}

// ── extraction ───────────────────────────────────────────────────────────────

/// `QAService.extract_qa_pairs`.
///
/// The shape is: chronological sort → keep only non-blank user/assistant turns
/// → precompute each session's last *real* user turn → walk forward pairing a
/// question with every assistant turn until the next unrelated user turn.
///
/// Two things in here are easy to get subtly wrong and are called out:
///
/// * the inner loop's exit index `j` becomes the outer loop's next `i` **only
///   when it advanced past `i + 1`**; otherwise `i` moves by one. A question
///   with no answer therefore re-examines the very next message as a possible
///   question of its own.
/// * `has_code_answer` looks at the next assistant turn's **unstripped**
///   content, while every other test in the method strips first.
fn extract_qa_pairs(project_name: &str, messages: &[Value]) -> Vec<QaPair> {
    // `sorted(messages, key=lambda m: m.get("timestamp","") if m.get("timestamp") else "")`
    // — a stable sort on the timestamp string, with a falsy timestamp sorting
    // as `""` (first). `sort_by` on a `Vec` of references is stable too, so
    // equal timestamps keep store order.
    let mut sorted: Vec<&Value> = messages.iter().collect();
    sorted.sort_by(|a, b| msg_text(a, "timestamp").cmp(msg_text(b, "timestamp")));

    let relevant: Vec<&Value> = sorted
        .into_iter()
        .filter(|msg| {
            matches!(
                msg.get("type").and_then(Value::as_str),
                Some("user" | "assistant")
            ) && !py_strip(msg_text(msg, "content")).is_empty()
        })
        .collect();

    // The highest index of a real user turn per session — a tool result is not
    // one. `messages` spans several sessions here (reindex merges them), which
    // is why this is keyed and not a single integer.
    let mut last_user_idx: HashMap<&str, usize> = HashMap::new();
    for (idx, msg) in relevant.iter().enumerate() {
        if msg.get("type").and_then(Value::as_str) != Some("user") {
            continue;
        }
        let content = py_strip(msg_text(msg, "content"));
        if is_tool_echo(content) {
            continue;
        }
        last_user_idx.insert(msg_text(msg, "session_id"), idx);
    }

    let mut pairs: Vec<QaPair> = Vec::new();
    let mut i = 0usize;
    while i < relevant.len() {
        let msg = relevant[i];
        if msg.get("type").and_then(Value::as_str) != Some("user") {
            i += 1;
            continue;
        }
        let content_owned = py_strip(msg_text(msg, "content")).to_owned();
        let content = content_owned.as_str();
        if is_tool_echo(content) {
            i += 1;
            continue;
        }

        let is_q = is_question(content);

        // The next assistant turn within the following FOUR messages —
        // `range(i + 1, min(i + 5, n))`, so the window is i+1..=i+4.
        let mut has_code_answer = false;
        for candidate in relevant
            .iter()
            .take((i + 5).min(relevant.len()))
            .skip(i + 1)
        {
            if candidate.get("type").and_then(Value::as_str) == Some("assistant") {
                has_code_answer = has_code_blocks(msg_text(candidate, "content"));
                break;
            }
        }

        if !is_q && !has_code_answer {
            i += 1;
            continue;
        }

        let mut answer_parts: Vec<String> = Vec::new();
        let mut all_answer_msgs: Vec<&Value> = Vec::new();
        let mut num_attempts = 0i64;
        let mut followup_count = 0i64;
        let session_id = msg_text(msg, "session_id").to_owned();
        let timestamp = msg_text(msg, "timestamp").to_owned();
        let mut model = "N/A".to_owned();

        let mut j = i + 1;
        while j < relevant.len() {
            let next = relevant[j];
            let next_type = next.get("type").and_then(Value::as_str).unwrap_or_default();
            let next_content = py_strip(msg_text(next, "content")).to_owned();

            if next_type == "assistant" {
                // NOTE the asymmetry: an assistant turn is skipped only for
                // `[Tool Result:`, never for `[Tool Error:` — the user branch
                // below tests both.
                if !next_content.is_empty() && !next_content.starts_with("[Tool Result:") {
                    answer_parts.push(next_content);
                    all_answer_msgs.push(next);
                    num_attempts += 1;
                    if let Some(msg_model) = next.get("model").and_then(Value::as_str)
                        && !msg_model.is_empty()
                        && msg_model != "N/A"
                    {
                        model = msg_model.to_owned();
                    }
                }
                j += 1;
            } else if next_type == "user" {
                if is_tool_echo(&next_content) {
                    j += 1;
                    continue;
                }
                if is_followup(&next_content) {
                    answer_parts.push(format!("\n---\n[Follow-up]: {next_content}"));
                    followup_count += 1;
                    j += 1;
                    continue;
                }
                break;
            } else {
                // Unreachable: `relevant` holds only user and assistant turns.
                // Kept because the reference has it and a future filter change
                // would need it back.
                j += 1;
            }
        }

        if !answer_parts.is_empty() {
            let answer_text = answer_parts.join("\n\n");
            let code_snippets = extract_code_snippets(&answer_text);
            let tools_used = extract_tools_used(&all_answer_msgs);
            let qa_id = generate_qa_id(&session_id, &timestamp, content);
            // `last_user_idx_by_session.get(session_id, i) <= i` — the default
            // is `i` itself, so a session with no recorded real user turn (only
            // reachable if this very turn were a tool echo, which it is not)
            // counts as ended.
            let ended_session = last_user_idx.get(session_id.as_str()).copied().unwrap_or(i) <= i;
            let (resolution_status, loop_count) =
                classify_resolution(followup_count, !code_snippets.is_empty(), ended_session);

            pairs.push(QaPair {
                id: qa_id,
                session_id: session_id.clone(),
                project: project_name.to_owned(),
                question_text: content_owned.clone(),
                answer_text,
                code_snippets,
                tools_used,
                timestamp: timestamp.clone(),
                model: model.clone(),
                num_attempts: num_attempts.max(1),
                resolution_status,
                loop_count,
            });
        }

        i = if j > i + 1 { j } else { i + 1 };
    }

    pairs
}

/// The two prefixes that mark a turn as a tool echo rather than prose.
fn is_tool_echo(content: &str) -> bool {
    content.starts_with("[Tool Result:") || content.starts_with("[Tool Error:")
}

/// `_is_question`.
///
/// A literal `?` anywhere wins immediately — including one inside a code block,
/// which is why so much of this corpus classifies as a question.
fn is_question(content: &str) -> bool {
    if py_strip(content).is_empty() {
        return false;
    }
    if content.contains('?') {
        return true;
    }
    let lowered = content.to_lowercase();
    let lowered = py_strip(&lowered);
    QUESTION_KEYWORDS.iter().any(|keyword| {
        lowered.starts_with(keyword)
            || lowered.contains(&format!("\n{keyword}"))
            || lowered.contains(&format!(". {keyword}"))
    })
}

/// `_is_followup`.
///
/// `pattern in content_lower[:100]` is a **code-point** window, and it is
/// checked in addition to `startswith`, so `"ok, but "` at position 5 counts.
fn is_followup(content: &str) -> bool {
    if py_strip(content).is_empty() {
        return false;
    }
    let lowered = content.to_lowercase();
    let lowered = py_strip(&lowered);
    let window: String = lowered.chars().take(FOLLOWUP_WINDOW).collect();
    FOLLOWUP_PATTERNS
        .iter()
        .any(|pattern| lowered.starts_with(pattern) || window.contains(pattern))
}

/// `_has_code_blocks` — a fence, or three-plus indented non-blank lines.
fn has_code_blocks(content: &str) -> bool {
    if content.is_empty() {
        return false;
    }
    if content.contains("```") {
        return true;
    }
    // `content.split("\n")` keeps the trailing empty field, which cannot start
    // with four spaces, so the count is unaffected — split on '\n' regardless,
    // because `lines()` would also swallow a trailing `\r`.
    content
        .split('\n')
        .filter(|line| line.starts_with("    ") && !py_strip(line).is_empty())
        .count()
        >= 3
}

/// `_extract_code_snippets` — `re.findall(r"```(?:\w*)\n(.*?)```", c, re.DOTALL)`.
///
/// Hand-rolled because this crate has no regex dependency, and transcribed from
/// the engine's semantics rather than from what the pattern "means":
///
/// * `\w*` is greedy and must be followed by `\n`. Backtracking cannot help —
///   a shorter run ends on a word character, which is not `\n` — so the info
///   string is exactly the maximal word-character run after the fence.
/// * `(.*?)` with `DOTALL` is the shortest span up to the next ```` ``` ````.
/// * a fence with no closing fence is not a match, and the engine then retries
///   from the NEXT position, which is why a run of four backticks can still
///   open a block.
/// * scanning resumes after the closing fence, so blocks never overlap.
fn extract_code_snippets(content: &str) -> Vec<String> {
    let mut snippets: Vec<String> = Vec::new();
    if content.is_empty() {
        return snippets;
    }
    let chars: Vec<char> = content.chars().collect();
    let fence = ['`', '`', '`'];
    let at_fence = |pos: usize| pos + 3 <= chars.len() && chars[pos..pos + 3] == fence;

    let mut pos = 0usize;
    while pos + 3 <= chars.len() {
        if !at_fence(pos) {
            pos += 1;
            continue;
        }
        let mut cursor = pos + 3;
        while cursor < chars.len() && (chars[cursor].is_alphanumeric() || chars[cursor] == '_') {
            cursor += 1;
        }
        if cursor >= chars.len() || chars[cursor] != '\n' {
            pos += 1;
            continue;
        }
        let body_start = cursor + 1;
        let mut end = body_start;
        while end + 3 <= chars.len() && !at_fence(end) {
            end += 1;
        }
        if end + 3 > chars.len() {
            // Unclosed fence: no match at THIS start. The engine retries from
            // the next position, which is what `pos += 1` is — it matters for
            // a run of four or more backticks, where the match can only begin
            // on the second one.
            pos += 1;
            continue;
        }
        let snippet: String = chars[body_start..end].iter().collect();
        let snippet = py_strip(&snippet);
        if !snippet.is_empty() && snippet.chars().count() > 10 {
            snippets.push(snippet.chars().take(SNIPPET_CHARS).collect());
        }
        pos = end + 3;
    }
    snippets
}

/// `_extract_tools_used` — `sorted(set(names))`.
///
/// A `BTreeSet<String>` is `sorted(set(...))`: Rust orders `str` by UTF-8 bytes
/// and CPython orders `str` by code point, and for UTF-8 those are the same
/// order.
fn extract_tools_used(messages: &[&Value]) -> Vec<String> {
    let mut tools: BTreeSet<String> = BTreeSet::new();
    for msg in messages {
        let Some(list) = msg.get("tools").and_then(Value::as_array) else {
            continue;
        };
        for tool in list {
            if let Some(name) = tool.get("name").and_then(Value::as_str)
                && !name.is_empty()
            {
                tools.insert(name.to_owned());
            }
        }
    }
    tools.into_iter().collect()
}

/// `_generate_qa_id` — `sha256(f"{sid}:{ts}:{q[:200]}")[:16]`, hex.
fn generate_qa_id(session_id: &str, timestamp: &str, question: &str) -> String {
    let preview: String = question.chars().take(ID_PREVIEW_CHARS).collect();
    let raw = format!("{session_id}:{timestamp}:{preview}");
    let digest = stax_etl::stats::sha256::digest(raw.as_bytes());
    // `hexdigest()[:16]` — sixteen hex CHARACTERS, so the first eight bytes.
    use std::fmt::Write as _;
    let mut out = String::with_capacity(16);
    for byte in &digest[..8] {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// `_classify_resolution` — four rules, checked in this order.
fn classify_resolution(followup_count: i64, has_code: bool, ended_session: bool) -> (String, i64) {
    if followup_count >= 2 {
        return ("looped".to_owned(), followup_count);
    }
    if has_code && followup_count <= 1 {
        return ("resolved".to_owned(), followup_count);
    }
    if ended_session && followup_count == 0 && !has_code {
        return ("abandoned".to_owned(), followup_count);
    }
    ("open".to_owned(), followup_count)
}

// ── QA's own FTS sanitiser ───────────────────────────────────────────────────

/// `QAService._sanitize_fts_query` — pass-through on operators, quote otherwise.
///
/// Deliberately NOT [`stax_core::ask::sanitize_fts_query`]; see the module docs
/// for why the two differ and why that matters.
fn sanitize_qa_fts_query(query: &str) -> String {
    let query = query.trim();
    if query.is_empty() {
        return "\"\"".to_owned();
    }
    if has_fts5_operator(query) {
        return query.to_owned();
    }
    query
        .split_whitespace()
        .map(|word| format!("\"{}\"*", word.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(" ")
}

/// `re.compile(r'\b(AND|OR|NOT|NEAR)\b|[*"]', re.IGNORECASE).search(query)`.
///
/// `\b` sits between a `\w` and a non-`\w`, and CPython's `\w` on a `str` is
/// unicode-aware — so `çAND` is one word and does not match, while `(AND)`
/// does.
fn has_fts5_operator(query: &str) -> bool {
    if query.contains('*') || query.contains('"') {
        return true;
    }
    let chars: Vec<char> = query.chars().collect();
    let is_word = |ch: char| ch.is_alphanumeric() || ch == '_';
    for keyword in ["and", "or", "not", "near"] {
        let keyword: Vec<char> = keyword.chars().collect();
        for start in 0..chars.len() {
            let end = start + keyword.len();
            if end > chars.len() {
                break;
            }
            let matches = chars[start..end]
                .iter()
                .zip(&keyword)
                .all(|(actual, expected)| actual.to_ascii_lowercase() == *expected);
            if !matches {
                continue;
            }
            let left_ok = start == 0 || !is_word(chars[start - 1]);
            let right_ok = end == chars.len() || !is_word(chars[end]);
            if left_ok && right_ok {
                return true;
            }
        }
    }
    false
}

// ── shared ───────────────────────────────────────────────────────────────────

/// The `except Exception` funnel: `{"error": …}` with a 500.
fn failure(message: String) -> JsonBody {
    let mut obj = Map::new();
    obj.insert("error".to_owned(), Value::from(message));
    JsonBody::with_status(StatusCode::INTERNAL_SERVER_ERROR, Value::Object(obj))
}

/// `row["question_text"][:500]`.
///
/// The column is `NOT NULL`, so Python slices a `str` unconditionally and a
/// NULL would `TypeError` into the route's 500. Rendered as null here rather
/// than manufacturing that message.
fn truncated(row: &rusqlite::Row<'_>, index: usize) -> rusqlite::Result<Value> {
    Ok(match row.get::<_, Option<String>>(index)? {
        Some(text) => Value::from(text.chars().take(TEXT_CHARS).collect::<String>()),
        None => Value::Null,
    })
}

/// `json.loads(row["code_snippets"] or "[]")`.
///
/// The `or "[]"` catches NULL *and* the empty string. A value that is not valid
/// JSON raises and 500s the route; the port answers `[]`, which is the same
/// thing the column's own default says it should have been (DIV-081).
fn json_column(row: &rusqlite::Row<'_>, index: usize) -> rusqlite::Result<Value> {
    let raw: Option<String> = row.get(index)?;
    let text = match raw {
        Some(text) if !text.is_empty() => text,
        _ => "[]".to_owned(),
    };
    Ok(serde_json::from_str(&text).unwrap_or_else(|_| Value::Array(Vec::new())))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Scratch(std::path::PathBuf);

    impl Scratch {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "stax-qa-{tag}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |d| d.as_nanos())
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

    fn state_at(dir: &std::path::Path) -> AppState {
        AppState::new(
            dir.join("store.db"),
            std::path::PathBuf::from("/nonexistent/pkg"),
            crate::state::Config::default(),
        )
    }

    fn params() -> ListParams {
        ListParams {
            project: None,
            date_from: None,
            date_to: None,
            search: None,
            resolution_status: None,
            page: 1,
            per_page: 20,
        }
    }

    /// The schema `QAService._ensure_schema` writes, with two rows.
    fn seed(path: &std::path::Path) {
        let conn = Connection::open(path).expect("open");
        conn.execute_batch(
            "CREATE TABLE qa_pairs (
                 id TEXT PRIMARY KEY, session_id TEXT NOT NULL, project TEXT NOT NULL,
                 question_text TEXT NOT NULL, answer_text TEXT NOT NULL,
                 code_snippets TEXT DEFAULT '[]', tools_used TEXT DEFAULT '[]',
                 timestamp TEXT, model TEXT, num_attempts INTEGER DEFAULT 1,
                 created_at TEXT NOT NULL,
                 resolution_status TEXT NOT NULL DEFAULT 'open',
                 loop_count INTEGER NOT NULL DEFAULT 0);
             CREATE VIRTUAL TABLE qa_fts USING fts5(
                 question_text, answer_text, content='qa_pairs',
                 content_rowid='rowid', tokenize='porter unicode61');
             CREATE TRIGGER qa_ai AFTER INSERT ON qa_pairs BEGIN
                 INSERT INTO qa_fts(rowid, question_text, answer_text)
                 VALUES (new.rowid, new.question_text, new.answer_text);
             END;
             CREATE TABLE qa_index_metadata (
                 project TEXT PRIMARY KEY, indexed_at TEXT NOT NULL,
                 qa_count INTEGER DEFAULT 0);
             INSERT INTO qa_pairs (id, session_id, project, question_text, answer_text,
                                   code_snippets, tools_used, timestamp, model,
                                   num_attempts, created_at, resolution_status, loop_count)
             VALUES ('q1', 's1', '-p-one', 'how do I index sqlite', 'use an index',
                     '[\"CREATE INDEX\"]', '[\"Bash\"]', '2026-01-01T00:00:00+00:00',
                     'claude-opus-4', 1, '2026-01-01T00:00:01+00:00', 'resolved', 0),
                    ('q2', 's2', '-p-two', 'why is the join slow', 'the planner picked a scan',
                     '[]', '[]', '2026-02-01T00:00:00+00:00', 'claude-sonnet-4',
                     3, '2026-02-01T00:00:01+00:00', 'looped', 2);
             INSERT INTO qa_index_metadata (project, indexed_at, qa_count)
             VALUES ('-p-two', '2026-02-01T00:00:00+00:00', 1),
                    ('-p-one', '2026-01-01T00:00:00+00:00', 1);",
        )
        .expect("seed");
    }

    #[test]
    fn the_plain_branch_orders_newest_first_and_nulls_both_snippets() {
        let scratch = Scratch::new("plain");
        let state = state_at(&scratch.0);
        seed(&qa_db_path(&state));
        let payload = list_qa(&state, &params());
        assert_eq!(payload["total"], serde_json::json!(2));
        // `ORDER BY q.timestamp DESC` — February first.
        assert_eq!(payload["results"][0]["id"], serde_json::json!("q2"));
        assert_eq!(payload["results"][0]["question_snippet"], Value::Null);
        // The JSON columns are DECODED, not echoed as strings.
        assert_eq!(
            payload["results"][1]["code_snippets"],
            serde_json::json!(["CREATE INDEX"])
        );
        let keys: Vec<&String> = payload.as_object().expect("object").keys().collect();
        assert_eq!(
            keys,
            vec!["results", "total", "page", "per_page", "total_pages"]
        );
    }

    #[test]
    fn the_fts_branch_adds_snippets_and_still_orders_by_time_not_rank() {
        let scratch = Scratch::new("fts");
        let state = state_at(&scratch.0);
        seed(&qa_db_path(&state));
        let mut params = params();
        params.search = Some("index".to_owned());
        let payload = list_qa(&state, &params);
        assert_eq!(payload["total"], serde_json::json!(1));
        assert!(
            payload["results"][0]["question_snippet"]
                .as_str()
                .is_some_and(|s| s.contains("<mark>")),
            "{:?}",
            payload["results"][0]["question_snippet"]
        );
    }

    #[test]
    fn a_filter_combines_with_the_match_and_narrows_it() {
        let scratch = Scratch::new("filter");
        let state = state_at(&scratch.0);
        seed(&qa_db_path(&state));
        let mut params = params();
        params.resolution_status = Some("looped".to_owned());
        assert_eq!(list_qa(&state, &params)["total"], serde_json::json!(1));

        params.search = Some("slow".to_owned());
        assert_eq!(list_qa(&state, &params)["total"], serde_json::json!(1));
        params.resolution_status = Some("resolved".to_owned());
        assert_eq!(list_qa(&state, &params)["total"], serde_json::json!(0));
    }

    #[test]
    fn the_detail_row_is_untruncated_and_carries_created_at() {
        let scratch = Scratch::new("detail");
        let state = state_at(&scratch.0);
        seed(&qa_db_path(&state));
        let payload = qa_by_id(&state, "q1").expect("found");
        let keys: Vec<&String> = payload.as_object().expect("object").keys().collect();
        assert_eq!(
            keys,
            vec![
                "id",
                "session_id",
                "project",
                "question_text",
                "answer_text",
                "code_snippets",
                "tools_used",
                "timestamp",
                "model",
                "num_attempts",
                "resolution_status",
                "loop_count",
                "created_at"
            ]
        );
        assert!(qa_by_id(&state, "nope").is_none());
    }

    #[test]
    fn stats_report_every_block_in_the_literals_order() {
        let scratch = Scratch::new("stats");
        let state = state_at(&scratch.0);
        seed(&qa_db_path(&state));
        let payload = stats(&state);
        assert_eq!(payload["total_pairs"], serde_json::json!(2));
        assert_eq!(payload["with_code_snippets"], serde_json::json!(1));
        assert_eq!(
            payload["by_date"][0]["date"],
            serde_json::json!("2026-02-01")
        );
        assert_eq!(
            payload["indexed_projects"][0]["project"],
            serde_json::json!("-p-one")
        );
        let keys: Vec<&String> = payload.as_object().expect("object").keys().collect();
        assert_eq!(
            keys,
            vec![
                "total_pairs",
                "by_project",
                "by_date",
                "indexed_projects",
                "with_code_snippets"
            ]
        );
    }

    #[test]
    fn a_missing_sidecar_is_an_empty_page_and_zero_stats() {
        let scratch = Scratch::new("absent");
        let state = state_at(&scratch.0);
        assert_eq!(
            stax_memory::pyjson::dumps_http(&list_qa(&state, &params())),
            r#"{"results":[],"total":0,"page":1,"per_page":20,"total_pages":0}"#
        );
        assert_eq!(
            stax_memory::pyjson::dumps_http(&stats(&state)),
            r#"{"total_pairs":0,"by_project":[],"by_date":[],"indexed_projects":[],"with_code_snippets":0}"#
        );
        assert!(qa_by_id(&state, "q1").is_none());
    }

    #[test]
    fn qas_sanitiser_passes_operators_through_where_searchs_would_quote_them() {
        // The whole reason this function is not the shared one.
        assert_eq!(sanitize_qa_fts_query("a AND b"), "a AND b");
        assert_eq!(sanitize_qa_fts_query("fox*"), "fox*");
        assert_eq!(sanitize_qa_fts_query("\"exact\""), "\"exact\"");
        // …but `stax_core`'s would have written `"a"* "AND"* "b"*`.
        assert_eq!(
            stax_core::ask::sanitize_fts_query("a AND b"),
            r#""a"* "AND"* "b"*"#
        );

        // No operator: per-word prefix terms, inner quotes doubled.
        assert_eq!(sanitize_qa_fts_query("slow join"), r#""slow"* "join"*"#);
        assert_eq!(sanitize_qa_fts_query("  spaced  "), r#""spaced"*"#);
        assert_eq!(sanitize_qa_fts_query("   "), "\"\"");
    }

    /// The conversation every extraction test below reads, chosen so one pass
    /// exercises: a question, two follow-ups, a tool echo that must not break
    /// the pair, an `N/A` model that must not win, a snippet under the
    /// ten-character floor, and a SECOND session whose question is not a
    /// question. Expected values are `QAService.extract_qa_pairs`' own, taken
    /// from the reference on this exact input.
    fn conversation() -> Vec<Value> {
        serde_json::json!([
          {"session_id":"s1","type":"user","timestamp":"2026-01-01T00:00:01","model":null,
           "content":"how do I fix the failing pytest?","tools":[],"tokens":{"input":5,"output":0}},
          {"session_id":"s1","type":"assistant","timestamp":"2026-01-01T00:00:02","model":"claude-opus-4-8",
           "content":"Try this:\n```python\nimport pytest\nassert 1 == 1\n```\n",
           "tools":[{"name":"Edit","id":"t1","input":{"file_path":"/repo/tests/test_x.py"}}],
           "tokens":{"input":0,"output":9}},
          {"session_id":"s1","type":"user","timestamp":"2026-01-01T00:00:03","model":null,
           "content":"that didn't work","tools":[],"tokens":{}},
          {"session_id":"s1","type":"assistant","timestamp":"2026-01-01T00:00:04","model":"claude-sonnet-4-5",
           "content":"Then run:\n```bash\npytest -q\n```",
           "tools":[{"name":"Bash","id":"t2","input":{"command":"pytest -q"}}],"tokens":{"input":1,"output":2}},
          {"session_id":"s1","type":"user","timestamp":"2026-01-01T00:00:05","model":null,
           "content":"[Tool Result: ok]","tools":[],"tokens":{}},
          {"session_id":"s1","type":"user","timestamp":"2026-01-01T00:00:06","model":null,
           "content":"still broken","tools":[],"tokens":{}},
          {"session_id":"s1","type":"assistant","timestamp":"2026-01-01T00:00:07","model":"N/A",
           "content":"Let me look at the docker deploy config.","tools":[],"tokens":{}},
          {"session_id":"s2","type":"user","timestamp":"2026-01-02T00:00:01","model":null,
           "content":"deploy to kubernetes please","tools":[],"tokens":{}},
          {"session_id":"s2","type":"assistant","timestamp":"2026-01-02T00:00:02","model":"claude-opus-4-8",
           "content":"Sure, prose only, no code here.",
           "tools":[{"name":"Grep","id":"t3","input":{"pattern":"terraform"}}],"tokens":{}},
          {"session_id":"s2","type":"user","timestamp":"2026-01-02T00:00:03","model":null,
           "content":"   ","tools":[],"tokens":{}}
        ])
        .as_array()
        .expect("array")
        .clone()
    }

    #[test]
    fn one_question_absorbs_every_follow_up_until_the_next_real_question() {
        let pairs = extract_qa_pairs("-p-fixture", &conversation());
        assert_eq!(
            pairs.len(),
            1,
            "the s2 turn is neither a question nor code-answered"
        );
        let qa = &pairs[0];
        // The whole thread, follow-ups inlined with the `\n---\n[Follow-up]: `
        // marker and the parts joined by a BLANK line.
        assert_eq!(
            qa.answer_text,
            concat!(
                "Try this:\n```python\nimport pytest\nassert 1 == 1\n```\n",
                "\n\n---\n[Follow-up]: that didn't work",
                "\n\nThen run:\n```bash\npytest -q\n```",
                "\n\n\n---\n[Follow-up]: still broken",
                "\n\nLet me look at the docker deploy config."
            )
        );
        // The `[Tool Result:` turn did NOT end the pair, and the pair ran on
        // past the end of its own session into s2's first user turn.
        assert_eq!(qa.num_attempts, 3);
        assert_eq!(qa.loop_count, 2);
        assert_eq!(qa.resolution_status, "looped");
        // `N/A` never wins the model, so the LAST real one does.
        assert_eq!(qa.model, "claude-sonnet-4-5");
        assert_eq!(qa.tools_used, vec!["Bash".to_owned(), "Edit".to_owned()]);
        // `pytest -q` is nine characters, under the `> 10` floor.
        assert_eq!(
            qa.code_snippets,
            vec!["import pytest\nassert 1 == 1".to_owned()]
        );
        assert_eq!(qa.id, "cbcfb2120e2bc4b7");
        assert_eq!(qa.timestamp, "2026-01-01T00:00:01");
    }

    #[test]
    fn the_pair_id_is_the_reference_digest_not_merely_stable() {
        // Measured against `_generate_qa_id` on the same three inputs: a
        // "stable id" test that only checks determinism would pass on the
        // wrong hash, and the id is a PRIMARY KEY.
        assert_eq!(
            generate_qa_id(
                "s1",
                "2026-01-01T00:00:01",
                "how do I fix the failing pytest?"
            ),
            "cbcfb2120e2bc4b7"
        );
    }

    #[test]
    fn fenced_blocks_are_kept_only_over_ten_characters_and_never_unclosed() {
        assert_eq!(
            extract_code_snippets(
                "a ```py\nshort\n``` b ```\nthis is long enough to keep\n``` c ```unclosed\nx"
            ),
            vec!["this is long enough to keep".to_owned()]
        );
        // An info string of non-word characters is not `\w*` followed by `\n`,
        // so the fence does not open.
        assert!(extract_code_snippets("```-\nnot a block, long enough\n```").is_empty());
        assert!(extract_code_snippets("").is_empty());
    }

    #[test]
    fn the_three_text_probes_answer_what_the_reference_answers() {
        assert_eq!(
            [
                is_question("no marker here"),
                is_question("How about it"),
                is_question("line\nwhy not"),
                is_question("a. fix this"),
                is_question("plain"),
            ],
            [false, true, true, true, false]
        );
        assert_eq!(
            [
                is_followup("nope"),
                is_followup("ok but that's wrong"),
                is_followup("brand new question"),
                // Past the 100-character window and not a prefix: not a
                // follow-up, which is the whole point of the window.
                is_followup(&format!("{}nope", "x".repeat(120))),
            ],
            [true, true, false, false]
        );
        assert_eq!(
            [
                has_code_blocks("```"),
                has_code_blocks("    a\n    b\n    c"),
                has_code_blocks("    a\n    b"),
                has_code_blocks(""),
            ],
            [true, true, false, false]
        );
    }

    #[test]
    fn the_writer_round_trips_and_a_second_run_changes_nothing_but_the_clock() {
        let scratch = Scratch::new("reindex");
        let state = state_at(&scratch.0);
        let conn = open_qa_for_write(&state).expect("schema");
        let pairs = extract_qa_pairs("-p-fixture", &conversation());
        assert_eq!(
            index_project(&conn, "-p-fixture", &pairs).expect("write"),
            1
        );

        let dump = |conn: &Connection| -> Vec<String> {
            let mut stmt = conn
                .prepare(
                    "SELECT id, session_id, project, question_text, answer_text, code_snippets, \
                     tools_used, timestamp, model, num_attempts, resolution_status, loop_count \
                     FROM qa_pairs ORDER BY id",
                )
                .expect("prepare");
            stmt.query_map([], |row| {
                Ok(format!(
                    "{}|{}|{}|{}|{}|{}|{}",
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, String>(6)?,
                    row.get::<_, String>(8)?,
                    row.get::<_, i64>(9)?,
                    row.get::<_, i64>(11)?,
                ))
            })
            .expect("query")
            .collect::<rusqlite::Result<Vec<_>>>()
            .expect("rows")
        };
        let first = dump(&conn);
        // `json.dumps` defaults, so the list separator carries a SPACE.
        assert!(first[0].contains(r#"["Bash", "Edit"]"#), "{first:?}");

        // Idempotence: the DELETE-then-insert leaves exactly the same rows, and
        // the FTS index tracks it — a stale `qa_fts` row would double the count.
        assert_eq!(
            index_project(&conn, "-p-fixture", &pairs).expect("rewrite"),
            1
        );
        assert_eq!(dump(&conn), first);
        let fts: i64 = conn
            .query_row("SELECT COUNT(*) FROM qa_fts", [], |row| row.get(0))
            .expect("fts count");
        assert_eq!(fts, 1, "the delete trigger kept the index in step");
    }

    #[test]
    fn the_operator_probe_respects_word_boundaries() {
        assert!(has_fts5_operator("a AND b"));
        assert!(has_fts5_operator("(NOT)"));
        assert!(has_fts5_operator("near miss"));
        // Substrings of longer words are NOT operators.
        assert!(!has_fts5_operator("android"));
        assert!(!has_fts5_operator("nearest"));
        assert!(!has_fts5_operator("candor"));
        assert!(!has_fts5_operator("snapshot_or_else"));
    }
}
