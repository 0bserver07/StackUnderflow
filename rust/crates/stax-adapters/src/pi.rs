//! Pi and OMP — the port of `python-legacy: adapters/pi.py`.
//!
//! Two sibling CLIs with one on-disk format and two roots:
//!
//! * Pi — `~/.pi/agent/sessions/`
//! * OMP — `~/.omp/agent/sessions/`
//!
//! They are **one adapter**, because the difference between two would be a
//! single constant. What keeps them distinguishable downstream is the
//! `project_slug`, which carries the source label as a prefix
//! (`pi-Users-me-app`, `omp-Users-me-app`), plus a `source_hint` of
//! `{"source": "pi"|"omp"}` so a report never has to re-derive it from the file
//! path.
//!
//! Events are one JSON object per line:
//!
//! ```text
//! {"type": "session", "id": "...", "timestamp": "...", "cwd": "..."}
//! {"type": "message", "id": "...", "timestamp": "...",
//!  "message": {"role": "assistant", "content": [...], "model": "gpt-5",
//!              "usage": {"input": …, "output": …, "cacheRead": …, "cacheWrite": …}}}
//! ```
//!
//! Only assistant messages **that carry a usage block** become records —
//! everything else, user turns included, yields nothing. `seq` is the byte
//! offset of the line, so resume is the same seek-and-compare as every other
//! JSONL adapter.

use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::base::{Record, SessionRef, SourceAdapter, Speed, stat_ref_fields};
use crate::jsonl::{JsonlLines, parse_json, py_bytes_strip};
use crate::{blocks, pyval, walk};

/// The provider key. One name for both CLIs — see the module docs.
pub const NAME: &str = "pi";

/// The model stamped on a turn whose `message.model` is missing or non-string
/// (`_DEFAULT_MODEL`).
///
/// Not `None`: a model-less record is dropped by the normalizer as unpriceable,
/// and Pi's own default really is this model.
pub const DEFAULT_MODEL: &str = "gpt-5";

/// The block types that count as a tool call (`_tools_from_content`).
pub const TOOL_BLOCK_TYPES: [&str; 2] = ["toolCall", "tool_use"];

/// The Pi / OMP source adapter (`PiAdapter`).
#[derive(Debug, Clone)]
pub struct PiAdapter {
    /// `(root, label)` pairs — the label feeds `project_slug` and `source_hint`.
    roots: Vec<(PathBuf, String)>,
}

impl Default for PiAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl PiAdapter {
    /// Both default roots, resolved against the home directory.
    ///
    /// Python evaluates `_DEFAULT_ROOTS` once at import; resolving once at
    /// construction is the same observable behaviour without a global.
    #[must_use]
    pub fn new() -> Self {
        #[allow(
            deprecated,
            reason = "std::env::home_dir is the platform-correct answer on the \
            1.97.1 pin; the claude and codex adapters carry the same allow"
        )]
        let home = std::env::home_dir().unwrap_or_default();
        Self {
            roots: vec![
                (home.join(".pi").join("agent").join("sessions"), "pi".into()),
                (
                    home.join(".omp").join("agent").join("sessions"),
                    "omp".into(),
                ),
            ],
        }
    }

    /// Inject explicit `(root, label)` pairs — the constructor parameter Python
    /// already has (`PiAdapter(roots=[(tmp, "pi")])`).
    #[must_use]
    pub fn with_roots(roots: Vec<(PathBuf, String)>) -> Self {
        Self { roots }
    }

    /// The `(root, label)` pairs this adapter scans.
    #[must_use]
    pub fn roots(&self) -> &[(PathBuf, String)] {
        &self.roots
    }
}

impl SourceAdapter for PiAdapter {
    fn name(&self) -> &str {
        NAME
    }

