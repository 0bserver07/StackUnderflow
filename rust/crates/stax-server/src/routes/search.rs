//! `routes/search.py` — 3 endpoints, wave 6.
//!
//! | Item | Method | FastAPI path | axum path | State |
//! |---|---|---|---|---|
//! | `RS-6-016` | `GET` | `/api/search` | `/api/search` | **ported** |
//! | `RS-6-017` | `POST` | `/api/search/reindex` | `/api/search/reindex` | **open** — DIV-078 |
//! | `RS-6-018` | `GET` | `/api/search/stats` | `/api/search/stats` | **ported** |
//!
//! # The sidecar, and why the port never creates it
//!
//! `SearchService` lives in `$STACKUNDERFLOW_HOME/search_index.db` — its own
//! SQLite file with a `messages` table, a `messages_fts` FTS5 index
//! (`content='messages'`, `tokenize='porter unicode61'`) and three sync
//! triggers. `store.db` is not involved in a query at all; the two databases
//! have independent WAL and lock domains and are joined in *code*, never by
//! `ATTACH`, which is the rule [`stax_core::lexical`] already established for
//! the CLI's half of the same index.
//!
//! Python's `SearchService.__init__` **creates** that file and applies its
//! schema as a side effect of the server merely starting. This port opens what
//! is there and never writes — the wave-0 decision, restated in
//! `stax_core::lexical`'s module docs. **DIV-077** records the one place the
//! difference is observable: on a home where the file does not exist yet,
//! Python answers from a freshly-created empty index and this answers from
//! "nothing there". Both produce the *same bytes* — an absent `messages` table
//! and an empty one are the same zero counts and the same empty result page —
//! which is why the narrowing is safe, but it is a narrowing and it is recorded
//! rather than assumed.
//!
//! # `POST /api/search/reindex` — not ported, DIV-078
//!
//! `reindex_all` walks every project in the store, rebuilds the whole FTS index
//! (a `DELETE` plus a row-per-message `INSERT`), and stamps a wall-clock
//! `elapsed_ms` into its response. A writer with a time-varying body cannot be
//! byte-diffed, and rebuilding a 251 K-row index is not what the Search tab
//! reads. Filed, not faked.

use axum::Router;
use axum::extract::{RawQuery, State};
use axum::http::StatusCode;
use axum::routing::get;
use rusqlite::{Connection, OpenFlags};
use serde_json::{Map, Value};
use stax_core::ask::{build_filter_clauses, sanitize_fts_query};

use crate::json::JsonBody;
use crate::qs::Query;
use crate::state::AppState;

/// `per_page = min(per_page, 100)` — the route's clamp, before the service.
const MAX_PER_PAGE: i64 = 100;

/// `row["content"][:500]` — a CPython `str` slice, so **code points**.
const CONTENT_CHARS: usize = 500;

/// Mount this module's endpoints onto `router`.
pub fn register(router: Router<AppState>) -> Router<AppState> {
    router
        .route("/api/search", get(search_messages))
        .route("/api/search/stats", get(search_index_stats))
}

/// `SEARCH_DB_PATH` — `app_dir() / "search_index.db"`.
fn index_path(state: &AppState) -> std::path::PathBuf {
    state.store_path().parent().map_or_else(
        || std::path::PathBuf::from("search_index.db"),
        |dir| dir.join("search_index.db"),
    )
}

