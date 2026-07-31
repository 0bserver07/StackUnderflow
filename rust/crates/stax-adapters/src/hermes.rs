//! Hermes — the port of `stackunderflow/adapters/hermes.py`.
//!
//! Conversation logs are JSONL under `~/.hermes/sessions/`, either flat or in
//! nested per-project subdirectories:
//!
//! ```text
//! ~/.hermes/sessions/{sessionId}.jsonl
//! ~/.hermes/sessions/{project}/{sessionId}.jsonl
//! ```
//!
//! The event vocabulary is small — a `session` header, `model_change` markers,
//! and `message` events carrying an Anthropic-shaped `message` object with a
//! `usage` block:
//!
//! ```text
//! {"type":"session","id":"…","timestamp":"…"}
//! {"type":"model_change","data":{"model":"claude-…"},"timestamp":"…"}
//! {"type":"message","id":"…","timestamp":"…",
//!  "message":{"role":"assistant","content":[{"type":"text","text":"…"}],
//!             "model":"claude-3-5-sonnet","provider":"anthropic",
//!             "usage":{"input":100,"output":50,"cacheRead":10,"cacheWrite":5}}}
//! ```
//!
//! ## Two rules that shape every record
//!
//! 1. **`usage` is the filter, not just a field.** An assistant message without
//!    a `usage` *object* yields no record at all — the source is a cost ledger
//!    first, and a turn with no usage block cannot be billed or attributed. User
//!    and system turns yield nothing for the same reason.
//! 2. **The model is a running value.** `message.model` wins when present;
//!    otherwise the most recent `model_change` does. A **resumed** read must
//!    therefore know about `model_change` events *before* its watermark or every
//!    record past the resume floor is stamped `hermes-unknown` and the
//!    normalizer drops it as unpriceable. [`scan_for_model`] is that pre-scan —
//!    the same failure mode `codex::model_before_offset` and
//!    [`crate::openclaw::scan_for_model`] exist to prevent, arrived at
//!    independently three times in the Python originals.
//!
//! `seq` is the byte offset of each line start, so a resumed read is a `seek`.
//!
//! ## DIVERGENCE (recorded, cross-cutting — this module is only where it was
//! measured)
//!
//! `hermes.py` parses with the **stdlib `json`**, not `orjson`, and so do 18 of
//! the 20 Python adapters — `claude.py` is the only `orjson` caller in the
//! package. [`crate::jsonl::parse_json`] was tuned to orjson's behaviour, which
//! differs from the stdlib's in two measurable ways:
//!
//! * **Depth.** stdlib `json` accepts nesting to 9997 and raises `RecursionError`
//!   at 9998; orjson (and therefore [`crate::jsonl::MAX_JSON_DEPTH`]) stops at
//!   1024. A line nested 1025–9997 deep is a record Python ingests and this port
//!   drops. It is counted, not silent
//!   ([`crate::jsonl::deep_json_skips`]), and no corpus measured so far comes
//!   near it. Above 9997 the divergence reverses and lands in this port's
//!   favour: `RecursionError` is **not** a `ValueError`, so it escapes the
//!   `except (json.JSONDecodeError, ValueError)` here and kills the whole file's
//!   ingest, where this port skips the line and reads on.
//! * **Non-finite literals.** stdlib `json` decodes `NaN`, `Infinity`,
//!   `-Infinity` and `1e999` (to `inf`); `serde_json` rejects all four and skips
//!   the whole line. `_safe_int` would coerce every one of them to `0`, so the
//!   difference is a dropped record rather than a wrong number.
//!
//! Measured on the reference interpreter, 2026-07-31. Both are properties of the
//! two *parsers*, not of this adapter, which is why the fix (if any) belongs to
//! [`crate::jsonl`] and to all 19 providers at once rather than here.

use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::base::{Record, SessionRef, SourceAdapter, Speed, home_dir, stat_ref_fields};
use crate::jsonl::{self, JsonlLines, py_bytes_strip};
use crate::{blocks, pyval, walk};

/// The provider key.
pub const NAME: &str = "hermes";