    fn enumerate(&self) -> Vec<SessionRef> {
        let cwd = current_dir_string();
        let mut out = Vec::new();
        for (root, label) in &self.roots {
            if !root.is_dir() {
                continue;
            }
            // `sorted(root.glob("**/*.jsonl"))` — this directory and every
            // subdirectory, sorted by path string.
            for path in walk::rglob_suffix(root, ".jsonl") {
                // Python warns and continues on a stat failure; a session that
                // vanished mid-walk must not take the other roots down.
                let Some((mtime, size)) = stat_ref_fields(&path) else {
                    continue;
                };
                let (peeked_id, peeked_cwd) = peek_session_meta(&path);
                let session_id = if peeked_id.is_empty() {
                    walk::file_stem(&path)
                } else {
                    peeked_id
                };
                let project_slug = if peeked_cwd.is_empty() {
                    label.clone()
                } else {
                    slug_for(&peeked_cwd, label, &cwd)
                };
                let mut hint = Map::new();
                hint.insert("source".to_string(), Value::from(label.clone()));
                out.push(SessionRef {
                    provider: NAME.to_string(),
                    project_slug,
                    session_id,
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
        // No up-front size check: `JsonlLines` enforces the 128 MB cap and
        // yields nothing above it, which is exactly what `iter_jsonl_lines`
        // does for the Python original.
        for (line_offset, raw_line) in JsonlLines::open(&session.file_path, since_offset) {
            if since_offset > 0 && line_offset <= since_offset {
                continue;
            }
            let stripped = py_bytes_strip(&raw_line);
            if stripped.is_empty() {
                continue;
            }
            // LOG: python debug-logs "Skipping malformed JSON line in %s".
            let Some(event) = parse_json(stripped) else {
                continue;
            };
            // Valid JSON that is not an object cannot be a session event.
            let Some(map) = event.as_object() else {
                continue;
            };
            if map.get("type").and_then(Value::as_str) != Some("message") {
                continue;
            }
            // `event.get("message") or {}` then an isinstance check: a falsy
            // message (null, "", []) becomes `{}`, whose role is absent, so
            // both paths land on "skip".
            let Some(message) = map.get("message").and_then(Value::as_object) else {
                continue;
            };
            if message.get("role").and_then(Value::as_str) != Some("assistant") {
                continue;
            }
            // A turn without a usage *dict* is not billable and yields nothing
            // at all — not a zero-token record.
            let Some(usage) = message.get("usage").and_then(Value::as_object) else {
                continue;
            };

            let model = message
                .get("model")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .map_or_else(|| DEFAULT_MODEL.to_string(), ToString::to_string);
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
                // The cwd lives on the *event*, not on the message.
                cwd: map
                    .get("cwd")
                    .and_then(Value::as_str)
                    .filter(|value| !value.is_empty())
                    .map(ToString::to_string),
                is_sidechain: false,
                uuid,
                parent_uuid: None,
                raw: event.clone(),
                speed: Speed::Standard,
            });
        }
    }

    fn watch_paths(&self) -> Vec<PathBuf> {
        // Both roots unconditionally — the watcher filters non-existent ones,
        // so a machine with only Pi installed still picks OMP up the day it is.
        self.roots.iter().map(|(root, _)| root.clone()).collect()
    }
}

/// `(session_id, cwd)` from the file's first line (`_peek_session_meta`).
///
/// Empty strings for every failure: unreadable file, blank first line, invalid
/// JSON, a first line that is not an object, or a first event that is not the
/// `session` header. The caller falls back to the filename stem.
fn peek_session_meta(path: &Path) -> (String, String) {
    let empty = || (String::new(), String::new());
    let Some(first) = walk::first_line(path) else {
        return empty();
    };
    let stripped = py_bytes_strip(&first);
    if stripped.is_empty() {
        return empty();
    }
    let Some(obj) = parse_json(stripped) else {
        return empty();
    };
    let Some(map) = obj.as_object() else {
        return empty();
    };
    if map.get("type").and_then(Value::as_str) != Some("session") {
        return empty();
    }
    let field = |key: &str| {
        map.get(key)
            .filter(|value| pyval::py_truthy(value))
            .map_or_else(String::new, pyval::py_str)
    };
    (field("id"), field("cwd"))
}

/// `<label><claude-style slug>` (`_slug_for`).
///
/// The label prefix is what keeps a Pi session and an OMP session in the same
/// directory from collapsing into one project.
fn slug_for(project_path: &str, label: &str, cwd: &str) -> String {
    format!("{label}{}", pyval::slug_for(project_path, cwd))
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

    #[test]
    fn the_label_prefixes_the_slug_so_pi_and_omp_stay_apart() {
        assert_eq!(
            slug_for("/Users/me/app", "pi", "/cwd"),
            "pi-Users-me-app",
            "the slug is the claude one with the source label glued on"
        );
        assert_eq!(slug_for("/Users/me/app", "omp", "/cwd"), "omp-Users-me-app");
        // Underscores collapse the same way they do everywhere else.
        assert_eq!(slug_for("/a/my_app/", "pi", "/cwd"), "pi-a-my-app");
    }

    #[test]
    fn both_default_roots_are_watched_even_when_absent() {
        let adapter = PiAdapter::new();
        let watched = adapter.watch_paths();
        assert_eq!(watched.len(), 2);
        assert!(watched[0].ends_with(".pi/agent/sessions"), "{watched:?}");
        assert!(watched[1].ends_with(".omp/agent/sessions"), "{watched:?}");
    }

    #[test]
    fn an_absent_root_enumerates_empty_rather_than_failing() {
        let adapter = PiAdapter::with_roots(vec![(
            PathBuf::from("/nonexistent/stax/pi"),
            "pi".to_string(),
        )]);
        assert!(adapter.enumerate().is_empty());
    }
}
