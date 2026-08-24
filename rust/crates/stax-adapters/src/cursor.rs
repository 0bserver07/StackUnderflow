//! Cursor IDE — the port of `python-legacy: adapters/cursor.py`.
//!
//! The **first database-kind adapter**: Cursor keeps everything in one SQLite
//! key/value table, `cursorDiskKV` inside `state.vscdb`, and two key prefixes
//! hold conversation data.
//!
//! * `bubbleId:%` — chat bubbles: `{conversationId, type (1 user / 2 assistant),
//!   text, modelInfo.modelName, tokenCount.{inputTokens,outputTokens}, createdAt}`.
//! * `agentKv:blob:%` — agent blobs: `{conversationId, role, content,
//!   providerOptions.cursor.modelName}`.
//!
//! One [`SessionRef`] per `conversationId`, `source_kind` is
//! [`crate::base::SourceKind::Database`], and `seq` is the SQLite **rowid** — so
//! a resumed read is `WHERE rowid > ?`, the same "strictly past this number"
//! comparison the JSONL adapters make with a byte offset. That is the whole
//! reason the two share one field.
//!
//! ## Where a project slug comes from when the source has no cwd
//!
//! Cursor records no working directory per conversation, so
//! [`workspace_slug_for_conversation`] infers one: it sweeps every absolute path
//! referenced by that conversation's bubbles (file selections, folder mentions,
//! tool payloads), then picks the *deepest* directory that is an ancestor of at
//! least half of them and runs it through the Claude/Codex slug rule. A
//! conversation with no path evidence falls back to the literal slug `cursor` so
//! it stays visible.
//!
//! ## Tokens are often estimated
//!
//! Cursor v3 writes zero counts on every bubble, so an explicit `tokenCount` is
//! preferred only when one of its two numbers is non-zero; otherwise the record
//! carries `len(text) // 4` and is stamped `raw["cost_source"] = "estimated"`.
//!
//! ## Deliberate omission: the fingerprint cache
//!
//! DIVERGENCE (structural, output-identical): `cursor.py` consults
//! `infra/cursor_cache.py` on a full read and *writes* the parsed record stream
//! back to `~/.stackunderflow/cache/cursor-results.json`. That is a caching
//! layer, not a parsing one — it stores exactly the records this module
//! produces, so hit and miss are indistinguishable in the output — and the
//! architecture decision for this port keeps adapters storage-free (the same
//! ruling that moved `materialize_metadata` out to a post-ingest hook). An
//! adapter that writes to the data directory also cannot be run against a
//! read-only dataset, which the parity harness does on every invocation.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use rusqlite::{Connection, OpenFlags};
use serde_json::{Map, Value};

use crate::base::{Record, SessionRef, SourceAdapter, SourceKind, Speed, home_dir};
use crate::cline::Platform;
use crate::jsonl;
use crate::pyval;

/// The provider key.
pub const NAME: &str = "cursor";

/// Model recorded when a payload declares none (`_model_from_payload`).
pub const DEFAULT_MODEL: &str = "cursor-auto";

/// Slug for a conversation with no workspace evidence (`_FALLBACK_SLUG`).
pub const FALLBACK_SLUG: &str = "cursor";

/// A path needs at least this many segments below `/` to be a workspace
/// candidate (`_MIN_PATH_DEPTH`) — it rejects `/Users/foo` itself.
pub const MIN_PATH_DEPTH: usize = 3;

/// The roots [`find_paths`] will start an absolute path at (`_PATH_RE`).
const PATH_ROOTS: [&str; 4] = ["Users", "home", "var", "opt"];

/// Every row either prefix selects, in rowid order past the watermark.
const READ_SQL: &str = "SELECT rowid, key, value FROM cursorDiskKV \
     WHERE (key LIKE 'bubbleId:%' OR key LIKE 'agentKv:blob:%') \
     AND rowid > ? ORDER BY rowid";

/// Every row either prefix selects, for conversation discovery.
const ENUMERATE_SQL: &str = "SELECT key, value FROM cursorDiskKV \
     WHERE key LIKE 'bubbleId:%' OR key LIKE 'agentKv:blob:%'";

/// One conversation's bubbles, for the workspace sweep.
const BUBBLES_SQL: &str = "SELECT value FROM cursorDiskKV WHERE key LIKE ?";

/// Cursor's `state.vscdb`, per platform (`_default_vscdb_path`).
///
/// The three constants Python ships, with the host injected so all three are
/// testable from one machine.
#[must_use]
pub fn default_vscdb_path(
    platform: Platform,
    home: Option<&Path>,
    appdata: Option<&OsStr>,
) -> PathBuf {
    match platform {
        Platform::Windows => Path::new(appdata.unwrap_or_else(|| OsStr::new("")))
            .join("Cursor")
            .join("User")
            .join("globalStorage")
            .join("state.vscdb"),
        Platform::Linux => home
            .unwrap_or_else(|| Path::new(""))
            .join(".config")
            .join("Cursor")
            .join("User")
            .join("globalStorage")
            .join("state.vscdb"),
        Platform::MacOs => home
            .unwrap_or_else(|| Path::new(""))
            .join("Library")
            .join("Application Support")
            .join("Cursor")
            .join("User")
            .join("globalStorage")
            .join("state.vscdb"),
    }
}

/// The Cursor source adapter (`CursorAdapter`).
#[derive(Debug, Clone)]
pub struct CursorAdapter {
    db_path: PathBuf,
}

impl Default for CursorAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl CursorAdapter {
    /// Resolve the platform default once at construction, as Python's
    /// `__init__` does.
    #[must_use]
    pub fn new() -> Self {
        Self {
            db_path: default_vscdb_path(
                Platform::current(),
                home_dir().as_deref(),
                std::env::var_os("APPDATA").as_deref(),
            ),
        }
    }

