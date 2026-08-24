//! Continue IDE — the port of `python-legacy: adapters/continue_adapter.py`.
//!
//! The module is `continue_ext` because `continue` is a Rust keyword; the
//! provider key on the wire is still `"continue"`, and nothing outside this file
//! sees the difference.
//!
//! ## Schema discovery, not a schema
//!
//! Continue's on-disk schema is undocumented and the maintainer's own install
//! reports an empty sessions file, so this adapter is **discovery-first and
//! defensive everywhere**:
//!
//! 1. walk `~/.continue/` for `*.db` / `*.sqlite` / `*.sqlite3`;
//! 2. for each, sniff a table that plausibly holds sessions — one whose *name*
//!    contains `session`, or failing that one carrying an id column plus a
//!    title-shaped and a timestamp-shaped column — and remember the first
//!    message-shaped sibling;
//! 3. `enumerate()` yields one ref per sessions row, with the resolved table
//!    names in `source_hint` so `read()` never re-introspects;
//! 4. `read()` reads the messages table, per-row defensively, with the `rowid`
//!    as `seq`.
//!
//! Tokens and model are best-effort: explicit `input_tokens` / `output_tokens`
//! columns win, otherwise the text length is divided by four, the record is
//! stamped `raw["cost_source"] = "estimated"`, and the model falls back to
//! `continue-auto`.
//!
//! A missing root, a DB with no sessions-shaped table, or a sessions table with
//! no message sibling all yield nothing. That is the correct empty state, not a
//! failure — and it is what this machine actually reports today.

use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::base::{Record, SessionRef, SourceAdapter, SourceKind, Speed, stat_ref_fields};
use crate::pytime::{self, Clock};
use crate::{pyval, sqlite, walk};

/// The provider key — the spelling every store row and CLI argument uses.
pub const NAME: &str = "continue";

/// The model stamped when a row declares none.
pub const DEFAULT_MODEL: &str = "continue-auto";

/// Suffixes treated as candidate SQLite databases (`_DB_SUFFIXES`).
pub const DB_SUFFIXES: [&str; 3] = [".db", ".sqlite", ".sqlite3"];

/// Timestamp-shaped column names on a sessions table (`_SESSION_TIMESTAMP_COLUMNS`).
pub const SESSION_TIMESTAMP_COLUMNS: [&str; 6] = [
    "createdat",
    "created_at",
    "updatedat",
    "updated_at",
    "timestamp",
    "ts",
];

/// Title-shaped column names on a sessions table (`_SESSION_TITLE_COLUMNS`).
pub const SESSION_TITLE_COLUMNS: [&str; 2] = ["title", "name"];

/// Timestamp columns read off a message row, in priority order
/// (`_MESSAGE_TIMESTAMP_COLUMNS`).
pub const MESSAGE_TIMESTAMP_COLUMNS: [&str; 4] = ["createdAt", "created_at", "timestamp", "ts"];

/// The Continue source adapter (`ContinueAdapter`).
#[derive(Debug, Clone)]
pub struct ContinueAdapter {
    root: PathBuf,
    clock: Clock,
}

impl Default for ContinueAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl ContinueAdapter {
    /// `~/.continue`, from the live environment.
    #[must_use]
    pub fn new() -> Self {
        #[allow(
            deprecated,
            reason = "std::env::home_dir is the platform-correct answer on the \
            1.97.1 pin; the claude and codex adapters carry the same allow"
        )]
        let home = std::env::home_dir().unwrap_or_default();
        Self {
            root: home.join(".continue"),
            clock: Clock::Live,
        }
    }

    /// Inject the root — `ContinueAdapter(root=…)`.
    #[must_use]
    pub fn with_root(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            clock: Clock::Live,
        }
    }

    /// Pin the clock behind the `datetime.now(tz=UTC)` timestamp fallback.
    #[must_use]
    pub fn with_clock(mut self, clock: Clock) -> Self {
        self.clock = clock;
        self
    }

    /// The root this adapter walks.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }
}

impl SourceAdapter for ContinueAdapter {
    fn name(&self) -> &str {
        NAME
    }

