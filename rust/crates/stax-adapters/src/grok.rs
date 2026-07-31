//! xAI's `grok` CLI — the port of `stackunderflow/adapters/grok.py`.
//!
//! Transcripts live at
//! `~/.grok/sessions/<url-encoded-cwd>/<session-uuid>/chat_history.jsonl`. The
//! sessions root is a portable dotfile — no platform branch — and each project
//! directory is the URL-encoded absolute working directory, so
//! `%2FUsers%2Fme%2Fproj` is `/Users/me/proj`. That decoded path goes through
//! *Claude Code's* slug rule ([`slug_for_path`]: every non-alphanumeric
//! character becomes `-`), which is what makes a repo's Grok sessions land under
//! the same project row as its Claude ones.
//!
//! ## Three quirks that shape every record
//!
//! 1. **No token usage anywhere.** Neither the transcript nor its siblings
//!    record token counts, so output tokens are *estimated* at `len(text) // 4`
//!    and every record carries `raw["cost_source"] = "estimated"` for the cost
//!    layer to down-weight.
//! 2. **Encrypted reasoning.** A `reasoning` record keeps its chain-of-thought
//!    in `encrypted_content` and has no `content` at all. Nothing is decrypted,
//!    so the text is empty and the turn estimates to zero tokens rather than
//!    failing.
//! 3. **No per-message timestamp.** One stamp is derived per *session* from the
//!    session directory's UUIDv7 (its top 48 bits are a unix-ms creation time)
//!    and falls back to the transcript's mtime ([`session_timestamp`]).
//!
//! `seq` is the byte offset of each line start, so a resumed read is a `seek`.

use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::base::{
    Record, SessionRef, SourceAdapter, Speed, child_dirs, file_name, home_dir, stat_ref_fields,
};
use crate::jsonl::{JsonlLines, parse_json, py_bytes_strip};
use crate::pyval;

/// The provider key.
pub const NAME: &str = "grok";

/// The only transcript file this adapter reads (`_TRANSCRIPT_NAME`).
///
/// Siblings (`events.jsonl`, `updates.jsonl`, `summary.json`, …) are ignored.
pub const TRANSCRIPT_NAME: &str = "chat_history.jsonl";

/// The only model the v0.2.x CLI ships (`_DEFAULT_MODEL`).
pub const DEFAULT_MODEL: &str = "grok-build";

/// Record `type` → canonical role (`_ROLE_BY_TYPE`).
///
/// A type outside this map (`system`, or anything new) is non-conversational and
/// yields no record. `reasoning` keeps its own role so the normalizer can treat
/// it as a billable assistant-side turn; `tool_result` / `backend_tool_call`
/// become non-billable `tool` rows for transcript fidelity.
pub const ROLE_BY_TYPE: [(&str, &str); 5] = [
    ("user", "user"),
    ("assistant", "assistant"),
    ("reasoning", "reasoning"),
    ("tool_result", "tool"),
    ("backend_tool_call", "tool"),
];

/// Roles whose visible content is model output we estimate tokens for
/// (`_BILLABLE_ROLES`).
pub const BILLABLE_ROLES: [&str; 2] = ["assistant", "reasoning"];

/// Grok tool name → canonical cross-source tool label (`_TOOL_NAME_MAP`).
pub const TOOL_NAME_MAP: [(&str, &str); 11] = [
    ("run_terminal_command", "Bash"),
    ("shell", "Bash"),
    ("exec_command", "Bash"),
    ("read_file", "Read"),
    ("list_dir", "Glob"),
    ("glob", "Glob"),
    ("grep", "Grep"),
    ("search", "Grep"),
    ("edit_file", "Edit"),
    ("write_file", "Edit"),
    ("create_file", "Edit"),
];

/// The Grok source adapter (`GrokAdapter`).
#[derive(Debug, Clone)]
pub struct GrokAdapter {
    root: PathBuf,
}

impl Default for GrokAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl GrokAdapter {
    /// `~/.grok/sessions`, resolved once at construction (`_grok_sessions_root`).
    #[must_use]
    pub fn new() -> Self {
        Self {
            root: home_dir()
                .unwrap_or_default()
                .join(".grok")
                .join("sessions"),
        }
    }

    /// Inject the sessions root — the constructor parameter Python already has.
    #[must_use]
    pub fn with_sessions_root(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The sessions root this adapter reads.
    #[must_use]
    pub fn sessions_root(&self) -> &Path {
        &self.root
    }
}

impl SourceAdapter for GrokAdapter {
    fn name(&self) -> &str {
        NAME
    }