    /// Inject the vscdb path — the constructor parameter Python already has.
    #[must_use]
    pub fn with_vscdb_path(path: impl Into<PathBuf>) -> Self {
        Self {
            db_path: path.into(),
        }
    }

    /// The vscdb this adapter reads.
    #[must_use]
    pub fn vscdb_path(&self) -> &Path {
        &self.db_path
    }

    /// Open the vscdb read-only (`_open_readonly`).
    ///
    /// Python builds a `file:{path}?mode=ro` URI; the flag is the same thing
    /// without the URI escaping bug that a `?` or `#` in a path would hit
    /// there. `immutable` is deliberately *not* set: Cursor writes to this file
    /// while it runs, and an immutable open would silently read a stale
    /// pre-WAL-checkpoint snapshot.
    fn open_readonly(path: &Path) -> rusqlite::Result<Connection> {
        Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
    }
}

impl SourceAdapter for CursorAdapter {
    fn name(&self) -> &str {
        NAME
    }

    fn enumerate(&self) -> Vec<SessionRef> {
        let path = &self.db_path;
        if !path.is_file() {
            // Cursor not installed / never used on this machine — clean exit.
            return Vec::new();
        }
        // LOG: python warns "Cannot stat Cursor vscdb %s".
        let Some((mtime, size)) = crate::base::stat_ref_fields(path) else {
            return Vec::new();
        };
        // LOG: python warns "Cannot open Cursor vscdb %s".
        let Ok(conn) = Self::open_readonly(path) else {
            return Vec::new();
        };

        // DIVERGENCE (deliberate, order-only): Python collects conversation ids
        // into a `set` and iterates it, so its ref order depends on string hash
        // randomisation and changes run to run. First-seen order is the same
        // *set* in a reproducible order — which the parity harness needs and
        // ingest does not care about.
        let Some(conversations) = conversation_ids(&conn) else {
            // LOG: python warns "Cursor vscdb query failed on %s" and yields
            // nothing at all — the yields sit after the try block.
            return Vec::new();
        };
        let mut slugs = Vec::with_capacity(conversations.len());
        for conversation in &conversations {
            let Some(slug) = workspace_slug_for_conversation(conversation, &conn) else {
                return Vec::new();
            };
            slugs.push(slug);
        }

        conversations
            .into_iter()
            .zip(slugs)
            .map(|(conversation, slug)| {
                let mut hint = Map::new();
                hint.insert("conversation_id".to_string(), conversation.clone().into());
                SessionRef {
                    provider: NAME.to_string(),
                    project_slug: slug,
                    session_id: conversation,
                    file_path: path.clone(),
                    file_mtime: mtime,
                    file_size: size,
                    source_kind: SourceKind::Database,
                    source_hint: Some(hint),
                }
            })
            .collect()
    }

    fn read_into(&self, session: &SessionRef, since_offset: i64, sink: &mut dyn FnMut(Record)) {
        let path = &session.file_path;
        if !path.is_file() {
            // LOG: python warns "Cursor vscdb missing at read time: %s".
            return;
        }
        let target = Target::of(session);
        // LOG: python warns "Cannot open Cursor vscdb %s".
        let Ok(conn) = Self::open_readonly(path) else {
            return;
        };
        let Ok(mut statement) = conn.prepare(READ_SQL) else {
            return;
        };
        let Ok(mut rows) = statement.query([since_offset]) else {
            return;
        };
        // LOG: python warns "Cursor vscdb read failed on %s" and stops, keeping
        // whatever it already yielded.
        while let Ok(Some(row)) = rows.next() {
            let Ok(rowid) = row.get::<_, i64>(0) else {
                return;
            };
            let (Some(key), Some(parsed)) = (column_text(row, 1), safe_json_object(row, 2)) else {
                continue;
            };
            // Cursor v3+ puts the conversation id in the key; older formats
            // surfaced it inside the JSON value — accept both.
            let conversation = conversation_id_from_key(&key).unwrap_or_else(|| {
                parsed
                    .get("conversationId")
                    .filter(|value| pyval::py_truthy(value))
                    .map_or_else(String::new, pyval::py_str)
            });
            if conversation.is_empty() {
                continue;
            }
            let Some(record) = record_from_row(rowid, &key, parsed, &conversation) else {
                continue;
            };
            if !target.matches(&conversation) {
                continue;
            }
            sink(record);
        }
    }

    /// The vscdb file itself (`watch_paths`).
    ///
    /// Cursor's storage is one SQLite file, and a file watcher reports any byte
    /// change on it through mtime+size, so watching the file is enough.
    fn watch_paths(&self) -> Vec<PathBuf> {
        vec![self.db_path.clone()]
    }
}

/// Which conversation a read is restricted to.
///
/// `(ref.source_hint or {}).get("conversation_id") or ref.session_id` — with the
/// one shape that idiom can produce and a `String` cannot: a hint holding a
/// *non-string* id, which Python then compares against every row's string id and
/// never matches.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Target {
    Conversation(String),
    NothingMatches,
}

impl Target {
    fn of(session: &SessionRef) -> Self {
        match session
            .source_hint
            .as_ref()
            .and_then(|hint| hint.get("conversation_id"))
        {
            Some(Value::String(id)) if !id.is_empty() => Self::Conversation(id.clone()),
            Some(other) if pyval::py_truthy(other) => Self::NothingMatches,
            _ => Self::Conversation(session.session_id.clone()),
        }
    }

    fn matches(&self, conversation: &str) -> bool {
        match self {
            Self::Conversation(target) => target == conversation,
            Self::NothingMatches => false,
        }
    }
}

