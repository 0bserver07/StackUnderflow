//! The Cline family — the port of `stackunderflow/adapters/cline.py`.
//!
//! **One module, three providers.** Cline, KiloCode and Roo Code are the same
//! VS Code extension surface under three extension ids, and Python models that
//! as three subclasses overriding three class attributes. Rust needs no
//! subclassing for that: [`ClineFamilyAdapter`] is one type parameterised by a
//! [`Variant`], and the registry hands out three instances of it. Adding a
//! fourth fork of Cline is one enum arm.
//!
//! Each task is a directory under
//! `<globalStorage>/<extension id>/tasks/<taskId>/` holding two JSON files:
//!
//! * `ui_messages.json` — a flat array of UI events. A `say == "api_req_started"`
//!   event is one assistant turn, and its `text` is a JSON-*stringified* object
//!   carrying `{tokensIn, tokensOut, cacheWrites, cacheReads, cost}`.
//! * `api_conversation_history.json` — flat `{role, content}` Anthropic-shape
//!   messages whose first user message embeds `<model>…</model>`.
//!
//! ## Two things this adapter does that look wrong and are not
//!
//! 1. **`seq` is the event index, not a byte offset.** `source_kind` is
//!    [`crate::base::SourceKind::File`], but resuming means "skip events at or
//!    before index N". The storage-aware contract holds because it only asserts
//!    monotonic `seq` and strictly-fewer records past a midpoint — see the same
//!    hybrid note on [`crate::gemini`].
//! 2. **`content_text` is the *user's* text, on an assistant record.**
//!    `ui_messages.json` is the user-facing source of truth and is always
//!    present; `api_conversation_history.json` can be truncated when a task was
//!    interrupted. The preceding user message is therefore what a turn carries,
//!    and the resume path keeps updating it while skipping so a post-resume turn
//!    still gets the right text.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::base::{
    Record, SessionRef, SourceAdapter, Speed, child_dirs, file_name, home_dir, stat_ref_fields,
};
use crate::jsonl;
use crate::pyval;

/// Model recorded when no `<model>` tag is present (`_DEFAULT_MODEL`).
pub const DEFAULT_MODEL: &str = "cline-auto";

/// The two files a task directory holds.
const UI_MESSAGES: &str = "ui_messages.json";
const API_CONVERSATION_HISTORY: &str = "api_conversation_history.json";

/// The four token keys on an `api_req_started` event (`_parse_api_req_text`).
const TOKEN_KEYS: [&str; 4] = ["tokensIn", "tokensOut", "cacheWrites", "cacheReads"];

/// Which host OS's VS Code layout to resolve (`sys.platform` branches).
///
/// A parameter rather than a compile-time `cfg!` at the call site so all three
/// layouts are testable from one host — the same reason
/// `tests/stackunderflow/adapters/test_platform_paths.py` monkeypatches
/// `sys.platform`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    /// `sys.platform.startswith("win")` — `%APPDATA%\Code\User\globalStorage`.
    Windows,
    /// `sys.platform.startswith("linux")` — `~/.config/Code/User/globalStorage`.
    Linux,
    /// macOS, and the fallback for every other POSIX host, exactly as Python's
    /// final `return` is.
    MacOs,
}

impl Platform {
    /// The host this binary was built for.
    #[must_use]
    pub fn current() -> Self {
        if cfg!(windows) {
            Self::Windows
        } else if cfg!(target_os = "linux") {
            Self::Linux
        } else {
            Self::MacOs
        }
    }
}

/// VS Code's `globalStorage` root (`_vscode_global_storage`).
///
/// `appdata` is `%APPDATA%`, read at call time on Windows so a test can supply
/// it; an unset value yields a *relative* `Code/User/globalStorage`, which is
/// what `Path(os.environ.get("APPDATA", ""))` produces.
#[must_use]
pub fn vscode_global_storage(
    platform: Platform,
    home: Option<&Path>,
    appdata: Option<&OsStr>,
) -> PathBuf {
    match platform {
        Platform::Windows => Path::new(appdata.unwrap_or_else(|| OsStr::new("")))
            .join("Code")
            .join("User")
            .join("globalStorage"),
        Platform::Linux => home
            .unwrap_or_else(|| Path::new(""))
            .join(".config")
            .join("Code")
            .join("User")
            .join("globalStorage"),
        Platform::MacOs => home
            .unwrap_or_else(|| Path::new(""))
            .join("Library")
            .join("Application Support")
            .join("Code")
            .join("User")
            .join("globalStorage"),
    }
}