    fn enumerate(&self) -> Vec<SessionRef> {
        if !self.root.is_dir() {
            // Not installed / never used — clean no-op rather than raise.
            return Vec::new();
        }
        let mut out = Vec::new();
        for project_dir in child_dirs(&self.root) {
            let project_slug = project_slug(&file_name(&project_dir));
            for session_dir in child_dirs(&project_dir) {
                let path = session_dir.join(TRANSCRIPT_NAME);
                if !path.is_file() {
                    continue;
                }
                // Python warns and continues on OSError here.
                let Some((mtime, size)) = stat_ref_fields(&path) else {
                    continue;
                };
                out.push(SessionRef::file(
                    NAME,
                    project_slug.clone(),
                    // The session UUID directory name is the session id.
                    file_name(&session_dir),
                    path,
                    mtime,
                    size,
                ));
            }
        }
        out
    }

    fn read_into(&self, session: &SessionRef, since_offset: i64, sink: &mut dyn FnMut(Record)) {
        // One stamp for the whole session: the source has no per-message time.
        let timestamp = session_timestamp(session);
        for (line_offset, raw_line) in JsonlLines::open(&session.file_path, since_offset) {
            if since_offset > 0 && line_offset <= since_offset {
                continue;
            }
            let stripped = py_bytes_strip(&raw_line);
            if stripped.is_empty() {
                continue;
            }
            // LOG: python debug-logs "Skipping malformed Grok JSON line in %s".
            let Some(obj) = parse_json(stripped) else {
                continue;
            };
            if !obj.is_object() {
                continue;
            }
            if let Some(record) = record_from_obj(&obj, session, line_offset, &timestamp) {
                sink(record);
            }
        }
    }

    /// `~/.grok/sessions` (`watch_paths`); a missing root is a clean no-op
    /// upstream, and [`SourceAdapter::source_roots`] falls back to it for
    /// `backup create`.
    fn watch_paths(&self) -> Vec<PathBuf> {
        vec![self.root.clone()]
    }
}

/// One transcript line → a `Record`, or `None` (`_record_from_obj`).
fn record_from_obj(obj: &Value, session: &SessionRef, seq: i64, timestamp: &str) -> Option<Record> {
    let map = obj.as_object()?;
    let kind = map.get("type").and_then(Value::as_str)?;
    let role = ROLE_BY_TYPE
        .iter()
        .find(|(from, _)| *from == kind)
        .map(|(_, role)| *role)?;

    let text = content_text(map);
    // No token usage in the source: estimate output from the visible content
    // for model turns. Encrypted reasoning has no readable text → 0 tokens.
    // User and tool rows carry no usage at all — only model turns are billed.
    let billable = BILLABLE_ROLES.contains(&role);
    let output_tokens = if billable {
        // Python's `len()` counts *characters*, not bytes: a transcript of CJK
        // prose would estimate 3× high on `str::len`.
        i64::try_from(text.chars().count() / 4).unwrap_or(i64::MAX)
    } else {
        0
    };

    let mut raw = map.clone();
    // Mark estimated so the cost layer knows the number is not authoritative.
    // An existing `cost_source` key keeps its position and takes the new value,
    // exactly as Python's dict assignment does.
    raw.insert("cost_source".to_string(), "estimated".into());

    Some(Record {
        provider: NAME.to_string(),
        session_id: session.session_id.clone(),
        seq,
        timestamp: timestamp.to_string(),
        role: role.to_string(),
        model: if billable {
            Some(model_from(map))
        } else {
            None
        },
        input_tokens: 0,
        output_tokens,
        cache_create_tokens: 0,
        cache_read_tokens: 0,
        content_text: text,
        tools: tools_from(map),
        cwd: None,
        is_sidechain: false,
        uuid: map
            .get("id")
            .filter(|value| pyval::py_truthy(value))
            .map_or_else(|| format!("{}:{seq}", session.session_id), pyval::py_str),
        parent_uuid: None,
        raw: Value::Object(raw),
        speed: Speed::Standard,
    })
}

/// `%2FUsers%2Fme%2Fproj` → the Claude-style slug for `/Users/me/proj`
/// (`_project_slug`).
#[must_use]
pub fn project_slug(encoded_dir_name: &str) -> String {
    slug_for_path(&unquote(encoded_dir_name))
}

/// Claude Code's project-directory slug (`claude_teams.slug_for_path`).
///
/// Every non-alphanumeric character becomes `-`, one dash per *character*: the
/// catch-all transform, not the separators-only one that
/// [`crate::pyval::slug_for`] applies. Verified against the real
/// `~/.claude/projects/-Users-yadkonrad--claude`, where the leading `.` of
/// `.claude` is itself a dash.
#[must_use]
pub fn slug_for_path(path: &str) -> String {
    path.chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '-' })
        .collect()
}