/// Every distinct conversation id in the table, in first-seen order.
///
/// `None` is Python's `except sqlite3.Error` branch: enumerate yields nothing.
fn conversation_ids(conn: &Connection) -> Option<Vec<String>> {
    let mut statement = conn.prepare(ENUMERATE_SQL).ok()?;
    let mut rows = statement.query([]).ok()?;
    let mut seen: Vec<String> = Vec::new();
    while let Some(row) = rows.next().ok()? {
        let Some(key) = column_text(row, 0) else {
            continue;
        };
        let conversation = conversation_id_from_key(&key).or_else(|| {
            safe_json_object(row, 1)?
                .get("conversationId")
                .filter(|value| pyval::py_truthy(value))
                .map(pyval::py_str)
        });
        let Some(conversation) = conversation.filter(|id| !id.is_empty()) else {
            continue;
        };
        if !seen.contains(&conversation) {
            seen.push(conversation);
        }
    }
    Some(seen)
}

/// The conversation id encoded in a `cursorDiskKV` key
/// (`_conversation_id_from_key`).
///
/// Cursor v3+ keys are `bubbleId:<conversationId>:<bubbleId>` and
/// `agentKv:blob:<conversationId>:<…>`. An older single-segment key
/// (`bubbleId:<bubbleId>`) kept the id in the JSON value instead, so it returns
/// `None` and the caller falls through.
#[must_use]
pub fn conversation_id_from_key(key: &str) -> Option<String> {
    let rest = key
        .strip_prefix("bubbleId:")
        .or_else(|| key.strip_prefix("agentKv:blob:"))?;
    let (head, _) = rest.split_once(':')?;
    (!head.is_empty()).then(|| head.to_string())
}

/// One row → a `Record`, or `None` (`_record_from_row`).
fn record_from_row(
    rowid: i64,
    key: &str,
    parsed: Map<String, Value>,
    conversation: &str,
) -> Option<Record> {
    let is_bubble = key.starts_with("bubbleId:");
    if !is_bubble && !key.starts_with("agentKv:blob:") {
        return None;
    }
    let role = role_from_payload(&parsed, is_bubble)?;
    let text = text_from_payload(&parsed);
    let tokens = tokens_from_payload(&parsed, &text);

    let mut raw = parsed;
    if tokens.estimated {
        raw.insert("cost_source".to_string(), "estimated".into());
    }
    Some(Record {
        provider: NAME.to_string(),
        session_id: conversation.to_string(),
        seq: rowid,
        timestamp: normalize_timestamp(raw.get("createdAt")),
        role,
        model: Some(model_from_payload(&raw, is_bubble)),
        input_tokens: tokens.input,
        output_tokens: tokens.output,
        cache_create_tokens: 0,
        cache_read_tokens: 0,
        content_text: text,
        tools: Vec::new(),
        cwd: None,
        is_sidechain: false,
        uuid: format!("{conversation}:{rowid}"),
        parent_uuid: None,
        raw: Value::Object(raw),
        speed: Speed::Standard,
    })
}

/// A bubble's numeric `type`, or an agent blob's `role` (`_role_from_payload`).
fn role_from_payload(parsed: &Map<String, Value>, is_bubble: bool) -> Option<String> {
    if is_bubble {
        // `parsed.get("type") == 1` is a *Python* equality: `True == 1` and
        // `1.0 == 1` are both true, so a JSON `true` or `1.0` is a user bubble.
        let kind = parsed.get("type")?;
        if py_equals_int(kind, 1) {
            return Some("user".to_string());
        }
        if py_equals_int(kind, 2) {
            return Some("assistant".to_string());
        }
        return None;
    }
    parsed
        .get("role")
        .and_then(Value::as_str)
        .filter(|role| !role.is_empty())
        .map(ToString::to_string)
}

/// Python's `value == n` for a JSON value and a small integer.
fn py_equals_int(value: &Value, target: i64) -> bool {
    match value {
        Value::Bool(flag) => i64::from(*flag) == target,
        #[allow(
            clippy::cast_precision_loss,
            reason = "target is 1 or 2 — exactly representable"
        )]
        Value::Number(number) => number.as_f64() == Some(target as f64),
        _ => false,
    }
}

/// A bubble's `text`, else an agent blob's `content` (`_text_from_payload`).
#[must_use]
pub fn text_from_payload(parsed: &Map<String, Value>) -> String {
    if let Some(text) = parsed
        .get("text")
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
    {
        return text.to_string();
    }
    let Some(content) = parsed.get("content") else {
        return String::new();
    };
    if let Some(text) = content.as_str() {
        return text.to_string();
    }
    let Some(blocks) = content.as_array() else {
        return String::new();
    };
    let mut pieces: Vec<String> = Vec::new();
    for block in blocks {
        if let Some(map) = block.as_object() {
            if let Some(text) = map.get("text").and_then(Value::as_str)
                && !text.is_empty()
            {
                pieces.push(text.to_string());
            }
        } else if let Some(text) = block.as_str() {
            pieces.push(text.to_string());
        }
    }
    pieces.join("\n")
}

/// `modelInfo.modelName` for a bubble, `providerOptions.cursor.modelName` for an
/// agent blob, `cursor-auto` for anything else (`_model_from_payload`).
#[must_use]
pub fn model_from_payload(parsed: &Map<String, Value>, is_bubble: bool) -> String {
    let name = if is_bubble {
        parsed
            .get("modelInfo")
            .and_then(Value::as_object)
            .and_then(|info| info.get("modelName"))
    } else {
        parsed
            .get("providerOptions")
            .and_then(Value::as_object)
            .and_then(|options| options.get("cursor"))
            .and_then(Value::as_object)
            .and_then(|cursor| cursor.get("modelName"))
    };
    name.and_then(Value::as_str)
        .filter(|name| !name.is_empty())
        .map_or_else(|| DEFAULT_MODEL.to_string(), ToString::to_string)
}

