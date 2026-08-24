//! Qwen Code — the port of `python-legacy: adapters/qwen.py`.
//!
//! Chats live at `$QWEN_DATA_DIR/projects/<project>/chats/*.jsonl`, defaulting
//! to `~/.qwen/projects`. One JSON object per line; `seq` is the byte offset of
//! the line start, so this is the plain byte-offset JSONL shape the Claude and
//! Codex adapters already established — a resumed read is a `seek`.
//!
//! ## The token flattening
//!
//! Qwen writes Google's `usageMetadata` shape, where cached input is counted
//! *inside* `promptTokenCount` and reasoning ("thoughts") is billed separately.
//! [`normalize_usage`] flattens that into the canonical four slots the store
//! keeps for every provider: fresh input only, reasoning folded into output,
//! cached input under `cache_read`, and `cache_creation` at 0 because Qwen never
//! surfaces a cache write. Identical in shape to
//! [`crate::codex::canonicalize_openai_usage`] — same convention, different key
//! names.
//!
//! ## Environment, injected
//!
//! `$QWEN_DATA_DIR` relocates the whole tree. Python resolves it once in
//! `__init__`, so [`QwenAdapter::new`] does too; [`QwenAdapter::with_projects_root`]
//! injects a root instead, which is how the tests and the parity harness avoid
//! `set_var` (forbidden: Rust 2024 makes it `unsafe`, the workspace forbids
//! `unsafe`).

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::base::{
    Record, SessionRef, SourceAdapter, Speed, file_name, file_stem, glob_suffix, home_dir,
    read_dir_sorted, stat_ref_fields,
};
use crate::jsonl::{JsonlLines, parse_json, py_bytes_strip};
use crate::pyval;

/// The provider key.
pub const NAME: &str = "qwen";

/// The `$QWEN_DATA_DIR` override (`qwen.py:97`).
pub const DATA_DIR_ENV: &str = "QWEN_DATA_DIR";

/// Model stamped on an assistant turn that records none (`_DEFAULT_MODEL`).
pub const DEFAULT_MODEL: &str = "qwen-auto";

/// Chats bigger than this are warned about but still parsed
/// (`_LARGE_FILE_BYTES`). The hard 128 MB skip lives in
/// [`crate::jsonl::MAX_SESSION_FILE_BYTES`].
pub const LARGE_FILE_BYTES: u64 = 64 * 1024 * 1024;

/// Qwen tool name → canonical cross-source tool label (`_TOOL_NAME_MAP`).
///
/// Unknown names pass through untouched so a new Qwen tool stays visible until
/// it is classified.
pub const TOOL_NAME_MAP: [(&str, &str); 12] = [
    ("shell", "Bash"),
    ("execute", "Bash"),
    ("exec_command", "Bash"),
    ("run_command", "Bash"),
    ("read_file", "Read"),
    ("edit_file", "Edit"),
    ("write_file", "Edit"),
    ("apply_diff", "Edit"),
    ("list_directory", "Glob"),
    ("glob", "Glob"),
    ("grep", "Grep"),
    ("search", "Grep"),
];

/// The Qwen projects root, with the environment injected (`_qwen_root`).
///
/// `$QWEN_DATA_DIR/projects` when the variable is set and non-empty (Python's
/// `if env:` is a plain truthiness test — it does *not* strip, unlike
/// `CLAUDE_CONFIG_DIR`), else `<home>/.qwen/projects`.
#[must_use]
pub fn resolve_projects_root(data_dir: Option<&OsStr>, home: Option<&Path>) -> PathBuf {
    match data_dir.filter(|value| !value.is_empty()) {
        Some(value) => Path::new(value).join("projects"),
        None => home.map_or_else(
            || PathBuf::from(".qwen").join("projects"),
            |home| home.join(".qwen").join("projects"),
        ),
    }
}

/// The Qwen source adapter (`QwenAdapter`).
#[derive(Debug, Clone)]
pub struct QwenAdapter {
    root: PathBuf,
}

