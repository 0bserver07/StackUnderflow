//! OpenAI Codex CLI — the port of `python-legacy: adapters/codex.py`.
//!
//! Rollouts live at `~/.codex/sessions/YYYY/MM/DD/rollout-<ts>-<uuid>.jsonl`.
//! Line 1 is a `session_meta` event carrying `id`, `cwd`, `originator` (must
//! start with `codex`) and `cli_version`. After that:
//!
//! * `turn_context` — **the model id's only home in real rollouts**, one per
//!   turn, and it changes mid-session when the user runs `/model`.
//! * `response_item` — messages and function calls; these become records.
//! * `event_msg` with `type == "token_count"` — per-turn usage, which is
//!   attached *retroactively* to the turn's last assistant record and then
//!   flushes the buffer.
//!
//! ## Two behaviours that exist because they were bugs
//!
//! 1. **Model stamping.** A `None` model makes the codex normalizer drop the
//!    turn as unpriceable — which is how 1,486 base messages sat at zero
//!    `usage_events` while every unit test stayed green. The adapter tracks the
//!    current `turn_context` model and stamps it on every record.
//! 2. **The batch-boundary seed.** A resumed read starts *past* the turn's
//!    `turn_context` (the ingest watermark is always a `response_item` offset,
//!    because `turn_context` lines yield no record and so never advance it).
//!    Without a seed, every boundary-straddling turn would be stamped
//!    `model=None` and silently dropped — permanent usage loss on the watcher
//!    path. [`CodexAdapter::model_before_offset`] re-scans the already-ingested
//!    prefix and yields nothing, so the resumed record set is byte-identical to
//!    what it was before the fix.
//!
//! Token shape: this adapter emits the *canonical* four slots. OpenAI's raw
//! shape (cached nested inside input, reasoning separate from output) is
//! flattened by the one seam shared with `OpenAIPricer.normalize_tokens`
//! ([`canonicalize_openai_usage`]).

use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::base::{Record, SessionRef, SourceAdapter, Speed, stat_ref_fields};
use crate::jsonl::{self, JsonlLines, py_bytes_strip};
use crate::pyval;

/// The provider key.
pub const NAME: &str = "codex";

/// Codex tool name → canonical cross-source tool label (`_TOOL_NAME_MAP`).
///
/// Unknown names pass through untouched so a new Codex tool stays visible until
/// it is classified.
pub const TOOL_NAME_MAP: [(&str, &str); 9] = [
    ("exec_command", "Bash"),
    ("read_file", "Read"),
    ("write_file", "Edit"),
    ("apply_diff", "Edit"),
    ("apply_patch", "Edit"),
    ("spawn_agent", "Agent"),
    ("close_agent", "Agent"),
    ("wait_agent", "Agent"),
    ("read_dir", "Glob"),
];

/// Rollouts bigger than this are warned about but still parsed
/// (`_LARGE_FILE_BYTES`). The hard 128 MB skip lives in
/// [`crate::jsonl::MAX_SESSION_FILE_BYTES`].
pub const LARGE_FILE_BYTES: u64 = 64 * 1024 * 1024;

/// Codex writes its own framing as `role: "user"` messages — the AGENTS.md
/// dump, the environment block, the goal-thread context. They are framework
/// text, not the human: left in, they become session "titles"
/// (`<codex_internal_context source="goal">Continue working toward…` on every
/// fleet session), pollute `memory decisions` search, and inflate
/// `message_count`. Skipping them is the same conversational filtering the
/// adapter already applies to Codex's `developer`/`system` pseudo-turns — a
/// mislabelled framework message is still a framework message. Measured
/// prefixes from real rollouts; an unknown future prefix fails open (the
/// message is kept), never silently drops a human prompt.
pub const INTERNAL_USER_PREFIXES: [&str; 5] = [
    "<codex_internal_context",
    "<environment_context>",
    "<user_instructions>",
    "<turn_context>",
    "# AGENTS.md instructions",
];

/// Is this `role: user` text Codex's own framing rather than the human?
#[must_use]
pub fn is_internal_user_text(text: &str) -> bool {
    INTERNAL_USER_PREFIXES
        .iter()
        .any(|prefix| text.starts_with(prefix))
}

