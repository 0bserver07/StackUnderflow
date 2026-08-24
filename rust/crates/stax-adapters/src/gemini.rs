//! Gemini CLI — the port of `python-legacy: adapters/gemini.py`.
//!
//! Chats live at `~/.gemini/tmp/<project>/chats/session-*.json{,l}`, and two
//! on-disk formats coexist in the same directory:
//!
//! 1. **CLI ≤0.38, single JSON** — one top-level object
//!    `{sessionId, startTime, messages: [...]}`, parsed whole.
//! 2. **CLI ≥0.39, JSONL** — a metadata line, then one line per message.
//!
//! The format is decided per file from its extension (`_format_for`), never by
//! sniffing the contents, because the CLI writes a stable extension per format.
//!
//! ## `seq` is not always a byte offset
//!
//! This adapter is the contract's **hybrid** case: `source_kind` is
//! [`crate::base::SourceKind::File`], but `seq` is the *index into the messages
//! array* for the single-JSON variant and the line's byte offset for the JSONL
//! one. `read(ref, since_offset = N)` therefore means "skip records at or before
//! seq N" in both — one comparison, two meanings, which is exactly why
//! [`crate::base::SessionRef`] collapses offset and rowid into one field. The
//! storage-aware contract test holds either way: it asserts monotonic `seq` and
//! strictly-fewer records past a midpoint, never that `seq` is a file position.
//!
//! ## Token flattening
//!
//! `tokens.cached` counts *inside* `tokens.input` and `tokens.thoughts` is
//! reasoning billed as output, so [`normalize_tokens`] produces the canonical
//! four slots the same way [`crate::qwen::normalize_usage`] does. Gemini never
//! surfaces a cache write, so `cache_creation` is 0.

use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::base::{
    Record, SessionRef, SourceAdapter, Speed, child_dirs, file_name, file_stem, home_dir,
    read_dir_sorted, stat_ref_fields,
};
use crate::jsonl::{JsonlLines, parse_json, py_bytes_strip, stat_or_skip};
use crate::pyval;

/// The provider key.
pub const NAME: &str = "gemini";

/// Model stamped on an assistant turn that records none (`_DEFAULT_MODEL`).
pub const DEFAULT_MODEL: &str = "gemini-auto";

/// Chats bigger than this are warned about but still parsed
/// (`_LARGE_FILE_BYTES`). The hard 128 MB skip lives in
/// [`crate::jsonl::MAX_SESSION_FILE_BYTES`].
pub const LARGE_FILE_BYTES: u64 = 64 * 1024 * 1024;

/// The `session-` filename prefix both formats share.
const SESSION_PREFIX: &str = "session-";

/// Gemini tool name → canonical cross-source tool label (`_TOOL_NAME_MAP`).
///
/// Unknown names pass through untouched so a new Gemini tool stays visible until
/// it is classified.
pub const TOOL_NAME_MAP: [(&str, &str); 11] = [
    ("shell", "Bash"),
    ("execute", "Bash"),
    ("run_shell_command", "Bash"),
    ("read_file", "Read"),
    ("edit_file", "Edit"),
    ("write_file", "Edit"),
    ("replace", "Edit"),
    ("list_directory", "Glob"),
    ("glob", "Glob"),
    ("grep", "Grep"),
    ("search_file_content", "Grep"),
];

/// Which on-disk format a chat file is in (`_format_for`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// CLI ≤0.38 — one top-level JSON object holding a `messages` array.
    SingleJson,
    /// CLI ≥0.39 — line-delimited JSON.
    Jsonl,
}

impl Format {
    /// The wire spelling stored in `SessionRef.source_hint["format"]`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SingleJson => "single_json",
            Self::Jsonl => "jsonl",
        }
    }
}

