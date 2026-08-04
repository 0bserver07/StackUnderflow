//! `routes/search.py` — 3 endpoints, wave 6.
//!
//! | Item | Method | FastAPI path | axum path | State |
//! |---|---|---|---|---|
//! | `RS-6-016` | `GET` | `/api/search` | `/api/search` | **ported** |
//! | `RS-6-017` | `POST` | `/api/search/reindex` | `/api/search/reindex` | **ported** — see `SEARCH-REINDEX-DIFFER.md` |
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
//! # `POST /api/search/reindex` — ported, and it has no case row. Ever.
//!
//! DIV-078 filed this as deferred; it is now written. What has *not* changed is
//! the ruling that produced DIV-078: **no row for it in
//! `parity/endpoint-cases.txt`, not even a `!` one.** `!` suppresses the
//! verdict, never the request (DIV-059), and this handler `DELETE`s and rebuilds
//! every row of `search_index.db` under `$STACKUNDERFLOW_HOME` — the home the
//! two harness servers *share*. One row here silently rewrites the answers of
//! every `X-*` case after it. And even a safe home could not host it: the
//! rebuild is idempotent, so whichever side ran first would consume the work.
//!
//! It is proven instead by `rust/SEARCH-REINDEX-DIFFER.md` — two throwaway
//! homes, two ports, an artefact diff of the rebuilt index, run twice.
//!
//! # What the writer is, in one paragraph
//!
//! `reindex_all` re-reads `queries.list_projects`, groups the rows by **slug**
//! (`UNIQUE(provider, slug)` lets one slug carry a claude row *and* a codex row,
//! and `index_project` `DELETE`s by slug — so a per-row loop would let the
//! second wipe the first), concatenates every row's `get_project_stats`
//! messages, and hands the merged list to `index_project`, which clears the
//! slug and re-inserts one row per message with non-blank content. The FTS5
//! index is maintained by the table's own triggers, so the writer never touches
//! `messages_fts` by name.
//!
//! Two fields in what it leaves behind are wall-clock and can never match:
//! `elapsed_ms` in the response, and `index_metadata.indexed_at` in the
//! artefact. Both are compared for shape, per the differ.

use std::collections::HashMap;

