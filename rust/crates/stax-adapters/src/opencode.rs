//! OpenCode — the port of `python-legacy: adapters/opencode.py`.
//!
//! Sessions live in one or more SQLite databases under the XDG data directory
//! (`$XDG_DATA_HOME/opencode/`, else `~/.local/share/opencode/`), matched as
//! `opencode*.db` — older installs really do ship several. Three tables:
//!
//! * `session` — `id, directory, title, time_created, time_archived, parent_id`
//! * `message` — `id, session_id, time_created, data`, `data` being JSON
//!   `{role, modelID, tokens: {input, output, reasoning, cache: {read, write}}, cost}`
//! * `part` — one row per content part of a message, `data` being JSON
//!   `{type, text?, tool?, …}`
//!
//! ## Two decisions that show up in the emitted rows
//!
//! 1. **Session ids are namespaced by database file.** Two `opencode*.db` files
//!    can hold the same inner UUID, so the public session id is
//!    `"{db_basename}:{session.id}"`, with the inner id preserved in
//!    `source_hint["session_id"]`. Losing that would silently merge two users'
//!    sessions in a cross-session mart join.
//! 2. **`tokens.reasoning` folds into `output_tokens`**, the same way OpenAI
//!    reasoning tokens are folded by [`crate::codex`] — reasoning bills as
//!    output, so the canonical four slots must already reflect it.
//!
//! `seq` is the message row's `rowid` and the refs declare
//! [`SourceKind::Database`](crate::base::SourceKind::Database): resume is
//! `rowid > watermark`, the same one-number comparison a byte offset gets.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::base::{Record, SessionRef, SourceAdapter, SourceKind, Speed, stat_ref_fields};
use crate::pytime::{self, Clock};
use crate::{pyval, sqlite, walk};

/// The provider key.
pub const NAME: &str = "opencode";

/// The model stamped when `data.modelID` is missing or blank.
pub const DEFAULT_MODEL: &str = "opencode-auto";

/// The XDG variable that relocates the data directory (`_default_data_dir`).
pub const XDG_DATA_HOME_ENV: &str = "XDG_DATA_HOME";

/// OpenCode's data directory, with the environment injected
/// (`_default_data_dir`).
///
/// `$XDG_DATA_HOME/opencode` when the variable is set and non-blank (Python
/// `.strip()`s it), else `~/.local/share/opencode` — which is the path the
/// OpenCode CLI itself uses on macOS too, where `XDG_DATA_HOME` is usually
/// unset.
#[must_use]
pub fn resolve_data_dir(xdg_data_home: Option<&OsStr>, home: Option<&Path>) -> PathBuf {
    let configured = xdg_data_home
        .map(|value| value.to_string_lossy().trim().to_string())
        .filter(|value| !value.is_empty());
    match configured {
        Some(value) => PathBuf::from(value).join(NAME),
        None => home
            .map_or_else(PathBuf::new, Path::to_path_buf)
            .join(".local")
            .join("share")
            .join(NAME),
    }
}

/// The canonical four token slots, as OpenCode's five-key shape collapses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Tokens {
    /// Fresh (uncached) input.
    pub input: i64,
    /// Billable output, with reasoning folded in.
    pub output: i64,
    /// Cache-write tokens.
    pub cache_create: i64,
    /// Cache-read tokens.
    pub cache_read: i64,
}

/// The OpenCode source adapter (`OpenCodeAdapter`).
#[derive(Debug, Clone)]
pub struct OpenCodeAdapter {
    data_dir: PathBuf,
    clock: Clock,
}