    fn enumerate(&self) -> Vec<SessionRef> {
        if !self.root.is_dir() {
            return Vec::new();
        }
        let mut out = Vec::new();
        for db_path in walk_db_files(&self.root) {
            // LOG: python warns "Cannot open Continue DB %s".
            let Some(conn) = sqlite::open_readonly(&db_path) else {
                continue;
            };
            // LOG: python warns "Cannot introspect Continue DB %s".
            let Some(schema) = sniff_schema(&conn) else {
                continue;
            };
            // LOG: python warns "Cannot stat Continue DB %s".
            let Some((mtime, size)) = stat_ref_fields(&db_path) else {
                continue;
            };
            // LOG: python warns "Continue sessions query failed on %s".
            let Some(rows) = session_rows(&conn, &schema.sessions_table) else {
                continue;
            };
            drop(conn);

            for (rowid, payload) in rows {
                let mut hint = Map::new();
                let session_id = extract_session_id(&payload, rowid);
                hint.insert(
                    "sessions_table".to_string(),
                    Value::from(schema.sessions_table.clone()),
                );
                hint.insert(
                    "messages_table".to_string(),
                    schema
                        .messages_table
                        .clone()
                        .map_or(Value::Null, Value::from),
                );
                hint.insert(
                    "session_row_id".to_string(),
                    Value::from(session_id.clone()),
                );
                out.push(SessionRef {
                    provider: NAME.to_string(),
                    project_slug: NAME.to_string(),
                    session_id,
                    file_path: db_path.clone(),
                    file_mtime: mtime,
                    file_size: size,
                    source_kind: SourceKind::Database,
                    source_hint: Some(hint),
                });
            }
        }
        out
    }

    fn read_into(&self, session: &SessionRef, since_offset: i64, sink: &mut dyn FnMut(Record)) {
        // LOG: python warns "Continue DB missing at read time: %s".
        if !session.file_path.is_file() {
            return;
        }
        // No messages table was discovered: there is nothing to read, and
        // yielding nothing is the correct behaviour rather than raising.
        let Some(messages_table) = session
            .source_hint
            .as_ref()
            .and_then(|hint| hint.get("messages_table"))
            .filter(|value| pyval::py_truthy(value))
            .and_then(Value::as_str)
        else {
            return;
        };
        // LOG: python warns "Cannot open Continue DB %s".
        let Some(conn) = sqlite::open_readonly(&session.file_path) else {
            return;
        };
        // LOG: python warns "Continue messages introspection failed on %s".
        let Some(columns) = column_names(&conn, messages_table) else {
            return;
        };
        let filter_column = pick_session_filter_column(&columns);

        // The table name comes from introspection, never from user input — the
        // Python source carries a `# noqa: S608` at each of these for the same
        // reason. Values stay bound parameters.
        let sql = match &filter_column {
            Some(column) => format!(
                "SELECT rowid, * FROM {messages_table} \
                 WHERE {column} = ? AND rowid > ? ORDER BY rowid"
            ),
            // A schema that exposes no session id at all: read every row.
            None => format!("SELECT rowid, * FROM {messages_table} WHERE rowid > ? ORDER BY rowid"),
        };
        // LOG: python warns "Continue read failed on %s".
        let Ok(mut stmt) = conn.prepare(&sql) else {
            return;
        };
        let query = match &filter_column {
            Some(_) => stmt.query(rusqlite::params![session.session_id, since_offset]),
            None => stmt.query(rusqlite::params![since_offset]),
        };
        let Ok(mut rows) = query else { return };

        while let Ok(Some(row)) = rows.next() {
            let Ok(rowid) = row.get::<_, i64>(0) else {
                break;
            };
            let mut payload = Map::new();
            for (index, column) in columns.iter().enumerate() {
                let Ok(value) = row.get_ref(index + 1) else {
                    break;
                };
                payload.insert(column.clone(), sqlite::value_to_json(value));
            }
            // Python wraps this call in a bare `except Exception` and skips the
            // row; nothing here can throw, so the guard has no Rust analogue.
            if let Some(record) = record_from_message(rowid, &payload, session, self.clock) {
                sink(record);
            }
        }
    }

    /// The `~/.continue` root (`source_roots`). Continue declares no
    /// `watch_paths`.
    fn source_roots(&self) -> Vec<PathBuf> {
        vec![self.root.clone()]
    }
}

/// The tables `enumerate` resolved for one database.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Schema {
    /// The table holding sessions.
    pub sessions_table: String,
    /// The first message-shaped sibling, when one exists.
    pub messages_table: Option<String>,
}

/// Every plausible SQLite file under `root` (`_walk_db_files`).
fn walk_db_files(root: &Path) -> Vec<PathBuf> {
    walk::rglob_all(root)
        .into_iter()
        .filter(|path| path.is_file())
        .filter(|path| {
            let name = walk::dir_name(path).to_lowercase();
            // `Path.suffix` is the last dot-run, and a name that *is* a suffix
            // (`.db`) has no suffix at all in pathlib — hence the rfind guard.
            match name.rfind('.') {
                Some(index) if index > 0 => DB_SUFFIXES.contains(&&name[index..]),
                _ => false,
            }
        })
        .collect()
}