use axum::Router;
use axum::extract::{RawQuery, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use rusqlite::{Connection, OpenFlags};
use serde_json::{Map, Value};
use stax_core::ask::{build_filter_clauses, sanitize_fts_query};
use stax_etl::stats::aggregator::round_py;

use crate::json::{JsonBody, bound_422, validation_422};
use crate::pyops::{char_prefix, floor_div, sql_value};
use crate::qs::Query;
use crate::state::AppState;

/// `per_page = min(per_page, 100)` — the route's clamp, before the service.
const MAX_PER_PAGE: i64 = 100;

/// `per_page: int = Query(20, ge=1)` — the floor is a DECLARED bound, so
/// FastAPI refuses a bad value before the handler body runs (DIV-079).
///
/// The asymmetry is the reference's: the ceiling is a silent clamp, the floor
/// is a 422. `per_page` is a divisor — `(total + per_page - 1) // per_page` —
/// and `?per_page=0` used to raise `ZeroDivisionError` into the handler's
/// blanket `except` as a **500**, while a negative reached SQLite as a
/// negative `LIMIT`, which SQLite reads as no limit at all.
pub(super) const MIN_PER_PAGE: i64 = 1;

/// `row["content"][:500]` — a CPython `str` slice, so **code points**.
const CONTENT_CHARS: usize = 500;

/// pydantic's `ge=1` failure for `per_page`, shared with `/api/qa` — the two
/// routes declare the same bound, so they answer the same bytes.
pub(super) fn per_page_floor_422(raw_input: &str) -> JsonBody {
    bound_422(
        "per_page",
        "greater_than_equal",
        "Input should be greater than or equal to 1",
        raw_input,
        "ge",
        MIN_PER_PAGE,
    )
}

/// Mount this module's endpoints onto `router`.
pub fn register(router: Router<AppState>) -> Router<AppState> {
    router
        .route("/api/search", get(search_messages))
        .route("/api/search/reindex", post(reindex_search))
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
        // clamped one, including on the empty-query early return. The floor
        // below it is pydantic's, and fires first.
        per_page: match query.int_or("per_page", 20) {
            Ok(value) if value < MIN_PER_PAGE => {
                return per_page_floor_422(query.get("per_page").unwrap_or_default());
            }
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

// ── POST /api/search/reindex ─────────────────────────────────────────────────

/// `reindex_search` — the route, its clock, and its one error leg.
///
/// `start_time = time.time()` is taken **before** the `try`, so the store
/// connect is inside the measurement. `elapsed_ms` is `round(ms, 2)` and is
/// assigned onto the service's dict *after* it returns, so it is the LAST key —
/// after `projects_indexed`, `total_messages_indexed`, `errors`.
async fn reindex_search(State(state): State<AppState>) -> JsonBody {
    let start = std::time::Instant::now();
    let outcome = tokio::task::spawn_blocking(move || reindex_all(&state)).await;
    // `time.time() - start_time` is read after the work, before the response is
    // built, on both legs of the `try` — but only the success leg uses it.
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
        // `except Exception as e: return {"error": f"Reindex failed: {str(e)}"}`
        // with a 500. The message body embeds a CPython exception string, which
        // this port can match in shape but not in wording — the DIV-137 case,
        // recorded in `parity/DIV-e-reindex.md`.
        Ok(Err(err)) => reindex_failure(&format!("Reindex failed: {}", py_error_text(&err))),
        Err(err) => reindex_failure(&format!("Reindex failed: {err}")),
    }
}

/// `str(e)` for an exception that escaped the service — the DIV-137 shape.
///
/// All three reindex routes end `f"…: {str(e)}"`, so the body embeds whatever
/// CPython's exception stringifies to. Three renderings of the *same* SQLite
/// failure were measured on one probe (the store made unreadable while both
/// servers were up):
///
/// ```text
/// CPython  sqlite3.OperationalError   unable to open database file
/// anyhow   Display (outermost)        opening /…/store.db
/// anyhow   root_cause() Display       Error code 14: unable to open database file
/// ```
///
/// Neither `anyhow` form matches, and the third is the *worse* of the two on a
/// different error: `rusqlite::ffi::Error`'s `Display` is a static description
/// per result code, so `no such table: x` renders as `SQL logic error` and the
/// specific message is lost. That message is not lost from the *value* though —
/// `rusqlite::Error::SqliteFailure(_, Some(msg))` carries `sqlite3_errmsg`'s
/// text, which is byte-for-byte the string CPython puts in its exception. So
/// this walks the chain for a `rusqlite::Error` and takes that field.
///
/// A *narrowing, not a proof*: a failure whose innermost error is a Rust-side
/// condition with no CPython counterpart still cannot match, and no probe has
/// issued one (law 6). Recorded in `parity/DIV-e-reindex.md`.
pub(super) fn py_error_text(err: &anyhow::Error) -> String {
    for cause in err.chain() {
        if let Some(rusqlite::Error::SqliteFailure(_, Some(message))) =
            cause.downcast_ref::<rusqlite::Error>()
        {
            return message.clone();
        }
    }
    err.root_cause().to_string()
}

/// The route's 500: `{"error": …}`.
fn reindex_failure(message: &str) -> JsonBody {
    let mut obj = Map::new();
    obj.insert("error".to_owned(), Value::from(message));
    JsonBody::with_status(StatusCode::INTERNAL_SERVER_ERROR, Value::Object(obj))
}

/// `SearchService._get_conn` + `_ensure_schema`, on a WRITE handle.
///
/// The read path deliberately never creates this file (DIV-077); the writer has
/// to, because Python creates it in `SearchService.__init__` at server startup
/// and a reindex on a home that has never had one is a legitimate first run.
/// The narrowing that remains — Python's file exists from startup, this one
/// from the first reindex — is DIV-077's, unchanged: an absent index and an
/// empty one answer the same bytes.
///
/// The two pragmas are not decoration. `journal_mode=WAL` is written into the
/// database header, so it is part of the artefact the differ compares.
/// `SearchService._ensure_schema`, statement for statement.
///
/// The indentation inside these literals is not a style choice and rustfmt must
/// not be allowed to think it is: SQLite stores the **verbatim text** of a
/// `CREATE` statement in `sqlite_master.sql`, so `.schema search_index.db` on a
/// port-built index would differ from a reference-built one by nothing but
/// whitespace — a real byte divergence in the artefact the differ compares.
/// These are the reference's strings, character for character, with the
/// `IF NOT EXISTS` that SQLite strips before storing put back.
const SEARCH_SCHEMA: [&str; 10] = [
    r#"CREATE TABLE IF NOT EXISTS messages (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    session_id TEXT NOT NULL,
                    project TEXT NOT NULL,
                    role TEXT NOT NULL,
                    content TEXT NOT NULL,
                    timestamp TEXT,
                    model TEXT,
                    tokens_input INTEGER DEFAULT 0,
                    tokens_output INTEGER DEFAULT 0
                )"#,
    r#"CREATE VIRTUAL TABLE IF NOT EXISTS messages_fts USING fts5(
                    content,
                    content='messages',
                    content_rowid='id',
                    tokenize='porter unicode61'
                )"#,
    r#"CREATE TRIGGER IF NOT EXISTS messages_ai AFTER INSERT ON messages BEGIN
                    INSERT INTO messages_fts(rowid, content) VALUES (new.id, new.content);
                END"#,
    r#"CREATE TRIGGER IF NOT EXISTS messages_ad AFTER DELETE ON messages BEGIN
                    INSERT INTO messages_fts(messages_fts, rowid, content) VALUES('delete', old.id, old.content);
                END"#,
    r#"CREATE TRIGGER IF NOT EXISTS messages_au AFTER UPDATE ON messages BEGIN
                    INSERT INTO messages_fts(messages_fts, rowid, content) VALUES('delete', old.id, old.content);
                    INSERT INTO messages_fts(rowid, content) VALUES (new.id, new.content);
                END"#,
    r#"CREATE TABLE IF NOT EXISTS index_metadata (
                    project TEXT PRIMARY KEY,
                    indexed_at TEXT NOT NULL,
                    message_count INTEGER DEFAULT 0
                )"#,
    r#"CREATE INDEX IF NOT EXISTS idx_messages_project ON messages(project)"#,
    r#"CREATE INDEX IF NOT EXISTS idx_messages_timestamp ON messages(timestamp)"#,
    r#"CREATE INDEX IF NOT EXISTS idx_messages_role ON messages(role)"#,
    r#"CREATE INDEX IF NOT EXISTS idx_messages_model ON messages(model)"#,
];