/// `SearchService._get_conn`, read-only — see DIV-077 in the module docs.
fn open_index(state: &AppState) -> Option<Connection> {
    let path = index_path(state);
    if !path.exists() {
        return None;
    }
    Connection::open_with_flags(
        &path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .ok()
}

// ── GET /api/search ──────────────────────────────────────────────────────────

/// The declared signature, in order.
struct SearchParams {
    q: String,
    project: Option<String>,
    date_from: Option<String>,
    date_to: Option<String>,
    model: Option<String>,
    role: Option<String>,
    page: i64,
    per_page: i64,
}

async fn search_messages(State(state): State<AppState>, RawQuery(raw): RawQuery) -> JsonBody {
    let query = Query::parse(raw.as_deref().unwrap_or_default());
    let params = SearchParams {
        q: query.str_or("q", "").to_owned(),
        project: query.get("project").map(str::to_owned),
        date_from: query.get("date_from").map(str::to_owned),
        date_to: query.get("date_to").map(str::to_owned),
        model: query.get("model").map(str::to_owned),
        role: query.get("role").map(str::to_owned),
        page: match query.int_or("page", 1) {
            Ok(value) => value,
            Err(err) => return validation_422(&err),
        },
        // `per_page = min(per_page, 100)` happens in the ROUTE, before the
        // service sees it — so the value echoed back in the response is the
        // clamped one, including on the empty-query early return.
        per_page: match query.int_or("per_page", 20) {
            Ok(value) => value.min(MAX_PER_PAGE),
            Err(err) => return validation_422(&err),
        },
    };

    match tokio::task::spawn_blocking(move || run_search(&state, &params)).await {
        Ok(payload) => JsonBody::ok(payload),
        Err(err) => {
            // `except Exception as e: … return {"error": f"Search failed: {e}"}`
            // with a 500. `deps.logger.error` has no wire effect.
            let mut obj = Map::new();
            obj.insert(
                "error".to_owned(),
                Value::from(format!("Search failed: {err}")),
            );
            JsonBody::with_status(StatusCode::INTERNAL_SERVER_ERROR, Value::Object(obj))
        }
    }
}

/// `SearchService.search`.
///
/// Every early return in the Python method — the empty query, and the two
/// `sqlite3.OperationalError` swallows around the count and the page — produces
/// the SAME six-key empty body, with `page` and `per_page` echoed as the caller
/// sent them (pre-clamp for `page`, post-clamp for `per_page`). Reproduced
/// literally, because "0 results" and "your query is not valid FTS5" being
/// indistinguishable is the current contract.
fn run_search(state: &AppState, params: &SearchParams) -> Value {
    if params.q.trim().is_empty() {
        return empty_results(params);
    }
    let Some(conn) = open_index(state) else {
        return empty_results(params);
    };

    let safe_query = sanitize_fts_query(&params.q);
    let (where_sql, filter_params) = build_filter_clauses(
        params.project.as_deref(),
        params.date_from.as_deref(),
        params.date_to.as_deref(),
        params.model.as_deref(),
        params.role.as_deref(),
    );

    let mut bound: Vec<rusqlite::types::Value> =
        vec![rusqlite::types::Value::Text(safe_query.clone())];
    bound.extend(
        filter_params
            .iter()
            .map(|value| rusqlite::types::Value::Text(value.clone())),
    );

    let count_sql = format!(
        "SELECT COUNT(*) as total \
         FROM messages_fts \
         JOIN messages m ON messages_fts.rowid = m.id \
         WHERE messages_fts MATCH ? \
         {where_sql}"
    );
    let Ok(total) = conn.query_row(
        &count_sql,
        rusqlite::params_from_iter(bound.iter()),
        |row| row.get::<_, i64>(0),
    ) else {
        return empty_results(params);
    };

    let total_pages = if total > 0 {
        floor_div(total + params.per_page - 1, params.per_page)
    } else {
        0
    };

    // The clamp order is the method's: floor at 1, then ceiling at the last
    // page — and the ceiling only applies when there IS a last page.
    let mut page = params.page;
    if page < 1 {
        page = 1;
    }
    if page > total_pages && total_pages > 0 {
        page = total_pages;
    }
    let offset = (page - 1).saturating_mul(params.per_page);

    let results_sql = format!(
        "SELECT \
         m.id, \
         m.session_id, \
         m.project, \
         m.role, \
         m.content, \
         m.timestamp, \
         m.model, \
         m.tokens_input, \
         m.tokens_output, \
         snippet(messages_fts, 0, '<mark>', '</mark>', '...', 48) as snippet, \
         rank \
         FROM messages_fts \
         JOIN messages m ON messages_fts.rowid = m.id \
         WHERE messages_fts MATCH ? \
         {where_sql} \
         ORDER BY rank \
         LIMIT ? OFFSET ?"
    );
    let mut page_bound = bound;
    page_bound.push(rusqlite::types::Value::Integer(params.per_page));
    page_bound.push(rusqlite::types::Value::Integer(offset));

    let Ok(mut stmt) = conn.prepare(&results_sql) else {
        return empty_results(params);
    };
    let Ok(rows) = stmt
        .query_map(rusqlite::params_from_iter(page_bound.iter()), |row| {
            let mut obj = Map::new();
            obj.insert("id".to_owned(), sql_value(row, 0)?);
            obj.insert("session_id".to_owned(), sql_value(row, 1)?);
            obj.insert("project".to_owned(), sql_value(row, 2)?);
            obj.insert("role".to_owned(), sql_value(row, 3)?);
            obj.insert(
                "content".to_owned(),
                match row.get::<_, Option<String>>(4)? {
                    Some(text) => Value::from(char_prefix(&text, CONTENT_CHARS)),
                    None => Value::Null,
                },
            );
            obj.insert("timestamp".to_owned(), sql_value(row, 5)?);
            obj.insert("model".to_owned(), sql_value(row, 6)?);
            obj.insert("tokens_input".to_owned(), sql_value(row, 7)?);
            obj.insert("tokens_output".to_owned(), sql_value(row, 8)?);
            obj.insert("snippet".to_owned(), sql_value(row, 9)?);
            obj.insert("relevance".to_owned(), sql_value(row, 10)?);
            Ok(Value::Object(obj))
        })
        .and_then(Iterator::collect::<rusqlite::Result<Vec<_>>>)
    else {
        return empty_results(params);
    };

    let mut obj = Map::new();
    obj.insert("results".to_owned(), Value::Array(rows));
    obj.insert("total".to_owned(), Value::from(total));
    obj.insert("page".to_owned(), Value::from(page));
    obj.insert("per_page".to_owned(), Value::from(params.per_page));
    obj.insert("total_pages".to_owned(), Value::from(total_pages));
    obj.insert("query".to_owned(), Value::from(params.q.clone()));
    Value::Object(obj)
}

/// The six-key body every early return shares.
fn empty_results(params: &SearchParams) -> Value {
    let mut obj = Map::new();
    obj.insert("results".to_owned(), Value::Array(Vec::new()));
    obj.insert("total".to_owned(), Value::from(0));
    // NOTE: `page` here is the RAW request value — the clamp lives past this
    // return, so `?page=-3` on an empty query echoes `-3`.
    obj.insert("page".to_owned(), Value::from(params.page));
    obj.insert("per_page".to_owned(), Value::from(params.per_page));
    obj.insert("total_pages".to_owned(), Value::from(0));
    obj.insert("query".to_owned(), Value::from(params.q.clone()));
    Value::Object(obj)
}

// ── GET /api/search/stats ────────────────────────────────────────────────────

async fn search_index_stats(State(state): State<AppState>) -> JsonBody {
    match tokio::task::spawn_blocking(move || index_stats(&state)).await {
        Ok(payload) => JsonBody::ok(payload),
        Err(err) => {
            let mut obj = Map::new();
            obj.insert(
                "error".to_owned(),
                Value::from(format!("Failed to get search stats: {err}")),
            );
            JsonBody::with_status(StatusCode::INTERNAL_SERVER_ERROR, Value::Object(obj))
        }
    }
}

/// `get_index_stats()` plus the route's `stats["indexed_projects"] = …`.
///
/// The assignment happens in the route, *after* the service built its dict, so
/// `indexed_projects` is the LAST key — not wherever a struct field order would
/// have put it.
fn index_stats(state: &AppState) -> Value {
    let conn = open_index(state);
    let scalar = |sql: &str| -> i64 {
        conn.as_ref()
            .and_then(|conn| conn.query_row(sql, [], |row| row.get::<_, i64>(0)).ok())
            .unwrap_or(0)
    };
    let total_messages = scalar("SELECT COUNT(*) as c FROM messages");
    let total_projects = scalar("SELECT COUNT(*) as c FROM index_metadata");

    let models: Vec<Value> = conn
        .as_ref()
        .and_then(|conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT DISTINCT model FROM messages \
                     WHERE model IS NOT NULL AND model != '' AND model != 'N/A'",
                )
                .ok()?;
            let rows = stmt
                .query_map([], |row| sql_value(row, 0))
                .ok()?
                .collect::<rusqlite::Result<Vec<_>>>()
                .ok()?;
            Some(rows)
        })
        .unwrap_or_default();

    let indexed: Vec<Value> = conn
        .as_ref()
        .and_then(|conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT project, indexed_at, message_count FROM index_metadata ORDER BY project",
                )
                .ok()?;
            let rows = stmt
                .query_map([], |row| {
                    // `dict(row)` — the SELECT's column order, verbatim.
                    let mut obj = Map::new();
                    obj.insert("project".to_owned(), sql_value(row, 0)?);
                    obj.insert("indexed_at".to_owned(), sql_value(row, 1)?);
                    obj.insert("message_count".to_owned(), sql_value(row, 2)?);
                    Ok(Value::Object(obj))
                })
                .ok()?
                .collect::<rusqlite::Result<Vec<_>>>()
                .ok()?;
            Some(rows)
        })
        .unwrap_or_default();

    let mut obj = Map::new();
    obj.insert("total_messages".to_owned(), Value::from(total_messages));
    obj.insert("total_projects".to_owned(), Value::from(total_projects));
    obj.insert("models".to_owned(), Value::Array(models));
    obj.insert("indexed_projects".to_owned(), Value::Array(indexed));
    Value::Object(obj)
}