/// `(sessions_table, messages_table)` or `None` on a miss (`_sniff_schema`).
///
/// Conservative on purpose. A sessions table is one whose **name** contains
/// `session`; failing that, one carrying an `id`/`sessionid` column *plus* a
/// title-shaped column *plus* a timestamp-shaped one. The messages table is the
/// first other table whose name contains `message`, `conversation` or `history`.
/// A sessions-only database still enumerates.
#[must_use]
pub fn sniff_schema(conn: &rusqlite::Connection) -> Option<Schema> {
    let table_names = list_tables(conn)?;
    if table_names.is_empty() {
        return None;
    }
    let mut sessions_table = table_names
        .iter()
        .find(|name| name.to_lowercase().contains("session"))
        .cloned();

    if sessions_table.is_none() {
        for name in &table_names {
            let lowered: Vec<String> = column_names(conn, name)
                .unwrap_or_default()
                .iter()
                .map(|column| column.to_lowercase())
                .collect();
            let has_id = lowered.iter().any(|c| c == "id" || c == "sessionid");
            let has_title = SESSION_TITLE_COLUMNS
                .iter()
                .any(|candidate| lowered.iter().any(|c| c == candidate));
            let has_timestamp = SESSION_TIMESTAMP_COLUMNS
                .iter()
                .any(|candidate| lowered.iter().any(|c| c == candidate));
            if has_id && has_title && has_timestamp {
                sessions_table = Some(name.clone());
                break;
            }
        }
    }
    let sessions_table = sessions_table?;

    let messages_table = table_names
        .iter()
        .find(|name| {
            let lowered = name.to_lowercase();
            (lowered.contains("message")
                || lowered.contains("conversation")
                || lowered.contains("history"))
                && **name != sessions_table
        })
        .cloned();
    Some(Schema {
        sessions_table,
        messages_table,
    })
}

/// `SELECT name FROM sqlite_master WHERE type='table' ORDER BY name`
/// (`_list_tables`).
fn list_tables(conn: &rusqlite::Connection) -> Option<Vec<String>> {
    let mut stmt = conn
        .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
        .ok()?;
    let mut rows = stmt.query([]).ok()?;
    let mut out = Vec::new();
    loop {
        match rows.next() {
            // `[r[0] for r in cur if isinstance(r[0], str)]` — a non-text name
            // is skipped, not an error.
            Ok(Some(row)) => {
                if let Ok(rusqlite::types::ValueRef::Text(bytes)) = row.get_ref(0) {
                    out.push(String::from_utf8_lossy(bytes).into_owned());
                }
            }
            Ok(None) => return Some(out),
            Err(_) => return None,
        }
    }
}

/// `[d[0] for d in conn.execute(f"SELECT * FROM {table} LIMIT 0").description]`
/// (`_column_names`).
fn column_names(conn: &rusqlite::Connection, table: &str) -> Option<Vec<String>> {
    let stmt = conn
        .prepare(&format!("SELECT * FROM {table} LIMIT 0"))
        .ok()?;
    Some(
        stmt.column_names()
            .into_iter()
            .map(ToString::to_string)
            .collect(),
    )
}

/// `SELECT rowid, * FROM {sessions_table}` — note: no `ORDER BY`, exactly as
/// the Python original writes it.
fn session_rows(
    conn: &rusqlite::Connection,
    sessions_table: &str,
) -> Option<Vec<(i64, Map<String, Value>)>> {
    let columns = column_names(conn, sessions_table)?;
    let mut stmt = conn
        .prepare(&format!("SELECT rowid, * FROM {sessions_table}"))
        .ok()?;
    let mut rows = stmt.query([]).ok()?;
    let mut out = Vec::new();
    // Python materialises the whole result inside the try, so a mid-iteration
    // failure discards the list rather than yielding half of it.
    loop {
        match rows.next() {
            Ok(Some(row)) => {
                let rowid = row.get::<_, i64>(0).ok()?;
                let mut payload = Map::new();
                for (index, column) in columns.iter().enumerate() {
                    let value = row.get_ref(index + 1).ok()?;
                    payload.insert(column.clone(), sqlite::value_to_json(value));
                }
                out.push((rowid, payload));
            }
            Ok(None) => return Some(out),
            Err(_) => return None,
        }
    }
}