/// The sessions root (`_DEFAULT_ROOT`), relative to the home directory.
pub const ROOT_RELATIVE: &str = ".hermes/sessions";

/// The model stamped when neither the message nor any `model_change` declares
/// one (`_DEFAULT_MODEL`).
pub const DEFAULT_MODEL: &str = "hermes-unknown";

/// The project slug used for a transcript that sits directly in a root
/// (`fp.parent.name if fp.parent != root else "hermes"`).
pub const ROOT_PROJECT_SLUG: &str = "hermes";

/// The block types that count as a tool call (`_tools_from_content`).
pub const TOOL_BLOCK_TYPES: [&str; 2] = ["tool_use", "toolCall"];

/// The Hermes source adapter (`HermesAdapter`).
#[derive(Debug, Clone)]
pub struct HermesAdapter {
    roots: Vec<PathBuf>,
}

impl Default for HermesAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl HermesAdapter {
    /// `~/.hermes/sessions`, resolved once at construction.
    #[must_use]
    pub fn new() -> Self {
        Self::with_optional_roots(None)
    }

    /// Inject explicit roots — `HermesAdapter(roots=[…])`.
    #[must_use]
    pub fn with_roots(roots: Vec<PathBuf>) -> Self {
        Self { roots }
    }

    /// The Python constructor exactly: an explicit list wins, including an
    /// empty one; `None` falls back to the home dotfile.
    #[must_use]
    pub fn with_optional_roots(roots: Option<Vec<PathBuf>>) -> Self {
        Self {
            roots: roots
                .unwrap_or_else(|| vec![home_dir().unwrap_or_default().join(ROOT_RELATIVE)]),
        }
    }

    /// The roots this adapter walks, in order.
    #[must_use]
    pub fn roots(&self) -> &[PathBuf] {
        &self.roots
    }
}

impl SourceAdapter for HermesAdapter {
    fn name(&self) -> &str {
        NAME
    }

    fn enumerate(&self) -> Vec<SessionRef> {
        let mut out = Vec::new();
        for root in &self.roots {
            if !root.is_dir() {
                // Not installed / never used — a clean no-op, never an error.
                continue;
            }
            // `sorted(root.glob("**/*.jsonl"))`: this directory and every
            // subdirectory, sorted by path *string* (see `walk`'s note on why
            // `PathBuf: Ord` is the wrong comparison here).
            for path in walk::rglob_suffix(root, ".jsonl") {
                // Python warns and continues on a stat failure.
                let Some((mtime, size)) = stat_ref_fields(&path) else {
                    continue;
                };
                let peeked = peek_session_id(&path);
                let session_id = if peeked.is_empty() {
                    walk::file_stem(&path)
                } else {
                    peeked
                };
                out.push(SessionRef::file(
                    NAME,
                    project_slug(&path, root),
                    session_id,
                    path,
                    mtime,
                    size,
                ));
            }
        }
        out
    }