/// The token counts on one payload, and whether they were estimated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PayloadTokens {
    /// Input tokens — the estimate lands here, not on output.
    pub input: i64,
    /// Output tokens.
    pub output: i64,
    /// Whether `len(text) // 4` produced them.
    pub estimated: bool,
}

/// Explicit `tokenCount` when either number is non-zero, else `len(text) // 4`
/// (`_tokens_from_payload`).
///
/// Cursor v3 returns zero counts on every bubble, which is what the estimate is
/// for.
#[must_use]
pub fn tokens_from_payload(parsed: &Map<String, Value>, text: &str) -> PayloadTokens {
    if let Some(counts) = parsed.get("tokenCount").and_then(Value::as_object) {
        let input = pyval::safe_int(counts.get("inputTokens"));
        let output = pyval::safe_int(counts.get("outputTokens"));
        if input > 0 || output > 0 {
            return PayloadTokens {
                input,
                output,
                estimated: false,
            };
        }
    }
    PayloadTokens {
        // Python's `len()` counts *characters*, not bytes.
        input: i64::try_from(text.chars().count() / 4).unwrap_or(i64::MAX),
        output: 0,
        estimated: true,
    }
}

/// `createdAt` (ms-epoch or ISO string) → ISO 8601 (`_normalize_timestamp`).
///
/// DIVERGENCE (unavoidable, wall-clock): every failure path here is
/// `datetime.now(tz=UTC).isoformat()` in both implementations, so a row with no
/// usable `createdAt` produces a *different string on every run* and cannot be
/// byte-compared across two processes. The parity fixtures therefore carry a
/// `createdAt` on every row, and the fallback's shape is pinned by a unit test.
#[must_use]
pub fn normalize_timestamp(raw: Option<&Value>) -> String {
    let Some(raw) = raw else {
        return now_iso();
    };
    match raw {
        Value::Null => now_iso(),
        // `isinstance(raw, (int, float))` — and `bool` is a subclass of `int`.
        Value::Bool(_) | Value::Number(_) => py_number(raw)
            .and_then(|millis| pyval::epoch_seconds_to_iso(millis / 1000.0))
            .unwrap_or_else(now_iso),
        Value::String(text) => {
            let trimmed = text.trim();
            if trimmed.is_empty() {
                return now_iso();
            }
            // Already-ISO string — accepted if it parses, with a naive one
            // pinned to UTC. `Z` is rewritten first, as Python does.
            if let Some(stamp) = parse_isoformat(&trimmed.replace('Z', "+00:00")) {
                return stamp;
            }
            // Numeric string?
            trimmed
                .parse::<f64>()
                .ok()
                .and_then(|millis| pyval::epoch_seconds_to_iso(millis / 1000.0))
                .unwrap_or_else(now_iso)
        }
        Value::Array(_) | Value::Object(_) => now_iso(),
    }
}

/// `float(v)` for the numeric branch of [`normalize_timestamp`].
fn py_number(value: &Value) -> Option<f64> {
    match value {
        Value::Bool(flag) => Some(if *flag { 1.0 } else { 0.0 }),
        Value::Number(number) => number.as_f64(),
        _ => None,
    }
}

/// `datetime.now(tz=UTC).isoformat()`.
fn now_iso() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    pyval::epoch_seconds_to_iso(now.as_secs_f64()).unwrap_or_default()
}

/// `datetime.fromisoformat(s)` followed by `.isoformat()`, with a naive value
/// pinned to UTC.
///
/// Supported grammar — the extended ISO 8601 forms Cursor writes:
/// `YYYY-MM-DD` optionally followed by any single separator character and
/// `HH[:MM[:SS[.f…]]]`, optionally followed by `±HH[:MM[:SS]]`. Fractions are
/// truncated to microseconds, as CPython truncates them.
///
/// DIVERGENCE (documented): Python 3.11+ also accepts basic (`YYYYMMDD`), week
/// (`YYYY-Www-D`) and ordinal (`YYYY-DDD`) dates. Those return `None` here and
/// fall through to the numeric-string branch — for a `createdAt` field, which is
/// either a millisecond count or an RFC 3339 stamp, the shapes do not occur.
#[must_use]
pub fn parse_isoformat(text: &str) -> Option<String> {
    let (date, after_date) = text.split_at_checked(10)?;
    let (year, month, day) = parse_date(date)?;
    // `fromisoformat` accepts *any* single character as the date/time
    // separator, `T` and a space included.
    let rest = match after_date.chars().next() {
        None => "",
        Some(separator) => &after_date[separator.len_utf8()..],
    };
    // Split a trailing UTC offset off the time.
    let (time, offset) = match rest.rfind(['+', '-']) {
        Some(index) => {
            let (time, sign_and_offset) = rest.split_at(index);
            (time, Some(parse_offset(sign_and_offset)?))
        }
        None => (rest, None),
    };
    let (hour, minute, second, micros) = if time.is_empty() {
        (0, 0, 0, 0)
    } else {
        parse_time(time)?
    };
    let fraction = if micros == 0 {
        String::new()
    } else {
        format!(".{micros:06}")
    };
    // A naive value is pinned to UTC by the caller's `replace(tzinfo=UTC)`.
    let offset = offset.unwrap_or_else(|| "+00:00".to_string());
    Some(format!(
        "{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}{fraction}{offset}"
    ))
}

/// `YYYY-MM-DD`, calendar-validated.
fn parse_date(text: &str) -> Option<(u32, u32, u32)> {
    let bytes = text.as_bytes();
    if bytes.len() != 10 || bytes[4] != b'-' || bytes[7] != b'-' {
        return None;
    }
    let year: u32 = text.get(0..4)?.parse().ok()?;
    let month: u32 = text.get(5..7)?.parse().ok()?;
    let day: u32 = text.get(8..10)?.parse().ok()?;
    if year == 0 || !(1..=12).contains(&month) || day == 0 || day > days_in_month(year, month) {
        return None;
    }
    Some((year, month, day))
}

