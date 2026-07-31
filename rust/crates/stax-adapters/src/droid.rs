//! Droid (Factory) — the port of `stackunderflow/adapters/droid.py`.
//!
//! ```text
//! {factoryDir}/sessions/{projectHash}/{sessionId}.jsonl
//! {factoryDir}/sessions/{projectHash}/{sessionId}.settings.json
//! ```
//!
//! `$FACTORY_DIR` relocates the base (Droid's own convention); the default is
//! `~/.factory`. The JSONL carries the conversation, the `.settings.json`
//! side-car carries the model **and the session's whole token usage**.
//!
//! ## The quirk that shapes everything: session-level tokens
//!
//! Droid records no per-message usage. The Python original spreads the session
//! totals **evenly** across the assistant messages, with the last one absorbing
//! the remainder so the sum is preserved exactly
//! ([`distribute_session_tokens`]). Zero assistant messages means the totals are
//! dropped on the floor — pricing a record that does not exist would be invented
//! data.
//!
//! ## Ported bug: the resumed read misattributes the split
//!
//! `read()` counts assistant messages it *skips* below the watermark so the
//! remaining records get the right slice. But `iter_jsonl_lines` **seeks** to
//! `since_offset`, so the only line the skip branch ever sees is the one
//! starting exactly at the watermark: on a resumed read `assistant_idx` starts
//! at ~0 and the first record past the floor is handed slice 0. The
//! distribution is stable across resumes (it is recomputed from the whole file),
//! but its *attribution* is not. Ported as-is — `read(ref, since_offset)` must
//! be byte-identical to Python's, and a "fix" here would be a silent divergence
//! in the one place a watcher-driven ingest lives. See the `DIVERGENCE` note on
//! [`DroidAdapter::read_into`].

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::base::{Record, SessionRef, SourceAdapter, Speed, stat_ref_fields};
use crate::jsonl::{self, JsonlLines, py_bytes_strip};
use crate::{blocks, pyval, walk};

/// The provider key.
pub const NAME: &str = "droid";

/// The environment variable that relocates the Factory base (`_factory_root`).
pub const FACTORY_DIR_ENV: &str = "FACTORY_DIR";

/// The block type that counts as a tool call (`_tools_from_content`).
pub const TOOL_BLOCK_TYPES: [&str; 1] = ["tool_use"];

/// The canonical four token slots, as the side-car reports them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Tokens {
    /// Fresh (uncached) input.
    pub input: i64,
    /// Billable output, with extended-thinking tokens folded in.
    pub output: i64,
    /// Cache-write tokens.
    pub cache_creation: i64,
    /// Cache-read tokens.
    pub cache_read: i64,
}

/// Factory's base directory, with the environment injected (`_factory_root`).
///
/// `$FACTORY_DIR` when set and non-blank (Python `.strip()`s it), `~` expanded;
/// otherwise `<home>/.factory`.
#[must_use]
pub fn resolve_factory_root(factory_dir: Option<&OsStr>, home: Option<&Path>) -> PathBuf {
    let configured = factory_dir
        .map(|value| value.to_string_lossy().trim().to_string())
        .filter(|value| !value.is_empty());
    match configured {
        Some(value) => expand_user(Path::new(&value), home),
        None => home.map_or_else(|| PathBuf::from(".factory"), |home| home.join(".factory")),
    }
}

/// Expand a leading `~` against `home`, as `Path.expanduser()` does.
fn expand_user(path: &Path, home: Option<&Path>) -> PathBuf {
    let Some(home) = home else {
        return path.to_path_buf();
    };
    let mut parts = path.components();
    match parts.next() {
        Some(std::path::Component::Normal(first)) if first == OsStr::new("~") => {
            home.join(parts.as_path())
        }
        _ => path.to_path_buf(),
    }
}

/// The Droid source adapter (`DroidAdapter`).
#[derive(Debug, Clone)]
pub struct DroidAdapter {
    root: PathBuf,
}