    fn read_into(&self, session: &SessionRef, since_offset: i64, sink: &mut dyn FnMut(Record)) {
        // The 128 MB cap is checked up front, before the model pre-scan, so an
        // oversize file costs one `stat` rather than a full linear read.
        if jsonl::stat_or_skip(&session.file_path).is_none() {
            return;
        }
        let mut current_model = scan_for_model(&session.file_path, since_offset);

        for (line_offset, raw_line) in JsonlLines::open(&session.file_path, since_offset) {
            if since_offset > 0 && line_offset <= since_offset {
                continue;
            }
            let stripped = py_bytes_strip(&raw_line);
            if stripped.is_empty() {
                continue;
            }
            // LOG: python debug-logs "Skipping malformed JSON line in %s".
            let Some(event) = jsonl::parse_json(stripped) else {
                continue;
            };
            // Valid JSON that is not an object (list / string / number) cannot
            // be a session event — skip, do not crash the read.
            let Some(map) = event.as_object() else {
                continue;
            };
            let etype = map.get("type").and_then(Value::as_str);

            if etype == Some("model_change") {
                let candidate = model_from_model_change(map);
                if !candidate.is_empty() {
                    current_model = Some(candidate);
                }
                continue;
            }
            if etype != Some("message") {
                continue;
            }

            // `event.get("message") or {}` then an isinstance check: a falsy
            // non-dict (`null`, `0`, `""`, `[]`) becomes an empty dict whose
            // role is absent, and a truthy non-dict fails the check. Both land
            // on "no record", which is what this single guard expresses.
            let Some(message) = map.get("message").and_then(Value::as_object) else {
                continue;
            };
            if message.get("role").and_then(Value::as_str) != Some("assistant") {
                continue;
            }
            let Some(usage) = message.get("usage").and_then(Value::as_object) else {
                continue;
            };

            let model = message
                .get("model")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .map_or_else(
                    || {
                        current_model
                            .clone()
                            .unwrap_or_else(|| DEFAULT_MODEL.to_string())
                    },
                    ToString::to_string,
                );
            let content = message.get("content");

            sink(Record {
                provider: NAME.to_string(),
                session_id: session.session_id.clone(),
                seq: line_offset,
                timestamp: map
                    .get("timestamp")
                    .filter(|value| pyval::py_truthy(value))
                    .map_or_else(String::new, pyval::py_str),
                role: "assistant".to_string(),
                model: Some(model),
                // `_normalize_usage` maps the source's camelCase cache slots
                // onto the canonical record fields.
                input_tokens: pyval::safe_int(usage.get("input")),
                output_tokens: pyval::safe_int(usage.get("output")),
                cache_create_tokens: pyval::safe_int(usage.get("cacheWrite")),
                cache_read_tokens: pyval::safe_int(usage.get("cacheRead")),
                content_text: blocks::message_text(content),
                tools: blocks::tool_names(content, &TOOL_BLOCK_TYPES),
                // The cwd is on the *event*, not on the message.
                cwd: map
                    .get("cwd")
                    .and_then(Value::as_str)
                    .filter(|value| !value.is_empty())
                    .map(ToString::to_string),
                is_sidechain: false,
                uuid: map
                    .get("id")
                    .filter(|value| pyval::py_truthy(value))
                    .map_or_else(
                        || format!("{}:{line_offset}", session.session_id),
                        pyval::py_str,
                    ),
                parent_uuid: None,
                raw: event.clone(),
                speed: Speed::Standard,
            });
        }
    }

    /// The sessions roots (`watch_paths`); [`SourceAdapter::source_roots`]
    /// falls back to them for `backup create`, as `cli.py`'s `getattr` chain
    /// does.
    fn watch_paths(&self) -> Vec<PathBuf> {
        self.roots.clone()
    }
}

/// The project slug for a transcript found under `root`
/// (`fp.parent.name if fp.parent != root else "hermes"`).
///
/// `pathlib` compares paths component-wise, so a root spelled with a trailing
/// separator still equals the file's parent; `Path`'s `PartialEq` is the same
/// comparison, which is why this is a plain `==`.
#[must_use]
pub fn project_slug(path: &Path, root: &Path) -> String {
    match path.parent() {
        Some(parent) if parent != root => walk::dir_name(parent),
        // A file directly in the root, or a path with no parent at all: Python
        // reaches the same `"hermes"` for both.
        _ => ROOT_PROJECT_SLUG.to_string(),
    }
}

/// The `session` header's id, or `""` (`_peek_session_id`).
fn peek_session_id(path: &Path) -> String {
    let Some(first) = walk::first_line(path) else {
        return String::new();
    };
    let stripped = py_bytes_strip(&first);
    if stripped.is_empty() {
        return String::new();
    }
    let Some(obj) = jsonl::parse_json(stripped) else {
        return String::new();
    };
    // A non-object first line must not crash enumerate() — fall back to the
    // filename-stem session id.
    let Some(map) = obj.as_object() else {
        return String::new();
    };
    if map.get("type").and_then(Value::as_str) != Some("session") {
        return String::new();
    }
    map.get("id")
        .filter(|value| pyval::py_truthy(value))
        .map_or_else(String::new, pyval::py_str)
}

