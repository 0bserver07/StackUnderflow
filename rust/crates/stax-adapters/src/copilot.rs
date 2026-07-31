//! GitHub Copilot — the port of `stackunderflow/adapters/copilot.py`.
//!
//! Two on-disk formats, one `read()`, because they share a per-line event shape:
//!
//! 1. **Legacy CLI** — `~/.copilot/session-state/{sessionId}/events.jsonl`, with
//!    an optional `workspace.{json,yaml,yml}` beside it carrying the project cwd.
//! 2. **VS Code transcript** —
//!    `…/workspaceStorage/{hash}/GitHub.copilot-chat/transcripts/*.jsonl`, whose
//!    first line is a `session.start` header and whose parent hash is the only
//!    project signal available.
//!
//! Events are `session.model_change`, `session.start`, `user.message` and
//! `assistant.message`. Only the last becomes a record, and only when its output
//! token count is positive — an empty assistant turn is filtered out entirely.
//!
//! ## Tokens are frequently estimated
//!
//! An explicit `outputTokens` / `inputTokens` wins. Otherwise output is
//! `len(text) // 4` and input is `len(last user message) // 4`, and the record
//! carries `raw["cost_source"] = "estimated"`. `len` is Python's — code points.
//!
//! ## Model resolution order, and the drift it was written to stop
//!
//! `event.model` → the rolling `session.model_change` value → a tool-call-id
//! prefix (`toolu…` is Anthropic-family, `call_…` is OpenAI) → `copilot-auto`.
//!
//! The heuristic sits **below** the rolling model on purpose. An earlier
//! ordering put it above, which downgraded a fully-qualified
//! `claude-sonnet-4-5-20250929` to the family-only `claude-auto` on any turn
//! that happened to call a tool — losing model granularity in the marts. That
//! regression is the reason this order is spelled out in a comment in both
//! implementations.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::base::{Record, SessionRef, SourceAdapter, Speed, stat_ref_fields};
use crate::jsonl::{self, JsonlLines, py_bytes_strip};
use crate::pytime::{self, Clock};
use crate::pyval;
use crate::walk;

/// The provider key.
pub const NAME: &str = "copilot";

/// The model stamped when neither the event, the session, nor a tool-call id
/// says anything.
pub const DEFAULT_MODEL: &str = "copilot-auto";

/// The sub-path inside each `workspaceStorage/{hash}/` directory
/// (`_COPILOT_CHAT_SUBDIR`).
pub const CHAT_SUBDIR: [&str; 2] = ["GitHub.copilot-chat", "transcripts"];

/// The workspace side-car file names, in probe order.
pub const WORKSPACE_FILES: [&str; 3] = ["workspace.json", "workspace.yaml", "workspace.yml"];

/// VS Code's `workspaceStorage` root, with the platform and environment
/// injected (`_default_vscode_workspace_storage`).
///
/// `os` is `std::env::consts::OS`. Python branches on `sys.platform` with
/// darwin / linux / win arms and a darwin default; the mapping is one-to-one.
#[must_use]
pub fn resolve_vscode_workspace_storage(
    os: &str,
    appdata: Option<&OsStr>,
    home: Option<&Path>,
) -> PathBuf {
    let tail = |base: PathBuf| base.join("Code").join("User").join("workspaceStorage");
    if os == "windows" {
        return tail(PathBuf::from(appdata.unwrap_or_else(|| OsStr::new(""))));
    }
    let home = home.map_or_else(PathBuf::new, Path::to_path_buf);
    if os == "linux" {
        return tail(home.join(".config"));
    }
    tail(home.join("Library").join("Application Support"))
}

/// The Copilot source adapter (`CopilotAdapter`).
#[derive(Debug, Clone)]
pub struct CopilotAdapter {
    legacy_root: PathBuf,
    vscode_root: PathBuf,
    clock: Clock,
}

