//! `routes/qa.py` — 4 endpoints, wave 6.
//!
//! | Item | Method | FastAPI path | axum path | State |
//! |---|---|---|---|---|
//! | `RS-6-012` | `GET` | `/api/qa` | `/api/qa` | **ported** |
//! | `RS-6-013` | `GET` | `/api/qa/stats` | `/api/qa/stats` | **ported** |
//! | `RS-6-014` | `GET` | `/api/qa/{qa_id}` | `/api/qa/{qa_id}` | **ported** |
//! | `RS-6-015` | `POST` | `/api/qa/reindex` | `/api/qa/reindex` | **open** — DIV-080 |
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
//! # `POST /api/qa/reindex` — not ported, DIV-080
//!
//! `reindex_all` re-extracts Q&A pairs from every session in the store
//! (`extract_qa_pairs`, ~160 lines of turn-pairing heuristics), writes them,
//! and stamps a wall-clock `elapsed_ms`. Writer, time-varying body, not on the
//! read path the Q&A tab uses. Filed, not faked.

use axum::Router;
use axum::extract::{Path, RawQuery, State};
use axum::http::StatusCode;
use axum::routing::get;
use rusqlite::{Connection, OpenFlags};
use serde_json::{Map, Value};

use crate::json::JsonBody;
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

/// A SQLite cell as `sqlite3.Row` hands it to `json.dumps` — no coercion.
fn sql_value(row: &rusqlite::Row<'_>, index: usize) -> rusqlite::Result<Value> {
    use rusqlite::types::ValueRef;
    Ok(match row.get_ref(index)? {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(value) => Value::from(value),
        ValueRef::Real(value) => Value::from(value),
        ValueRef::Text(bytes) => Value::from(String::from_utf8_lossy(bytes).into_owned()),
        ValueRef::Blob(_) => Value::Null,
    })
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

/// Python's `//`, floor division — see [`super::search`] for the same note.
fn floor_div(numerator: i64, denominator: i64) -> i64 {
    if denominator == 0 {
        return 0;
    }
    let quotient = numerator / denominator;
    let remainder = numerator % denominator;
    if remainder != 0 && ((remainder < 0) != (denominator < 0)) {
        quotient - 1
    } else {
        quotient
    }
}

/// FastAPI's `422` for an uncoercible query parameter (DIV-053).
fn validation_422(err: &crate::qs::QueryError) -> JsonBody {
    let mut entry = Map::new();
    entry.insert("type".to_owned(), Value::from(err.kind));
    entry.insert(
        "loc".to_owned(),
        Value::Array(vec![Value::from("query"), Value::from(err.field.clone())]),
    );
    entry.insert(
        "msg".to_owned(),
        Value::from("Input should be a valid integer, unable to parse string as an integer"),
    );
    entry.insert("input".to_owned(), Value::from(err.input.clone()));
    let mut obj = Map::new();
    obj.insert(
        "detail".to_owned(),
        Value::Array(vec![Value::Object(entry)]),
    );
    JsonBody::with_status(StatusCode::UNPROCESSABLE_ENTITY, Value::Object(obj))
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
