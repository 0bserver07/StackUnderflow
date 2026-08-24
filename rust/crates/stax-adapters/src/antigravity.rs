//! Google Antigravity — the port of `python-legacy: adapters/antigravity.py`.
//!
//! Antigravity encrypts its per-turn transcripts at rest (`conversations/*.pb`,
//! `implicit/*.pb`, `brain/<uuid>/`) with a key held in the macOS Keychain, and
//! the scheme lives inside a 134 MB Go binary. This adapter surfaces the two
//! surfaces that are **plaintext**:
//!
//! 1. `~/.gemini/antigravity{,-ide}/agyhub_summaries_proto.pb` — repeated
//!    `ConversationSummary` records (uuid, title, timestamps, workspace URI, git
//!    remote), read with a hand-rolled protobuf wire-format decoder so one file
//!    shape does not cost a protobuf dependency.
//! 2. `~/.gemini/antigravity-cli/history.jsonl` — one line per user prompt, the
//!    only place prompt text is readable.
//!
//! Every emitted record carries `raw["cost_source"] = "encrypted"` so the cost
//! layer can render an explicit "tokens unavailable" state rather than guessing
//! dollars from content length.
//!
//! ## Why the refs say `database`
//!
//! One summary file yields *many* sessions. File-mode dedup is per file and
//! would collapse them into one, so the refs declare
//! [`SourceKind::Database`](crate::base::SourceKind::Database) and the ingest
//! layer keys on `(file_path, session_id)` with `seq` as the watermark. `seq`
//! here is an event index, not a byte offset — which is exactly what the
//! one-number resume contract was collapsed into.

use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::base::{Record, SessionRef, SourceAdapter, SourceKind, Speed, stat_ref_fields};
use crate::jsonl::{JsonlLines, parse_json, py_bytes_strip};
use crate::{pytime, pyval};

/// The provider key.
pub const NAME: &str = "antigravity";

/// The two IDE surfaces that share a data shape (`_IDE_ROOTS`).
pub const IDE_DIRS: [&str; 2] = ["antigravity", "antigravity-ide"];

/// The CLI surface (`_CLI_ROOT`).
pub const CLI_DIR: &str = "antigravity-cli";

/// The summary file's basename (`_SUMMARY_BASENAME`).
pub const SUMMARY_BASENAME: &str = "agyhub_summaries_proto.pb";

/// The CLI history file's basename (`_HISTORY_BASENAME`).
pub const HISTORY_BASENAME: &str = "history.jsonl";

// ── minimal protobuf wire-format reader ──────────────────────────────────────

const WIRE_VARINT: u64 = 0;
const WIRE_FIXED64: u64 = 1;
const WIRE_LEN_DELIM: u64 = 2;
const WIRE_FIXED32: u64 = 5;

/// One decoded protobuf field value.
///
/// Length-delimited values stay raw bytes — the caller decides whether they are
/// UTF-8 or a sub-message, exactly as the Python decoder's `bytes` return does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WireValue {
    /// A varint, fixed32, or fixed64.
    Int(u64),
    /// A length-delimited run of bytes.
    Bytes(Vec<u8>),
}

/// One protobuf message, decoded to `{field_number: [values…]}`
/// (`_decode_fields`).
///
/// `None` is the `ValueError` branch every caller catches: a truncated varint,
/// a varint over 64 bits, or an unsupported wire type. A *short* fixed-width or
/// length-delimited field is not an error in the Python original — the slice
/// simply comes back short and the cursor runs off the end, ending the loop —
/// and it is not one here either.
#[must_use]
pub fn decode_fields(buf: &[u8]) -> Option<Vec<(u64, WireValue)>> {
    let mut out: Vec<(u64, WireValue)> = Vec::new();
    let mut pos = 0_usize;
    while pos < buf.len() {
        let (tag, next) = read_varint(buf, pos)?;
        pos = next;
        let field = tag >> 3;
        let value = match tag & 7 {
            WIRE_VARINT => {
                let (value, next) = read_varint(buf, pos)?;
                pos = next;
                WireValue::Int(value)
            }
            WIRE_FIXED64 => {
                let value = little_endian(buf, pos, 8);
                pos = pos.saturating_add(8);
                WireValue::Int(value)
            }
            WIRE_LEN_DELIM => {
                let (length, next) = read_varint(buf, pos)?;
                pos = next;
                let length = usize::try_from(length).unwrap_or(usize::MAX);
                let end = pos.saturating_add(length).min(buf.len());
                let value = WireValue::Bytes(buf[pos.min(buf.len())..end].to_vec());
                pos = pos.saturating_add(length);
                value
            }
            WIRE_FIXED32 => {
                let value = little_endian(buf, pos, 4);
                pos = pos.saturating_add(4);
                WireValue::Int(value)
            }
            // "unsupported wire type" — the ValueError branch.
            _ => return None,
        };
        out.push((field, value));
    }
    Some(out)
}