impl Default for CopilotAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl CopilotAdapter {
    /// Both roots, from the live environment.
    #[must_use]
    pub fn new() -> Self {
        #[allow(
            deprecated,
            reason = "std::env::home_dir is the platform-correct answer on the \
            1.97.1 pin; the claude and codex adapters carry the same allow"
        )]
        let home = std::env::home_dir();
        let legacy_root = home
            .clone()
            .unwrap_or_default()
            .join(".copilot")
            .join("session-state");
        Self {
            legacy_root,
            vscode_root: resolve_vscode_workspace_storage(
                std::env::consts::OS,
                std::env::var_os("APPDATA").as_deref(),
                home.as_deref(),
            ),
            clock: Clock::Live,
        }
    }

    /// Inject both roots — the two keyword arguments Python already has.
    #[must_use]
    pub fn with_roots(
        legacy_root: impl Into<PathBuf>,
        vscode_workspace_storage: impl Into<PathBuf>,
    ) -> Self {
        Self {
            legacy_root: legacy_root.into(),
            vscode_root: vscode_workspace_storage.into(),
            clock: Clock::Live,
        }
    }

    /// Pin the clock behind the `datetime.now(tz=UTC)` timestamp fallback.
    #[must_use]
    pub fn with_clock(mut self, clock: Clock) -> Self {
        self.clock = clock;
        self
    }

    /// The legacy CLI root.
    #[must_use]
    pub fn legacy_root(&self) -> &Path {
        &self.legacy_root
    }

    /// The VS Code `workspaceStorage` root.
    #[must_use]
    pub fn vscode_root(&self) -> &Path {
        &self.vscode_root
    }

    /// `~/.copilot/session-state/{sessionId}/events.jsonl` (`_enumerate_legacy`).
    fn enumerate_legacy(&self, out: &mut Vec<SessionRef>) {
        if !self.legacy_root.is_dir() {
            return;
        }
        for session_dir in walk::child_dirs(&self.legacy_root) {
            let events = session_dir.join("events.jsonl");
            if !events.is_file() {
                continue;
            }
            let Some((mtime, size)) = stat_ref_fields(&events) else {
                continue;
            };
            let mut hint = Map::new();
            hint.insert("format".to_string(), Value::from("legacy"));
            out.push(SessionRef {
                provider: NAME.to_string(),
                project_slug: legacy_project_slug(&session_dir),
                session_id: walk::dir_name(&session_dir),
                file_path: events,
                file_mtime: mtime,
                file_size: size,
                source_kind: crate::base::SourceKind::File,
                source_hint: Some(hint),
            });
        }
    }

    /// `workspaceStorage/{hash}/GitHub.copilot-chat/transcripts/*.jsonl`
    /// (`_enumerate_vscode_transcripts`).
    fn enumerate_vscode(&self, out: &mut Vec<SessionRef>) {
        if !self.vscode_root.is_dir() {
            return;
        }
        for workspace_dir in walk::child_dirs(&self.vscode_root) {
            let mut transcripts = workspace_dir.clone();
            for part in CHAT_SUBDIR {
                transcripts = transcripts.join(part);
            }
            if !transcripts.is_dir() {
                continue;
            }
            let workspace_hash = walk::dir_name(&workspace_dir);
            for path in walk::glob_suffix(&transcripts, ".jsonl") {
                let Some((mtime, size)) = stat_ref_fields(&path) else {
                    continue;
                };
                let mut hint = Map::new();
                hint.insert("format".to_string(), Value::from("vscode-transcript"));
                hint.insert(
                    "workspace_hash".to_string(),
                    Value::from(workspace_hash.clone()),
                );
                out.push(SessionRef {
                    provider: NAME.to_string(),
                    // The `or "copilot"` in the Python source is dead — an
                    // f-string with a literal prefix is never falsy — so the
                    // slug is always the prefixed hash. Ported as written.
                    project_slug: format!("copilot-vscode/{workspace_hash}"),
                    session_id: walk::file_stem(&path),
                    file_path: path,
                    file_mtime: mtime,
                    file_size: size,
                    source_kind: crate::base::SourceKind::File,
                    source_hint: Some(hint),
                });
            }
        }
    }
}

impl SourceAdapter for CopilotAdapter {
    fn name(&self) -> &str {
        NAME
    }

    fn enumerate(&self) -> Vec<SessionRef> {
        let mut out = Vec::new();
        self.enumerate_legacy(&mut out);
        self.enumerate_vscode(&mut out);
        out
    }

