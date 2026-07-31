//! Kiro (kiroagent) — the port of `stackunderflow/adapters/kiro.py`.
//!
//! Chat files live under a VS Code-style `globalStorage` root:
//!
//! ```text
//! macOS    ~/Library/Application Support/Kiro/User/globalStorage/kiro.kiroagent/
//! Linux    ~/.config/Kiro/User/globalStorage/kiro.kiroagent/          (untested)
//! Windows  %APPDATA%\Kiro\User\globalStorage\kiro.kiroagent\          (untested)
//! ```
//!
//! Each `.chat` file is one JSON document holding a whole execution, and the
//! adapter rolls that execution up into **one** assistant record.
//!
//! Three quirks that are not obvious from the shape:
//!
//! 1. **Tokens are estimated**, `len(text) // 4` over the human and bot sides
//!    separately, because Kiro records no usage. Every record carries
//!    `raw["cost_source"] = "estimated"` so the cost layer can down-weight it.
//!    `len` there is Python's — **code points, not bytes** — which is why this
//!    port counts `chars()`.
//! 2. **Model ids are dot-separated** (`claude.3.5.sonnet`) and are rewritten to
//!    the dash form so the Anthropic pricer's family heuristic matches.
//! 3. **Tool names are scraped out of the bot text** as `<tool_use><name>X</name>`
//!    fragments — a permissive split, never an XML parse, so a truncated
//!    fragment yields no tools instead of an error.
//!
//! Resume is by event index over a single record: `since_offset > 0` yields
//! nothing, which is what "past the only record" means.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::base::{Record, SessionRef, SourceAdapter, Speed, stat_ref_fields};
use crate::jsonl;
use crate::pyval;
use crate::walk;

/// The provider key.
pub const NAME: &str = "kiro";

/// The model stamped when `metadata.modelId` is missing or blank
/// (`_DEFAULT_MODEL`).
pub const DEFAULT_MODEL: &str = "kiro-auto";

/// The extension-scoped directory every layout ends in.
pub const STORAGE_LEAF: &str = "kiro.kiroagent";

/// Kiro's `globalStorage` root, with the platform and environment injected
/// (`_kiro_global_storage`).
///
/// `os` is `std::env::consts::OS`; the Python original branches on
/// `sys.platform`, and the two agree on the only distinction that matters here
/// (windows / linux / everything-else-is-macOS). Injected rather than read so
/// the Windows and Linux layouts are testable on any box — the same
/// `sys.platform` test pattern the Python suite uses.
#[must_use]
pub fn resolve_global_storage(os: &str, appdata: Option<&OsStr>, home: Option<&Path>) -> PathBuf {
    let tail = |base: PathBuf| {
        base.join("Kiro")
            .join("User")
            .join("globalStorage")
            .join(STORAGE_LEAF)
    };
    if os == "windows" {
        // `Path(os.environ.get("APPDATA", ""))` — an unset APPDATA yields a
        // relative path, and `.is_dir()` then quietly fails. Ported as-is.
        return tail(PathBuf::from(appdata.unwrap_or_else(|| OsStr::new(""))));
    }
    let home = home.map_or_else(PathBuf::new, Path::to_path_buf);
    if os == "linux" {
        return tail(home.join(".config"));
    }
    tail(home.join("Library").join("Application Support"))
}

/// The Kiro source adapter (`KiroAdapter`).
#[derive(Debug, Clone)]
pub struct KiroAdapter {
    root: PathBuf,
}