/// The format of `path`, from its extension alone (`_format_for`).
#[must_use]
pub fn format_for(path: &Path) -> Format {
    let suffix = path
        .extension()
        .map(|ext| ext.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    if suffix == "jsonl" {
        Format::Jsonl
    } else {
        Format::SingleJson
    }
}

/// The Gemini CLI source adapter (`GeminiAdapter`).
#[derive(Debug, Clone)]
pub struct GeminiAdapter {
    root: PathBuf,
}

impl Default for GeminiAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl GeminiAdapter {
    /// `~/.gemini/tmp`, resolved once at construction — Python resolves it once
    /// at *import* (a module-level constant), which is the same freeze from any
    /// caller's point of view.
    #[must_use]
    pub fn new() -> Self {
        Self {
            root: home_dir().unwrap_or_default().join(".gemini").join("tmp"),
        }
    }

    /// Inject the projects root — the constructor parameter Python already has.
    #[must_use]
    pub fn with_projects_root(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The projects root this adapter reads.
    #[must_use]
    pub fn projects_root(&self) -> &Path {
        &self.root
    }

    /// The whole-document read (`_read_single_json`).
    fn read_single_json(session: &SessionRef, since_offset: i64, sink: &mut dyn FnMut(Record)) {
        // No streaming option for a top-level JSON object, so the 128 MB cap is
        // the only protection: over it, yield nothing.
        if stat_or_skip(&session.file_path).is_none() {
            return;
        }
        // LOG: python warns "Cannot read Gemini chat %s" / "Malformed Gemini JSON in %s".
        let Ok(raw) = std::fs::read(&session.file_path) else {
            return;
        };
        let Some(doc) = parse_json(&raw) else {
            return;
        };
        let Some(map) = doc.as_object() else {
            return;
        };
        let session_id = map
            .get("sessionId")
            .filter(|value| pyval::py_truthy(value))
            .map_or_else(|| session.session_id.clone(), pyval::py_str);
        let Some(messages) = map.get("messages").and_then(Value::as_array) else {
            return;
        };
        for (index, message) in messages.iter().enumerate() {
            #[allow(
                clippy::cast_possible_wrap,
                reason = "a messages array longer than i64::MAX cannot be parsed"
            )]
            let seq = index as i64;
            // `seq` is the message index here, so `since_offset` is "the
            // highest index already seen" — yield strictly past it.
            if since_offset > 0 && seq <= since_offset {
                continue;
            }
            if !message.is_object() {
                continue;
            }
            if let Some(record) = record_from_message(message, seq, &session_id) {
                sink(record);
            }
        }
    }

    /// The line-delimited read (`_read_jsonl`).
    fn read_jsonl(session: &SessionRef, since_offset: i64, sink: &mut dyn FnMut(Record)) {
        let mut session_id = session.session_id.clone();
        for (line_offset, raw_line) in JsonlLines::open(&session.file_path, since_offset) {
            if since_offset > 0 && line_offset <= since_offset {
                continue;
            }
            let stripped = py_bytes_strip(&raw_line);
            if stripped.is_empty() {
                continue;
            }
            // LOG: python debug-logs "Skipping malformed Gemini JSONL line in %s".
            let Some(entry) = parse_json(stripped) else {
                continue;
            };
            let Some(map) = entry.as_object() else {
                continue;
            };
            // The ≥0.39 metadata line carries `sessionId` and no message
            // `type`: it is not a record, but it does refine the session id
            // every later record is attached to.
            if !matches!(
                map.get("type").and_then(Value::as_str),
                Some("user" | "gemini" | "info")
            ) {
                if let Some(refined) = map
                    .get("sessionId")
                    .and_then(Value::as_str)
                    .filter(|value| !value.is_empty())
                {
                    session_id = refined.to_string();
                }
                continue;
            }
            if let Some(record) = record_from_message(&entry, line_offset, &session_id) {
                sink(record);
            }
        }
    }
}

impl SourceAdapter for GeminiAdapter {
    fn name(&self) -> &str {
        NAME
    }