/// The tasks root for one extension id (`_default_tasks_root`).
#[must_use]
pub fn default_tasks_root(
    extension_id: &str,
    platform: Platform,
    home: Option<&Path>,
    appdata: Option<&OsStr>,
) -> PathBuf {
    vscode_global_storage(platform, home, appdata)
        .join(extension_id)
        .join("tasks")
}

/// One member of the Cline family: a provider key, an extension id, and the
/// literal project slug every one of its tasks lands under.
///
/// The family carries no per-project context the way Claude does — every task
/// belongs to the same logical project, named after the provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Variant {
    /// The Cline extension, `saoudrizwan.claude-dev`.
    Cline,
    /// The KiloCode extension, `kilocode.kilo-code`.
    KiloCode,
    /// The Roo Code extension, `rooveterinaryinc.roo-cline`.
    RooCode,
}

impl Variant {
    /// Every variant, in Python's class-name sort order — which is the order
    /// the registry walks them in.
    pub const ALL: [Self; 3] = [Self::Cline, Self::KiloCode, Self::RooCode];

    /// The provider key (`name`).
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Cline => "cline",
            Self::KiloCode => "kilocode",
            Self::RooCode => "roocode",
        }
    }

    /// The VS Code extension id (`_extension_id`).
    #[must_use]
    pub const fn extension_id(self) -> &'static str {
        match self {
            Self::Cline => "saoudrizwan.claude-dev",
            Self::KiloCode => "kilocode.kilo-code",
            Self::RooCode => "rooveterinaryinc.roo-cline",
        }
    }

    /// The project slug every task of this variant lands under
    /// (`_project_slug`).
    #[must_use]
    pub const fn project_slug(self) -> &'static str {
        match self {
            Self::Cline => "cline",
            Self::KiloCode => "kilocode",
            Self::RooCode => "roocode",
        }
    }
}

/// The Cline-family source adapter (`_VsCodeClineAdapter` and its three
/// subclasses).
#[derive(Debug, Clone)]
pub struct ClineFamilyAdapter {
    variant: Variant,
    root: PathBuf,
}

impl ClineFamilyAdapter {
    /// Resolve `variant`'s tasks root from the live environment, once, as
    /// Python's `__init__` does.
    #[must_use]
    pub fn new(variant: Variant) -> Self {
        Self {
            root: default_tasks_root(
                variant.extension_id(),
                Platform::current(),
                home_dir().as_deref(),
                std::env::var_os("APPDATA").as_deref(),
            ),
            variant,
        }
    }

    /// Inject the tasks root — the constructor parameter Python already has.
    #[must_use]
    pub fn with_tasks_root(variant: Variant, root: impl Into<PathBuf>) -> Self {
        Self {
            variant,
            root: root.into(),
        }
    }

    /// Which member of the family this is.
    #[must_use]
    pub const fn variant(&self) -> Variant {
        self.variant
    }

    /// The `tasks/` root this adapter reads.
    #[must_use]
    pub fn tasks_root(&self) -> &Path {
        &self.root
    }
}

impl SourceAdapter for ClineFamilyAdapter {
    fn name(&self) -> &str {
        self.variant.name()
    }

    fn enumerate(&self) -> Vec<SessionRef> {
        if !self.root.is_dir() {
            // Not installed / never used — clean no-op rather than raise.
            return Vec::new();
        }
        let mut out = Vec::new();
        for task_dir in child_dirs(&self.root) {
            let ui_messages = task_dir.join(UI_MESSAGES);
            if !ui_messages.is_file() {
                continue;
            }
            // Python warns and continues on OSError here.
            let Some((mtime, size)) = stat_ref_fields(&ui_messages) else {
                continue;
            };
            out.push(SessionRef::file(
                self.variant.name(),
                self.variant.project_slug(),
                file_name(&task_dir),
                ui_messages,
                mtime,
                size,
            ));
        }
        out
    }