/// A stable session id from a sessions row (`_extract_session_id`).
///
/// The first of `sessionId` / `session_id` / `id` / `uuid` holding a **string
/// or int** whose `str()` is non-empty; otherwise `session-<rowid>`. A float
/// does not qualify — `isinstance(v, (str, int))` excludes it.
#[must_use]
pub fn extract_session_id(payload: &Map<String, Value>, fallback_rowid: i64) -> String {
    for key in ["sessionId", "session_id", "id", "uuid"] {
        let Some(value) = payload.get(key) else {
            continue;
        };
        let rendered = match value {
            Value::String(text) => text.clone(),
            Value::Bool(flag) => if *flag { "True" } else { "False" }.to_string(),
            Value::Number(number) if number.is_i64() || number.is_u64() => pyval::py_str(value),
            _ => continue,
        };
        if !rendered.is_empty() {
            return rendered;
        }
    }
    format!("session-{fallback_rowid}")
}

/// The column used to filter messages by session (`_pick_session_filter_column`).
#[must_use]
pub fn pick_session_filter_column(columns: &[String]) -> Option<String> {
    for candidate in ["sessionid", "session_id", "session"] {
        if let Some(column) = columns
            .iter()
            .find(|column| column.to_lowercase() == candidate)
        {
            return Some(column.clone());
        }
    }
    None
}

/// One defensively-parsed message row → a `Record` (`_record_from_message`).
fn record_from_message(
    rowid: i64,
    payload: &Map<String, Value>,
    session: &SessionRef,
    clock: Clock,
) -> Option<Record> {
    // Without a role we cannot categorise the record; skip rather than guess.
    let role = coerce_role(payload.get("role"))?;
    let text = coerce_text(or_key(payload, "content", "text"));
    let model = coerce_str(payload.get("model")).unwrap_or_else(|| DEFAULT_MODEL.to_string());
    let timestamp = coerce_timestamp(
        MESSAGE_TIMESTAMP_COLUMNS
            .iter()
            .find_map(|column| payload.get(*column)),
        clock,
    );

    let mut input_tokens = pyval::safe_int(or_key(payload, "inputTokens", "input_tokens"));
    let mut output_tokens = pyval::safe_int(or_key(payload, "outputTokens", "output_tokens"));
    let mut estimated = false;
    if input_tokens == 0 && output_tokens == 0 && !text.is_empty() {
        // Fall back to text-length estimation on the semantically appropriate
        // side. `len` is Python's — code points.
        let estimate = i64::try_from(text.chars().count() / 4).unwrap_or(i64::MAX);
        if role == "assistant" {
            output_tokens = estimate;
        } else {
            input_tokens = estimate;
        }
        estimated = true;
    }

    let mut raw_payload = payload.clone();
    if estimated {
        raw_payload.insert("cost_source".to_string(), Value::from("estimated"));
    }

    Some(Record {
        provider: NAME.to_string(),
        session_id: session.session_id.clone(),
        seq: rowid,
        timestamp,
        role,
        model: Some(model),
        input_tokens: input_tokens.max(0),
        output_tokens: output_tokens.max(0),
        cache_create_tokens: 0,
        cache_read_tokens: 0,
        content_text: text,
        tools: Vec::new(),
        cwd: None,
        is_sidechain: false,
        uuid: format!("{}:{rowid}", session.session_id),
        parent_uuid: None,
        raw: Value::Object(raw_payload),
        speed: Speed::Standard,
    })
}

/// `mapping.get(first) or mapping.get(second)` — Python's truthiness chain.
fn or_key<'a>(map: &'a Map<String, Value>, first: &str, second: &str) -> Option<&'a Value> {
    map.get(first)
        .filter(|value| pyval::py_truthy(value))
        .or_else(|| map.get(second))
}

/// A non-blank string role, stripped and lower-cased (`_coerce_role`).
fn coerce_role(value: Option<&Value>) -> Option<String> {
    let text = value?.as_str()?;
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(trimmed.to_lowercase())
}

/// A non-blank string, stripped (`_coerce_str`).
fn coerce_str(value: Option<&Value>) -> Option<String> {
    let text = value?.as_str()?;
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(trimmed.to_string())
}

