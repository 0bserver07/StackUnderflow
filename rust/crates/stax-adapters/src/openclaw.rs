//! OpenClaw and its rebrand-cousins — the port of
//! `python-legacy: adapters/openclaw.py`.
//!
//! One tool, four names. The adapter probes each candidate base in a fixed
//! order and reads from whichever exist:
//!
//! ```text
//! ~/.openclaw/agents/{agent}/sessions/{sessionId}.jsonl
//! ~/.clawdbot/agents/…
//! ~/.moltbot/agents/…
//! ~/.moldbot/agents/…
//! ```
//!
//! The order is deterministic rather than incidental so a cross-listed agent id
//! resolves the same way on every machine (the rebrands do not in practice share
//! agent ids; the guarantee is cheap and a test can rely on it).
//!
//! ## The model is a running value
//!
//! `message.model` is authoritative when present. When it is not, the model
//! comes from the most recent `model_change` event — which means a **resumed**
//! read must know about `model_change` events that sit *before* its watermark,
//! or every record past the resume floor is stamped `openclaw-unknown` and the
//! normalizer drops it as unpriceable. [`scan_for_model`] is that pre-scan; it
//! is the same failure the codex adapter's `model_before_offset` exists to
//! prevent, arrived at independently in the Python original.

use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::base::{Record, SessionRef, SourceAdapter, Speed, stat_ref_fields};
use crate::jsonl::{self, JsonlLines, py_bytes_strip};
use crate::{blocks, pyval, walk};

/// The provider key.
pub const NAME: &str = "openclaw";

/// The candidate base directories, in probe order (`_CANDIDATE_BASES`).
pub const CANDIDATE_BASES: [&str; 4] = [
    ".openclaw/agents",
    ".clawdbot/agents",
    ".moltbot/agents",
    ".moldbot/agents",
];

/// The model stamped when neither the message nor any `model_change` declares
/// one (`_DEFAULT_MODEL`).
pub const DEFAULT_MODEL: &str = "openclaw-unknown";

/// The block types that count as a tool call (`_tools_from_content`).
pub const TOOL_BLOCK_TYPES: [&str; 2] = ["tool_use", "toolCall"];

/// The OpenClaw source adapter (`OpenClawAdapter`).
#[derive(Debug, Clone)]
pub struct OpenClawAdapter {
    bases: Vec<PathBuf>,
}

impl Default for OpenClawAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl OpenClawAdapter {
    /// All four candidate bases, expanded against the home directory.
    #[must_use]
    pub fn new() -> Self {
        #[allow(
            deprecated,
            reason = "std::env::home_dir is the platform-correct answer on the \
            1.97.1 pin; the claude and codex adapters carry the same allow"
        )]
        let home = std::env::home_dir().unwrap_or_default();
        Self {
            bases: CANDIDATE_BASES
                .iter()
                .map(|relative| home.join(relative))
                .collect(),
        }
    }

    /// Inject explicit base directories — `OpenClawAdapter(base_dirs=[…])`.
    #[must_use]
    pub fn with_bases(bases: Vec<PathBuf>) -> Self {
        Self { bases }
    }

    /// The base directories this adapter probes, in order.
    #[must_use]
    pub fn bases(&self) -> &[PathBuf] {
        &self.bases
    }
}

impl SourceAdapter for OpenClawAdapter {
    fn name(&self) -> &str {
        NAME
    }

    fn enumerate(&self) -> Vec<SessionRef> {
        let mut out = Vec::new();
        for base in &self.bases {
            if !base.is_dir() {
                continue;
            }
            for agent_dir in walk::child_dirs(base) {
                let sessions_dir = agent_dir.join("sessions");
                if !sessions_dir.is_dir() {
                    continue;
                }
                for path in walk::glob_suffix(&sessions_dir, ".jsonl") {
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
                    let agent = walk::dir_name(&agent_dir);
                    let project_slug = if agent.is_empty() {
                        NAME.to_string()
                    } else {
                        agent
                    };
                    out.push(SessionRef::file(
                        NAME,
                        project_slug,
                        session_id,
                        path,
                        mtime,
                        size,
                    ));
                }
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
            let Some(message) = map.get("message").and_then(Value::as_object) else {
                continue;
            };
            // One record per assistant message *with usage*; user and system
            // turns do not drive cost and yield nothing.
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
            let uuid = map
                .get("id")
                .filter(|value| pyval::py_truthy(value))
                .map_or_else(
                    || format!("{}:{line_offset}", session.session_id),
                    pyval::py_str,
                );

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
                input_tokens: pyval::safe_int(usage.get("input")),
                output_tokens: pyval::safe_int(usage.get("output")),
                cache_create_tokens: pyval::safe_int(usage.get("cacheWrite")),
                cache_read_tokens: pyval::safe_int(usage.get("cacheRead")),
                content_text: blocks::message_text(content),
                tools: blocks::tool_names(content, &TOOL_BLOCK_TYPES),
                cwd: None,
                is_sidechain: false,
                uuid,
                parent_uuid: None,
                raw: event.clone(),
                speed: Speed::Standard,
            });
        }
    }

    fn watch_paths(&self) -> Vec<PathBuf> {
        self.bases.clone()
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
        assert_eq!(model_from_model_change(&event(json!({"model": 7}))), "");
        assert_eq!(model_from_model_change(&event(json!({}))), "");
    }

    #[test]
    fn the_four_rebrand_bases_are_probed_in_a_fixed_order() {
        let adapter = OpenClawAdapter::new();
        let bases: Vec<String> = adapter
            .bases()
            .iter()
            .map(|path| walk::dir_name(path.parent().expect("parent")))
            .collect();
        assert_eq!(
            bases,
            vec![".openclaw", ".clawdbot", ".moltbot", ".moldbot"]
        );
        assert_eq!(adapter.watch_paths(), adapter.bases());
    }

    #[test]
    fn an_absent_base_enumerates_empty_rather_than_failing() {
        let adapter = OpenClawAdapter::with_bases(vec![PathBuf::from("/nonexistent/stax/claw")]);
        assert!(adapter.enumerate().is_empty());
    }

    #[test]
    fn a_fresh_read_needs_no_model_seed() {
        assert_eq!(scan_for_model(Path::new("/nonexistent"), 0), None);
        assert_eq!(scan_for_model(Path::new("/nonexistent"), -5), None);
    }
}