    fn read_into(&self, session: &SessionRef, since_offset: i64, sink: &mut dyn FnMut(Record)) {
        let Some(ui_events) = load_json_array(&session.file_path) else {
            return;
        };
        let history = session
            .file_path
            .parent()
            .map(|dir| dir.join(API_CONVERSATION_HISTORY))
            .and_then(|path| load_json_array(&path))
            .unwrap_or_default();

        // The model is declared once, on the first user message.
        let model = extract_model_tag(&history).unwrap_or_else(|| DEFAULT_MODEL.to_string());
        let mut last_user_text = String::new();

        for (index, event) in ui_events.iter().enumerate() {
            #[allow(
                clippy::cast_possible_wrap,
                reason = "an events array longer than i64::MAX cannot be parsed"
            )]
            let seq = index as i64;
            let say = say_of(event);
            if since_offset > 0 && seq <= since_offset {
                // Keep tracking the user's text while skipping: otherwise the
                // text seen before the resume point is silently lost and the
                // first post-resume turn carries the wrong one.
                if let Some(text) = say
                    .filter(|say| is_user_say(say))
                    .and_then(|_| text_of(event))
                {
                    last_user_text = text;
                }
                continue;
            }
            let Some(say) = say else { continue };
            if is_user_say(say) {
                if let Some(text) = text_of(event) {
                    last_user_text = text;
                }
                continue;
            }
            if say != "api_req_started" {
                continue;
            }
            let tokens = parse_api_req_text(event.get("text"));
            sink(Record {
                provider: self.variant.name().to_string(),
                session_id: session.session_id.clone(),
                seq,
                timestamp: ts_to_iso(event.get("ts")),
                role: "assistant".to_string(),
                model: Some(model.clone()),
                input_tokens: tokens[0],
                output_tokens: tokens[1],
                cache_create_tokens: tokens[2],
                cache_read_tokens: tokens[3],
                content_text: last_user_text.clone(),
                tools: Vec::new(),
                cwd: None,
                is_sidechain: false,
                uuid: format!("{}:{seq}", session.session_id),
                parent_uuid: None,
                raw: event.clone(),
                speed: Speed::Standard,
            });
        }
    }

    /// The extension's `tasks/` root (`watch_paths`). The watcher filters
    /// non-existent roots, so a machine without the extension contributes
    /// nothing.
    fn watch_paths(&self) -> Vec<PathBuf> {
        vec![self.root.clone()]
    }
}

/// `event.get("say")` when the event is a `type == "say"` object.
fn say_of(event: &Value) -> Option<&str> {
    let map = event.as_object()?;
    if map.get("type").and_then(Value::as_str) != Some("say") {
        return None;
    }
    map.get("say").and_then(Value::as_str)
}

/// Whether a `say` value carries the user's own text.
fn is_user_say(say: &str) -> bool {
    say == "user_feedback" || say == "text"
}

/// `event.get("text")` when it is a string — a non-string leaves the previous
/// value in place, as Python's `isinstance` guard does.
fn text_of(event: &Value) -> Option<String> {
    event
        .get("text")
        .and_then(Value::as_str)
        .map(ToString::to_string)
}

/// The JSON array at `path`, or `None` on any failure (`_load_json_array`).
///
/// Both files are top-level arrays; anything else — an object, malformed JSON,
/// an unreadable file — is "no usable data" rather than an error, so one broken
/// task cannot poison a batch enumerate.
#[must_use]
pub fn load_json_array(path: &Path) -> Option<Vec<Value>> {
    // LOG: python warns "Cannot read Cline JSON %s" / "Cline JSON %s is not a list".
    let raw = std::fs::read(path).ok()?;
    let value: Value = jsonl::parse_json(&raw)?;
    match value {
        Value::Array(items) => Some(items),
        _ => None,
    }
}

/// The model declared in `<model>…</model>` on the first user message
/// (`_extract_model_tag`).
///
/// Only the *opening* user message carries the declaration, so the scan stops at
/// the first user entry that has any text at all — a model change mid-task is
/// not supported, in either implementation.
#[must_use]
pub fn extract_model_tag(history: &[Value]) -> Option<String> {
    for entry in history {
        let Some(map) = entry.as_object() else {
            continue;
        };
        if map.get("role").and_then(Value::as_str) != Some("user") {
            continue;
        }
        let text = content_to_text(map.get("content"));
        if text.is_empty() {
            continue;
        }
        // DIVERGENCE (character-class, unreachable in practice): Python's
        // `str.strip()` also strips the C0 separators `\x1c`-`\x1f`, which are
        // not in Unicode's White_Space property and so survive `str::trim`. A
        // model id padded with a file separator is not a shape Cline writes.
        return match_model_tag(&text)
            .map(|model| model.trim().to_string())
            .filter(|model| !model.is_empty());
    }
    None
}