impl Default for QwenAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl QwenAdapter {
    /// Read the live environment, as Python's `__init__` does — once, at
    /// construction.
    #[must_use]
    pub fn new() -> Self {
        Self {
            root: resolve_projects_root(
                std::env::var_os(DATA_DIR_ENV).as_deref(),
                home_dir().as_deref(),
            ),
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
}

impl SourceAdapter for QwenAdapter {
    fn name(&self) -> &str {
        NAME
    }

    fn enumerate(&self) -> Vec<SessionRef> {
        if !self.root.is_dir() {
            // Not installed / never used — clean no-op rather than raise.
            return Vec::new();
        }
        let mut out = Vec::new();
        for project_dir in read_dir_sorted(&self.root) {
            if !project_dir.is_dir() {
                continue;
            }
            let chats_dir = project_dir.join("chats");
            if !chats_dir.is_dir() {
                continue;
            }
            for path in glob_suffix(&chats_dir, ".jsonl") {
                // Python warns and continues on OSError here; `None` is the
                // same outcome without the crash risk.
                let Some((mtime, size)) = stat_ref_fields(&path) else {
                    continue;
                };
                if size > LARGE_FILE_BYTES {
                    // LOG: python warns "Qwen chat %s is %d bytes; reading anyway".
                }
                out.push(SessionRef::file(
                    NAME,
                    file_name(&project_dir),
                    // Finalised in `read()` from the entry's `sessionId`; the
                    // stem keeps the id deterministic before the file is opened.
                    file_stem(&path),
                    path,
                    mtime,
                    size,
                ));
            }
        }
        out
    }

    fn read_into(&self, session: &SessionRef, since_offset: i64, sink: &mut dyn FnMut(Record)) {
        for (line_offset, raw_line) in JsonlLines::open(&session.file_path, since_offset) {
            // `since_offset == 0` means "fresh read, yield everything";
            // otherwise the caller already saw the record at exactly that
            // offset.
            if since_offset > 0 && line_offset <= since_offset {
                continue;
            }
            let stripped = py_bytes_strip(&raw_line);
            if stripped.is_empty() {
                continue;
            }
            // LOG: python debug-logs "Skipping malformed Qwen JSON line in %s".
            let Some(entry) = parse_json(stripped) else {
                continue;
            };
            // Valid JSON that is not an object cannot be a chat entry.
            if !entry.is_object() {
                continue;
            }
            if let Some(record) = record_from_entry(&entry, session, line_offset) {
                sink(record);
            }
        }
    }

    /// `backup create` copies the projects root (`source_roots`).
    ///
    /// Qwen declares `source_roots` and *not* `watch_paths`, so the ETL watcher
    /// gets nothing and falls back to periodic ingest while backup still copies
    /// the tree. Porting that asymmetry rather than "fixing" it keeps the
    /// watcher's provider set identical across the two implementations.
    fn source_roots(&self) -> Vec<PathBuf> {
        vec![self.root.clone()]
    }
}

/// One JSONL entry → a `Record`, or `None` (`_record_from_entry`).
fn record_from_entry(entry: &Value, session: &SessionRef, seq: i64) -> Option<Record> {
    let map = entry.as_object()?;
    // Only the two conversational types produce records; a non-string `type`
    // matches neither.
    let role = match map.get("type").and_then(Value::as_str) {
        Some(role @ ("user" | "assistant")) => role,
        _ => return None,
    };

    let parts = map
        .get("message")
        .filter(|value| value.is_object())
        .and_then(|message| message.get("parts"));
    let tokens = normalize_usage(map.get("usageMetadata"));
    let session_id = map
        .get("sessionId")
        .filter(|value| pyval::py_truthy(value))
        .map_or_else(|| session.session_id.clone(), pyval::py_str);

    Some(Record {
        provider: NAME.to_string(),
        session_id: session_id.clone(),
        seq,
        timestamp: map
            .get("timestamp")
            .filter(|value| pyval::py_truthy(value))
            .map_or_else(String::new, pyval::py_str),
        role: role.to_string(),
        // A non-string model would poison the Record contract downstream, so it
        // is treated as absent; an assistant turn then falls back to the
        // synthetic default and a user turn stays model-less.
        model: match map.get("model").and_then(Value::as_str) {
            Some(model) if !model.is_empty() => Some(model.to_string()),
            _ if role == "assistant" => Some(DEFAULT_MODEL.to_string()),
            _ => None,
        },
        input_tokens: tokens.input,
        output_tokens: tokens.output,
        cache_create_tokens: tokens.cache_creation,
        cache_read_tokens: tokens.cache_read,
        content_text: text_from_parts(parts),
        tools: tools_from_parts(parts),
        cwd: None,
        is_sidechain: false,
        uuid: map
            .get("uuid")
            .filter(|value| pyval::py_truthy(value))
            .map_or_else(|| format!("{session_id}:{seq}"), pyval::py_str),
        parent_uuid: None,
        raw: entry.clone(),
        speed: Speed::Standard,
    })
}

/// Concatenate every `.text` across content parts (`_text_from_parts`).
///
/// Thought parts keep their text: reasoning traces stay searchable, the same
/// call the Claude adapter makes for `thinking` blocks.
#[must_use]
pub fn text_from_parts(parts: Option<&Value>) -> String {
    let Some(blocks) = parts.and_then(Value::as_array) else {
        return String::new();
    };
    let mut pieces: Vec<String> = Vec::new();
    for part in blocks {
        if let Some(map) = part.as_object() {
            // A *missing*, empty, or non-string `text` contributes nothing at
            // all here — note the asymmetry with the Gemini adapter, where a
            // present-but-empty `text` still costs a newline in the join.
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

/// Tool names from `functionCall` blocks (`_tools_from_parts`).
#[must_use]
pub fn tools_from_parts(parts: Option<&Value>) -> Vec<String> {
    let Some(blocks) = parts.and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut names = Vec::new();
    for part in blocks {
        let Some(name) = part
            .as_object()
            .and_then(|part| part.get("functionCall"))
            .filter(|value| value.is_object())
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

/// Flatten Qwen's `usageMetadata` into the canonical four slots
/// (`_normalize_usage`).
#[must_use]
pub fn normalize_usage(usage: Option<&Value>) -> crate::codex::CanonicalTokens {
    let Some(usage) = usage.and_then(Value::as_object) else {
        return crate::codex::CanonicalTokens {
            input: 0,
            output: 0,
            cache_creation: 0,
            cache_read: 0,
        };
    };
    canonicalize(usage)
}

fn canonicalize(usage: &Map<String, Value>) -> crate::codex::CanonicalTokens {
    let prompt = pyval::safe_int(usage.get("promptTokenCount"));
    let candidates = pyval::safe_int(usage.get("candidatesTokenCount"));
    let thoughts = pyval::safe_int(usage.get("thoughtsTokenCount"));
    let cached = pyval::safe_int(usage.get("cachedContentTokenCount"));
    crate::codex::CanonicalTokens {
        input: (prompt - cached).max(0),
        output: candidates.saturating_add(thoughts),
        cache_creation: 0,
        cache_read: cached,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn data_dir_env_relocates_the_projects_root() {
        let home = Path::new("/home/me");
        assert_eq!(
            resolve_projects_root(None, Some(home)),
            Path::new("/home/me/.qwen/projects")
        );
        assert_eq!(
            resolve_projects_root(Some(OsStr::new("/data/qwen")), Some(home)),
            Path::new("/data/qwen/projects")
        );
        assert_eq!(
            resolve_projects_root(Some(OsStr::new("")), Some(home)),
            Path::new("/home/me/.qwen/projects"),
            "an empty QWEN_DATA_DIR is Python-falsy"
        );
    }

    #[test]
    fn usage_flattens_cached_out_of_input_and_thoughts_into_output() {
        // The checked-in beta-normalizer fixture's first assistant turn.
        let tokens = normalize_usage(Some(&json!({
            "promptTokenCount": 1500,
            "candidatesTokenCount": 400,
            "thoughtsTokenCount": 100,
            "cachedContentTokenCount": 800,
        })));
        assert_eq!(tokens.input, 700);
        assert_eq!(tokens.output, 500);
        assert_eq!(tokens.cache_creation, 0);
        assert_eq!(tokens.cache_read, 800);
    }

    #[test]
    fn garbage_usage_degrades_to_zero_and_never_goes_negative() {
        let tokens = normalize_usage(Some(&json!({
            "promptTokenCount": "garbage",
            "cachedContentTokenCount": 99,
            "candidatesTokenCount": [1],
        })));
        assert_eq!(tokens.input, 0, "cached must never push input negative");
        assert_eq!(tokens.cache_read, 99);
        assert_eq!(tokens.output, 0);

        let missing = normalize_usage(None);
        assert_eq!(missing.input, 0);
        assert_eq!(missing.output, 0);
        let not_an_object = normalize_usage(Some(&json!("nope")));
        assert_eq!(not_an_object.cache_read, 0);
    }

    #[test]
    fn text_keeps_thought_parts_and_skips_empty_ones() {
        let parts = json!([
            {"text": "visible"},
            {"text": "", "thought": true},
            {"text": "reasoning", "thought": true},
            {"functionCall": {"name": "shell"}},
            "bare",
            42,
        ]);
        assert_eq!(text_from_parts(Some(&parts)), "visible\nreasoning\nbare");
        assert_eq!(text_from_parts(None), "");
        assert_eq!(text_from_parts(Some(&json!("not a list"))), "");
    }

    #[test]
    fn tool_names_map_and_unknown_ones_pass_through() {
        let parts = json!([
            {"functionCall": {"name": "shell"}},
            {"functionCall": {"name": "my_new_tool"}},
            {"functionCall": {"name": ""}},
            {"functionCall": "not an object"},
            {"functionCall": {"name": 7}},
        ]);
        assert_eq!(
            tools_from_parts(Some(&parts)),
            vec!["Bash".to_string(), "my_new_tool".to_string()]
        );
        assert!(tools_from_parts(None).is_empty());
    }
}