// ── shared ───────────────────────────────────────────────────────────────────

/// A SQLite cell as `sqlite3.Row` hands it to `json.dumps` — no coercion.
///
/// The `messages` table declares affinities but SQLite stores what it is given,
/// so a `tokens_input` written as a float comes back a float and Python ships
/// `0.0`, not `0`. Reading through the declared type would "fix" that and
/// diverge; reading the *actual* type reproduces it.
fn sql_value(row: &rusqlite::Row<'_>, index: usize) -> rusqlite::Result<Value> {
    use rusqlite::types::ValueRef;
    Ok(match row.get_ref(index)? {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(value) => Value::from(value),
        ValueRef::Real(value) => Value::from(value),
        ValueRef::Text(bytes) => Value::from(String::from_utf8_lossy(bytes).into_owned()),
        // `sqlite3` hands a BLOB to `json.dumps` as `bytes`, which raises
        // `TypeError` and 500s the route. No column here is ever a BLOB;
        // rendering it as null rather than panicking keeps the port total.
        ValueRef::Blob(_) => Value::Null,
    })
}

/// `text[:n]` — code points, not bytes.
fn char_prefix(text: &str, limit: usize) -> String {
    text.chars().take(limit).collect()
}

/// Python's `//` — floor division, which differs from Rust's `/` on negatives.
///
/// Reachable: `per_page` is clamped from ABOVE (`min(…, 100)`) and never from
/// below, so `?per_page=-5` reaches `(total + per_page - 1) // per_page` with a
/// negative divisor and CPython floors toward minus infinity where Rust
/// truncates toward zero.
fn floor_div(numerator: i64, denominator: i64) -> i64 {
    if denominator == 0 {
        // CPython raises `ZeroDivisionError`, which the route's `except
        // Exception` turns into a 500. `?per_page=0` is the only way in;
        // recorded as DIV-079 and answered with 0 rather than a panic.
        return 0;
    }
    // NOT `div_euclid`: that floors the *remainder* to non-negative, which is a
    // different function. `-4 // -5` is `0` in CPython and `1` under euclid.
    let quotient = numerator / denominator;
    let remainder = numerator % denominator;
    if remainder != 0 && ((remainder < 0) != (denominator < 0)) {
        quotient - 1
    } else {
        quotient
    }
}