/// `HH[:MM[:SS[.f…]]]` — hours are mandatory, everything after is not.
fn parse_time(text: &str) -> Option<(u32, u32, u32, u32)> {
    let (clock, fraction) = match text.split_once(['.', ',']) {
        Some((clock, fraction)) => (clock, Some(fraction)),
        None => (text, None),
    };
    let mut parts = clock.split(':');
    let hour: u32 = parse_two_digits(parts.next()?)?;
    let minute = match parts.next() {
        Some(field) => parse_two_digits(field)?,
        None => 0,
    };
    let second = match parts.next() {
        Some(field) => parse_two_digits(field)?,
        None => 0,
    };
    if parts.next().is_some() || hour > 23 || minute > 59 || second > 59 {
        return None;
    }
    let micros = match fraction {
        None => 0,
        Some(digits) => {
            if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
                return None;
            }
            // Truncate past microseconds, pad shorter fractions out.
            let mut padded: String = digits.chars().take(6).collect();
            while padded.len() < 6 {
                padded.push('0');
            }
            padded.parse().ok()?
        }
    };
    Some((hour, minute, second, micros))
}

/// `±HH[:MM[:SS]]` re-rendered the way `timezone.isoformat` does.
fn parse_offset(text: &str) -> Option<String> {
    let sign = match text.as_bytes().first()? {
        b'+' => '+',
        b'-' => '-',
        _ => return None,
    };
    let mut parts = text.get(1..)?.split(':');
    let hour = parse_two_digits(parts.next()?)?;
    let minute = match parts.next() {
        Some(field) => parse_two_digits(field)?,
        None => 0,
    };
    let second = match parts.next() {
        Some(field) => parse_two_digits(field)?,
        None => 0,
    };
    if parts.next().is_some() || hour > 23 || minute > 59 || second > 59 {
        return None;
    }
    // Python prints the seconds field only when it is non-zero.
    if second == 0 {
        Some(format!("{sign}{hour:02}:{minute:02}"))
    } else {
        Some(format!("{sign}{hour:02}:{minute:02}:{second:02}"))
    }
}

fn parse_two_digits(text: &str) -> Option<u32> {
    if text.len() != 2 || !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

const fn days_in_month(year: u32, month: u32) -> u32 {
    let leap = year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400));
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        _ => 28,
    }
}

// ── workspace-slug derivation ────────────────────────────────────────────────

/// Best-effort `project_slug` for one conversation
/// (`_workspace_slug_for_conversation`).
///
/// `None` is the `sqlite3.Error` branch that aborts enumerate; a conversation
/// with no usable path evidence returns [`FALLBACK_SLUG`], not `None`.
#[must_use]
pub fn workspace_slug_for_conversation(conversation: &str, conn: &Connection) -> Option<String> {
    let paths = collect_paths_for_conversation(conversation, conn)?;
    Some(derive_workspace_root(&paths).map_or_else(
        || FALLBACK_SLUG.to_string(),
        |root| pyval::slug_for(&root, "/"),
    ))
}

/// Every absolute path referenced by a conversation's bubbles
/// (`_collect_paths_for_conversation`).
fn collect_paths_for_conversation(conversation: &str, conn: &Connection) -> Option<Vec<String>> {
    // LOG: python debug-logs "Cursor path lookup failed for conv %s" and
    // returns the paths collected so far (none).
    let Ok(mut statement) = conn.prepare(BUBBLES_SQL) else {
        return Some(Vec::new());
    };
    // The LIKE pattern is built by interpolation in Python too, so a `%` or `_`
    // inside a conversation id is a wildcard in both — ported as-is.
    let Ok(mut rows) = statement.query([format!("bubbleId:{conversation}:%")]) else {
        return Some(Vec::new());
    };
    let mut paths = Vec::new();
    while let Some(row) = rows.next().ok()? {
        let Some(parsed) = safe_json_object(row, 0) else {
            continue;
        };
        paths_in_bubble(&parsed, &mut paths);
    }
    Some(paths)
}

/// Absolute paths from one bubble payload (`_paths_in_bubble`).
fn paths_in_bubble(parsed: &Map<String, Value>, out: &mut Vec<String>) {
    let absolute = |value: Option<&Value>| -> Option<String> {
        value
            .and_then(Value::as_str)
            .filter(|text| text.starts_with('/'))
            .map(ToString::to_string)
    };

    if let Some(context) = parsed.get("context").and_then(Value::as_object) {
        // Chip-attached file selections. A truthy non-list would make Python's
        // `for` raise, so the list check is load-bearing, not defensive.
        if let Some(selections) = context.get("fileSelections").and_then(Value::as_array) {
            for selection in selections {
                let Some(uri) = selection
                    .as_object()
                    .and_then(|selection| selection.get("uri"))
                    .and_then(Value::as_object)
                else {
                    continue;
                };
                // Both keys contribute — they are not alternatives.
                out.extend(absolute(uri.get("fsPath")));
                out.extend(absolute(uri.get("path")));
            }
        }
        // `mentions` keeps URI-keyed maps for both files and folders.
        if let Some(mentions) = context.get("mentions").and_then(Value::as_object) {
            for bucket in ["fileSelections", "folderSelections"] {
                let Some(container) = mentions.get(bucket).and_then(Value::as_object) else {
                    continue;
                };
                for key in container.keys() {
                    if let Some(path) = key.strip_prefix("file://") {
                        out.push(path.to_string());
                    }
                }
            }
        }
    }

    // Folders explicitly attached to the chat (drag-and-dropped).
    if let Some(folders) = parsed.get("attachedFoldersNew").and_then(Value::as_array) {
        for folder in folders {
            let Some(folder) = folder.as_object() else {
                continue;
            };
            if let Some(uri) = folder.get("uri").and_then(Value::as_object) {
                // `uri.get("fsPath") or uri.get("path")` — one of the two.
                let candidate = uri
                    .get("fsPath")
                    .filter(|value| pyval::py_truthy(value))
                    .or_else(|| uri.get("path"));
                out.extend(absolute(candidate));
            }
            out.extend(absolute(folder.get("path")));
        }
    }

    // Tool calls embed paths inside JSON-encoded strings; sweep the whole block
    // rather than enumerating every tool's schema.
    if let Some(tool_data) = parsed.get("toolFormerData").and_then(Value::as_object) {
        for field in ["rawArgs", "params"] {
            if let Some(text) = tool_data.get(field).and_then(Value::as_str) {
                out.extend(find_paths(text));
            }
        }
    }
}