/// The most recent `model_change` model in bytes `[0, until_offset)`
/// (`_scan_for_model`).
///
/// `until_offset <= 0` means a fresh read, which needs no seed: the main loop
/// will see every `model_change` itself.
#[must_use]
pub fn scan_for_model(path: &Path, until_offset: i64) -> Option<String> {
    if until_offset <= 0 {
        return None;
    }
    let mut current = None;
    // Python iterates the raw file handle here rather than `iter_jsonl_lines`;
    // the size cap has already been paid by the caller.
    for (line_offset, raw_line) in JsonlLines::open(path, 0) {
        if line_offset >= until_offset {
            break;
        }
        let stripped = py_bytes_strip(&raw_line);
        if stripped.is_empty() {
            continue;
        }
        let Some(obj) = jsonl::parse_json(stripped) else {
            continue;
        };
        let Some(map) = obj.as_object() else {
            continue;
        };
        if map.get("type").and_then(Value::as_str) != Some("model_change") {
            continue;
        }
        let candidate = model_from_model_change(map);
        if !candidate.is_empty() {
            current = Some(candidate);
        }
    }
    current
}

/// `data.model`, then a flat `model`, else `""` (`_model_from_model_change`).
fn model_from_model_change(event: &Map<String, Value>) -> String {
    if let Some(model) = event
        .get("data")
        .and_then(Value::as_object)
        .and_then(|data| data.get("model"))
        .and_then(Value::as_str)
        && !model.is_empty()
    {
        return model.to_string();
    }
    event
        .get("model")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .unwrap_or_default()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn event(value: Value) -> Map<String, Value> {
        value.as_object().expect("object").clone()
    }

    #[test]
    fn model_change_prefers_the_nested_data_block() {
        assert_eq!(
            model_from_model_change(&event(json!({"data": {"model": "a"}, "model": "b"}))),
            "a"
        );
        assert_eq!(model_from_model_change(&event(json!({"model": "b"}))), "b");
        // Empty and non-string models are "no declaration", not a model named
        // "None" — the caller keeps whatever it had.
        assert_eq!(
            model_from_model_change(&event(json!({"data": {"model": ""}, "model": "b"}))),
            "b"
        );
        assert_eq!(
            model_from_model_change(&event(json!({"data": "not a dict", "model": "b"}))),
            "b"
        );
        assert_eq!(model_from_model_change(&event(json!({"model": 7}))), "");
        assert_eq!(model_from_model_change(&event(json!({}))), "");
    }

    #[test]
    fn the_slug_is_the_parent_directory_unless_that_is_the_root() {
        let root = Path::new("/h/sessions");
        assert_eq!(
            project_slug(Path::new("/h/sessions/s.jsonl"), root),
            "hermes"
        );
        // A root spelled with a trailing separator is the same root.
        assert_eq!(
            project_slug(Path::new("/h/sessions/s.jsonl"), Path::new("/h/sessions/")),
            "hermes"
        );
        assert_eq!(
            project_slug(Path::new("/h/sessions/proj/s.jsonl"), root),
            "proj"
        );
    }

    #[test]
    fn an_absent_root_enumerates_empty_rather_than_failing() {
        let adapter = HermesAdapter::with_roots(vec![PathBuf::from("/nonexistent/stax/hermes")]);
        assert!(adapter.enumerate().is_empty());
        assert_eq!(adapter.name(), NAME);
        // watch_paths is declared, so source_roots falls back to it.
        assert_eq!(adapter.source_roots(), adapter.watch_paths());
    }

    #[test]
    fn a_fresh_read_needs_no_model_seed() {
        assert_eq!(scan_for_model(Path::new("/nonexistent"), 0), None);
        assert_eq!(scan_for_model(Path::new("/nonexistent"), -5), None);
    }

    #[test]
    fn the_default_root_is_the_home_dotfile() {
        let adapter = HermesAdapter::new();
        assert_eq!(adapter.roots().len(), 1);
        assert!(adapter.roots()[0].ends_with("sessions"));
        // An explicitly empty list is honoured — it is not "no roots given".
        assert!(
            HermesAdapter::with_optional_roots(Some(Vec::new()))
                .roots()
                .is_empty()
        );
    }
}