    fn enumerate(&self) -> Vec<SessionRef> {
        if !self.root.is_dir() {
            return Vec::new();
        }
        let mut out = Vec::new();
        for project_dir in child_dirs(&self.root) {
            let chats_dir = project_dir.join("chats");
            if !chats_dir.is_dir() {
                continue;
            }
            // Both `session-*.json` and `session-*.jsonl` may live in the same
            // directory; the format is decided per file in `read()`.
            for path in glob_session_files(&chats_dir) {
                // Python warns and continues on OSError here.
                let Some((mtime, size)) = stat_ref_fields(&path) else {
                    continue;
                };
                if size > LARGE_FILE_BYTES {
                    // LOG: python warns "Gemini chat %s is %d bytes; reading anyway".
                }
                let mut hint = Map::new();
                hint.insert("format".to_string(), format_for(&path).as_str().into());
                out.push(SessionRef {
                    provider: NAME.to_string(),
                    project_slug: file_name(&project_dir),
                    // Finalised in `read()` from the document's `sessionId`.
                    session_id: file_stem(&path),
                    file_path: path,
                    file_mtime: mtime,
                    file_size: size,
                    source_kind: crate::base::SourceKind::File,
                    source_hint: Some(hint),
                });
            }
        }
        out
    }

    fn read_into(&self, session: &SessionRef, since_offset: i64, sink: &mut dyn FnMut(Record)) {
        match format_for(&session.file_path) {
            Format::Jsonl => Self::read_jsonl(session, since_offset, sink),
            Format::SingleJson => Self::read_single_json(session, since_offset, sink),
        }
    }

    /// `backup create` copies the projects root (`source_roots`).
    ///
    /// Like Qwen, Gemini declares `source_roots` and *not* `watch_paths`: the
    /// watcher gets nothing and falls back to periodic ingest.
    fn source_roots(&self) -> Vec<PathBuf> {
        vec![self.root.clone()]
    }
}

/// `sorted(glob("session-*.json") + glob("session-*.jsonl"))`.
fn glob_session_files(dir: &Path) -> Vec<PathBuf> {
    read_dir_sorted(dir)
        .into_iter()
        .filter(|path| {
            let name = file_name(path);
            name.starts_with(SESSION_PREFIX)
                && (name.ends_with(".json") || name.ends_with(".jsonl"))
        })
        .collect()
}

/// One message → a `Record`, or `None` (`_record_from_message`).
///
/// Shared by both formats — only `seq` and the session id differ.
fn record_from_message(message: &Value, seq: i64, session_id: &str) -> Option<Record> {
    let map = message.as_object()?;
    // `info` entries are framework chrome (model_change, session_start, …) and
    // are skipped the way the Claude adapter skips `summary` lines.
    let role = match map.get("type").and_then(Value::as_str) {
        Some("user") => "user",
        Some("gemini") => "assistant",
        _ => return None,
    };
    let tokens = normalize_tokens(map.get("tokens"));
    Some(Record {
        provider: NAME.to_string(),
        session_id: session_id.to_string(),
        seq,
        timestamp: map
            .get("timestamp")
            .filter(|value| pyval::py_truthy(value))
            .map_or_else(String::new, pyval::py_str),
        role: role.to_string(),
        // A non-string model (dict / list / number) would poison the Record
        // contract and crash the store write downstream, so it is treated as
        // absent.
        model: match map.get("model").and_then(Value::as_str) {
            Some(model) if !model.is_empty() => Some(model.to_string()),
            _ if role == "assistant" => Some(DEFAULT_MODEL.to_string()),
            _ => None,
        },
        input_tokens: tokens.input,
        output_tokens: tokens.output,
        cache_create_tokens: tokens.cache_creation,
        cache_read_tokens: tokens.cache_read,
        content_text: text_from_content(map.get("content")),
        tools: tools_from_message(map),
        cwd: None,
        is_sidechain: false,
        uuid: map
            .get("id")
            .filter(|value| pyval::py_truthy(value))
            .map_or_else(|| format!("{session_id}:{seq}"), pyval::py_str),
        parent_uuid: None,
        raw: message.clone(),
        speed: Speed::Standard,
    })
}

/// Flatten `content` (a string or a list of `{text}` blocks) into one string
/// (`_text_from_content`).
#[must_use]
pub fn text_from_content(content: Option<&Value>) -> String {
    let Some(content) = content else {
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
            // Note the asymmetry with the Qwen adapter: a present-but-*empty*
            // `text` is appended here, so it still costs a newline in the join.
            if let Some(text) = map.get("text").and_then(Value::as_str) {
                pieces.push(text.to_string());
            }
        } else if let Some(text) = block.as_str() {
            pieces.push(text.to_string());
        }
    }
    pieces.join("\n")
}