impl Default for OpenCodeAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl OpenCodeAdapter {
    /// The XDG-resolved data directory, from the live environment.
    #[must_use]
    pub fn new() -> Self {
        #[allow(
            deprecated,
            reason = "std::env::home_dir is the platform-correct answer on the \
            1.97.1 pin; the claude and codex adapters carry the same allow"
        )]
        let home = std::env::home_dir();
        Self::with_env(std::env::var_os(XDG_DATA_HOME_ENV), home)
    }

    /// Inject both environment inputs — `$XDG_DATA_HOME` and the home directory.
    #[must_use]
    pub fn with_env(xdg_data_home: Option<OsString>, home: Option<PathBuf>) -> Self {
        Self {
            data_dir: resolve_data_dir(xdg_data_home.as_deref(), home.as_deref()),
            clock: Clock::Live,
        }
    }

    /// Inject the data directory directly — `OpenCodeAdapter(data_dir=…)`.
    #[must_use]
    pub fn with_data_dir(data_dir: impl Into<PathBuf>) -> Self {
        Self {
            data_dir: data_dir.into(),
            clock: Clock::Live,
        }
    }

    /// Pin the clock behind the `datetime.now(tz=UTC)` timestamp fallback.
    #[must_use]
    pub fn with_clock(mut self, clock: Clock) -> Self {
        self.clock = clock;
        self
    }

    /// The data directory this adapter scans.
    #[must_use]
    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }
}

impl SourceAdapter for OpenCodeAdapter {
    fn name(&self) -> &str {
        NAME
    }