/// `re.compile(r"<model>([^<]+)</model>", re.IGNORECASE).search(text)`.
///
/// Hand-rolled because this crate carries no regex dependency, and exact: the
/// capture class excludes `<`, so the group always ends at the first `<` after
/// the opening tag and no backtracking is possible — a `<model>` that is not
/// closed immediately cannot match at that position, and the search moves on to
/// the next one.
fn match_model_tag(text: &str) -> Option<String> {
    const OPEN: &str = "<model>";
    const CLOSE: &str = "</model>";
    // ASCII-only lowering keeps every byte index aligned with `text`, which
    // `str::to_lowercase` would not (some characters change length).
    let folded = text.to_ascii_lowercase();
    let mut from = 0;
    while let Some(found) = folded[from..].find(OPEN) {
        let start = from + found + OPEN.len();
        let rest = &folded[start..];
        if let Some(end) = rest.find('<')
            && end > 0
            && rest[end..].starts_with(CLOSE)
        {
            return Some(text[start..start + end].to_string());
        }
        from = start;
    }
    None
}

/// Flatten Anthropic-shape content into one string (`_content_to_text`).
#[must_use]
pub fn content_to_text(content: Option<&Value>) -> String {
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
            // A present-but-empty `text` is kept, so it costs a newline in the
            // join — the Gemini convention, not the Qwen one.
            if let Some(text) = map.get("text").and_then(Value::as_str) {
                pieces.push(text.to_string());
            }
        } else if let Some(text) = block.as_str() {
            pieces.push(text.to_string());
        }
    }
    pieces.join("\n")
}

/// The four token counts on an `api_req_started` event, in
/// [`TOKEN_KEYS`] order (`_parse_api_req_text`).
///
/// The `text` field holds *JSON inside a JSON string*. Missing or malformed
/// values are 0 — pricing happens later, so a partial event still produces a
/// valid record.
#[must_use]
pub fn parse_api_req_text(text: Option<&Value>) -> [i64; 4] {
    let mut out = [0_i64; 4];
    let Some(text) = text.and_then(Value::as_str).filter(|text| !text.is_empty()) else {
        return out;
    };
    let Some(parsed) = jsonl::parse_json(text.as_bytes()) else {
        return out;
    };
    let Some(map) = parsed.as_object() else {
        return out;
    };
    for (slot, key) in out.iter_mut().zip(TOKEN_KEYS) {
        *slot = pyval::safe_int(map.get(key));
    }
    out
}

/// Cline's `ts` (epoch milliseconds) → ISO 8601 UTC, or `""` (`_ts_to_iso`).
///
/// Absent, empty, non-numeric, non-positive, and out-of-range values all yield
/// `""`: the contract only requires the field to parse as ISO 8601 when it is
/// non-empty, and a record is still emitted either way.
#[must_use]
pub fn ts_to_iso(ts: Option<&Value>) -> String {
    let Some(ts) = ts else {
        return String::new();
    };
    if matches!(ts, Value::Null) || ts.as_str() == Some("") {
        return String::new();
    }
    let Some(millis) = py_float(ts) else {
        return String::new();
    };
    // NaN fails this comparison and goes on to fail the conversion, exactly as
    // `millis <= 0` then `fromtimestamp(nan)` does.
    if millis <= 0.0 {
        return String::new();
    }
    pyval::epoch_seconds_to_iso(millis / 1000.0).unwrap_or_default()
}