/// `re.findall(r"/(?:Users|home|var|opt)/[A-Za-z0-9_./\-]+", text)`.
///
/// Hand-rolled — this crate carries no regex dependency. The four roots are
/// mutually exclusive prefixes, so the alternation needs no backtracking, and
/// the character class is greedy with nothing after it: one match runs to the
/// first character outside the class, and the scan resumes there.
#[must_use]
pub fn find_paths(text: &str) -> Vec<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != b'/' {
            index += 1;
            continue;
        }
        let mut matched = None;
        for root in PATH_ROOTS {
            let start = index + 1;
            if !bytes[start..].starts_with(root.as_bytes()) {
                continue;
            }
            let after_root = start + root.len();
            if bytes.get(after_root) != Some(&b'/') {
                continue;
            }
            let mut end = after_root + 1;
            while end < bytes.len() && is_path_byte(bytes[end]) {
                end += 1;
            }
            // The `+` needs at least one character after the second slash.
            if end > after_root + 1 {
                matched = Some(end);
            }
            break;
        }
        match matched {
            Some(end) => {
                out.push(text[index..end].to_string());
                index = end;
            }
            None => index += 1,
        }
    }
    out
}

const fn is_path_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'/' | b'-')
}

/// The deepest directory covering at least half of `paths`
/// (`_derive_workspace_root`).
///
/// Every ancestor of every path is a candidate; each is scored by how many input
/// paths it contains, and the winner is the highest coverage, then the longest,
/// then the alphabetically last — the tie-breaks Python's `sort(reverse=True)`
/// on `(coverage, len, name)` produces.
#[must_use]
pub fn derive_workspace_root(paths: &[String]) -> Option<String> {
    if paths.is_empty() {
        return None;
    }
    // A workspace is a directory, never a file: drop the basename of anything
    // whose leaf looks like a filename and let the ancestor walk supply the
    // rest.
    let mut candidates: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for path in paths {
        // `p.rsplit("/", 1)` on a slash-less string yields the string itself.
        let (head, leaf) = path
            .rsplit_once('/')
            .map_or((path.as_str(), path.as_str()), |(head, leaf)| (head, leaf));
        let mut current = if leaf.contains('.') && !leaf.is_empty() {
            head.to_string()
        } else {
            path.clone()
        };
        if current.is_empty() {
            continue;
        }
        candidates.insert(current.clone());
        loop {
            let parent = current
                .rsplit_once('/')
                .map_or_else(|| current.clone(), |(head, _)| head.to_string());
            if parent.is_empty() || parent == current {
                break;
            }
            current = parent;
            candidates.insert(current.clone());
        }
    }

    // At least half, rounded up — but with one or two paths in total, demand
    // full coverage so a stray reference cannot become the workspace by itself.
    let total = paths.len();
    let threshold = if total <= 2 { total } else { total.div_ceil(2) };
    let mut scored: Vec<(usize, usize, String)> = Vec::new();
    for candidate in candidates {
        // `/Users/foo` is two segments — skip until we are at least one level
        // into the user's filesystem.
        if candidate.trim_matches('/').split('/').count() < MIN_PATH_DEPTH {
            continue;
        }
        let coverage = paths
            .iter()
            .filter(|path| is_ancestor_of(&candidate, path))
            .count();
        if coverage >= threshold {
            scored.push((coverage, candidate.len(), candidate));
        }
    }
    scored.sort_by(|a, b| b.cmp(a));
    scored.into_iter().next().map(|(_, _, name)| name)
}

/// Whether `directory` is an ancestor of (or equal to) `path`
/// (`_is_ancestor_of`).
fn is_ancestor_of(directory: &str, path: &str) -> bool {
    path == directory || path.starts_with(&format!("{}/", directory.trim_end_matches('/')))
}

// ── column helpers ───────────────────────────────────────────────────────────

/// A TEXT / BLOB column as a string, or `None` for any other type.
///
/// DIVERGENCE (fixed-in-rust): `sqlite3` decodes TEXT strictly and raises
/// `OperationalError` on invalid UTF-8, which aborts the whole read; this
/// replaces the bad bytes and keeps going, exactly as Python's own
/// `decode(errors="replace")` does one line later for BLOBs.
fn column_text(row: &rusqlite::Row<'_>, index: usize) -> Option<String> {
    match row.get_ref(index).ok()? {
        rusqlite::types::ValueRef::Text(bytes) | rusqlite::types::ValueRef::Blob(bytes) => {
            Some(String::from_utf8_lossy(bytes).into_owned())
        }
        _ => None,
    }
}