impl Default for DroidAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl DroidAdapter {
    /// `$FACTORY_DIR/sessions` (or `~/.factory/sessions`), from the live
    /// environment.
    #[must_use]
    pub fn new() -> Self {
        #[allow(
            deprecated,
            reason = "std::env::home_dir is the platform-correct answer on the \
            1.97.1 pin; the claude and codex adapters carry the same allow"
        )]
        let home = std::env::home_dir();
        Self::with_env(std::env::var_os(FACTORY_DIR_ENV), home)
    }

    /// Inject both environment inputs — `$FACTORY_DIR` and the home directory.
    #[must_use]
    pub fn with_env(factory_dir: Option<OsString>, home: Option<PathBuf>) -> Self {
        Self {
            root: resolve_factory_root(factory_dir.as_deref(), home.as_deref()).join("sessions"),
        }
    }

    /// Inject the `sessions/` directory directly — the constructor parameter
    /// Python already has.
    #[must_use]
    pub fn with_sessions_root(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The `sessions/` directory this adapter reads.
    #[must_use]
    pub fn sessions_root(&self) -> &Path {
        &self.root
    }
}

impl SourceAdapter for DroidAdapter {
    fn name(&self) -> &str {
        NAME
    }

    fn enumerate(&self) -> Vec<SessionRef> {
        if !self.root.is_dir() {
            return Vec::new();
        }
        let cwd = current_dir_string();
        let mut out = Vec::new();
        for project_dir in walk::child_dirs(&self.root) {
            for path in walk::glob_suffix(&project_dir, ".jsonl") {
                // Python warns and continues on a stat failure.
                let Some((mtime, size)) = stat_ref_fields(&path) else {
                    continue;
                };
                let (peeked_id, peeked_cwd) = read_session_meta(&path);
                let session_id = if peeked_id.is_empty() {
                    walk::file_stem(&path)
                } else {
                    peeked_id
                };
                let project_slug = if peeked_cwd.is_empty() {
                    walk::dir_name(&project_dir)
                } else {
                    pyval::slug_for(&peeked_cwd, &cwd)
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
        out
    }

    /// # DIVERGENCE (none — the resume misattribution is ported deliberately)
    ///
    /// The `assistant_idx` bookkeeping in the `line_offset <= since_offset`
    /// branch is unreachable for every offset but the watermark's own line,
    /// because the reader seeks. That is the Python behaviour, byte for byte,
    /// and the parity harness sweeps every line boundary to prove it. See the
    /// module docs for why it is not "fixed" here.
    fn read_into(&self, session: &SessionRef, since_offset: i64, sink: &mut dyn FnMut(Record)) {
        // Checked before the side-car load and the assistant pre-pass, so a
        // file we would refuse to parse costs one `stat`.
        if jsonl::stat_or_skip(&session.file_path).is_none() {
            return;
        }
        let settings_path = walk::with_suffix(&session.file_path, ".settings.json");
        let (model, totals) = load_settings(&settings_path);
        let assistant_count = count_assistant_messages(&session.file_path);
        let per_record = distribute_session_tokens(totals, assistant_count);
        let mut assistant_idx = 0_usize;

        for (line_offset, raw_line) in JsonlLines::open(&session.file_path, since_offset) {
            if since_offset > 0 && line_offset <= since_offset {
                // The caller already saw this record; still count the assistant
                // messages we skip so the *remaining* records get the right
                // slice of the distributed totals.
                if line_is_assistant_message(&raw_line) {
                    assistant_idx += 1;
                }
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
            match map.get("type").and_then(Value::as_str) {
                // Already consumed for the SessionRef; nothing to emit.
                Some("session_start") | None => continue,
                Some("message") => {}
                Some(_) => continue,
            }
            let Some(message) = map.get("message").and_then(Value::as_object) else {
                continue;
            };
            let role = match message.get("role").and_then(Value::as_str) {
                Some(role @ ("user" | "assistant")) => role,
                _ => continue,
            };

            let tokens = if role == "assistant" {
                per_record.get(assistant_idx).copied().unwrap_or_default()
            } else {
                Tokens::default()
            };
            if role == "assistant" {
                assistant_idx += 1;
            }
            let content = message.get("content");

            sink(Record {
                provider: NAME.to_string(),
                session_id: session.session_id.clone(),
                seq: line_offset,
                timestamp: map
                    .get("timestamp")
                    .filter(|value| pyval::py_truthy(value))
                    .map_or_else(String::new, pyval::py_str),
                role: role.to_string(),
                model: model.clone(),
                input_tokens: tokens.input,
                output_tokens: tokens.output,
                cache_create_tokens: tokens.cache_creation,
                cache_read_tokens: tokens.cache_read,
                content_text: blocks::message_text(content),
                tools: blocks::tool_names(content, &TOOL_BLOCK_TYPES),
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

    /// The `sessions/` root (`source_roots`). Droid declares no `watch_paths`,
    /// so the watcher falls back to periodic ingest — the default `[]` from the
    /// trait is exactly Python's missing-method case.
    fn source_roots(&self) -> Vec<PathBuf> {
        vec![self.root.clone()]
    }
}

/// `(session_id, cwd)` from the `session_start` header (`_read_session_meta`).
fn read_session_meta(path: &Path) -> (String, String) {
    let empty = || (String::new(), String::new());
    let Some(first) = walk::first_line(path) else {
        return empty();
    };
    let stripped = py_bytes_strip(&first);
    if stripped.is_empty() {
        return empty();
    }
    let Some(obj) = jsonl::parse_json(stripped) else {
        return empty();
    };
    let Some(map) = obj.as_object() else {
        return empty();
    };
    if map.get("type").and_then(Value::as_str) != Some("session_start") {
        return empty();
    }
    let field = |key: &str| {
        map.get(key)
            .filter(|value| pyval::py_truthy(value))
            .map_or_else(String::new, pyval::py_str)
    };
    (field("id"), field("cwd"))
}

/// The `.settings.json` side-car: `(model, session totals)` (`_load_settings`).
///
/// A missing, unreadable, or non-object side-car is `(None, zeros)` — never an
/// error. `thinkingTokens` folds into `output` so the Anthropic pricer bills the
/// session the way Anthropic does.
#[must_use]
pub fn load_settings(path: &Path) -> (Option<String>, Tokens) {
    if !path.is_file() {
        return (None, Tokens::default());
    }
    // LOG: python warns "Cannot read Droid settings %s".
    let Ok(text) = std::fs::read(path) else {
        return (None, Tokens::default());
    };
    let Some(obj) = jsonl::parse_json(&text) else {
        return (None, Tokens::default());
    };
    let Some(map) = obj.as_object() else {
        return (None, Tokens::default());
    };
    let model = map
        .get("model")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string);
    let empty = serde_json::Map::new();
    let usage = map
        .get("tokenUsage")
        .and_then(Value::as_object)
        .unwrap_or(&empty);
    let tokens = Tokens {
        input: pyval::safe_int(usage.get("inputTokens")),
        output: pyval::safe_int(usage.get("outputTokens"))
            .saturating_add(pyval::safe_int(usage.get("thinkingTokens"))),
        cache_creation: pyval::safe_int(usage.get("cacheCreationTokens")),
        cache_read: pyval::safe_int(usage.get("cacheReadTokens")),
    };
    (model, tokens)
}

/// One pass over the file counting assistant messages
/// (`_count_assistant_messages`).
fn count_assistant_messages(path: &Path) -> usize {
    JsonlLines::open(path, 0)
        .filter(|(_, line)| line_is_assistant_message(line))
        .count()
}

/// Whether one raw line is a `message` event with `role == "assistant"`
/// (`_line_is_assistant_message`).
fn line_is_assistant_message(raw: &[u8]) -> bool {
    let stripped = py_bytes_strip(raw);
    if stripped.is_empty() {
        return false;
    }
    let Some(obj) = jsonl::parse_json(stripped) else {
        return false;
    };
    let Some(map) = obj.as_object() else {
        return false;
    };
    if map.get("type").and_then(Value::as_str) != Some("message") {
        return false;
    }
    map.get("message")
        .and_then(Value::as_object)
        .and_then(|message| message.get("role"))
        .and_then(Value::as_str)
        == Some("assistant")
}

/// Spread session totals evenly across `n` assistant messages, remainder to the
/// last (`_distribute_session_tokens`).
///
/// `n == 0` returns an empty list: the totals are dropped rather than attributed
/// to a record that does not exist.
#[must_use]
pub fn distribute_session_tokens(totals: Tokens, n: usize) -> Vec<Tokens> {
    if n == 0 {
        return Vec::new();
    }
    let divisor = i64::try_from(n).unwrap_or(i64::MAX);
    let base = Tokens {
        input: totals.input / divisor,
        output: totals.output / divisor,
        cache_creation: totals.cache_creation / divisor,
        cache_read: totals.cache_read / divisor,
    };
    let remainder = Tokens {
        input: totals.input - base.input * divisor,
        output: totals.output - base.output * divisor,
        cache_creation: totals.cache_creation - base.cache_creation * divisor,
        cache_read: totals.cache_read - base.cache_read * divisor,
    };
    let mut out = vec![base; n];
    if let Some(last) = out.last_mut() {
        last.input += remainder.input;
        last.output += remainder.output;
        last.cache_creation += remainder.cache_creation;
        last.cache_read += remainder.cache_read;
    }
    out
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
    fn factory_dir_env_overrides_the_home_layout() {
        let home = Path::new("/home/me");
        assert_eq!(
            resolve_factory_root(None, Some(home)),
            Path::new("/home/me/.factory")
        );
        assert_eq!(
            resolve_factory_root(Some(OsStr::new("  ")), Some(home)),
            Path::new("/home/me/.factory"),
            "a blank FACTORY_DIR is Python-falsy after .strip()"
        );
        assert_eq!(
            resolve_factory_root(Some(OsStr::new("/opt/factory")), Some(home)),
            Path::new("/opt/factory")
        );
        assert_eq!(
            resolve_factory_root(Some(OsStr::new("~/elsewhere")), Some(home)),
            Path::new("/home/me/elsewhere")
        );
        assert!(
            DroidAdapter::with_env(Some("/opt/factory".into()), Some(home.into()))
                .sessions_root()
                .ends_with("sessions")
        );
    }

    #[test]
    fn the_distribution_sums_back_to_the_session_totals() {
        let totals = Tokens {
            input: 4000,
            output: 1400,
            cache_creation: 600,
            cache_read: 2000,
        };
        // The fixture's numbers over its two assistant messages.
        let split = distribute_session_tokens(totals, 2);
        assert_eq!(split.len(), 2);
        assert_eq!(split[0].input + split[1].input, 4000);
        assert_eq!(split[0].output + split[1].output, 1400);

        // An indivisible total puts the remainder on the last record.
        let odd = distribute_session_tokens(
            Tokens {
                input: 10,
                output: 7,
                cache_creation: 0,
                cache_read: 1,
            },
            3,
        );
        assert_eq!(
            odd.iter().map(|t| t.input).collect::<Vec<_>>(),
            vec![3, 3, 4]
        );
        assert_eq!(
            odd.iter().map(|t| t.output).collect::<Vec<_>>(),
            vec![2, 2, 3]
        );
        assert_eq!(
            odd.iter().map(|t| t.cache_read).collect::<Vec<_>>(),
            vec![0, 0, 1]
        );
    }

    #[test]
    fn zero_assistant_messages_drops_the_totals_rather_than_inventing_a_record() {
        let totals = Tokens {
            input: 4000,
            output: 1200,
            cache_creation: 600,
            cache_read: 2000,
        };
        assert!(distribute_session_tokens(totals, 0).is_empty());
    }

    #[test]
    fn a_missing_side_car_is_zeros_and_no_model() {
        let (model, totals) = load_settings(Path::new("/nonexistent/stax/x.settings.json"));
        assert_eq!(model, None);
        assert_eq!(totals, Tokens::default());
    }

    #[test]
    fn an_absent_root_enumerates_empty_rather_than_failing() {
        let adapter = DroidAdapter::with_sessions_root("/nonexistent/stax/droid");
        assert!(adapter.enumerate().is_empty());
        // Droid declares source_roots but no watch_paths.
        assert_eq!(adapter.source_roots().len(), 1);
        assert!(adapter.watch_paths().is_empty());
    }
}