fn open_index_for_write(state: &AppState) -> rusqlite::Result<Connection> {
    let conn = Connection::open(index_path(state))?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    for statement in SEARCH_SCHEMA {
        // One `conn.execute` per statement, as the reference does, and NOT one
        // `execute_batch` of the lot: the batch would still work, but keeping
        // the calls separate is what keeps the stored text one statement wide.
        conn.execute(statement, [])?;
    }
    Ok(conn)
}

/// `msg.get(key, "")` for a value the formatter always writes as a `str`.
pub(super) fn msg_text<'a>(msg: &'a Value, key: &str) -> &'a str {
    msg.get(key).and_then(Value::as_str).unwrap_or_default()
}

/// CPython's `str.strip()`, which is not `str::trim`.
///
/// `str.strip()` removes every character `str.isspace()` accepts, and that set
/// includes `U+001C`..`U+001F` — which Rust's `char::is_whitespace` does not.
/// `stax_core`'s `is_regex_space` is already the owner of exactly that
/// predicate (it is also what Python's `\s` matches), so this is a two-line
/// adapter rather than a fourth definition.
pub(super) fn py_strip(text: &str) -> &str {
    text.trim_matches(stax_core::queries::pyint::is_regex_space)
}

/// `msg.get("model", "")` — which is NOT the same as [`msg_text`].
///
/// `dict.get(key, default)` returns the *stored* value when the key is present,
/// so a message whose `model` is `None` binds SQL NULL, while a message with no
/// `model` key at all binds `''`. The formatter emits the key with a null for
/// every user turn, so this is the common path, not a corner.
fn msg_nullable(msg: &Value, key: &str) -> Option<String> {
    match msg.get(key) {
        None => Some(String::new()),
        Some(Value::String(text)) => Some(text.clone()),
        Some(Value::Null) => None,
        Some(other) => Some(other.to_string()),
    }
}