/// `urllib.parse.unquote` with UTF-8 and `errors="replace"`.
///
/// Each `%XX` is one byte; the bytes of a *run* of escapes decode together, so
/// `%C3%A9` is one `é` rather than two replacement characters. An escape that is
/// not two hex digits stays literal, as Python's `KeyError` branch leaves it.
///
/// DIVERGENCE (unreachable in practice): Python splits the input into ASCII and
/// non-ASCII runs first, so a `%XX` escape *adjacent* to a literal non-ASCII
/// character decodes separately there and jointly here. A project directory name
/// is a percent-encoded path; mixing the two encodings inside one is not a shape
/// the CLI can produce.
#[must_use]
pub fn unquote(value: &str) -> String {
    if !value.contains('%') {
        return value.to_string();
    }
    let bytes = value.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%'
            && index + 2 < bytes.len()
            && let (Some(high), Some(low)) =
                (hex_value(bytes[index + 1]), hex_value(bytes[index + 2]))
        {
            out.push(high * 16 + low);
            index += 3;
            continue;
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

const fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// The ISO 8601 stamp every record in a session carries (`_session_timestamp`).
///
/// The session directory's UUIDv7 creation time when it parses, else the
/// transcript's mtime.
///
/// DIVERGENCE (fixed-in-rust): an mtime outside `datetime`'s range raises
/// straight out of Python's `read()` generator, killing the file's ingest;
/// here it yields an empty timestamp, which the store already tolerates.
#[must_use]
pub fn session_timestamp(session: &SessionRef) -> String {
    #[allow(
        clippy::cast_precision_loss,
        reason = "a UUIDv7 timestamp is 48 bits — exactly representable in f64"
    )]
    if let Some(millis) = uuidv7_unix_ms(&session.session_id)
        && let Some(stamp) = pyval::epoch_seconds_to_iso(millis as f64 / 1000.0)
    {
        return stamp;
    }
    pyval::epoch_seconds_to_iso(session.file_mtime).unwrap_or_default()
}

/// The unix-ms timestamp in a UUIDv7's top 48 bits, or `None`
/// (`_uuidv7_unix_ms`).
///
/// Accepts every spelling `uuid.UUID()` does — braces, `urn:uuid:`, and dashes
/// anywhere — and returns `None` for anything that is not a *version 7* UUID.
#[must_use]
pub fn uuidv7_unix_ms(session_id: &str) -> Option<i64> {
    let normalized = session_id.replace("urn:", "").replace("uuid:", "");
    let digits: String = normalized
        .trim_matches(|ch| ch == '{' || ch == '}')
        .replace('-', "");
    if digits.len() != 32 || !digits.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let value = u128::from_str_radix(&digits, 16).ok()?;
    if (value >> 76) & 0xF != 7 {
        return None;
    }
    i64::try_from((value >> 80) & 0xFFFF_FFFF_FFFF).ok()
}

/// The readable text of one record (`_content_text`).
///
/// `assistant` / `system` / `tool_result` content is a plain string; `user`
/// content is a list of `{type, text}` parts; `reasoning` carries only
/// `encrypted_content`, so it resolves to the empty string.
#[must_use]
pub fn content_text(obj: &Map<String, Value>) -> String {
    let Some(content) = obj.get("content") else {
        return String::new();
    };
    if let Some(text) = content.as_str() {
        return text.to_string();
    }
    let Some(parts) = content.as_array() else {
        return String::new();
    };
    let mut pieces: Vec<String> = Vec::new();
    for part in parts {
        if let Some(map) = part.as_object() {
            if let Some(text) = map.get("text").and_then(Value::as_str)
                && !text.is_empty()
            {
                pieces.push(text.to_string());
            }
        } else if let Some(text) = part.as_str() {
            pieces.push(text.to_string());
        }
    }
    pieces.join("\n")
}

/// Tool names from an assistant record's `tool_calls` (`_tools_from`).
#[must_use]
pub fn tools_from(obj: &Map<String, Value>) -> Vec<String> {
    let Some(calls) = obj.get("tool_calls").and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut names = Vec::new();
    for call in calls {
        let Some(name) = call
            .as_object()
            .and_then(|call| call.get("name"))
            .and_then(Value::as_str)
            .filter(|name| !name.is_empty())
        else {
            continue;
        };
        names.push(
            TOOL_NAME_MAP
                .iter()
                .find(|(from, _)| *from == name)
                .map_or_else(|| name.to_string(), |(_, to)| (*to).to_string()),
        );
    }
    names
}