/// Tool names from `toolCalls` (`_tools_from_message`).
#[must_use]
pub fn tools_from_message(message: &Map<String, Value>) -> Vec<String> {
    let Some(calls) = message.get("toolCalls").and_then(Value::as_array) else {
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

/// Flatten Gemini's `tokens` block into the canonical four slots
/// (`_normalize_tokens`).
#[must_use]
pub fn normalize_tokens(tokens: Option<&Value>) -> crate::codex::CanonicalTokens {
    let Some(tokens) = tokens.and_then(Value::as_object) else {
        return crate::codex::CanonicalTokens {
            input: 0,
            output: 0,
            cache_creation: 0,
            cache_read: 0,
        };
    };
    let raw_in = pyval::safe_int(tokens.get("input"));
    let raw_out = pyval::safe_int(tokens.get("output"));
    let cached = pyval::safe_int(tokens.get("cached"));
    let thoughts = pyval::safe_int(tokens.get("thoughts"));
    crate::codex::CanonicalTokens {
        input: (raw_in - cached).max(0),
        output: raw_out.saturating_add(thoughts),
        cache_creation: 0,
        cache_read: cached,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn format_comes_from_the_extension_only() {
        assert_eq!(format_for(Path::new("session-a.jsonl")), Format::Jsonl);
        assert_eq!(format_for(Path::new("session-a.JSONL")), Format::Jsonl);
        assert_eq!(format_for(Path::new("session-a.json")), Format::SingleJson);
        assert_eq!(format_for(Path::new("session-a")), Format::SingleJson);
        assert_eq!(Format::Jsonl.as_str(), "jsonl");
        assert_eq!(Format::SingleJson.as_str(), "single_json");
    }

    #[test]
    fn tokens_flatten_cached_out_of_input_and_thoughts_into_output() {
        // The checked-in beta-normalizer fixture's first gemini turn.
        let tokens = normalize_tokens(Some(&json!({
            "input": 1200, "output": 600, "cached": 400, "thoughts": 150, "total": 2350,
        })));
        assert_eq!(tokens.input, 800);
        assert_eq!(tokens.output, 750);
        assert_eq!(tokens.cache_creation, 0);
        assert_eq!(tokens.cache_read, 400);
    }

    #[test]
    fn garbage_tokens_degrade_to_zero_and_never_go_negative() {
        let tokens = normalize_tokens(Some(&json!({"input": 10, "cached": 99})));
        assert_eq!(tokens.input, 0);
        assert_eq!(tokens.cache_read, 99);
        let missing = normalize_tokens(None);
        assert_eq!(missing.input, 0);
        let not_an_object = normalize_tokens(Some(&json!([1, 2])));
        assert_eq!(not_an_object.output, 0);
    }

    #[test]
    fn an_empty_text_block_still_costs_a_newline() {
        assert_eq!(text_from_content(Some(&json!("plain"))), "plain");
        assert_eq!(
            text_from_content(Some(&json!([{"text": "a"}, {"text": ""}, {"text": "b"}]))),
            "a\n\nb"
        );
        // A *missing* or non-string `text` contributes nothing.
        assert_eq!(
            text_from_content(Some(&json!([{"nope": 1}, {"text": 7}, "bare"]))),
            "bare"
        );
        assert_eq!(text_from_content(None), "");
        assert_eq!(text_from_content(Some(&json!(42))), "");
    }

    #[test]
    fn tool_names_map_and_unknown_ones_pass_through() {
        let message = json!({"toolCalls": [
            {"id": "c1", "name": "write_file"},
            {"id": "c2", "name": "my_new_tool"},
            {"id": "c3", "name": ""},
            {"id": "c4"},
            "not an object",
        ]});
        assert_eq!(
            tools_from_message(message.as_object().expect("object")),
            vec!["Edit".to_string(), "my_new_tool".to_string()]
        );
        assert!(tools_from_message(&Map::new()).is_empty());
    }
}