/// Message text out of whatever the column held (`_coerce_text`).
///
/// The tail of that function is the interesting part: a value that is neither
/// string, list, dict nor `None` is fed to `json.loads`, which raises
/// `TypeError` on an int or a float and falls through to `str(v)`. A dict whose
/// `content`/`text` is not a string takes the same route and comes back as its
/// Python `repr`. Both are reproduced here — see [`crate::pyval::py_str`].
fn coerce_text(value: Option<&Value>) -> String {
    let Some(value) = value else {
        // `_coerce_text(None)` hits the `v is None` branch → "".
        return String::new();
    };
    match value {
        Value::String(text) => text.clone(),
        Value::Null => String::new(),
        Value::Array(items) => {
            let mut pieces: Vec<&str> = Vec::new();
            for block in items {
                if let Some(map) = block.as_object() {
                    if let Some(text) = or_key(map, "text", "content").and_then(Value::as_str)
                        && !text.is_empty()
                    {
                        pieces.push(text);
                    }
                } else if let Some(text) = block.as_str() {
                    pieces.push(text);
                }
            }
            pieces.join("\n")
        }
        Value::Object(map) => {
            if let Some(nested) = or_key(map, "content", "text").and_then(Value::as_str) {
                return nested.to_string();
            }
            pyval::py_str(value)
        }
        // `json.loads(5)` is a TypeError, so the value comes back as `str(v)`.
        Value::Number(_) | Value::Bool(_) => pyval::py_str(value),
    }
}