/// A record's `model_id`, defaulting to `grok-build` (`_model_from`).
#[must_use]
pub fn model_from(obj: &Map<String, Value>) -> String {
    obj.get("model_id")
        .and_then(Value::as_str)
        .filter(|model| !model.is_empty())
        .map_or_else(|| DEFAULT_MODEL.to_string(), ToString::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn project_slug_decodes_then_applies_the_catch_all_transform() {
        assert_eq!(project_slug("%2FUsers%2Fme%2Fproj"), "-Users-me-proj");
        // The real directory that proved the transform is catch-all, not
        // separators-only: the leading `.` of `.claude` becomes a dash too.
        assert_eq!(
            project_slug("%2FUsers%2Fyadkonrad%2F.claude"),
            "-Users-yadkonrad--claude"
        );
        assert_eq!(project_slug("%2Fa%2Fmy_project"), "-a-my-project");
        // A run of escapes decodes as one UTF-8 sequence.
        assert_eq!(unquote("%C3%A9"), "é");
        // A malformed escape stays literal.
        assert_eq!(unquote("100%25 and 100%zz and %"), "100% and 100%zz and %");
        assert_eq!(unquote("no-escapes"), "no-escapes");
    }

    #[test]
    fn uuidv7_timestamps_parse_and_other_versions_do_not() {
        // 2024-01-01T00:00:00Z == 1_704_067_200_000 ms == 0x18cc251f400.
        let v7 = "018cc251-f400-7000-8000-000000000000";
        assert_eq!(uuidv7_unix_ms(v7), Some(1_704_067_200_000));
        // Same layout, version 4 — not a v7 clock.
        assert_eq!(uuidv7_unix_ms("018cc251-f400-4000-8000-000000000000"), None);
        // Every spelling `uuid.UUID()` accepts.
        assert_eq!(
            uuidv7_unix_ms("urn:uuid:018cc251-f400-7000-8000-000000000000"),
            Some(1_704_067_200_000)
        );
        assert_eq!(
            uuidv7_unix_ms(
                "{018cc251f4007000 8000000000000000}"
                    .replace(' ', "")
                    .as_str()
            ),
            Some(1_704_067_200_000)
        );
        assert_eq!(uuidv7_unix_ms("not-a-uuid"), None);
        assert_eq!(uuidv7_unix_ms(""), None);
    }

    #[test]
    fn session_timestamp_prefers_the_uuid_then_falls_back_to_mtime() {
        let from_uuid = SessionRef::file(
            NAME,
            "-p",
            "018cc251-f400-7000-8000-000000000000",
            "/tmp/chat_history.jsonl",
            1_700_000_000.0,
            0,
        );
        assert_eq!(session_timestamp(&from_uuid), "2024-01-01T00:00:00+00:00");

        let from_mtime = SessionRef::file(
            NAME,
            "-p",
            "not-a-uuid",
            "/tmp/chat_history.jsonl",
            1_704_067_200.5,
            0,
        );
        assert_eq!(
            session_timestamp(&from_mtime),
            "2024-01-01T00:00:00.500000+00:00"
        );

        // Both sources unusable: an empty stamp, not a panic.
        let unusable = SessionRef::file(NAME, "-p", "x", "/tmp/x", f64::INFINITY, 0);
        assert_eq!(session_timestamp(&unusable), "");
    }

    #[test]
    fn encrypted_reasoning_has_no_readable_text() {
        let reasoning = json!({
            "type": "reasoning",
            "encrypted_content": "AAAA…",
            "summary": [],
            "id": "r1",
        });
        let map = reasoning.as_object().expect("object");
        assert_eq!(content_text(map), "");
        // …and still reports the default model, because it *is* model output.
        assert_eq!(model_from(map), DEFAULT_MODEL);
    }

    #[test]
    fn content_text_handles_both_shapes() {
        let user = json!({"content": [{"type": "text", "text": "a"}, {"text": ""}, "bare"]});
        assert_eq!(content_text(user.as_object().expect("object")), "a\nbare");
        let assistant = json!({"content": "plain string"});
        assert_eq!(
            content_text(assistant.as_object().expect("object")),
            "plain string"
        );
        let neither = json!({"content": 42});
        assert_eq!(content_text(neither.as_object().expect("object")), "");
    }

    #[test]
    fn tool_names_map_and_unknown_ones_pass_through() {
        let obj = json!({"tool_calls": [
            {"id": "1", "name": "run_terminal_command", "arguments": "{}"},
            {"id": "2", "name": "brand_new_tool"},
            {"id": "3", "name": ""},
            "not an object",
        ]});
        assert_eq!(
            tools_from(obj.as_object().expect("object")),
            vec!["Bash".to_string(), "brand_new_tool".to_string()]
        );
    }
}