    fn enumerate(&self) -> Vec<SessionRef> {
        // OpenCode not installed / never used — a clean exit, not an error.
        if !self.data_dir.is_dir() {
            return Vec::new();
        }
        let mut out = Vec::new();
        for db_path in walk::read_dir_sorted(&self.data_dir) {
            let name = walk::dir_name(&db_path);
            if !(name.starts_with(NAME) && name.ends_with(".db")) || !db_path.is_file() {
                continue;
            }
            // LOG: python warns "Cannot stat OpenCode DB %s".
            let Some((mtime, size)) = stat_ref_fields(&db_path) else {
                continue;
            };
            // LOG: python warns "Cannot open OpenCode DB %s".
            let Some(conn) = sqlite::open_readonly(&db_path) else {
                continue;
            };
            // LOG: python warns "OpenCode DB %s session query failed".
            // A database written against a different schema has no `session`
            // table; that is a skip, not a failure.
            let Some(session_ids) = query_session_ids(&conn) else {
                continue;
            };
            drop(conn);

            for inner_sid in session_ids {
                let mut hint = Map::new();
                hint.insert(
                    "db_path".to_string(),
                    Value::from(db_path.to_string_lossy().into_owned()),
                );
                hint.insert("session_id".to_string(), Value::from(inner_sid.clone()));
                out.push(SessionRef {
                    provider: NAME.to_string(),
                    project_slug: NAME.to_string(),
                    // The db basename keeps two files' identical inner UUIDs
                    // from colliding downstream.
                    session_id: format!("{name}:{inner_sid}"),
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
        // LOG: python warns "OpenCode DB missing at read time: %s".
        if !session.file_path.is_file() {
            return;
        }
        let inner_sid = session
            .source_hint
            .as_ref()
            .and_then(|hint| hint.get("session_id"))
            .filter(|value| pyval::py_truthy(value))
            .and_then(Value::as_str)
            .map_or_else(
                // Fallback: everything after the first `:` of the public id,
                // i.e. `str.partition(":")[2]`.
                || {
                    session
                        .session_id
                        .split_once(':')
                        .map_or_else(String::new, |(_, tail)| tail.to_string())
                },
                ToString::to_string,
            );

        // LOG: python warns "Cannot open OpenCode DB %s".
        let Some(conn) = sqlite::open_readonly(&session.file_path) else {
            return;
        };
        // LOG: python warns "OpenCode DB read failed on %s".
        let Ok(mut stmt) = conn.prepare(
            "SELECT rowid, id, time_created, data FROM message \
             WHERE session_id = ? AND rowid > ? ORDER BY rowid",
        ) else {
            return;
        };
        let Ok(mut rows) = stmt.query(rusqlite::params![inner_sid, since_offset]) else {
            return;
        };
        // Python's generator dies mid-iteration on a sqlite3.Error and the
        // caller keeps whatever it already yielded; `while let Ok(Some(_))`
        // is the same shape.
        while let Ok(Some(row)) = rows.next() {
            let (Ok(rowid), Ok(msg_id), Ok(time_created), Ok(data)) = (
                row.get::<_, i64>(0),
                row.get_ref(1),
                row.get_ref(2),
                row.get_ref(3),
            ) else {
                break;
            };
            let Some(parsed) = sqlite::json_object_column(data) else {
                continue;
            };
            let msg_id = sqlite::owned(msg_id);
            let timestamp = normalize_timestamp(time_created, self.clock);
            // One query per message so a broken part cannot drop the message.
            let parts = load_parts(&conn, &msg_id);
            if let Some(record) = record_from_message(rowid, timestamp, &parsed, &parts, session) {
                sink(record);
            }
        }
    }

    /// The data directory (`source_roots`). OpenCode declares no `watch_paths`.
    fn source_roots(&self) -> Vec<PathBuf> {
        vec![self.data_dir.clone()]
    }
}

/// `SELECT id FROM session ORDER BY id`, each id through `str()`.
///
/// `None` is the `sqlite3.Error` branch — most often "no such table: session".
fn query_session_ids(conn: &rusqlite::Connection) -> Option<Vec<String>> {
    let mut stmt = conn.prepare("SELECT id FROM session ORDER BY id").ok()?;
    let mut rows = stmt.query([]).ok()?;
    let mut out = Vec::new();
    // Python materialises with `fetchall()` inside the try, so a mid-iteration
    // error discards the whole list rather than half of it.
    loop {
        match rows.next() {
            Ok(Some(row)) => match row.get_ref(0) {
                Ok(value) => out.push(sqlite::value_to_py_str(value)),
                Err(_) => return None,
            },
            Ok(None) => return Some(out),
            Err(_) => return None,
        }
    }
}

/// The parsed `data` blob of every part on `msg_id` (`_load_parts`).
///
/// A failed query is an empty part list, logged and swallowed: one broken part
/// must not drop the message it belongs to.
fn load_parts(
    conn: &rusqlite::Connection,
    msg_id: &rusqlite::types::Value,
) -> Vec<Map<String, Value>> {
    let mut parts = Vec::new();
    // LOG: python warns "OpenCode part query failed for msg %s".
    let Ok(mut stmt) = conn.prepare("SELECT data FROM part WHERE message_id = ? ORDER BY rowid")
    else {
        return parts;
    };
    let Ok(mut rows) = stmt.query(rusqlite::params![msg_id]) else {
        return parts;
    };
    while let Ok(Some(row)) = rows.next() {
        let Ok(blob) = row.get_ref(0) else { break };
        if let Some(parsed) = sqlite::json_object_column(blob) {
            parts.push(parsed);
        }
    }
    parts
}

/// One `message` row plus its parts → a `Record` (`_record_from_message`).
///
/// `None` when the payload carries no usable `role` — a record we cannot
/// categorise is skipped rather than guessed at.
fn record_from_message(
    rowid: i64,
    timestamp: String,
    parsed: &Map<String, Value>,
    parts: &[Map<String, Value>],
    session: &SessionRef,
) -> Option<Record> {
    let role = parsed
        .get("role")
        .and_then(Value::as_str)
        .filter(|role| !role.is_empty())?;
    let model = parsed
        .get("modelID")
        .and_then(Value::as_str)
        .filter(|model| !model.is_empty())
        .unwrap_or(DEFAULT_MODEL);
    let tokens = tokens_from_payload(parsed);

    let mut raw_payload = parsed.clone();
    // Informational only — the cost layer recomputes against the pricer — but
    // kept so a parity check can see what OpenCode itself charged. A JSON
    // `null` cost is `None` in Python and is not stamped.
    if let Some(cost) = parsed.get("cost").filter(|value| !value.is_null()) {
        raw_payload.insert("embedded_cost".to_string(), cost.clone());
    }

    Some(Record {
        provider: NAME.to_string(),
        session_id: session.session_id.clone(),
        seq: rowid,
        timestamp,
        role: role.to_string(),
        model: Some(model.to_string()),
        input_tokens: tokens.input,
        output_tokens: tokens.output,
        cache_create_tokens: tokens.cache_create,
        cache_read_tokens: tokens.cache_read,
        content_text: content_from_parts(parts),
        tools: tools_from_parts(parts),
        cwd: None,
        is_sidechain: false,
        uuid: format!("{}:{rowid}", session.session_id),
        parent_uuid: None,
        raw: Value::Object(raw_payload),
        speed: Speed::Standard,
    })
}

/// OpenCode's five token keys → the canonical four (`_tokens_from_payload`).
#[must_use]
pub fn tokens_from_payload(parsed: &Map<String, Value>) -> Tokens {
    let Some(tokens) = parsed.get("tokens").and_then(Value::as_object) else {
        return Tokens::default();
    };
    let cache = tokens.get("cache").and_then(Value::as_object);
    Tokens {
        input: pyval::safe_int(tokens.get("input")),
        // Reasoning bills as output, matching how OpenAI reasoning is treated.
        output: pyval::safe_int(tokens.get("output"))
            .saturating_add(pyval::safe_int(tokens.get("reasoning"))),
        cache_create: cache.map_or(0, |cache| pyval::safe_int(cache.get("write"))),
        cache_read: cache.map_or(0, |cache| pyval::safe_int(cache.get("read"))),
    }
}

/// Concatenate the text parts, ignoring tool parts (`_content_from_parts`).
fn content_from_parts(parts: &[Map<String, Value>]) -> String {
    let mut pieces: Vec<&str> = Vec::new();
    for part in parts {
        if part.get("type").and_then(Value::as_str) != Some("text") {
            continue;
        }
        if let Some(text) = part.get("text").and_then(Value::as_str)
            && !text.is_empty()
        {
            pieces.push(text);
        }
    }
    pieces.join("\n")
}

/// Tool names from `type == "tool"` parts (`_tools_from_parts`).
///
/// The name lives at `data.tool`; a dict-wrapped `{"name": …}` is tolerated in
/// case of schema drift.
fn tools_from_parts(parts: &[Map<String, Value>]) -> Vec<String> {
    let mut tools = Vec::new();
    for part in parts {
        if part.get("type").and_then(Value::as_str) != Some("tool") {
            continue;
        }
        match part.get("tool") {
            Some(Value::String(tool)) if !tool.is_empty() => tools.push(tool.clone()),
            Some(Value::Object(map)) => {
                if let Some(name) = map.get("name").and_then(Value::as_str)
                    && !name.is_empty()
                {
                    tools.push(name.to_string());
                }
            }
            _ => {}
        }
    }
    tools
}

/// `time_created` (ms epoch or ISO string) → ISO 8601 UTC
/// (`_normalize_timestamp`).
///
/// Every failure path lands on *now*, which is why the parity fixtures always
/// carry a parseable value: two processes never agree on the microsecond.
fn normalize_timestamp(raw: rusqlite::types::ValueRef<'_>, clock: Clock) -> String {
    use rusqlite::types::ValueRef;
    let now = || clock.now_iso();
    match raw {
        ValueRef::Null => now(),
        ValueRef::Integer(number) =>
        {
            #[allow(
                clippy::cast_precision_loss,
                reason = "matches Python's `float(raw) / 1000.0` exactly"
            )]
            pytime::from_timestamp_iso(number as f64 / 1000.0).unwrap_or_else(now)
        }
        ValueRef::Real(number) => pytime::from_timestamp_iso(number / 1000.0).unwrap_or_else(now),
        ValueRef::Text(bytes) | ValueRef::Blob(bytes) => {
            // A BLOB reaches neither branch in Python (it is `bytes`, not
            // `str`) and falls straight through to `now`; here it is decoded
            // first, so a BLOB holding an ISO string parses. Recorded rather
            // than "fixed": no OpenCode install writes one.
            let text = String::from_utf8_lossy(bytes);
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn payload(value: Value) -> Map<String, Value> {
        value.as_object().expect("object").clone()
    }

    #[test]
    fn xdg_wins_over_the_home_layout() {
        let home = Path::new("/home/me");
        assert_eq!(
            resolve_data_dir(None, Some(home)),
            Path::new("/home/me/.local/share/opencode")
        );
        assert_eq!(
            resolve_data_dir(Some(OsStr::new("  ")), Some(home)),
            Path::new("/home/me/.local/share/opencode"),
            "a blank XDG_DATA_HOME is Python-falsy after .strip()"
        );
        assert_eq!(
            resolve_data_dir(Some(OsStr::new("/xdg")), Some(home)),
            Path::new("/xdg/opencode")
        );
    }

    #[test]
    fn reasoning_tokens_fold_into_output() {
        // The fixture's second message.
        let tokens = tokens_from_payload(&payload(json!({
            "tokens": {"input": 1800, "output": 520, "reasoning": 130,
                       "cache": {"read": 900, "write": 250}}
        })));
        assert_eq!(
            tokens,
            Tokens {
                input: 1800,
                output: 650,
                cache_create: 250,
                cache_read: 900,
            }
        );
    }

    #[test]
    fn garbage_token_shapes_degrade_to_zero() {
        assert_eq!(tokens_from_payload(&payload(json!({}))), Tokens::default());
        assert_eq!(
            tokens_from_payload(&payload(json!({"tokens": "nope"}))),
            Tokens::default()
        );
        assert_eq!(
            tokens_from_payload(&payload(json!({
                "tokens": {"input": "x", "output": null, "cache": []}
            }))),
            Tokens::default()
        );
        // Negative counts clamp rather than becoming negative cost.
        assert_eq!(
            tokens_from_payload(&payload(json!({"tokens": {"input": -5}}))).input,
            0
        );
    }

    #[test]
    fn parts_split_into_text_and_tools() {
        let parts = vec![
            payload(json!({"type": "text", "text": "first"})),
            payload(json!({"type": "tool", "tool": "edit_file"})),
            payload(json!({"type": "text", "text": "second"})),
            payload(json!({"type": "tool", "tool": {"name": "wrapped"}})),
            payload(json!({"type": "text"})),
            payload(json!({"type": "tool", "tool": 7})),
        ];
        assert_eq!(content_from_parts(&parts), "first\nsecond");
        assert_eq!(tools_from_parts(&parts), vec!["edit_file", "wrapped"]);
    }

    #[test]
    fn timestamps_take_the_ms_path_and_fall_back_to_an_injected_now() {
        use rusqlite::types::ValueRef;
        let clock = Clock::Fixed(std::time::UNIX_EPOCH);
        // The fixture's `time_created`, in milliseconds.
        assert_eq!(
            normalize_timestamp(ValueRef::Integer(1_745_596_801_000), clock),
            "2025-04-25T16:00:01+00:00"
        );
        // `0` is falsy but is neither None nor `== ""`, so it takes the numeric
        // path and lands on the epoch rather than on *now*.
        assert_eq!(
            normalize_timestamp(ValueRef::Integer(0), clock),
            "1970-01-01T00:00:00+00:00"
        );
        // An ISO string is round-tripped through fromisoformat.
        assert_eq!(
            normalize_timestamp(ValueRef::Text(b"2026-04-25T18:00:00Z"), clock),
            "2026-04-25T18:00:00+00:00"
        );
        // A numeric *string* falls through to the float branch, still in ms.
        assert_eq!(
            normalize_timestamp(ValueRef::Text(b"1745596801000"), clock),
            "2025-04-25T16:00:01+00:00"
        );
        // The three branches that cannot be diffed against Python, because
        // Python calls `datetime.now()` for them too: NULL, blank, garbage.
        assert_eq!(
            normalize_timestamp(ValueRef::Null, clock),
            "1970-01-01T00:00:00+00:00"
        );
        assert_eq!(
            normalize_timestamp(ValueRef::Text(b"   "), clock),
            "1970-01-01T00:00:00+00:00"
        );
        assert_eq!(
            normalize_timestamp(ValueRef::Text(b"banana"), clock),
            "1970-01-01T00:00:00+00:00"
        );
    }

    #[test]
    fn an_absent_data_dir_enumerates_empty_rather_than_failing() {
        let adapter = OpenCodeAdapter::with_data_dir("/nonexistent/stax/opencode");
        assert!(adapter.enumerate().is_empty());
        assert_eq!(adapter.source_roots().len(), 1);
        assert!(adapter.watch_paths().is_empty());
    }
}