/// `_read_varint` — `None` for the truncated / over-long `ValueError` branch.
fn read_varint(buf: &[u8], mut pos: usize) -> Option<(u64, usize)> {
    let mut result = 0_u64;
    let mut shift = 0_u32;
    loop {
        let byte = *buf.get(pos)?;
        pos += 1;
        result |= u64::from(byte & 0x7F) << shift;
        if byte & 0x80 == 0 {
            return Some((result, pos));
        }
        shift += 7;
        if shift > 63 {
            return None;
        }
    }
}

/// `int.from_bytes(buf[pos:pos+width], "little")` — a short slice is not an
/// error, it is a smaller number.
fn little_endian(buf: &[u8], pos: usize, width: usize) -> u64 {
    let end = pos.saturating_add(width).min(buf.len());
    let mut value = 0_u64;
    for (index, byte) in buf[pos.min(buf.len())..end].iter().enumerate() {
        value |= u64::from(*byte) << (8 * index);
    }
    value
}

/// The first value for `field`, if any.
fn field_value(fields: &[(u64, WireValue)], field: u64) -> Option<&WireValue> {
    fields
        .iter()
        .find(|(number, _)| *number == field)
        .map(|(_, value)| value)
}

/// `_maybe_str` — a length-delimited field decoded as strict UTF-8.
fn maybe_str(fields: &[(u64, WireValue)], field: u64) -> Option<String> {
    match field_value(fields, field)? {
        WireValue::Bytes(bytes) => String::from_utf8(bytes.clone()).ok(),
        WireValue::Int(_) => None,
    }
}

/// `_maybe_int`.
fn maybe_int(fields: &[(u64, WireValue)], field: u64) -> Option<u64> {
    match field_value(fields, field)? {
        WireValue::Int(value) => Some(*value),
        WireValue::Bytes(_) => None,
    }
}

/// `_maybe_submsg`.
fn maybe_submsg(fields: &[(u64, WireValue)], field: u64) -> Option<&[u8]> {
    match field_value(fields, field)? {
        WireValue::Bytes(bytes) => Some(bytes),
        WireValue::Int(_) => None,
    }
}

/// A `google.protobuf.Timestamp` sub-message → whole Unix seconds
/// (`_read_timestamp`).
///
/// `nanos` (field 2) is dropped: downstream only ever renders second precision.
fn read_timestamp(submsg: Option<&[u8]>) -> Option<i64> {
    let fields = decode_fields(submsg?)?;
    maybe_int(&fields, 1).map(|seconds| i64::try_from(seconds).unwrap_or(i64::MAX))
}

// ── summary file parser ──────────────────────────────────────────────────────

/// One decoded `ConversationSummary` (`_ConversationMeta`).
///
/// The field map was recovered with `protoc --decode_raw` against real files:
///
/// ```text
/// Top                { repeated ConversationSummary entries = 1; }
/// ConversationSummary{ string uuid = 1; ConversationData data = 2; }
/// ConversationData   { string title = 1; Timestamp last_updated = 3;
///                      Timestamp started = 7; WorkspaceInfo workspace = 9;
///                      Timestamp last_activity = 10; }
/// WorkspaceInfo      { string uri = 1; GitInfo git = 3; string branch = 4; }
/// GitInfo            { string repo_path = 1; string remote_url = 2; }
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConversationMeta {
    /// The conversation UUID — the session id.
    pub uuid: String,
    /// The conversation title, when the summary carries one.
    pub title: Option<String>,
    /// Start time, whole Unix seconds.
    pub started_at: Option<i64>,
    /// Last-activity time, whole Unix seconds.
    pub last_at: Option<i64>,
    /// The workspace `file://` URI as written.
    pub workspace_uri: Option<String>,
    /// The workspace URI resolved to a filesystem path.
    pub workspace_path: Option<String>,
    /// The git remote URL.
    pub git_remote: Option<String>,
    /// The git branch.
    pub branch: Option<String>,
}