/// Python's `float(v)` for a value decoded from JSON: `None` is the `TypeError`
/// / `ValueError` branch.
fn py_float(value: &Value) -> Option<f64> {
    match value {
        Value::Bool(flag) => Some(if *flag { 1.0 } else { 0.0 }),
        Value::Number(number) => number.as_f64(),
        // `float(str)` strips surrounding whitespace and accepts `inf` / `nan`.
        //
        // DIVERGENCE (literal-grammar): Python also accepts digit separators —
        // `float("1_745_596_800_000")` is a number there and a parse failure
        // here, which lands on `""` instead of a timestamp. `ts` is written by
        // the extension as a JSON number; the underscored *string* form has no
        // producer.
        Value::String(text) => text.trim().parse::<f64>().ok(),
        Value::Null | Value::Array(_) | Value::Object(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn every_variant_names_its_own_extension_and_slug() {
        assert_eq!(Variant::Cline.name(), "cline");
        assert_eq!(Variant::Cline.extension_id(), "saoudrizwan.claude-dev");
        assert_eq!(Variant::KiloCode.name(), "kilocode");
        assert_eq!(Variant::KiloCode.extension_id(), "kilocode.kilo-code");
        assert_eq!(Variant::RooCode.name(), "roocode");
        assert_eq!(
            Variant::RooCode.extension_id(),
            "rooveterinaryinc.roo-cline"
        );
        for variant in Variant::ALL {
            assert_eq!(variant.name(), variant.project_slug());
        }
    }

    #[test]
    fn global_storage_branches_per_platform() {
        let home = Path::new("/home/me");
        let appdata = OsStr::new("C:\\Users\\me\\AppData\\Roaming");
        assert_eq!(
            vscode_global_storage(Platform::Linux, Some(home), None),
            Path::new("/home/me/.config/Code/User/globalStorage")
        );
        assert_eq!(
            vscode_global_storage(Platform::MacOs, Some(home), None),
            Path::new("/home/me/Library/Application Support/Code/User/globalStorage")
        );
        assert_eq!(
            vscode_global_storage(Platform::Windows, Some(home), Some(appdata)),
            Path::new("C:\\Users\\me\\AppData\\Roaming/Code/User/globalStorage")
        );
        // An unset APPDATA yields a relative path, as `Path("")` does.
        assert_eq!(
            vscode_global_storage(Platform::Windows, Some(home), None),
            Path::new("Code/User/globalStorage")
        );
        assert_eq!(
            default_tasks_root("kilocode.kilo-code", Platform::Linux, Some(home), None),
            Path::new("/home/me/.config/Code/User/globalStorage/kilocode.kilo-code/tasks")
        );
    }

    #[test]
    fn model_tag_is_case_insensitive_and_stops_at_the_first_user_message() {
        let history = vec![json!({
            "role": "user",
            "content": [{"type": "text", "text": "<MODEL>claude-sonnet-4-5</model>\nhi"}],
        })];
        assert_eq!(
            extract_model_tag(&history).as_deref(),
            Some("claude-sonnet-4-5")
        );
        // A later user message's tag is never reached.
        let shadowed = vec![
            json!({"role": "user", "content": "no tag here"}),
            json!({"role": "user", "content": "<model>ignored</model>"}),
        ];
        assert_eq!(extract_model_tag(&shadowed), None);
        // Non-user entries are skipped entirely, as are text-less ones.
        let skipped = vec![
            json!({"role": "assistant", "content": "<model>nope</model>"}),
            json!({"role": "user", "content": []}),
            json!({"role": "user", "content": "<model>found</model>"}),
        ];
        assert_eq!(extract_model_tag(&skipped).as_deref(), Some("found"));
        assert_eq!(extract_model_tag(&[]), None);
    }

    #[test]
    fn an_unclosed_model_tag_never_matches() {
        assert_eq!(match_model_tag("<model>unclosed"), None);
        assert_eq!(match_model_tag("<model></model>"), None, "empty capture");
        assert_eq!(match_model_tag("<model>  </model>"), Some("  ".to_string()));
        // The search moves past a failed opening tag to the next one.
        assert_eq!(
            match_model_tag("<model>a<b</model> <model>real</model>").as_deref(),
            Some("real")
        );
    }

    #[test]
    fn api_req_tokens_default_to_zero_on_every_garbage_shape() {
        assert_eq!(
            parse_api_req_text(Some(&json!(
                r#"{"tokensIn": 1200, "tokensOut": 350, "cacheWrites": 200, "cacheReads": 600}"#
            ))),
            [1200, 350, 200, 600]
        );
        // Negative and infinite counts floor at zero.
        assert_eq!(
            parse_api_req_text(Some(&json!(r#"{"tokensIn": -5, "tokensOut": 1e999}"#))),
            [0, 0, 0, 0]
        );
        assert_eq!(parse_api_req_text(Some(&json!("not json"))), [0; 4]);
        assert_eq!(parse_api_req_text(Some(&json!("[1,2]"))), [0; 4]);
        assert_eq!(parse_api_req_text(Some(&json!(42))), [0; 4]);
        assert_eq!(parse_api_req_text(None), [0; 4]);
    }

    #[test]
    fn timestamps_degrade_to_empty_rather_than_raising() {
        assert_eq!(
            ts_to_iso(Some(&json!(1_745_596_800_000_i64))),
            "2025-04-25T16:00:00+00:00"
        );
        assert_eq!(
            ts_to_iso(Some(&json!("1745596800000"))),
            "2025-04-25T16:00:00+00:00",
            "float(str) is accepted"
        );
        assert_eq!(ts_to_iso(Some(&json!(0))), "");
        assert_eq!(ts_to_iso(Some(&json!(-1))), "");
        assert_eq!(ts_to_iso(Some(&json!(""))), "");
        assert_eq!(ts_to_iso(Some(&json!(null))), "");
        assert_eq!(ts_to_iso(Some(&json!([1]))), "");
        assert_eq!(ts_to_iso(Some(&json!("garbage"))), "");
        assert_eq!(ts_to_iso(Some(&json!(1e300))), "", "out of datetime range");
        assert_eq!(ts_to_iso(None), "");
    }
}