impl Default for KiroAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl KiroAdapter {
    /// The platform-appropriate storage root, from the live environment.
    #[must_use]
    pub fn new() -> Self {
        #[allow(
            deprecated,
            reason = "std::env::home_dir is the platform-correct answer on the \
            1.97.1 pin; the claude and codex adapters carry the same allow"
        )]
        let home = std::env::home_dir();
        Self::with_env(std::env::consts::OS, std::env::var_os("APPDATA"), home)
    }

    /// Inject the platform and both environment inputs.
    #[must_use]
    pub fn with_env(os: &str, appdata: Option<OsString>, home: Option<PathBuf>) -> Self {
        Self {
            root: resolve_global_storage(os, appdata.as_deref(), home.as_deref()),
        }
    }

    /// Inject the storage root directly — `KiroAdapter(storage_root=…)`.
    #[must_use]
    pub fn with_storage_root(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The storage root this adapter reads.
    #[must_use]
    pub fn storage_root(&self) -> &Path {
        &self.root
    }
}

impl SourceAdapter for KiroAdapter {
    fn name(&self) -> &str {
        NAME
    }

    fn enumerate(&self) -> Vec<SessionRef> {
        if !self.root.is_dir() {
            return Vec::new();
        }
        let mut out = Vec::new();
        // `.chat` files sit at the storage root and under workspace-scoped
        // subtrees, so the walk is recursive.
        for path in walk::rglob_suffix(&self.root, ".chat") {
            // Python warns and continues on a stat failure.
            let Some((mtime, size)) = stat_ref_fields(&path) else {
                continue;
            };
            let (workflow_id, project_slug) = peek_metadata(&path);
            let session_id = if workflow_id.is_empty() {
                walk::file_stem(&path)
            } else {
                workflow_id
            };
            let project_slug = if project_slug.is_empty() {
                NAME.to_string()
            } else {
                project_slug
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
        out
    }

    fn read_into(&self, session: &SessionRef, since_offset: i64, sink: &mut dyn FnMut(Record)) {
        // Single-document JSON cannot stream, so the 128 MB cap is the only
        // safety net; above it we yield nothing, like every JSONL adapter.
        if jsonl::stat_or_skip(&session.file_path).is_none() {
            return;
        }
        // LOG: python warns "Cannot read Kiro chat %s".
        let Ok(text) = std::fs::read(&session.file_path) else {
            return;
        };
        let Some(document) = jsonl::parse_json(&text) else {
            return;
        };
        let Some(data) = document.as_object() else {
            return;
        };

        let empty = Map::new();
        // `data.get("metadata") or {}` then an isinstance check: falsy *and*
        // non-dict both become the empty mapping.
        let meta = data
            .get("metadata")
            .filter(|value| pyval::py_truthy(value))
            .and_then(Value::as_object)
            .unwrap_or(&empty);

        let model = match meta.get("modelId") {
            Some(Value::String(raw)) => normalize_model(raw),
            // A non-string modelId never reaches `_normalize_model`.
            _ => DEFAULT_MODEL.to_string(),
        };
        let timestamp = meta
            .get("startTime")
            .filter(|value| pyval::py_truthy(value))
            .or_else(|| meta.get("endTime").filter(|value| pyval::py_truthy(value)))
            .map_or_else(String::new, pyval::py_str);
        let chat = data
            .get("chat")
            .filter(|value| pyval::py_truthy(value))
            .and_then(Value::as_array)
            .map_or(&[][..], Vec::as_slice);

        // One logical turn per execution: a watermark past it yields nothing.
        if since_offset > 0 {
            return;
        }

        let (human_text, bot_text) = join_chat(chat);
        let mut raw_payload = data.clone();
        raw_payload.insert("cost_source".to_string(), Value::from("estimated"));

        sink(Record {
            provider: NAME.to_string(),
            session_id: session.session_id.clone(),
            seq: 0,
            timestamp,
            role: "assistant".to_string(),
            model: Some(model),
            input_tokens: estimate_tokens(&human_text),
            output_tokens: estimate_tokens(&bot_text),
            cache_create_tokens: 0,
            cache_read_tokens: 0,
            tools: extract_tools(&bot_text),
            content_text: bot_text,
            cwd: None,
            is_sidechain: false,
            uuid: data
                .get("executionId")
                .filter(|value| pyval::py_truthy(value))
                .map_or_else(|| format!("{}:0", session.session_id), pyval::py_str),
            parent_uuid: None,
            raw: Value::Object(raw_payload),
            speed: Speed::Standard,
        });
    }

    /// The storage root (`source_roots`). Kiro declares no `watch_paths`.
    fn source_roots(&self) -> Vec<PathBuf> {
        vec![self.root.clone()]
    }
}

/// `(workflow_id, project_slug)` from the document's metadata (`_peek_metadata`).
///
/// The project slug is the parent directory name, a stand-in for "workspace":
/// Kiro's workspace-hash → directory mapping is not exposed.
fn peek_metadata(path: &Path) -> (String, String) {
    let Ok(text) = std::fs::read(path) else {
        return (String::new(), String::new());
    };
    let Some(document) = jsonl::parse_json(&text) else {
        return (String::new(), String::new());
    };
    let Some(map) = document.as_object() else {
        return (String::new(), String::new());
    };
    let workflow_id = map
        .get("metadata")
        .filter(|value| pyval::py_truthy(value))
        .and_then(Value::as_object)
        .and_then(|meta| meta.get("workflowId"))
        .filter(|value| pyval::py_truthy(value))
        .map_or_else(String::new, pyval::py_str);
    let parent = path.parent().map_or_else(String::new, walk::dir_name);
    let parent = if parent.is_empty() {
        NAME.to_string()
    } else {
        parent
    };
    (workflow_id, parent)
}

/// `claude.3.5.sonnet` → `claude-3-5-sonnet` (`_normalize_model`).
#[must_use]
pub fn normalize_model(model_id: &str) -> String {
    if model_id.is_empty() {
        return DEFAULT_MODEL.to_string();
    }
    let normalized = model_id.replace('.', "-");
    let trimmed = normalized.trim();
    if trimmed.is_empty() {
        DEFAULT_MODEL.to_string()
    } else {
        trimmed.to_string()
    }
}

/// `(human_text, bot_text)`, each side joined with newlines (`_join_chat`).
///
/// The `tool` role is ignored for the token estimate; tool *names* come out of
/// the bot text instead.
fn join_chat(chat: &[Value]) -> (String, String) {
    let mut human: Vec<&str> = Vec::new();
    let mut bot: Vec<&str> = Vec::new();
    for entry in chat {
        let Some(map) = entry.as_object() else {
            continue;
        };
        // `content if isinstance(content, str) else ""` — a non-string content
        // still contributes an empty piece, and so still costs a newline.
        let text = map.get("content").and_then(Value::as_str).unwrap_or("");
        match map.get("role").and_then(Value::as_str) {
            Some("human") => human.push(text),
            Some("bot") => bot.push(text),
            _ => {}
        }
    }
    (human.join("\n"), bot.join("\n"))
}

/// `max(len(text) // 4, 0)` — Python's `len`, i.e. code points.
fn estimate_tokens(text: &str) -> i64 {
    i64::try_from(text.chars().count() / 4).unwrap_or(i64::MAX)
}

/// Tool names from `<tool_use><name>X</name>` markers (`_extract_tools`).
///
/// Deliberately a permissive scan: the `<name>` is searched for *after* the
/// `<tool_use>` marker with no bound, so a malformed fragment simply finds the
/// next well-formed one, and a missing terminator ends the scan rather than
/// raising.
#[must_use]
pub fn extract_tools(bot_text: &str) -> Vec<String> {
    if bot_text.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut cursor = 0_usize;
    while let Some(start) = bot_text[cursor..].find("<tool_use>").map(|at| cursor + at) {
        let Some(open) = bot_text[start..].find("<name>").map(|at| start + at) else {
            break;
        };
        let Some(close) = bot_text[open..].find("</name>").map(|at| open + at) else {
            break;
        };
        let name = bot_text[open + "<name>".len()..close].trim();
        if !name.is_empty() {
            out.push(name.to_string());
        }
        cursor = close + "</name>".len();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn every_platform_layout_resolves_without_running_on_it() {
        let home = Path::new("/home/me");
        assert_eq!(
            resolve_global_storage("linux", None, Some(home)),
            Path::new("/home/me/.config/Kiro/User/globalStorage/kiro.kiroagent")
        );
        assert_eq!(
            resolve_global_storage("macos", None, Some(home)),
            Path::new(
                "/home/me/Library/Application Support/Kiro/User/globalStorage/kiro.kiroagent"
            )
        );
        assert_eq!(
            resolve_global_storage("windows", Some(OsStr::new(r"C:\Users\me\AppData")), None),
            Path::new(r"C:\Users\me\AppData")
                .join("Kiro")
                .join("User")
                .join("globalStorage")
                .join("kiro.kiroagent")
        );
        // An unset APPDATA yields a relative path, exactly as Python's
        // `Path(os.environ.get("APPDATA", ""))` does.
        assert_eq!(
            resolve_global_storage("windows", None, Some(home)),
            Path::new("Kiro/User/globalStorage/kiro.kiroagent")
        );
    }

    #[test]
    fn model_ids_lose_their_dots_and_blank_ones_fall_back() {
        assert_eq!(normalize_model("claude.3.5.sonnet"), "claude-3-5-sonnet");
        assert_eq!(normalize_model("gpt-5"), "gpt-5");
        assert_eq!(normalize_model(""), DEFAULT_MODEL);
        assert_eq!(normalize_model("   "), DEFAULT_MODEL);
        assert_eq!(normalize_model("  a.b  "), "a-b");
    }

    #[test]
    fn tool_scraping_survives_truncated_fragments() {
        assert_eq!(
            extract_tools("x<tool_use><name>Edit</name>y<tool_use><name>Read</name>"),
            vec!["Edit", "Read"]
        );
        // No marker, no name, no terminator: three ways to find nothing.
        assert!(extract_tools("plain text").is_empty());
        assert!(extract_tools("<tool_use>no name here").is_empty());
        assert!(extract_tools("<tool_use><name>unterminated").is_empty());
        assert!(extract_tools("<tool_use><name>  </name>").is_empty());
        assert!(extract_tools("").is_empty());
        // The name search is unbounded on purpose — a stray marker picks up
        // the next well-formed fragment rather than aborting the scan.
        assert_eq!(
            extract_tools("<tool_use>junk<name>Bash</name>"),
            vec!["Bash"]
        );
    }

    #[test]
    fn token_estimates_count_code_points_not_bytes() {
        // Eight ASCII characters -> 2; eight emoji (4 bytes each) -> also 2.
        assert_eq!(estimate_tokens("abcdefgh"), 2);
        assert_eq!(estimate_tokens("🙂🙂🙂🙂🙂🙂🙂🙂"), 2);
        assert_eq!(estimate_tokens(""), 0);
        assert_eq!(estimate_tokens("abc"), 0);
    }

    #[test]
    fn chat_sides_are_joined_separately_and_tool_turns_ignored() {
        let chat = json!([
            {"role": "human", "content": "ask"},
            {"role": "bot", "content": "answer"},
            {"role": "tool", "content": "ignored"},
            {"role": "bot", "content": 7},
            "not a dict",
        ]);
        let (human, bot) = join_chat(chat.as_array().expect("array"));
        assert_eq!(human, "ask");
        // The non-string bot content contributes an empty piece, and the
        // newline it costs is the observable difference.
        assert_eq!(bot, "answer\n");
    }

    #[test]
    fn an_absent_root_enumerates_empty_rather_than_failing() {
        let adapter = KiroAdapter::with_storage_root("/nonexistent/stax/kiro");
        assert!(adapter.enumerate().is_empty());
        assert_eq!(adapter.source_roots().len(), 1);
        assert!(adapter.watch_paths().is_empty());
    }
}