/// Decode the top-level summaries file (`_parse_summaries`).
///
/// Every failure — unreadable file, malformed top-level message, a malformed
/// entry — degrades to fewer conversations, never to an error.
#[must_use]
pub fn parse_summaries(path: &Path) -> Vec<ConversationMeta> {
    // LOG: python warns "Cannot read Antigravity summary %s".
    let Ok(data) = std::fs::read(path) else {
        return Vec::new();
    };
    // LOG: python warns "Antigravity summary %s is malformed".
    let Some(top) = decode_fields(&data) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (number, value) in &top {
        if *number != 1 {
            continue;
        }
        let WireValue::Bytes(entry) = value else {
            continue;
        };
        let Some(conv_fields) = decode_fields(entry) else {
            continue;
        };
        let mut meta = ConversationMeta {
            uuid: maybe_str(&conv_fields, 1).unwrap_or_default(),
            ..ConversationMeta::default()
        };
        if meta.uuid.is_empty() {
            continue;
        }
        let Some(data_sub) = maybe_submsg(&conv_fields, 2) else {
            out.push(meta);
            continue;
        };
        let Some(data_fields) = decode_fields(data_sub) else {
            out.push(meta);
            continue;
        };
        meta.title = maybe_str(&data_fields, 1);
        meta.started_at = read_timestamp(maybe_submsg(&data_fields, 7));
        // `A or B` in Python: a zero timestamp is falsy and falls through to
        // the older `last_updated` field, exactly like a missing one.
        meta.last_at = read_timestamp(maybe_submsg(&data_fields, 10))
            .filter(|seconds| *seconds != 0)
            .or_else(|| read_timestamp(maybe_submsg(&data_fields, 3)));

        if let Some(ws_sub) = maybe_submsg(&data_fields, 9) {
            let ws_fields = decode_fields(ws_sub).unwrap_or_default();
            meta.workspace_uri = maybe_str(&ws_fields, 1);
            if let Some(uri) = meta.workspace_uri.as_deref().filter(|uri| !uri.is_empty()) {
                meta.workspace_path = path_from_file_uri(uri);
            }
            meta.branch = maybe_str(&ws_fields, 4);
            if let Some(git_sub) = maybe_submsg(&ws_fields, 3) {
                let git_fields = decode_fields(git_sub).unwrap_or_default();
                meta.git_remote = maybe_str(&git_fields, 2);
            }
        }
        out.push(meta);
    }
    out
}

/// One conversation's CLI-history summary (`_scan_cli_history`'s value).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CliMeta {
    /// The workspace path, when the entry recorded a string one.
    pub workspace: Option<String>,
    /// Earliest timestamp seen, whole Unix seconds.
    pub first_ts: Option<i64>,
    /// Latest timestamp seen, whole Unix seconds.
    pub last_ts: Option<i64>,
}

/// Group `history.jsonl` by `conversationId`, in first-seen order
/// (`_scan_cli_history`).
#[must_use]
pub fn scan_cli_history(path: &Path) -> Vec<(String, CliMeta)> {
    let mut grouped: Vec<(String, CliMeta)> = Vec::new();
    for (_, raw_line) in JsonlLines::open(path, 0) {
        let stripped = py_bytes_strip(&raw_line);
        if stripped.is_empty() {
            continue;
        }
        let Some(obj) = parse_json(stripped) else {
            continue;
        };
        // A non-object line must not crash enumerate().
        let Some(map) = obj.as_object() else {
            continue;
        };
        let Some(uuid) = map
            .get("conversationId")
            .and_then(Value::as_str)
            .filter(|uuid| !uuid.is_empty())
        else {
            continue;
        };
        let ts_s = py_int(map.get("timestamp")).map(|ms| ms.div_euclid(1000));
        let index = match grouped.iter().position(|(key, _)| key == uuid) {
            Some(index) => index,
            None => {
                grouped.push((
                    uuid.to_string(),
                    CliMeta {
                        // A non-string workspace would crash `_slug_for`
                        // downstream, so it is dropped here.
                        workspace: map
                            .get("workspace")
                            .and_then(Value::as_str)
                            .map(ToString::to_string),
                        first_ts: ts_s,
                        last_ts: ts_s,
                    },
                ));
                continue;
            }
        };
        if let Some(ts_s) = ts_s {
            let entry = &mut grouped[index].1;
            if entry.first_ts.is_none_or(|first| ts_s < first) {
                entry.first_ts = Some(ts_s);
            }
            if entry.last_ts.is_none_or(|last| ts_s > last) {
                entry.last_ts = Some(ts_s);
            }
        }
    }
    grouped
}