    fn read_into(&self, session: &SessionRef, since_offset: i64, sink: &mut dyn FnMut(Record)) {
        let path = &session.file_path;
        // LOG: python warns "Copilot session file missing at read time: %s".
        if !path.is_file() {
            return;
        }
        // The explicit cap check sits next to the is_file guard in the Python
        // original even though the reader re-stats; kept for the same reason.
        if jsonl::stat_or_skip(path).is_none() {
            return;
        }

        let mut current_model: Option<String> = None;
        let mut last_user_text = String::new();

        for (line_offset, raw_line) in JsonlLines::open(path, since_offset) {
            if since_offset > 0 && line_offset <= since_offset {
                continue;
            }
            let stripped = py_bytes_strip(&raw_line);
            if stripped.is_empty() {
                continue;
            }
            // LOG: python warns "Malformed JSON line in %s".
            let Some(event) = jsonl::parse_json(stripped) else {
                continue;
            };
            let Some(map) = event.as_object() else {
                continue;
            };

            match map.get("type").and_then(Value::as_str) {
                // Both header events update the rolling model and emit nothing.
                Some("session.model_change" | "session.start") => {
                    if let Some(candidate) = extract_model(map) {
                        current_model = Some(candidate);
                    }
                    continue;
                }
                Some("user.message") => {
                    let text = extract_text(map);
                    if !text.is_empty() {
                        last_user_text = text;
                    }
                    continue;
                }
                Some("assistant.message") => {}
                _ => continue,
            }

            let text = extract_text(map);
            let (out_tokens, out_estimated) = output_tokens_for(map, &text);
            let (in_tokens, in_estimated) = input_tokens_for(map, &last_user_text);
            // A purely empty assistant turn is filtered out: no explicit count
            // and nothing to estimate from.
            if out_tokens <= 0 {
                continue;
            }

            let tool_calls = tool_calls_field(map);
            let model = extract_model(map)
                .or_else(|| current_model.clone())
                .or_else(|| infer_model_from_tool_calls(tool_calls))
                .unwrap_or_else(|| DEFAULT_MODEL.to_string());
            // Bind the inference into rolling state so later turns without a
            // model field stay coherent.
            current_model = Some(model.clone());

            let mut raw_payload = map.clone();
            if out_estimated || in_estimated {
                raw_payload.insert("cost_source".to_string(), Value::from("estimated"));
            }

            sink(Record {
                provider: NAME.to_string(),
                session_id: session.session_id.clone(),
                seq: line_offset,
                timestamp: extract_timestamp(map, self.clock),
                role: "assistant".to_string(),
                model: Some(model),
                input_tokens: in_tokens,
                output_tokens: out_tokens,
                cache_create_tokens: 0,
                cache_read_tokens: 0,
                content_text: text,
                tools: extract_tool_names(map),
                cwd: None,
                is_sidechain: false,
                uuid: format!("{}:{line_offset}", session.session_id),
                parent_uuid: None,
                raw: Value::Object(raw_payload),
                speed: Speed::Standard,
            });
        }
    }

    /// The legacy root only (`source_roots`).
    ///
    /// VS Code's `workspaceStorage` is deliberately excluded: that tree mixes
    /// gigabytes of unrelated workspace state with the chat files, and rsyncing
    /// it whole would bloat every backup snapshot.
    fn source_roots(&self) -> Vec<PathBuf> {
        vec![self.legacy_root.clone()]
    }
}