/// `msg.get("tokens", {}).get(key, 0)`.
fn msg_tokens(msg: &Value, key: &str) -> i64 {
    msg.get("tokens")
        .and_then(|tokens| tokens.get(key))
        .and_then(Value::as_i64)
        .unwrap_or(0)
}

/// `SearchService.index_project` — clear the slug, re-insert, stamp metadata.
///
/// Returns the number of rows actually inserted, which is **not** what the
/// response reports: `reindex_all` adds `len(merged)`, the message count
/// *before* the blank-content skip below. On this corpus that gap is large and
/// legitimate (architect finding 1: `content_text` is ~86% empty on
/// agent-heavy sessions), so a `total_messages_indexed` far above the row count
/// is the reference's answer, not a bug.
fn index_project(conn: &Connection, project: &str, messages: &[Value]) -> rusqlite::Result<i64> {
    // Python runs the whole method on one connection and commits at the end,
    // rolling back on any error. An explicit transaction is that, exactly.
    conn.execute_batch("BEGIN")?;
    let result = index_project_body(conn, project, messages);
    match &result {
        Ok(_) => conn.execute_batch("COMMIT")?,
        Err(_) => conn.execute_batch("ROLLBACK")?,
    }
    result
}

fn index_project_body(
    conn: &Connection,
    project: &str,
    messages: &[Value],
) -> rusqlite::Result<i64> {
    conn.execute("DELETE FROM messages WHERE project = ?", [project])?;

    let mut count = 0i64;
    {
        let mut insert = conn.prepare(
            "INSERT INTO messages \
             (session_id, project, role, content, timestamp, model, tokens_input, tokens_output) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )?;
        for msg in messages {
            let content = msg_text(msg, "content");
            // `if not content or not content.strip(): continue`
            if py_strip(content).is_empty() {
                continue;
            }
            insert.execute(rusqlite::params![
                msg_text(msg, "session_id"),
                project,
                // `msg.get("type", "unknown")` — the ONE default here that is
                // not the empty string.
                msg.get("type").and_then(Value::as_str).unwrap_or("unknown"),
                content,
                msg_nullable(msg, "timestamp"),
                msg_nullable(msg, "model"),
                msg_tokens(msg, "input"),
                msg_tokens(msg, "output"),
            ])?;
            count += 1;
        }
    }

    conn.execute(
        "INSERT OR REPLACE INTO index_metadata (project, indexed_at, message_count) \
         VALUES (?, ?, ?)",
        rusqlite::params![project, now_iso(), count],
    )?;
    Ok(count)
}

/// `datetime.now(UTC).isoformat()` — the field the differ can never equality-check.
pub(super) fn now_iso() -> String {
    stax_core::queries::pytime::isoformat_utc(stax_core::queries::pytime::now_micros())
}

/// Slug → every `projects.id` carrying it, in `list_projects` order.
///
/// `defaultdict(list)` keyed by slug: **insertion order is the response's
/// error order and the index-write order**, and `list_projects` is
/// `ORDER BY last_modified DESC`, so a `HashMap` iteration here would randomise
/// both. The `Vec<String>` alongside is that order.
pub(super) fn group_by_slug(
    rows: &[stax_core::api::ProjectRow],
    wanted: Option<&[String]>,
) -> Vec<(String, Vec<i64>)> {
    let mut order: Vec<String> = Vec::new();
    let mut groups: HashMap<String, Vec<i64>> = HashMap::new();
    for row in rows {
        if let Some(wanted) = wanted
            && !wanted.contains(&row.slug)
        {
            continue;
        }
        if !groups.contains_key(&row.slug) {
            order.push(row.slug.clone());
        }
        groups.entry(row.slug.clone()).or_default().push(row.id);
    }
    order
        .into_iter()
        .map(|slug| {
            let ids = groups.remove(&slug).unwrap_or_default();
            (slug, ids)
        })
        .collect()
}

/// `SearchService.reindex_all(None, None, projects=projects)`.
///
/// The route builds `projects` from `queries.list_projects` and the service
/// then re-reads the same table and filters to those slugs — an identity
/// filter, reproduced because it is not identity when the caller passes a
/// narrower list (the ingest path does).
fn reindex_all(state: &AppState) -> anyhow::Result<Value> {
    // The route's own connection, opened and closed before the service's.
    let wanted: Vec<String> = {
        let conn = state.connect()?;
        stax_core::api::store_list_projects(&conn)?
            .into_iter()
            .map(|row| row.slug)
            .collect()
    };

    let conn = state.connect()?;
    let rows = stax_core::api::store_list_projects(&conn)?;
    // `if projects` — an EMPTY project list is falsy, so it means "no filter",
    // not "filter to nothing". On an empty store both readings give the same
    // (empty) answer; on a caller that passed `[]` deliberately they do not.
    let groups = group_by_slug(&rows, (!wanted.is_empty()).then_some(wanted.as_slice()));

    let index = open_index_for_write(state)?;
    let mut total_messages = 0i64;
    let mut projects_indexed = 0i64;
    let mut errors: Vec<Value> = Vec::new();

    for (slug, ids) in groups {
        match merged_messages(&conn, &ids) {
            Ok(merged) if merged.is_empty() => {}
            Ok(merged) => match index_project(&index, &slug, &merged) {
                Ok(_) => {
                    total_messages += i64::try_from(merged.len()).unwrap_or(i64::MAX);
                    projects_indexed += 1;
                }
                Err(err) => errors.push(index_error(&slug, &err.to_string())),
            },
            Err(err) => errors.push(index_error(&slug, &py_error_text(&err))),
        }
    }

    let mut obj = Map::new();
    obj.insert("projects_indexed".to_owned(), Value::from(projects_indexed));
    obj.insert(
        "total_messages_indexed".to_owned(),
        Value::from(total_messages),
    );
    obj.insert("errors".to_owned(), Value::Array(errors));
    Ok(Value::Object(obj))
}

/// `{"project": slug, "error": str(e)}`.
pub(super) fn index_error(slug: &str, message: &str) -> Value {
    let mut obj = Map::new();
    obj.insert("project".to_owned(), Value::from(slug));
    obj.insert("error".to_owned(), Value::from(message));
    Value::Object(obj)
}

/// `for pid in ids: msgs, _ = get_project_stats(conn, project_id=pid); merged.extend(msgs)`.
///
/// One call **per project id**, concatenated — not one call with the id list.
/// The two are not the same: `build_enriched_dataset` dedups and sorts within a
/// call, so a single multi-id call would interleave two providers' messages
/// where Python appends one provider's block after the other's.
pub(super) fn merged_messages(conn: &Connection, ids: &[i64]) -> anyhow::Result<Vec<Value>> {
    let mut merged: Vec<Value> = Vec::new();
    for id in ids {
        let (messages, _stats) = stax_etl::stats::dataset::get_project_stats(conn, &[*id], 0)?;
        merged.extend(messages);
    }
    Ok(merged)
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

    /// DIV-079 — the declared `ge=1` floor, on both routes that share it.
    ///
    /// The bytes are pydantic's for a constrained int, measured against
    /// fastapi 0.141.1 / pydantic 2.13.4 (the venv `endpoint-parity.sh`
    /// boots) with `per_page: int = Query(20, ge=1)` — not transcribed. `ctx`
    /// is present and last; `input` echoes the RAW query string.
    #[test]
    fn the_per_page_floor_is_pydantics_bound_error() {
        let zero = per_page_floor_422("0");
        assert_eq!(
            axum::response::IntoResponse::into_response(per_page_floor_422("0")).status(),
            StatusCode::UNPROCESSABLE_ENTITY
        );
        assert_eq!(
            zero.render(),
            r#"{"detail":[{"type":"greater_than_equal","loc":["query","per_page"],"msg":"Input should be greater than or equal to 1","input":"0","ctx":{"ge":1}}]}"#
        );
        assert_eq!(
            per_page_floor_422("-5").render(),
            r#"{"detail":[{"type":"greater_than_equal","loc":["query","per_page"],"msg":"Input should be greater than or equal to 1","input":"-5","ctx":{"ge":1}}]}"#
        );
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

    // ── POST /api/search/reindex ────────────────────────────────────────────

    fn project_row(id: i64, provider: &str, slug: &str) -> stax_core::api::ProjectRow {
        stax_core::api::ProjectRow {
            id,
            provider: provider.to_owned(),
            slug: slug.to_owned(),
            path: None,
            display_name: slug.to_owned(),
            first_seen: 0.0,
            last_modified: 0.0,
        }
    }

    #[test]
    fn one_slug_across_two_providers_is_one_group_carrying_both_ids() {
        // The rule `index_project` forces: it DELETEs by slug, so a per-row
        // loop would let the codex pass wipe the claude pass. Order is
        // `list_projects`' (last_modified DESC), not sorted.
        let rows = [
            project_row(9, "claude", "-zeta"),
            project_row(1, "claude", "-alpha"),
            project_row(2, "codex", "-alpha"),
        ];
        let groups = group_by_slug(&rows, None);
        assert_eq!(
            groups,
            vec![
                ("-zeta".to_owned(), vec![9]),
                ("-alpha".to_owned(), vec![1, 2]),
            ]
        );
        // …and the caller's slug list narrows it.
        let wanted = ["-alpha".to_owned()];
        assert_eq!(
            group_by_slug(&rows, Some(&wanted)),
            vec![("-alpha".to_owned(), vec![1, 2])]
        );
    }

    #[test]
    fn the_writer_indexes_only_non_blank_content_and_stamps_the_metadata() {
        let scratch = Scratch::new("write");
        let state = state_at(&scratch.0);
        let conn = open_index_for_write(&state).expect("schema");
        let messages: Vec<Value> = serde_json::json!([
            {"session_id": "s1", "type": "user", "timestamp": "2026-01-01T00:00:00",
             "model": null, "content": "the quick brown fox", "tokens": {"input": 3, "output": 4}},
            {"session_id": "s1", "type": "assistant", "timestamp": "2026-01-01T00:00:01",
             "model": "claude-opus-4-8", "content": "   ", "tokens": {"input": 1, "output": 1}},
            {"session_id": "s2", "timestamp": "2026-01-01T00:00:02",
             "content": "a second message", "tokens": {}}
        ])
        .as_array()
        .expect("array")
        .clone();

        // Two of the three rows: the whitespace-only one is skipped, and the
        // RETURNED count is the inserted count.
        assert_eq!(index_project(&conn, "-p-one", &messages).expect("write"), 2);

        let (role, model, tokens_in): (String, Option<String>, i64) = conn
            .query_row(
                "SELECT role, model, tokens_input FROM messages WHERE session_id = 's1'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("row");
        // `msg.get("type", "unknown")` on a message that HAS the key…
        assert_eq!(role, "user");
        // …and `msg.get("model", "")` on one whose value is None: SQL NULL,
        // not the empty string.
        assert_eq!(model, None);
        assert_eq!(tokens_in, 3);

        // The third message has no `type` key at all, so it takes the default.
        let missing: String = conn
            .query_row(
                "SELECT role FROM messages WHERE session_id = 's2'",
                [],
                |row| row.get(0),
            )
            .expect("row");
        assert_eq!(missing, "unknown");
        // …and no `model` key at all, which is `''`, not NULL.
        let empty: Option<String> = conn
            .query_row(
                "SELECT model FROM messages WHERE session_id = 's2'",
                [],
                |row| row.get(0),
            )
            .expect("row");
        assert_eq!(empty.as_deref(), Some(""));

        let (project, count): (String, i64) = conn
            .query_row(
                "SELECT project, message_count FROM index_metadata",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("metadata");
        assert_eq!((project.as_str(), count), ("-p-one", 2));
    }

    #[test]
    fn a_second_pass_replaces_rather_than_doubles_and_the_fts_follows() {
        let scratch = Scratch::new("idem");
        let state = state_at(&scratch.0);
        let conn = open_index_for_write(&state).expect("schema");
        let messages: Vec<Value> = serde_json::json!([
            {"session_id": "s1", "type": "user", "timestamp": "t", "model": "m",
             "content": "the quick brown fox", "tokens": {"input": 1, "output": 2}}
        ])
        .as_array()
        .expect("array")
        .clone();

        for _ in 0..3 {
            index_project(&conn, "-p-one", &messages).expect("write");
        }
        let rows: i64 = conn
            .query_row("SELECT COUNT(*) FROM messages", [], |row| row.get(0))
            .expect("count");
        assert_eq!(rows, 1, "DELETE-then-INSERT, three times over");
        // The FTS index is trigger-maintained; a missing delete trigger shows
        // up here as three hits for one row.
        let hits: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM messages_fts WHERE messages_fts MATCH 'quick'",
                [],
                |row| row.get(0),
            )
            .expect("fts");
        assert_eq!(hits, 1);
        // And the schema is `CREATE … IF NOT EXISTS`, so re-opening a populated
        // index is not an error and does not clear it.
        drop(conn);
        let conn = open_index_for_write(&state).expect("reopen");
        let rows: i64 = conn
            .query_row("SELECT COUNT(*) FROM messages", [], |row| row.get(0))
            .expect("count");
        assert_eq!(rows, 1);
    }

    #[test]
    fn py_strip_removes_the_four_separators_rust_leaves_behind() {
        // `str.strip()` accepts every character `str.isspace()` does, and that
        // includes U+001C..U+001F. `str::trim` does not, so a message whose
        // content is only a file separator would have been indexed as content.
        assert_eq!(py_strip("\u{1c}\u{1f} x \u{1e}"), "x");
        assert!(py_strip("\u{1c}\u{1d}\u{1e}\u{1f}").is_empty());
        assert!(!"\u{1c}".trim().is_empty(), "…which trim would have kept");
    }

    #[test]
    fn the_error_text_is_sqlites_message_not_anyhows_context_chain() {
        // Measured, three ways, on the same failure:
        //   CPython  `str(e)`               → "no such table: nope"
        //   anyhow   `{err}`                → "while listing" (the context)
        //   anyhow   `root_cause()`         → "SQL logic error" (the STATIC
        //                                      per-code description — the
        //                                      specific message is gone)
        // Only the `SqliteFailure` message field carries `sqlite3_errmsg`'s
        // text, which is the string CPython embeds.
        let conn = Connection::open_in_memory().expect("memory db");
        let raw = conn
            .prepare("SELECT * FROM nope")
            .expect_err("no such table");
        let wrapped = anyhow::Error::new(raw).context("while listing");
        assert_eq!(py_error_text(&wrapped), "no such table: nope");
        assert_eq!(
            wrapped.to_string(),
            "while listing",
            "…which is what a bare {{err}} would have shipped"
        );
    }

    #[test]
    fn an_error_with_no_sqlite_cause_falls_back_to_the_root() {
        let wrapped = anyhow::anyhow!("the inner thing").context("the outer thing");
        assert_eq!(py_error_text(&wrapped), "the inner thing");
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