/// `isinstance(value, int)` — which in Python includes `bool` and excludes
/// `float`.
fn py_int(value: Option<&Value>) -> Option<i64> {
    match value? {
        Value::Bool(flag) => Some(i64::from(*flag)),
        Value::Number(number) => number.as_i64(),
        _ => None,
    }
}

// ── adapter ──────────────────────────────────────────────────────────────────

/// The Antigravity source adapter (`AntigravityAdapter`).
#[derive(Debug, Clone)]
pub struct AntigravityAdapter {
    home: PathBuf,
}

impl Default for AntigravityAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl AntigravityAdapter {
    /// `~/.gemini`, from the live environment.
    #[must_use]
    pub fn new() -> Self {
        #[allow(
            deprecated,
            reason = "std::env::home_dir is the platform-correct answer on the \
            1.97.1 pin; the claude and codex adapters carry the same allow"
        )]
        let home = std::env::home_dir().unwrap_or_default();
        Self {
            home: home.join(".gemini"),
        }
    }

    /// Inject the Gemini home — `AntigravityAdapter(gemini_home=…)`.
    #[must_use]
    pub fn with_gemini_home(home: impl Into<PathBuf>) -> Self {
        Self { home: home.into() }
    }

    /// The Gemini home this adapter reads.
    #[must_use]
    pub fn gemini_home(&self) -> &Path {
        &self.home
    }

    /// `<home>/antigravity-cli/history.jsonl`.
    fn history_path(&self) -> PathBuf {
        self.home.join(CLI_DIR).join(HISTORY_BASENAME)
    }
}

impl SourceAdapter for AntigravityAdapter {
    fn name(&self) -> &str {
        NAME
    }