/// Parse a `value` column into a JSON object (`_safe_json_loads`).
///
/// Tolerates TEXT and BLOB; anything else (an integer, a real, NULL) is `None`,
/// and so is JSON that decodes to something other than an object.
fn safe_json_object(row: &rusqlite::Row<'_>, index: usize) -> Option<Map<String, Value>> {
    let bytes = match row.get_ref(index).ok()? {
        rusqlite::types::ValueRef::Text(bytes) | rusqlite::types::ValueRef::Blob(bytes) => bytes,
        _ => return None,
    };
    match jsonl::parse_json(String::from_utf8_lossy(bytes).as_bytes()) {
        Some(Value::Object(map)) => Some(map),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn object(value: Value) -> Map<String, Value> {
        value.as_object().expect("object").clone()
    }

    #[test]
    fn vscdb_path_branches_per_platform() {
        let home = Path::new("/home/me");
        assert_eq!(
            default_vscdb_path(Platform::Linux, Some(home), None),
            Path::new("/home/me/.config/Cursor/User/globalStorage/state.vscdb")
        );
        assert_eq!(
            default_vscdb_path(Platform::MacOs, Some(home), None),
            Path::new("/home/me/Library/Application Support/Cursor/User/globalStorage/state.vscdb")
        );
        assert_eq!(
            default_vscdb_path(
                Platform::Windows,
                Some(home),
                Some(OsStr::new("C:/AppData"))
            ),
            Path::new("C:/AppData/Cursor/User/globalStorage/state.vscdb")
        );
    }

    #[test]
    fn conversation_ids_come_from_v3_keys_and_nowhere_else() {
        assert_eq!(
            conversation_id_from_key("bubbleId:conv-1:bubble-9").as_deref(),
            Some("conv-1")
        );
        assert_eq!(
            conversation_id_from_key("agentKv:blob:conv-2:k1").as_deref(),
            Some("conv-2")
        );
        // A legacy single-segment key keeps the id in the JSON value.
        assert_eq!(conversation_id_from_key("bubbleId:b1"), None);
        assert_eq!(conversation_id_from_key("bubbleId::b1"), None);
        assert_eq!(conversation_id_from_key("other:conv-3:x"), None);
    }

    #[test]
    fn a_bubble_type_is_compared_the_way_python_compares_it() {
        let user = object(json!({"type": 1}));
        assert_eq!(role_from_payload(&user, true).as_deref(), Some("user"));
        let assistant = object(json!({"type": 2}));
        assert_eq!(
            role_from_payload(&assistant, true).as_deref(),
            Some("assistant")
        );
        // `True == 1` and `1.0 == 1` are both true in Python.
        assert_eq!(
            role_from_payload(&object(json!({"type": true})), true).as_deref(),
            Some("user")
        );
        assert_eq!(
            role_from_payload(&object(json!({"type": 1.0})), true).as_deref(),
            Some("user")
        );
        assert_eq!(role_from_payload(&object(json!({"type": 3})), true), None);
        assert_eq!(role_from_payload(&object(json!({"type": "1"})), true), None);
        // An agent blob's role is whatever string it carries.
        assert_eq!(
            role_from_payload(&object(json!({"role": "tool"})), false).as_deref(),
            Some("tool")
        );
        assert_eq!(role_from_payload(&object(json!({"role": ""})), false), None);
    }

    #[test]
    fn tokens_prefer_explicit_counts_and_estimate_otherwise() {
        let explicit = object(json!({"tokenCount": {"inputTokens": 120, "outputTokens": 480}}));
        assert_eq!(
            tokens_from_payload(&explicit, "ignored"),
            PayloadTokens {
                input: 120,
                output: 480,
                estimated: false
            }
        );
        // Cursor v3 writes zeros on every bubble — that is the estimate path.
        let zeros = object(json!({"tokenCount": {"inputTokens": 0, "outputTokens": 0}}));
        assert_eq!(
            tokens_from_payload(&zeros, "12345678"),
            PayloadTokens {
                input: 2,
                output: 0,
                estimated: true
            }
        );
        // Garbage counts must degrade to the estimate, never raise.
        let garbage = object(json!({"tokenCount": {"inputTokens": "x", "outputTokens": [1]}}));
        assert_eq!(tokens_from_payload(&garbage, "abcd").input, 1);
        // `len()` counts characters, not bytes.
        assert_eq!(tokens_from_payload(&Map::new(), "日本語です").input, 1);
    }

    #[test]
    fn model_falls_back_to_cursor_auto() {
        assert_eq!(
            model_from_payload(&object(json!({"modelInfo": {"modelName": "gpt-5"}})), true),
            "gpt-5"
        );
        assert_eq!(
            model_from_payload(
                &object(json!({"providerOptions": {"cursor": {"modelName": "claude"}}})),
                false
            ),
            "claude"
        );
        assert_eq!(model_from_payload(&Map::new(), true), DEFAULT_MODEL);
        assert_eq!(
            model_from_payload(&object(json!({"modelInfo": "not an object"})), true),
            DEFAULT_MODEL
        );
    }

    #[test]
    fn timestamps_accept_ms_epoch_and_iso_and_fall_back_to_now() {
        assert_eq!(
            normalize_timestamp(Some(&json!(1_714_000_000_000_i64))),
            "2024-04-24T23:06:40+00:00"
        );
        assert_eq!(
            normalize_timestamp(Some(&json!("2026-04-29T10:00:00Z"))),
            "2026-04-29T10:00:00+00:00"
        );
        assert_eq!(
            normalize_timestamp(Some(&json!("2026-04-29T10:00:00+02:00"))),
            "2026-04-29T10:00:00+02:00",
            "an offset is preserved, not converted"
        );
        assert_eq!(
            normalize_timestamp(Some(&json!("2026-04-29T10:00:00.5"))),
            "2026-04-29T10:00:00.500000+00:00"
        );
        assert_eq!(
            normalize_timestamp(Some(&json!("2026-04-29"))),
            "2026-04-29T00:00:00+00:00"
        );
        assert_eq!(
            normalize_timestamp(Some(&json!("1714000000000"))),
            "2024-04-24T23:06:40+00:00",
            "a numeric string is milliseconds"
        );
        // Every fallback is wall-clock: assert the shape, which is all that can
        // be asserted about `datetime.now()`.
        for garbage in [
            json!(null),
            json!(""),
            json!([1]),
            json!({"a": 1}),
            json!("nope"),
        ] {
            let stamp = normalize_timestamp(Some(&garbage));
            assert!(
                stamp.ends_with("+00:00") && stamp.len() >= 25,
                "{garbage} produced {stamp}"
            );
        }
        assert!(normalize_timestamp(None).ends_with("+00:00"));
    }

    #[test]
    fn isoformat_rejects_what_python_rejects() {
        assert_eq!(parse_isoformat("2026-13-01"), None, "month 13");
        assert_eq!(parse_isoformat("2026-02-30"), None, "february 30");
        assert_eq!(
            parse_isoformat("2024-02-29").as_deref(),
            Some("2024-02-29T00:00:00+00:00")
        );
        assert_eq!(parse_isoformat("2026-01-01T24:00:00"), None, "hour 24");
        assert_eq!(parse_isoformat("2026-01-01T00:60:00"), None);
        assert_eq!(parse_isoformat("20260101"), None, "basic format");
        assert_eq!(parse_isoformat("nope"), None);
        // A space separator is accepted, as `fromisoformat` accepts any.
        assert_eq!(
            parse_isoformat("2026-01-01 05:06:07").as_deref(),
            Some("2026-01-01T05:06:07+00:00")
        );
        // Fractions past microseconds truncate.
        assert_eq!(
            parse_isoformat("2026-01-01T00:00:00.1234567").as_deref(),
            Some("2026-01-01T00:00:00.123456+00:00")
        );
    }

    #[test]
    fn path_sweep_matches_the_python_regex() {
        assert_eq!(
            find_paths(r#"{"target_file": "/Users/me/proj/a.ts", "n": 1}"#),
            vec!["/Users/me/proj/a.ts".to_string()]
        );
        assert_eq!(
            find_paths("/home/me/x /opt/y /var/z /etc/nope /homeless/nope"),
            vec![
                "/home/me/x".to_string(),
                "/opt/y".to_string(),
                "/var/z".to_string(),
            ]
        );
        // The class stops at the first character outside it.
        assert_eq!(find_paths("/Users/me/a b"), vec!["/Users/me/a".to_string()]);
        // A root with nothing after the second slash is not a match.
        assert_eq!(find_paths("/Users/"), Vec::<String>::new());
        assert_eq!(find_paths("/Users"), Vec::<String>::new());
    }

    #[test]
    fn workspace_root_is_the_deepest_directory_covering_half() {
        let paths = vec![
            "/Users/me/proj/src/a.ts".to_string(),
            "/Users/me/proj/src/b.ts".to_string(),
            "/Users/me/proj/README.md".to_string(),
        ];
        assert_eq!(
            derive_workspace_root(&paths).as_deref(),
            Some("/Users/me/proj")
        );
        // One stray reference cannot become the workspace on its own.
        assert_eq!(
            derive_workspace_root(&[
                "/Users/me/proj/a.ts".to_string(),
                "/Users/other/thing/b.ts".to_string(),
            ]),
            None
        );
        // Nothing above the user directory qualifies.
        assert_eq!(
            derive_workspace_root(&["/Users/me/a.ts".to_string()]),
            None,
            "/Users/me is two segments — below _MIN_PATH_DEPTH"
        );
        assert_eq!(derive_workspace_root(&[]), None);
    }

    #[test]
    fn a_bubble_yields_every_path_shape_cursor_writes() {
        let bubble = object(json!({
            "context": {
                "fileSelections": [
                    {"uri": {"fsPath": "/Users/me/proj/a.ts", "path": "/Users/me/proj/a.ts"}},
                    {"uri": {"fsPath": "relative/no"}},
                    "not an object",
                ],
                "mentions": {
                    "folderSelections": {"file:///Users/me/proj/src": {}},
                    "fileSelections": {"file:///Users/me/proj/b.ts": {}, "nope": {}},
                },
            },
            "attachedFoldersNew": [
                {"uri": {"path": "/Users/me/proj/docs"}},
                {"path": "/Users/me/proj/e2e"},
            ],
            "toolFormerData": {"rawArgs": "{\"f\": \"/Users/me/proj/c.ts\"}", "params": 7},
        }));
        let mut paths = Vec::new();
        paths_in_bubble(&bubble, &mut paths);
        assert_eq!(
            paths,
            vec![
                "/Users/me/proj/a.ts".to_string(),
                "/Users/me/proj/a.ts".to_string(),
                "/Users/me/proj/b.ts".to_string(),
                "/Users/me/proj/src".to_string(),
                "/Users/me/proj/docs".to_string(),
                "/Users/me/proj/e2e".to_string(),
                "/Users/me/proj/c.ts".to_string(),
            ]
        );
    }

    #[test]
    fn text_prefers_the_bubble_field_then_the_agent_content() {
        assert_eq!(text_from_payload(&object(json!({"text": "hi"}))), "hi");
        assert_eq!(
            text_from_payload(&object(json!({"text": "", "content": "fallback"}))),
            "fallback"
        );
        assert_eq!(
            text_from_payload(&object(
                json!({"content": [{"text": "a"}, {"text": ""}, "bare", 7]})
            )),
            "a\nbare"
        );
        assert_eq!(text_from_payload(&Map::new()), "");
    }
}