/// `"claude-auto"` / `"gpt-auto"` from the first recognisable tool-call id
/// (`_infer_model_from_tool_calls`).
///
/// `toolu_…` and `toolu_bdrk_…` are Anthropic (bare and Bedrock); `call_…` is
/// OpenAI. The names are vendor-prefixed so `CopilotPricer.canonicalize` can
/// route on its `claude-` / `gpt-` heuristic. An unrecognised id does not stop
/// the scan — the next tool call gets a turn.
#[must_use]
pub fn infer_model_from_tool_calls(tool_calls: Option<&Value>) -> Option<String> {
    let items = tool_calls?.as_array()?;
    for call in items {
        let Some(map) = call.as_object() else {
            continue;
        };
        let id = map
            .get("id")
            .filter(|value| pyval::py_truthy(value))
            .or_else(|| map.get("toolCallId"));
        let Some(id) = id.and_then(Value::as_str).filter(|id| !id.is_empty()) else {
            continue;
        };
        let lowered = id.to_lowercase();
        // `^toolu(?:_bdrk)?_` — the optional group can always match empty and
        // let the trailing underscore do the work, so the regex and this prefix
        // accept exactly the same ids.
        if lowered.starts_with("toolu_") {
            return Some("claude-auto".to_string());
        }
        if lowered.starts_with("call_") {
            return Some("gpt-auto".to_string());
        }
    }
    None
}

/// The event's `toolCalls`, or the `data` envelope's (`read`, inline).
fn tool_calls_field(event: &Map<String, Value>) -> Option<&Value> {
    match event.get("toolCalls") {
        Some(value) if value.is_array() => Some(value),
        _ => event
            .get("data")
            .and_then(Value::as_object)
            .and_then(|data| data.get("toolCalls")),
    }
}

/// Message text from either a flat `content` or a `data` envelope
/// (`_extract_text`).
fn extract_text(event: &Map<String, Value>) -> String {
    // Newer transcripts wrap the payload in `data`.
    if let Some(data) = event.get("data").and_then(Value::as_object) {
        let candidate = or_key(data, "content", "text");
        if let Some(text) = candidate.and_then(Value::as_str) {
            return text.to_string();
        }
        if let Some(items) = candidate.and_then(Value::as_array) {
            return flatten_content_blocks(items);
        }
    }
    // Legacy / flat shape.
    let candidate = event
        .get("content")
        .filter(|value| pyval::py_truthy(value))
        .or_else(|| event.get("text").filter(|value| pyval::py_truthy(value)))
        .or_else(|| event.get("message"));
    let Some(candidate) = candidate else {
        return String::new();
    };
    if let Some(text) = candidate.as_str() {
        return text.to_string();
    }
    if let Some(items) = candidate.as_array() {
        return flatten_content_blocks(items);
    }
    if let Some(map) = candidate.as_object()
        && let Some(nested) = or_key(map, "content", "text").and_then(Value::as_str)
    {
        return nested.to_string();
    }
    String::new()
}

/// `mapping.get(first) or mapping.get(second)` — Python's truthiness chain.
fn or_key<'a>(map: &'a Map<String, Value>, first: &str, second: &str) -> Option<&'a Value> {
    map.get(first)
        .filter(|value| pyval::py_truthy(value))
        .or_else(|| map.get(second))
}

/// Join the text of a list of content blocks (`_flatten_content_blocks`).
fn flatten_content_blocks(items: &[Value]) -> String {
    let mut pieces: Vec<&str> = Vec::new();
    for block in items {
        if let Some(map) = block.as_object() {
            if let Some(text) = or_key(map, "text", "content").and_then(Value::as_str)
                && !text.is_empty()
            {
                pieces.push(text);
            }
        } else if let Some(text) = block.as_str() {
            pieces.push(text);
        }
    }
    pieces.join("\n")
}

/// The explicit model id on this event, flat or enveloped (`_extract_model`).
fn extract_model(event: &Map<String, Value>) -> Option<String> {
    const KEYS: [&str; 3] = ["model", "modelName", "modelId"];
    for key in KEYS {
        if let Some(value) = event.get(key).and_then(Value::as_str)
            && !value.is_empty()
        {
            return Some(value.to_string());
        }
    }
    let data = event.get("data").and_then(Value::as_object)?;
    for key in KEYS {
        if let Some(value) = data.get(key).and_then(Value::as_str)
            && !value.is_empty()
        {
            return Some(value.to_string());
        }
    }
    None
}

/// `(output_tokens, estimated)` (`_output_tokens_for`).
fn output_tokens_for(event: &Map<String, Value>, text: &str) -> (i64, bool) {
    tokens_for(event, "outputTokens", text)
}