    fn enumerate(&self) -> Vec<SessionRef> {
        let cwd = current_dir_string();
        let mut out = Vec::new();
        let mut seen: Vec<String> = Vec::new();

        // The CLI history first: it builds the uuid → workspace lookup the IDE
        // summaries fall back on for conversations that only ever ran in the CLI.
        let history = self.history_path();
        let history_is_file = history.is_file();
        let history_stat = if history_is_file {
            stat_ref_fields(&history)
        } else {
            None
        };
        let cli_meta = if history_stat.is_some() {
            scan_cli_history(&history)
        } else {
            Vec::new()
        };
        let history_value = if history_is_file {
            Value::from(history.to_string_lossy().into_owned())
        } else {
            Value::Null
        };

        for dir in IDE_DIRS {
            let summary = self.home.join(dir).join(SUMMARY_BASENAME);
            if !summary.is_file() {
                continue;
            }
            // Python warns and continues on a stat failure.
            let Some((mtime, size)) = stat_ref_fields(&summary) else {
                continue;
            };
            for conv in parse_summaries(&summary) {
                if seen.contains(&conv.uuid) {
                    continue;
                }
                seen.push(conv.uuid.clone());
                // Workspace fallback: summary > CLI history > the literal.
                let mut workspace = conv.workspace_path.clone().filter(|path| !path.is_empty());
                if workspace.is_none()
                    && let Some((_, meta)) = cli_meta.iter().find(|(uuid, _)| *uuid == conv.uuid)
                {
                    workspace = meta.workspace.clone().filter(|path| !path.is_empty());
                }
                let mut hint = Map::new();
                hint.insert("title".into(), optional_string(conv.title.clone()));
                hint.insert("started_at".into(), optional_int(conv.started_at));
                hint.insert("last_at".into(), optional_int(conv.last_at));
                hint.insert(
                    "workspace_uri".into(),
                    optional_string(conv.workspace_uri.clone()),
                );
                hint.insert(
                    "git_remote".into(),
                    optional_string(conv.git_remote.clone()),
                );
                hint.insert("branch".into(), optional_string(conv.branch.clone()));
                hint.insert("history_jsonl".into(), history_value.clone());
                out.push(SessionRef {
                    provider: NAME.to_string(),
                    project_slug: slug_for(workspace.as_deref(), &cwd),
                    session_id: conv.uuid,
                    file_path: summary.clone(),
                    file_mtime: mtime,
                    file_size: size,
                    source_kind: SourceKind::Database,
                    source_hint: Some(hint),
                });
            }
        }

        // Pure-CLI conversations, i.e. the ones with no summary entry.
        if let Some((mtime, size)) = history_stat {
            for (uuid, meta) in &cli_meta {
                if seen.iter().any(|other| other == uuid) {
                    continue;
                }
                seen.push(uuid.clone());
                let mut hint = Map::new();
                hint.insert("title".into(), Value::Null);
                hint.insert("started_at".into(), optional_int(meta.first_ts));
                hint.insert("last_at".into(), optional_int(meta.last_ts));
                hint.insert("workspace_uri".into(), Value::Null);
                hint.insert("git_remote".into(), Value::Null);
                hint.insert("branch".into(), Value::Null);
                hint.insert(
                    "history_jsonl".into(),
                    Value::from(history.to_string_lossy().into_owned()),
                );
                out.push(SessionRef {
                    provider: NAME.to_string(),
                    project_slug: slug_for(
                        meta.workspace.as_deref().filter(|path| !path.is_empty()),
                        &cwd,
                    ),
                    session_id: uuid.clone(),
                    file_path: history.clone(),
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
        let empty = Map::new();
        let hint = session.source_hint.as_ref().unwrap_or(&empty);
        let mut records: Vec<Record> = Vec::new();

        // The synthetic title marker gives the UI something to label the
        // conversation with even when no CLI prompt exists to show.
        if let Some(title) = hint.get("title").filter(|value| pyval::py_truthy(value)) {
            let started_at = hint
                .get("started_at")
                .filter(|value| pyval::py_truthy(value));
            let timestamp = started_at
                .and_then(|value| py_int(Some(value)))
                .and_then(pytime::from_timestamp_secs_iso)
                .unwrap_or_default();
            records.push(make_record(
                session,
                0,
                timestamp,
                format!("[antigravity title] {}", pyval::py_str(title)),
            ));
        }

        let history = self.history_path();
        if history.is_file() {
            let mut seq = 1_i64;
            // LOG: python warns "Cannot read Antigravity history %s".
            for (_, raw_line) in JsonlLines::open(&history, 0) {
                let stripped = py_bytes_strip(&raw_line);
                if stripped.is_empty() {
                    continue;
                }
                let Some(obj) = parse_json(stripped) else {
                    continue;
                };
                // Valid JSON that is not an object cannot be a history entry.
                let Some(map) = obj.as_object() else {
                    continue;
                };
                if map.get("conversationId").and_then(Value::as_str) != Some(&session.session_id) {
                    continue;
                }
                let timestamp = py_int(map.get("timestamp"))
                    .map(|ms| ms.div_euclid(1000))
                    .and_then(pytime::from_timestamp_secs_iso)
                    .unwrap_or_default();
                let content = map
                    .get("display")
                    .filter(|value| pyval::py_truthy(value))
                    .map_or_else(String::new, pyval::py_str);
                records.push(make_record(session, seq, timestamp, content));
                seq += 1;
            }
        }

        // `since_offset == 0` means "fresh read, yield everything"; otherwise
        // the caller already saw the record at exactly that seq.
        for record in records {
            if since_offset > 0 && record.seq <= since_offset {
                continue;
            }
            sink(record);
        }
    }

    /// The three surfaces, existent or not (`watch_paths`).
    ///
    /// Parent directories rather than the files themselves, so a *new*
    /// conversation is picked up as well as an edit to an existing one. The
    /// watcher filters non-existent roots.
    fn watch_paths(&self) -> Vec<PathBuf> {
        vec![
            self.home.join(IDE_DIRS[0]),
            self.home.join(IDE_DIRS[1]),
            self.home.join(CLI_DIR),
        ]
    }
}

/// One plaintext marker record (`_make_record`).
fn make_record(session: &SessionRef, seq: i64, timestamp: String, content: String) -> Record {
    let mut raw = Map::new();
    raw.insert("cost_source".to_string(), Value::from("encrypted"));
    raw.insert(
        "source_hint".to_string(),
        session
            .source_hint
            .clone()
            .map_or_else(|| Value::Object(Map::new()), Value::Object),
    );
    Record {
        provider: session.provider.clone(),
        session_id: session.session_id.clone(),
        seq,
        timestamp,
        role: "user".to_string(),
        model: None,
        input_tokens: 0,
        output_tokens: 0,
        cache_create_tokens: 0,
        cache_read_tokens: 0,
        content_text: content,
        tools: Vec::new(),
        cwd: None,
        is_sidechain: false,
        uuid: format!("{}:{seq}", session.session_id),
        parent_uuid: None,
        raw: Value::Object(raw),
        speed: Speed::Standard,
    }
}

/// `file:///abs/path` → `/abs/path` (`_path_from_file_uri`).
#[must_use]
pub fn path_from_file_uri(uri: &str) -> Option<String> {
    let rest = uri.strip_prefix("file://")?;
    // `urlparse` ends the netloc at the first `/`, `?` or `#`; the path runs
    // from there to the first `?` or `#`.
    let path = match rest.find(['/', '?', '#']) {
        Some(index) if rest.as_bytes()[index] == b'/' => {
            let tail = &rest[index..];
            tail.split(['?', '#']).next().unwrap_or("")
        }
        _ => "",
    };
    let path = unquote(path);
    if path.is_empty() { None } else { Some(path) }
}

/// `urllib.parse.unquote(text, errors="replace")`.
///
/// Consecutive `%XX` escapes are decoded as one byte run and then interpreted
/// as UTF-8 with replacement, which is what makes a percent-encoded multi-byte
/// character survive. A `%` not followed by two hex digits stays literal.
fn unquote(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut buffer: Vec<u8> = Vec::new();
    let mut index = 0_usize;
    while index < bytes.len() {
        if bytes[index] == b'%'
            && let (Some(high), Some(low)) = (
                bytes.get(index + 1).and_then(|b| hex_value(*b)),
                bytes.get(index + 2).and_then(|b| hex_value(*b)),
            )
        {
            buffer.push(high * 16 + low);
            index += 3;
            continue;
        }
        if !buffer.is_empty() {
            out.push_str(&String::from_utf8_lossy(&buffer));
            buffer.clear();
        }
        // Push one whole character, not one byte: `index` always sits on a
        // character boundary here because escapes are ASCII.
        let ch = text[index..].chars().next().unwrap_or('\u{fffd}');
        out.push(ch);
        index += ch.len_utf8();
    }
    if !buffer.is_empty() {
        out.push_str(&String::from_utf8_lossy(&buffer));
    }
    out
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// The claude-family slug, or the literal provider name (`_slug_for`).
fn slug_for(project_path: Option<&str>, cwd: &str) -> String {
    match project_path.filter(|path| !path.is_empty()) {
        Some(path) => pyval::slug_for(path, cwd),
        None => NAME.to_string(),
    }
}

fn optional_string(value: Option<String>) -> Value {
    value.map_or(Value::Null, Value::from)
}

fn optional_int(value: Option<i64>) -> Value {
    value.map_or(Value::Null, Value::from)
}

/// `os.getcwd()` for the slug derivation; `"/"` when the process has no cwd.
fn current_dir_string() -> String {
    std::env::current_dir().map_or_else(
        |_| "/".to_string(),
        |path| path.to_string_lossy().into_owned(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a length-delimited protobuf field.
    fn len_field(number: u64, payload: &[u8]) -> Vec<u8> {
        let mut out = varint((number << 3) | WIRE_LEN_DELIM);
        out.extend(varint(payload.len() as u64));
        out.extend_from_slice(payload);
        out
    }

    fn varint_field(number: u64, value: u64) -> Vec<u8> {
        let mut out = varint((number << 3) | WIRE_VARINT);
        out.extend(varint(value));
        out
    }

    fn varint(mut value: u64) -> Vec<u8> {
        let mut out = Vec::new();
        loop {
            let byte = u8::try_from(value & 0x7F).expect("masked");
            value >>= 7;
            if value == 0 {
                out.push(byte);
                return out;
            }
            out.push(byte | 0x80);
        }
    }

    #[test]
    fn the_wire_reader_decodes_the_shapes_the_summary_file_uses() {
        let mut buf = len_field(1, b"hello");
        buf.extend(varint_field(2, 300));
        let fields = decode_fields(&buf).expect("decodes");
        assert_eq!(maybe_str(&fields, 1).as_deref(), Some("hello"));
        assert_eq!(maybe_int(&fields, 2), Some(300));
        // Wrong-typed reads are None, not a panic.
        assert_eq!(maybe_int(&fields, 1), None);
        assert_eq!(maybe_str(&fields, 2), None);
        assert_eq!(maybe_submsg(&fields, 1), Some(&b"hello"[..]));
    }

    #[test]
    fn malformed_wire_data_is_none_rather_than_a_panic() {
        // A varint with the continuation bit set and no successor.
        assert_eq!(decode_fields(&[0x08, 0x80]), None);
        // An unsupported wire type (3 = group start, removed from proto3).
        assert_eq!(decode_fields(&[0x0b]), None);
        // A length-delimited field claiming more bytes than exist is short,
        // not fatal — the Python slice does the same.
        let short = decode_fields(&[0x0a, 0x7f, b'a']).expect("short slice is tolerated");
        assert_eq!(short, vec![(1, WireValue::Bytes(b"a".to_vec()))]);
    }

    #[test]
    fn a_whole_summary_entry_round_trips() {
        let git = len_field(2, b"git@github.com:me/app.git");
        let mut workspace = len_field(1, b"file:///Users/me/my%20app");
        workspace.extend(len_field(3, &git));
        workspace.extend(len_field(4, b"main"));
        let mut data = len_field(1, b"A title");
        data.extend(len_field(7, &varint_field(1, 1_745_596_800)));
        data.extend(len_field(9, &workspace));
        data.extend(len_field(10, &varint_field(1, 1_745_596_900)));
        let mut entry = len_field(1, b"uuid-1");
        entry.extend(len_field(2, &data));
        let top = len_field(1, &entry);

        let dir = std::env::temp_dir().join(format!("stax-antigravity-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let path = dir.join("agyhub_summaries_proto.pb");
        std::fs::write(&path, &top).expect("write");

        let convs = parse_summaries(&path);
        assert_eq!(convs.len(), 1);
        let conv = &convs[0];
        assert_eq!(conv.uuid, "uuid-1");
        assert_eq!(conv.title.as_deref(), Some("A title"));
        assert_eq!(conv.started_at, Some(1_745_596_800));
        assert_eq!(conv.last_at, Some(1_745_596_900));
        assert_eq!(
            conv.workspace_uri.as_deref(),
            Some("file:///Users/me/my%20app")
        );
        // Percent escapes are decoded, which is the whole point of `unquote`.
        assert_eq!(conv.workspace_path.as_deref(), Some("/Users/me/my app"));
        assert_eq!(conv.branch.as_deref(), Some("main"));
        assert_eq!(
            conv.git_remote.as_deref(),
            Some("git@github.com:me/app.git")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn file_uris_become_paths_and_everything_else_becomes_none() {
        assert_eq!(
            path_from_file_uri("file:///Users/me/app").as_deref(),
            Some("/Users/me/app")
        );
        assert_eq!(
            path_from_file_uri("file://host/Users/me/app").as_deref(),
            Some("/Users/me/app"),
            "the netloc is dropped, as urlparse drops it"
        );
        assert_eq!(
            path_from_file_uri("file:///a/b%C3%A9c").as_deref(),
            Some("/a/béc"),
            "consecutive escapes decode as one UTF-8 run"
        );
        assert_eq!(path_from_file_uri("file://"), None);
        assert_eq!(path_from_file_uri("https://example.com/x"), None);
        assert_eq!(path_from_file_uri("/plain/path"), None);
        // A stray percent stays literal rather than eating the next character.
        assert_eq!(unquote("100%done"), "100%done");
        assert_eq!(unquote("%zz"), "%zz");
    }

    #[test]
    fn the_slug_falls_back_to_the_provider_name() {
        assert_eq!(slug_for(Some("/Users/me/app"), "/cwd"), "-Users-me-app");
        assert_eq!(slug_for(Some(""), "/cwd"), NAME);
        assert_eq!(slug_for(None, "/cwd"), NAME);
    }

    #[test]
    fn an_absent_home_enumerates_empty_rather_than_failing() {
        let adapter = AntigravityAdapter::with_gemini_home("/nonexistent/stax/.gemini");
        assert!(adapter.enumerate().is_empty());
        assert_eq!(adapter.watch_paths().len(), 3);
        // Antigravity declares watch_paths, so source_roots falls back to them.
        assert_eq!(adapter.source_roots(), adapter.watch_paths());
    }
}