/// The Codex CLI source adapter (`CodexAdapter`).
#[derive(Debug, Clone)]
pub struct CodexAdapter {
    root: PathBuf,
}

impl Default for CodexAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl CodexAdapter {
    /// `~/.codex/sessions`, resolved once at construction — as Python's
    /// `sessions_root or (Path.home() / ".codex" / "sessions")` does.
    #[must_use]
    pub fn new() -> Self {
        #[allow(
            deprecated,
            reason = "std::env::home_dir is the platform-correct answer on the \
            1.97.1 pin; stax-core's settings module carries the same allow"
        )]
        let home = std::env::home_dir().unwrap_or_default();
        Self {
            root: home.join(".codex").join("sessions"),
        }
    }

    /// Inject the sessions root — the constructor parameter Python already has.
    #[must_use]
    pub fn with_sessions_root(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The rollout root this adapter reads.
    #[must_use]
    pub fn sessions_root(&self) -> &Path {
        &self.root
    }

    /// The first-line `session_meta` event, normalised to the modern wrapper
    /// shape `{type, timestamp, payload}` (`_read_session_meta`).
    ///
    /// Pre-0.20 rollouts omit the wrapper and inline the metadata on the root
    /// object; those are coerced into the wrapper shape so `enumerate` treats
    /// both formats uniformly. A non-object first line means "not a rollout we
    /// understand" and returns `None` — returning rather than raising is the
    /// point: an unparseable file must not abort enumeration for the provider.
    fn read_session_meta(path: &Path) -> Option<Value> {
        let first_line = read_first_line(path)?;
        let stripped = py_bytes_strip(&first_line);
        if stripped.is_empty() {
            return None;
        }
        let obj: Value = jsonl::parse_json(stripped)?;
        let map = obj.as_object()?;
        if map.get("type").and_then(Value::as_str) == Some("session_meta") {
            if !map.get("payload").is_some_and(Value::is_object) {
                return None;
            }
            return Some(obj);
        }
        // Legacy inline shape: accept if it at least carries a string `id`.
        if map.get("id").and_then(Value::as_str).is_some() {
            let mut wrapper = Map::new();
            wrapper.insert("type".to_string(), Value::from("session_meta"));
            wrapper.insert(
                "timestamp".to_string(),
                map.get("timestamp").cloned().unwrap_or_else(|| "".into()),
            );
            wrapper.insert("payload".to_string(), obj.clone());
            return Some(Value::Object(wrapper));
        }
        None
    }

    /// The last model declared by `session_meta`/`turn_context` in bytes
    /// `[0, upto)` (`_model_before_offset`).
    ///
    /// A linear scan of the already-ingested prefix. Re-run on every incremental
    /// tick, so the total cost over a session's life is O(prefix²) in the worst
    /// case — negligible at real rollout sizes, and correctness beats a
    /// schema-level model watermark. JSON parsing runs only on lines that can
    /// possibly match, and only a non-empty string model updates the seed.
    #[must_use]
    pub fn model_before_offset(path: &Path, upto: i64) -> Option<String> {
        let prefix = jsonl::read_prefix(path, upto)?;
        let mut model = None;
        for line in jsonl::splitlines(&prefix) {
            if !contains(line, br#""turn_context""#) && !contains(line, br#""session_meta""#) {
                continue;
            }
            let Some(event) = jsonl::parse_json(line) else {
                continue;
            };
            let Some(map) = event.as_object() else {
                continue;
            };
            if !matches!(
                map.get("type").and_then(Value::as_str),
                Some("session_meta" | "turn_context")
            ) {
                continue;
            }
            let Some(payload) = map.get("payload").and_then(Value::as_object) else {
                continue;
            };
            if let Some(candidate) = payload.get("model").and_then(Value::as_str)
                && !candidate.is_empty()
            {
                model = Some(candidate.to_string());
            }
        }
        model
    }

    /// One `response_item` → a `Record`, or `None` (`_record_from_response_item`).
    fn record_from_response_item(
        event: &Value,
        payload: &Map<String, Value>,
        session: &SessionRef,
        seq: i64,
        model: Option<&str>,
    ) -> Option<Record> {
        let kind = payload.get("type").and_then(Value::as_str);
        let timestamp = event
            .get("timestamp")
            .filter(|value| pyval::py_truthy(value))
            .map_or_else(String::new, pyval::py_str);
        let base = |role: &str, content_text: String, tools: Vec<String>| Record {
            provider: NAME.to_string(),
            session_id: session.session_id.clone(),
            seq,
            timestamp: timestamp.clone(),
            role: role.to_string(),
            model: model.map(ToString::to_string),
            input_tokens: 0,
            output_tokens: 0,
            cache_create_tokens: 0,
            cache_read_tokens: 0,
            content_text,
            tools,
            cwd: None,
            is_sidechain: false,
            uuid: format!("{}:{seq}", session.session_id),
            parent_uuid: None,
            raw: event.clone(),
            speed: Speed::Standard,
        };

        match kind {
            Some("message") => {
                // Codex also emits "developer" / "system" pseudo-turns for
                // framework messages; skipping them matches Claude's
                // conversational filtering.
                let role = payload.get("role").and_then(Value::as_str)?;
                if role != "user" && role != "assistant" {
                    return None;
                }
                let text = message_text(payload.get("content"));
                // Framework text mislabelled `user` — see INTERNAL_USER_PREFIXES.
                if role == "user" && is_internal_user_text(&text) {
                    return None;
                }
                Some(base(role, text, Vec::new()))
            }
            Some("function_call") => {
                let raw_name = payload
                    .get("name")
                    .filter(|value| pyval::py_truthy(value))
                    .map_or_else(String::new, pyval::py_str);
                // LOG: python debug-logs spawn_agent / wait_agent / close_agent
                // as "not expanded in Phase 1".
                let label = TOOL_NAME_MAP
                    .iter()
                    .find(|(from, _)| *from == raw_name)
                    .map_or(raw_name, |(_, to)| (*to).to_string());
                let tools = if label.is_empty() {
                    Vec::new()
                } else {
                    vec![label]
                };
                Some(base("assistant", String::new(), tools))
            }
            _ => None,
        }
    }
}