/// `(input_tokens, estimated)` (`_input_tokens_for`).
fn input_tokens_for(event: &Map<String, Value>, last_user_text: &str) -> (i64, bool) {
    tokens_for(event, "inputTokens", last_user_text)
}

/// The shared body of the two token resolvers: an explicit positive count from
/// the event, then from its `data` envelope, then a `len // 4` estimate.
fn tokens_for(event: &Map<String, Value>, key: &str, text: &str) -> (i64, bool) {
    let explicit = pyval::safe_int(event.get(key));
    if explicit > 0 {
        return (explicit, false);
    }
    if let Some(data) = event.get("data").and_then(Value::as_object) {
        let explicit = pyval::safe_int(data.get(key));
        if explicit > 0 {
            return (explicit, false);
        }
    }
    // Python's `len` counts code points.
    (
        i64::try_from(text.chars().count() / 4).unwrap_or(i64::MAX),
        true,
    )
}

/// Tool names off the assistant event (`_extract_tool_names`).
fn extract_tool_names(event: &Map<String, Value>) -> Vec<String> {
    let Some(items) = tool_calls_field(event).and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut names = Vec::new();
    for call in items {
        let Some(map) = call.as_object() else {
            continue;
        };
        if let Some(name) = or_key(map, "name", "toolName").and_then(Value::as_str)
            && !name.is_empty()
        {
            names.push(name.to_string());
        }
    }
    names
}

/// An ISO 8601 UTC timestamp, falling back to *now* (`_extract_timestamp`).
fn extract_timestamp(event: &Map<String, Value>, clock: Clock) -> String {
    const KEYS: [&str; 3] = ["timestamp", "ts", "createdAt"];
    for key in KEYS {
        if let Some(iso) = coerce_iso(event.get(key)) {
            return iso;
        }
    }
    if let Some(data) = event.get("data").and_then(Value::as_object) {
        for key in KEYS {
            if let Some(iso) = coerce_iso(data.get(key)) {
                return iso;
            }
        }
    }
    // Last resort — the read time. Adapters are expected to emit parseable ISO
    // strings, and the contract test enforces it.
    clock.now_iso()
}

/// One candidate value → ISO 8601, or `None` (`_coerce_iso`).
///
/// Numbers above 10^12 are read as epoch **milliseconds**, everything else as
/// epoch seconds. `false` and `0` take the numeric path (Python's `bool` is an
/// `int`, and `0 == ""` is false), so both render as the epoch rather than as
/// "no timestamp".
fn coerce_iso(value: Option<&Value>) -> Option<String> {
    let value = value?;
    match value {
        Value::Null => None,
        Value::String(text) if text.is_empty() => None,
        Value::String(text) => {
            let trimmed = text.trim();
            if trimmed.is_empty() {
                return None;
            }
            pytime::isoformat_roundtrip(trimmed)
        }
        Value::Bool(flag) => pytime::from_timestamp_iso(if *flag { 1.0 } else { 0.0 }),
        Value::Number(number) => {
            let raw = number.as_f64()?;
            if raw > 1e12 {
                pytime::from_timestamp_iso(raw / 1000.0)
            } else {
                pytime::from_timestamp_iso(raw)
            }
        }
        Value::Array(_) | Value::Object(_) => None,
    }
}

/// The legacy session's project slug, from its workspace side-car
/// (`_legacy_project_slug`).
///
/// YAML support is deliberately rudimentary — a top-level `cwd:` line and
/// nothing else, so one field does not cost a YAML dependency.
#[must_use]
pub fn legacy_project_slug(session_dir: &Path) -> String {
    for name in WORKSPACE_FILES {
        let candidate = session_dir.join(name);
        if !candidate.is_file() {
            continue;
        }
        // `read_text(errors="replace")` — undecodable bytes become U+FFFD
        // rather than an error.
        let Ok(bytes) = std::fs::read(&candidate) else {
            continue;
        };
        let text = String::from_utf8_lossy(&bytes);
        if name.ends_with(".json") {
            let Some(obj) = jsonl::parse_json(text.as_bytes()) else {
                continue;
            };
            if let Some(cwd) = obj
                .as_object()
                .and_then(|map| map.get("cwd"))
                .and_then(Value::as_str)
                && !cwd.is_empty()
            {
                return slugify_cwd(cwd);
            }
        } else {
            for line in split_lines(&text) {
                let stripped = line.trim();
                if let Some(rest) = stripped.strip_prefix("cwd:") {
                    let cwd = rest.trim().trim_matches(['"', '\'']);
                    if !cwd.is_empty() {
                        return slugify_cwd(cwd);
                    }
                }
            }
        }
    }
    // No workspace file — every legacy session lives under one logical project.
    NAME.to_string()
}