/// A message row's timestamp column → ISO 8601 UTC (`_coerce_timestamp`).
///
/// Numbers above 10^12 are epoch **milliseconds**, everything else epoch
/// seconds. Every failure lands on *now*, which is why the parity fixtures
/// always carry a parseable value.
fn coerce_timestamp(value: Option<&Value>, clock: Clock) -> String {
    let now = || clock.now_iso();
    let Some(value) = value else { return now() };
    match value {
        Value::Null => now(),
        Value::String(text) if text.is_empty() => now(),
        Value::String(text) => {
            let trimmed = text.trim();
            if trimmed.is_empty() {
                return now();
            }
            if let Some(iso) = pytime::isoformat_roundtrip(trimmed) {
                return iso;
            }
            sqlite::py_float(trimmed)
                .and_then(|seconds| pytime::from_timestamp_iso(seconds / 1000.0))
                .unwrap_or_else(now)
        }
        Value::Bool(flag) => {
            pytime::from_timestamp_iso(if *flag { 1.0 } else { 0.0 }).unwrap_or_else(now)
        }
        Value::Number(number) => {
            let Some(raw) = number.as_f64() else {
                return now();
            };
            let seconds = if raw > 1e12 { raw / 1000.0 } else { raw };
            pytime::from_timestamp_iso(seconds).unwrap_or_else(now)
        }
        Value::Array(_) | Value::Object(_) => now(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn payload(value: Value) -> Map<String, Value> {
        value.as_object().expect("object").clone()
    }

    fn schema_of(sql: &str) -> Option<Schema> {
        let conn = rusqlite::Connection::open_in_memory().expect("memory db");
        conn.execute_batch(sql).expect("schema");
        sniff_schema(&conn)
    }

    #[test]
    fn a_name_containing_session_wins_the_sniff() {
        let schema = schema_of(
            "CREATE TABLE sessions (id TEXT, title TEXT, createdAt INTEGER);
             CREATE TABLE messages (session_id TEXT, role TEXT, content TEXT);",
        )
        .expect("sniffed");
        assert_eq!(schema.sessions_table, "sessions");
        assert_eq!(schema.messages_table.as_deref(), Some("messages"));
    }

    #[test]
    fn the_column_shape_fallback_needs_id_title_and_a_timestamp() {
        // No `session` in any name, but the shape is right.
        let schema = schema_of(
            "CREATE TABLE convos (id TEXT, title TEXT, updated_at INTEGER);
             CREATE TABLE history (role TEXT, content TEXT);",
        )
        .expect("sniffed");
        assert_eq!(schema.sessions_table, "convos");
        assert_eq!(schema.messages_table.as_deref(), Some("history"));

        // A title but no timestamp is not enough.
        assert_eq!(
            schema_of("CREATE TABLE convos (id TEXT, title TEXT);"),
            None
        );
        // An unrelated database sniffs to nothing rather than guessing.
        assert_eq!(schema_of("CREATE TABLE settings (k TEXT, v TEXT);"), None);
    }

    #[test]
    fn a_sessions_only_database_still_enumerates() {
        let schema =
            schema_of("CREATE TABLE sessions (id TEXT, title TEXT, ts INTEGER);").expect("sniffed");
        assert_eq!(schema.messages_table, None);
    }

    #[test]
    fn session_ids_prefer_the_declared_columns_then_fall_back_to_the_rowid() {
        assert_eq!(
            extract_session_id(&payload(json!({"sessionId": "a", "id": "b"})), 7),
            "a"
        );
        assert_eq!(extract_session_id(&payload(json!({"id": 12})), 7), "12");
        // A float is not `isinstance(v, (str, int))` and is skipped.
        assert_eq!(
            extract_session_id(&payload(json!({"id": 1.5, "uuid": "u"})), 7),
            "u"
        );
        assert_eq!(
            extract_session_id(&payload(json!({"id": "", "uuid": null})), 7),
            "session-7"
        );
        assert_eq!(extract_session_id(&payload(json!({})), 7), "session-7");
    }

    #[test]
    fn the_filter_column_is_matched_case_insensitively() {
        let columns = vec!["Role".to_string(), "SessionID".to_string()];
        assert_eq!(
            pick_session_filter_column(&columns).as_deref(),
            Some("SessionID")
        );
        assert_eq!(pick_session_filter_column(&["role".to_string()]), None);
    }

    #[test]
    fn text_coercion_covers_every_column_type_sqlite_can_hold() {
        assert_eq!(coerce_text(Some(&json!("plain"))), "plain");
        assert_eq!(coerce_text(Some(&json!(null))), "");
        assert_eq!(coerce_text(None), "");
        // Numbers come back as `str(v)`, via the json.loads TypeError branch.
        assert_eq!(coerce_text(Some(&json!(5))), "5");
        assert_eq!(coerce_text(Some(&json!(1.5))), "1.5");
        // Lists and dicts cannot come out of SQLite, but the helper handles
        // them because the Python one does.
        assert_eq!(
            coerce_text(Some(&json!([{"text": "a"}, "b", {"content": "c"}]))),
            "a\nb\nc"
        );
        assert_eq!(coerce_text(Some(&json!({"content": "deep"}))), "deep");
        assert_eq!(coerce_text(Some(&json!({"a": 1}))), "{'a': 1}");
    }

    #[test]
    fn estimation_only_kicks_in_when_both_counts_are_zero() {
        let session = SessionRef {
            provider: NAME.into(),
            project_slug: NAME.into(),
            session_id: "s".into(),
            file_path: PathBuf::from("/tmp/x.db"),
            file_mtime: 0.0,
            file_size: 0,
            source_kind: SourceKind::Database,
            source_hint: None,
        };
        let clock = Clock::Fixed(std::time::UNIX_EPOCH);

        let explicit = record_from_message(
            1,
            &payload(json!({"role": "assistant", "content": "hello world here",
                            "input_tokens": 1100, "output_tokens": 420,
                            "createdAt": 1_745_596_802_000_i64})),
            &session,
            clock,
        )
        .expect("record");
        assert_eq!((explicit.input_tokens, explicit.output_tokens), (1100, 420));
        assert!(
            !explicit
                .raw
                .as_object()
                .expect("obj")
                .contains_key("cost_source")
        );
        assert_eq!(explicit.timestamp, "2025-04-25T16:00:02+00:00");

        // An assistant turn estimates on the output side…
        let estimated = record_from_message(
            2,
            &payload(json!({"role": "assistant", "content": "abcdefgh"})),
            &session,
            clock,
        )
        .expect("record");
        assert_eq!((estimated.input_tokens, estimated.output_tokens), (0, 2));
        assert_eq!(
            estimated.raw.get("cost_source").and_then(Value::as_str),
            Some("estimated")
        );
        // …and a user turn on the input side.
        let user = record_from_message(
            3,
            &payload(json!({"role": "USER ", "content": "abcdefgh"})),
            &session,
            clock,
        )
        .expect("record");
        assert_eq!((user.input_tokens, user.output_tokens), (2, 0));
        assert_eq!(user.role, "user", "roles are stripped and lower-cased");
        assert_eq!(user.model.as_deref(), Some(DEFAULT_MODEL));

        // No role at all is a skip, not a guess.
        assert!(
            record_from_message(4, &payload(json!({"content": "x"})), &session, clock).is_none()
        );
    }

    #[test]
    fn an_absent_root_enumerates_empty_rather_than_failing() {
        let adapter = ContinueAdapter::with_root("/nonexistent/stax/continue");
        assert!(adapter.enumerate().is_empty());
        assert_eq!(adapter.source_roots().len(), 1);
        assert!(adapter.watch_paths().is_empty());
        assert_eq!(adapter.name(), "continue");
    }
}