impl SourceAdapter for CodexAdapter {
    fn name(&self) -> &str {
        NAME
    }

    fn enumerate(&self) -> Vec<SessionRef> {
        if !self.root.is_dir() {
            return Vec::new();
        }
        let cwd = current_dir_string();
        let mut out = Vec::new();
        for path in glob_rollouts(&self.root) {
            // Python warns and continues on OSError here (a directory named
            // `rollout-*.jsonl`, an unreadable file); `None` covers both.
            let Some(meta) = Self::read_session_meta(&path) else {
                continue;
            };
            let Some(payload) = meta.get("payload").and_then(Value::as_object) else {
                continue;
            };
            // Case-insensitive: shipping builds use "codex-tui", "codex_cli_rs",
            // "Codex Desktop". Legacy rollouts (pre-`session_meta` wrapper)
            // carry no originator at all, but their location under
            // ~/.codex/sessions is signal enough.
            let originator = payload
                .get("originator")
                .filter(|value| pyval::py_truthy(value))
                .map_or_else(String::new, pyval::py_str);
            if !originator.is_empty() && !originator.to_lowercase().starts_with("codex") {
                continue;
            }
            let session_id = payload
                .get("id")
                .filter(|value| pyval::py_truthy(value))
                .map_or_else(String::new, pyval::py_str);
            if session_id.is_empty() {
                continue;
            }
            // DIVERGENCE (fixed-in-rust): Python passes a truthy non-string
            // `cwd` straight to `os.path.abspath`, which raises TypeError out
            // of enumerate() and takes the whole provider down. Here a
            // non-string cwd is treated as absent.
            let project_slug = match payload.get("cwd").and_then(Value::as_str) {
                Some(cwd_value) if !cwd_value.is_empty() => pyval::slug_for(cwd_value, &cwd),
                _ => format!("codex-{session_id}"),
            };
            // Python calls `fp.stat()` unguarded here; a file that vanishes
            // between the glob and the stat raises. Skipping is the same
            // outcome minus the crash.
            let Some((mtime, size)) = stat_ref_fields(&path) else {
                continue;
            };
            if size > LARGE_FILE_BYTES {
                // LOG: python warns "Codex rollout %s is %d bytes; reading anyway".
            }
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

    /// # Two JSON-parser divergences, both measured
    ///
    /// This adapter is the one place the port cannot be bit-exact, because the
    /// Python original parses with the **stdlib `json`** while the Claude
    /// adapter uses `orjson`, and `serde_json` matches neither on two inputs:
    ///
    /// 1. **Non-standard literals.** `Infinity` / `NaN` (what `json.dumps`
    ///    writes for `1e999`) are accepted by stdlib `json` and coerced to 0 by
    ///    `_safe_int`; `serde_json` rejects them, so the whole *line* is skipped.
    ///    Same zero tokens by a different route — but a `response_item` carrying
    ///    one would be dropped here and kept there.
    /// 2. **Integers beyond 64 bits.** Python keeps them exactly (bignum), so
    ///    `raw_json` reads `99999999999999999999999999`; `serde_json` degrades
    ///    to `f64` and writes `1e+26`. The Claude path does *not* diverge —
    ///    `orjson` degrades to a float too.
    ///
    /// Both are pinned by tests in `tests/codex_adapter.rs`.
    fn read_into(&self, session: &SessionRef, since_offset: i64, sink: &mut dyn FnMut(Record)) {
        // Records emitted since the most recent token_count, so tokens can be
        // attached retroactively to the turn's last assistant record before the
        // buffer is flushed in original order.
        let mut buffer: Vec<Record> = Vec::new();
        let mut current_model: Option<String> = None;
        if since_offset > 0 {
            current_model = Self::model_before_offset(&session.file_path, since_offset);
        }

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
            // Valid JSON that is not an object cannot be a rollout event.
            let Some(map) = event.as_object() else {
                continue;
            };
            let etype = map.get("type").and_then(Value::as_str);
            // A `payload` carrying a string/list would crash the dispatch
            // below; treat it as an empty payload.
            let empty = Map::new();
            let payload = map
                .get("payload")
                .and_then(Value::as_object)
                .unwrap_or(&empty);

            match etype {
                Some("session_meta" | "turn_context") => {
                    // `turn_context.payload.model` is the model's real home;
                    // some builds also inline one on `session_meta`. Either
                    // way: remember it, emit nothing.
                    if let Some(model) = payload.get("model").and_then(Value::as_str)
                        && !model.is_empty()
                    {
                        current_model = Some(model.to_string());
                    }
                }
                Some("response_item") => {
                    // seq = the byte offset where this line started, aligning
                    // with the Claude adapter so the storage-aware contract
                    // test ("resume from seq=midpoint") holds for both.
                    if let Some(record) = Self::record_from_response_item(
                        &event,
                        payload,
                        session,
                        line_offset,
                        current_model.as_deref(),
                    ) {
                        buffer.push(record);
                    }
                }
                Some("event_msg")
                    if payload.get("type").and_then(Value::as_str) == Some("token_count") =>
                {
                    if let Some(last) = payload
                        .get("info")
                        .and_then(Value::as_object)
                        .and_then(|info| info.get("last_token_usage"))
                        .and_then(Value::as_object)
                    {
                        attach_tokens_to_last_assistant(&mut buffer, last);
                    }
                    // Flush the completed turn whether or not the token info
                    // was usable.
                    for record in buffer.drain(..) {
                        sink(record);
                    }
                }
                // Other event_msg types (task_started, task_complete, error,
                // user_message, …) are ignored.
                _ => {}
            }
        }

        // End of file: flush records that never saw a token_count.
        for record in buffer {
            sink(record);
        }
    }

    /// `~/.codex/sessions` (`watch_paths`). The watcher filters non-existent
    /// roots, so a machine without Codex contributes nothing.
    fn watch_paths(&self) -> Vec<PathBuf> {
        vec![self.root.clone()]
    }
}

/// Concatenate every `.text` field across content blocks (`_message_text`).
fn message_text(content: Option<&Value>) -> String {
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
            // Note the asymmetry with the Claude adapter: here a *missing* or
            // non-string `text` contributes nothing at all, so it costs no
            // newline in the join.
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

/// Give the turn's last assistant record the turn's token usage
/// (`_attach_tokens_to_last_assistant`).
fn attach_tokens_to_last_assistant(buffer: &mut [Record], last_usage: &Map<String, Value>) {
    let Some(index) = last_assistant_index(buffer) else {
        return;
    };
    let canonical = canonicalize_openai_usage(last_usage);
    let target = &mut buffer[index];
    target.input_tokens = canonical.input;
    target.output_tokens = canonical.output;
    target.cache_create_tokens = canonical.cache_creation;
    target.cache_read_tokens = canonical.cache_read;
}

/// The canonical four token slots.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CanonicalTokens {
    /// Fresh (uncached) input.
    pub input: i64,
    /// Billable output, reasoning folded in.
    pub output: i64,
    /// Always 0 for OpenAI — cache writes are not billed.
    pub cache_creation: i64,
    /// Cached input.
    pub cache_read: i64,
}

/// Flatten OpenAI's raw token shape into the canonical four
/// (`OpenAIPricer.normalize_tokens`).
///
/// OpenAI embeds cached-input tokens *inside* `input_tokens` and bills reasoning
/// under output, so: subtract `cached_input_tokens` from input (making it match
/// Anthropic's "fresh input" meaning), fold `reasoning_output_tokens` into
/// output, map cached → `cache_read`, and leave `cache_creation` at 0.
///
/// Already-canonical keys pass through unchanged — that dual-shape tolerance is
/// what let the adapter migrate to this seam without a flag day, and the
/// cost-equivalence test
/// (`tests/.../infra/providers/test_codex_cost_equivalence.py`) pins it.
#[must_use]
pub fn canonicalize_openai_usage(raw: &Map<String, Value>) -> CanonicalTokens {
    if raw.contains_key("input_tokens") || raw.contains_key("cached_input_tokens") {
        let raw_input = pyval::safe_int(raw.get("input_tokens"));
        let cached = pyval::safe_int(raw.get("cached_input_tokens"));
        let raw_output = pyval::safe_int(raw.get("output_tokens"));
        let reasoning = pyval::safe_int(raw.get("reasoning_output_tokens"));
        return CanonicalTokens {
            input: (raw_input - cached).max(0),
            output: raw_output.saturating_add(reasoning),
            cache_creation: 0,
            cache_read: cached,
        };
    }
    CanonicalTokens {
        input: pyval::safe_int(raw.get("input")),
        output: pyval::safe_int(raw.get("output")),
        cache_creation: pyval::safe_int(raw.get("cache_creation")),
        cache_read: pyval::safe_int(raw.get("cache_read")),
    }
}

/// The record the turn's tokens belong to (`_last_assistant_index`).
///
/// Prefers the assistant *text* turn over a bare `function_call` record; falls
/// back to any assistant record for tool-only turns.
fn last_assistant_index(buffer: &[Record]) -> Option<usize> {
    buffer
        .iter()
        .rposition(|record| record.role == "assistant" && record.tools.is_empty())
        .or_else(|| buffer.iter().rposition(|record| record.role == "assistant"))
}

/// `sorted(root.glob("*/*/*/rollout-*.jsonl"))` — exactly three directory
/// levels, then a `rollout-*.jsonl` name, sorted by full path.
fn glob_rollouts(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for year in child_dirs(root) {
        for month in child_dirs(&year) {
            for day in child_dirs(&month) {
                for entry in read_dir_sorted(&day) {
                    let name = entry
                        .file_name()
                        .map(|name| name.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    if name.starts_with("rollout-") && name.ends_with(".jsonl") {
                        out.push(entry);
                    }
                }
            }
        }
    }
    out.sort();
    out
}

fn child_dirs(root: &Path) -> Vec<PathBuf> {
    read_dir_sorted(root)
        .into_iter()
        .filter(|path| path.is_dir())
        .collect()
}

fn read_dir_sorted(root: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut paths: Vec<PathBuf> = entries.flatten().map(|entry| entry.path()).collect();
    paths.sort();
    paths
}

fn read_first_line(path: &Path) -> Option<Vec<u8>> {
    use std::io::BufRead;
    let file = std::fs::File::open(path).ok()?;
    let mut reader = std::io::BufReader::new(file);
    let mut line = Vec::new();
    reader.read_until(b'\n', &mut line).ok()?;
    Some(line)
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

fn current_dir_string() -> String {
    std::env::current_dir().map_or_else(
        |_| "/".to_string(),
        |path| path.to_string_lossy().into_owned(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn usage(pairs: Value) -> Map<String, Value> {
        pairs.as_object().expect("object").clone()
    }

    #[test]
    fn provider_shape_flattens_the_way_the_pricer_does() {
        // The mock-data fixture's first token_count event.
        let canonical = canonicalize_openai_usage(&usage(json!({
            "input_tokens": 1200,
            "cached_input_tokens": 200,
            "output_tokens": 350,
            "reasoning_output_tokens": 150,
        })));
        assert_eq!(
            canonical,
            CanonicalTokens {
                input: 1000,
                output: 500,
                cache_creation: 0,
                cache_read: 200,
            }
        );
    }

    #[test]
    fn canonical_shape_passes_through() {
        let canonical = canonicalize_openai_usage(&usage(json!({
            "input": 7, "output": 9, "cache_creation": 1, "cache_read": 2,
        })));
        assert_eq!(
            canonical,
            CanonicalTokens {
                input: 7,
                output: 9,
                cache_creation: 1,
                cache_read: 2,
            }
        );
    }

    #[test]
    fn garbage_usage_values_degrade_to_zero() {
        let canonical = canonicalize_openai_usage(&usage(json!({
            "input_tokens": "garbage",
            "cached_input_tokens": [1],
            "output_tokens": null,
        })));
        assert_eq!(
            canonical,
            CanonicalTokens {
                input: 0,
                output: 0,
                cache_creation: 0,
                cache_read: 0,
            }
        );
    }

    #[test]
    fn cached_never_pushes_input_negative() {
        let canonical = canonicalize_openai_usage(&usage(json!({
            "input_tokens": 10, "cached_input_tokens": 99,
        })));
        assert_eq!(canonical.input, 0);
        assert_eq!(canonical.cache_read, 99);
    }

    #[test]
    fn tool_labels_map_and_unknown_names_pass_through() {
        let mapped: Vec<&str> = TOOL_NAME_MAP
            .iter()
            .filter(|(from, _)| *from == "exec_command")
            .map(|(_, to)| *to)
            .collect();
        assert_eq!(mapped, vec!["Bash"]);
        assert!(!TOOL_NAME_MAP.iter().any(|(from, _)| *from == "my_new_tool"));
    }

    #[test]
    fn message_text_concatenates_text_blocks() {
        assert_eq!(message_text(Some(&json!("plain"))), "plain");
        assert_eq!(
            message_text(Some(&json!([{"type": "text", "text": "a"}, {"text": "b"}]))),
            "a\nb"
        );
        assert_eq!(message_text(Some(&json!([{"type": "text"}]))), "");
        assert_eq!(message_text(None), "");
        assert_eq!(message_text(Some(&json!(42))), "");
    }

    #[test]
    fn codex_framing_mislabelled_user_is_skipped_and_humans_are_kept() {
        // Measured from real rollouts: the boot burst writes these as
        // role:"user" before the human ever types.
        for text in [
            "<codex_internal_context source=\"goal\"> Continue working toward the active thread goal.",
            "<environment_context>\n  <cwd>/x</cwd>",
            "<user_instructions>\nbe terse\n</user_instructions>",
            "<turn_context>\nmodel: gpt-5.6\n</turn_context>",
            "# AGENTS.md instructions for /Users/x/proj\n\n<contents…>",
        ] {
            assert!(is_internal_user_text(text), "{text:?} must be framing");
        }
        // The human's own prompt survives, even one that talks about the
        // framing or starts with markdown.
        for text in [
            "can you please tell me what we can borrow as inspiration",
            "## plan for today",
            "why does <environment_context> appear in my titles?",
            "# AGENTS review — read the file and summarise",
        ] {
            assert!(!is_internal_user_text(text), "{text:?} must be kept");
        }
    }
}