/// `str.splitlines()` over the separators a workspace file can realistically
/// carry.
///
/// **DIVERGENCE (recorded, unreachable in practice).** Python's *str*
/// `splitlines` also breaks on `\v`, `\f`, `\x1c`–`\x1e`, `\x85`, ` ` and
/// ` `; this splits on `\n`, `\r\n` and `\r`. A `cwd:` line separated from
/// the rest of the file by a form feed would be found there and not here.
fn split_lines(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let bytes = text.as_bytes();
    let (mut start, mut index) = (0_usize, 0_usize);
    while index < bytes.len() {
        match bytes[index] {
            b'\n' => {
                out.push(&text[start..index]);
                index += 1;
                start = index;
            }
            b'\r' => {
                out.push(&text[start..index]);
                index += if bytes.get(index + 1) == Some(&b'\n') {
                    2
                } else {
                    1
                };
                start = index;
            }
            _ => index += 1,
        }
    }
    if start < bytes.len() {
        out.push(&text[start..]);
    }
    out
}

/// `cwd.replace("/", "-").strip("-") or "copilot"` (`_slugify_cwd`).
///
/// Note what this is *not*: the claude-family slug. There is no `abspath`, no
/// `_` rewrite, and the leading dash is stripped — `/Users/me/app` becomes
/// `Users-me-app`, not `-Users-me-app`.
#[must_use]
pub fn slugify_cwd(cwd: &str) -> String {
    let slug = cwd.replace('/', "-");
    let trimmed = slug.trim_matches('-');
    if trimmed.is_empty() {
        NAME.to_string()
    } else {
        trimmed.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn event(value: Value) -> Map<String, Value> {
        value.as_object().expect("object").clone()
    }

    #[test]
    fn tool_call_ids_leak_the_upstream_vendor() {
        let anthropic = json!([{"id": "toolu_bdrk_01abc"}]);
        assert_eq!(
            infer_model_from_tool_calls(Some(&anthropic)).as_deref(),
            Some("claude-auto")
        );
        let bare = json!([{"id": "TOOLU_01abc"}]);
        assert_eq!(
            infer_model_from_tool_calls(Some(&bare)).as_deref(),
            Some("claude-auto"),
            "the prefix match is case-insensitive"
        );
        let openai = json!([{"toolCallId": "call_xyz"}]);
        assert_eq!(
            infer_model_from_tool_calls(Some(&openai)).as_deref(),
            Some("gpt-auto")
        );
        // An unrecognised id does not end the scan.
        let mixed = json!([{"id": "weird_1"}, "not a dict", {"id": "call_2"}]);
        assert_eq!(
            infer_model_from_tool_calls(Some(&mixed)).as_deref(),
            Some("gpt-auto")
        );
        assert_eq!(infer_model_from_tool_calls(Some(&json!([]))), None);
        assert_eq!(infer_model_from_tool_calls(Some(&json!("x"))), None);
        assert_eq!(infer_model_from_tool_calls(None), None);
    }

    #[test]
    fn text_comes_from_the_data_envelope_or_the_flat_shape() {
        assert_eq!(extract_text(&event(json!({"content": "flat"}))), "flat");
        assert_eq!(
            extract_text(&event(json!({"data": {"content": "wrapped"}}))),
            "wrapped"
        );
        // An empty `content` falls through the `or` chain to `text`.
        assert_eq!(
            extract_text(&event(json!({"content": "", "text": "second"}))),
            "second"
        );
        assert_eq!(
            extract_text(&event(
                json!({"content": [{"text": "a"}, "b", {"content": "c"}]})
            )),
            "a\nb\nc"
        );
        // A dict candidate exposes one level of nesting.
        assert_eq!(
            extract_text(&event(json!({"message": {"content": "deep"}}))),
            "deep"
        );
        // A `data` envelope whose candidate is neither str nor list falls back
        // to the flat shape rather than returning empty.
        assert_eq!(
            extract_text(&event(json!({"data": {"content": 7}, "content": "flat"}))),
            "flat"
        );
        assert_eq!(extract_text(&event(json!({}))), "");
    }

    #[test]
    fn explicit_counts_win_and_estimates_are_flagged() {
        assert_eq!(
            output_tokens_for(&event(json!({"outputTokens": 80})), "ignored"),
            (80, false)
        );
        assert_eq!(
            output_tokens_for(&event(json!({"data": {"outputTokens": 12}})), "ignored"),
            (12, false)
        );
        // Zero is not "explicit" — it falls through to the estimate.
        assert_eq!(
            output_tokens_for(&event(json!({"outputTokens": 0})), "abcdefgh"),
            (2, true)
        );
        assert_eq!(output_tokens_for(&event(json!({})), ""), (0, true));
        assert_eq!(input_tokens_for(&event(json!({})), "abcdefgh"), (2, true));
    }

    #[test]
    fn timestamps_coerce_from_every_shape_the_events_carry() {
        let clock = Clock::Fixed(std::time::UNIX_EPOCH);
        assert_eq!(
            extract_timestamp(&event(json!({"timestamp": "2026-04-25T14:00:00Z"})), clock),
            "2026-04-25T14:00:00+00:00"
        );
        // ms-epoch above the 10^12 threshold, seconds below it.
        assert_eq!(
            extract_timestamp(&event(json!({"ts": 1_745_596_801_000_i64})), clock),
            "2025-04-25T16:00:01+00:00"
        );
        assert_eq!(
            extract_timestamp(&event(json!({"createdAt": 1_745_596_801_i64})), clock),
            "2025-04-25T16:00:01+00:00"
        );
        // The `data` envelope is searched after the flat keys.
        assert_eq!(
            extract_timestamp(&event(json!({"data": {"timestamp": "2026-01-02"}})), clock),
            "2026-01-02T00:00:00+00:00"
        );
        // Nothing parseable — the injected clock, not a panic.
        assert_eq!(
            extract_timestamp(&event(json!({"timestamp": "banana"})), clock),
            "1970-01-01T00:00:00+00:00"
        );
    }

    #[test]
    fn the_legacy_slug_is_not_the_claude_slug() {
        assert_eq!(slugify_cwd("/Users/me/app"), "Users-me-app");
        assert_eq!(slugify_cwd("/"), NAME);
        assert_eq!(slugify_cwd(""), NAME);
        // No underscore rewrite, unlike every claude-family slug.
        assert_eq!(slugify_cwd("/a/my_app"), "a-my_app");
        assert_eq!(split_lines("a\r\nb\rc\nd"), vec!["a", "b", "c", "d"]);
    }

    #[test]
    fn both_roots_absent_enumerates_empty_rather_than_failing() {
        let adapter =
            CopilotAdapter::with_roots("/nonexistent/stax/copilot", "/nonexistent/stax/vscode");
        assert!(adapter.enumerate().is_empty());
        // Only the legacy root is backed up.
        assert_eq!(adapter.source_roots().len(), 1);
        assert!(adapter.watch_paths().is_empty());
        assert_eq!(legacy_project_slug(Path::new("/nonexistent/stax/x")), NAME);
    }

    #[test]
    fn platform_layouts_resolve_without_running_on_them() {
        let home = Path::new("/home/me");
        assert_eq!(
            resolve_vscode_workspace_storage("linux", None, Some(home)),
            Path::new("/home/me/.config/Code/User/workspaceStorage")
        );
        assert_eq!(
            resolve_vscode_workspace_storage("macos", None, Some(home)),
            Path::new("/home/me/Library/Application Support/Code/User/workspaceStorage")
        );
        assert_eq!(
            resolve_vscode_workspace_storage("windows", Some(OsStr::new("/appdata")), None),
            Path::new("/appdata/Code/User/workspaceStorage")
        );
    }
}