/// A query parameter that will not coerce is FastAPI's `422`, before the
/// handler's own `try` — same shape [`super::projects`] uses (DIV-053).
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

    fn params(q: &str, page: i64, per_page: i64) -> SearchParams {
        SearchParams {
            q: q.to_owned(),
            project: None,
            date_from: None,
            date_to: None,
            model: None,
            role: None,
            page,
            per_page,
        }
    }

    fn state_at(dir: &std::path::Path) -> AppState {
        AppState::new(
            dir.join("store.db"),
            std::path::PathBuf::from("/nonexistent/pkg"),
            crate::state::Config::default(),
        )
    }

    struct Scratch(std::path::PathBuf);

    impl Scratch {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "stax-search-{tag}-{}-{}",
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

    /// The schema `SearchService._ensure_schema` writes, with two rows.
    fn seed_index(path: &std::path::Path) {
        let conn = Connection::open(path).expect("open");
        conn.execute_batch(
            "CREATE TABLE messages (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 session_id TEXT NOT NULL, project TEXT NOT NULL, role TEXT NOT NULL,
                 content TEXT NOT NULL, timestamp TEXT, model TEXT,
                 tokens_input INTEGER DEFAULT 0, tokens_output INTEGER DEFAULT 0);
             CREATE VIRTUAL TABLE messages_fts USING fts5(
                 content, content='messages', content_rowid='id',
                 tokenize='porter unicode61');
             CREATE TRIGGER messages_ai AFTER INSERT ON messages BEGIN
                 INSERT INTO messages_fts(rowid, content) VALUES (new.id, new.content);
             END;
             CREATE TABLE index_metadata (
                 project TEXT PRIMARY KEY, indexed_at TEXT NOT NULL,
                 message_count INTEGER DEFAULT 0);
             INSERT INTO messages (session_id, project, role, content, timestamp, model,
                                   tokens_input, tokens_output)
             VALUES ('s1', '-p-one', 'user', 'the quick brown fox', '2026-01-01T00:00:00+00:00',
                     'claude-opus-4', 10, 20),
                    ('s2', '-p-two', 'assistant', 'a quick reply about foxes',
                     '2026-02-01T00:00:00+00:00', 'claude-sonnet-4', 30, 40);
             INSERT INTO index_metadata (project, indexed_at, message_count)
             VALUES ('-p-two', '2026-02-01T00:00:00+00:00', 1),
                    ('-p-one', '2026-01-01T00:00:00+00:00', 1);",
        )
        .expect("seed");
    }

    #[test]
    fn the_bundled_engine_has_fts5_and_the_index_answers() {
        let scratch = Scratch::new("hit");
        let state = state_at(&scratch.0);
        seed_index(&index_path(&state));
        let payload = run_search(&state, &params("quick", 1, 20));
        assert_eq!(payload["total"], serde_json::json!(2));
        assert_eq!(payload["total_pages"], serde_json::json!(1));
        assert_eq!(payload["query"], serde_json::json!("quick"));
        let results = payload["results"].as_array().expect("array");
        assert_eq!(results.len(), 2);
        // `snippet()` marks the matched term; `rank` is bm25, so negative.
        assert!(
            results[0]["snippet"]
                .as_str()
                .is_some_and(|s| s.contains("<mark>")),
            "{:?}",
            results[0]["snippet"]
        );
        assert!(results[0]["relevance"].as_f64().is_some_and(|r| r < 0.0));
        // The response key order is the dict literal's.
        let keys: Vec<&String> = payload.as_object().expect("object").keys().collect();
        assert_eq!(
            keys,
            vec![
                "results",
                "total",
                "page",
                "per_page",
                "total_pages",
                "query"
            ]
        );
    }

    #[test]
    fn the_project_filter_narrows_and_pagination_clamps_to_the_last_page() {
        let scratch = Scratch::new("filter");
        let state = state_at(&scratch.0);
        seed_index(&index_path(&state));

        let mut narrowed = params("quick", 1, 20);
        narrowed.project = Some("-p-one".to_owned());
        assert_eq!(run_search(&state, &narrowed)["total"], serde_json::json!(1));

        // Two hits at one per page is two pages; page 99 clamps to 2, and the
        // ECHOED page is the clamped one.
        let payload = run_search(&state, &params("quick", 99, 1));
        assert_eq!(payload["total_pages"], serde_json::json!(2));
        assert_eq!(payload["page"], serde_json::json!(2));
        assert_eq!(payload["results"].as_array().map(Vec::len), Some(1));

        // …but a floor-clamp only, at the other end.
        let payload = run_search(&state, &params("quick", -7, 1));
        assert_eq!(payload["page"], serde_json::json!(1));
    }

    #[test]
    fn an_empty_query_never_opens_the_index_and_echoes_the_raw_page() {
        let scratch = Scratch::new("empty");
        let state = state_at(&scratch.0);
        // No index file at all — the early return must not care.
        let payload = run_search(&state, &params("   ", -3, 5));
        assert_eq!(
            stax_memory::pyjson::dumps_http(&payload),
            r#"{"results":[],"total":0,"page":-3,"per_page":5,"total_pages":0,"query":"   "}"#
        );
    }

    #[test]
    fn a_missing_sidecar_reads_as_an_empty_index_not_a_500() {
        // DIV-077: Python's constructor would have created the file; the bytes
        // it then answers with are these.
        let scratch = Scratch::new("absent");
        let state = state_at(&scratch.0);
        assert_eq!(
            stax_memory::pyjson::dumps_http(&run_search(&state, &params("fox", 1, 20))),
            r#"{"results":[],"total":0,"page":1,"per_page":20,"total_pages":0,"query":"fox"}"#
        );
        assert_eq!(
            stax_memory::pyjson::dumps_http(&index_stats(&state)),
            r#"{"total_messages":0,"total_projects":0,"models":[],"indexed_projects":[]}"#
        );
    }

    #[test]
    fn stats_sort_indexed_projects_by_name_and_append_them_last() {
        let scratch = Scratch::new("stats");
        let state = state_at(&scratch.0);
        seed_index(&index_path(&state));
        let payload = index_stats(&state);
        assert_eq!(payload["total_messages"], serde_json::json!(2));
        assert_eq!(payload["total_projects"], serde_json::json!(2));
        // `ORDER BY project` — inserted two-then-one, returned one-then-two.
        assert_eq!(
            payload["indexed_projects"][0]["project"],
            serde_json::json!("-p-one")
        );
        let keys: Vec<&String> = payload.as_object().expect("object").keys().collect();
        assert_eq!(
            keys,
            vec![
                "total_messages",
                "total_projects",
                "models",
                "indexed_projects"
            ]
        );
    }

    #[test]
    fn floor_division_follows_cpython_on_a_negative_divisor() {
        assert_eq!(floor_div(21, 20), 1);
        assert_eq!(floor_div(20, 20), 1);
        // CPython: (2 + -5 - 1) // -5 == -4 // -5 == 0. Rust's `/` gives 0 too
        // here, but -6 // -5 is 1 in Python and 1 in Rust; -6 // 5 is -2 in
        // Python and -1 with `/`. The euclidean form is the one that matches.
        assert_eq!(floor_div(-6, 5), -2);
        assert_eq!(floor_div(-4, -5), 0);
    }

    #[test]
    fn operators_are_neutralised_into_literal_terms() {
        // Shared with the CLI's half of this index, so the two agree by
        // construction rather than by two copies staying in sync.
        assert_eq!(
            sanitize_fts_query("use NOT null"),
            r#""use"* "NOT"* "null"*"#
        );
        assert_eq!(sanitize_fts_query("!!!"), r#""""#);
    }
}
