//! Claude Code — the port of `stackunderflow/adapters/claude.py`.
//!
//! Two on-disk formats, both handled here:
//!
//! 1. **Modern**, one JSONL per session at `~/.claude/projects/<slug>/<uuid>.jsonl`.
//! 2. **Legacy**, a single centralised `~/.claude/history.jsonl` for projects
//!    that pre-date the per-project layout. Those projects are recognisable on
//!    disk as a project directory holding *no* `.jsonl` and a
//!    `.continuation_cache.json`; the adapter mints **one synthetic
//!    `legacy-<slug>` session** per such directory and `read()` filters the
//!    shared history file down to that project's lines. Ported faithfully
//!    because it is the input to spec §6b divergence 2.
//!
//! ## Environment, injected
//!
//! `CLAUDE_CONFIG_DIR` relocates `~/.claude` (WSL indexing Windows-side
//! sessions, custom installs). Python reads it inside `_claude_home()` on every
//! call, deliberately — `resolve_legacy_log_dir`'s docstring explains why an
//! `lru_cache` there would freeze whichever value it saw first. This port keeps
//! that: [`ClaudeAdapter::new`] resolves the environment per call, and
//! [`ClaudeAdapter::with_env`] / [`ClaudeAdapter::with_home`] inject it instead,
//! which is how the tests avoid `set_var` (forbidden: Rust 2024 makes it
//! `unsafe`, the workspace forbids `unsafe`).

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::base::{Record, SessionRef, SourceAdapter, Speed, stat_ref_fields};
use crate::jsonl::{JsonlLines, py_bytes_strip};
use crate::pyval;

/// The provider key.
pub const NAME: &str = "claude";

/// The `CLAUDE_CONFIG_DIR` override (`claude.py:41`).
pub const CONFIG_DIR_ENV: &str = "CLAUDE_CONFIG_DIR";

/// Variant homes Anthropic ships under separate XDG-style roots
/// (`claude.py:111`). Empty on a default install; listed so the watcher picks
/// one up the moment it is installed.
pub const VARIANT_HOMES: [&str; 4] = [
    ".claude-opus",
    ".claude-sonnet",
    ".claude-haiku",
    ".claude-glm",
];

/// Claude Code's config home, with the environment injected (`_claude_home`).
///
/// `$CLAUDE_CONFIG_DIR` when set and non-blank (Python `.strip()`s it), with a
/// leading `~` expanded; otherwise `<home>/.claude`.
#[must_use]
pub fn resolve_claude_home(config_dir: Option<&OsStr>, home: Option<&Path>) -> PathBuf {
    let configured = config_dir
        .map(|value| value.to_string_lossy().trim().to_string())
        .filter(|value| !value.is_empty());
    match configured {
        Some(value) => expand_user(Path::new(&value), home),
        None => home.map_or_else(|| PathBuf::from(".claude"), |home| home.join(".claude")),
    }
}

/// Expand a leading `~` / `~/` against `home`, as `pathlib.Path.expanduser` does.
///
/// `~user` forms are left literal — no StackUnderflow path uses them, and
/// honouring them means reading the password database.
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

/// Claude Code's config home from the live process environment (`claude_home`).
///
/// Every consumer outside this module that needs the *home* (not the projects
/// root) must call this: a hardcoded `~/.claude` silently no-ops for
/// `CLAUDE_CONFIG_DIR` users, which is how `backup create` once backed up
/// nothing for exactly those installs.
#[must_use]
pub fn claude_home() -> PathBuf {
    resolve_claude_home(
        std::env::var_os(CONFIG_DIR_ENV).as_deref(),
        home_dir().as_deref(),
    )
}

/// Claude Code's projects directory — THE accessor (`default_projects_root`).
#[must_use]
pub fn default_projects_root() -> PathBuf {
    claude_home().join("projects")
}

/// Stored path, or claude's legacy slug→dir fallback — claude ONLY
/// (`resolve_legacy_log_dir`).
///
/// THE single home for the fallback policy; three row-resolution sites used to
/// inline it. The `<projects-root>/<slug>` scheme is this adapter's, so stamping
/// it on a codex/cursor/grok project invents a directory that never existed: a
/// non-claude project with no stored path resolves to `""` (unknown), which
/// consumers must treat as "no on-disk dir", never as cwd.
///
/// `projects_root` lets a caller resolving many rows derive the root once
/// (`GET /api/projects` calls this 306× per request on the maintainer's store).
#[must_use]
pub fn resolve_legacy_log_dir(
    provider: Option<&str>,
    stored_path: Option<&str>,
    slug: &str,
    projects_root: Option<&Path>,
) -> String {
    if let Some(stored) = stored_path.filter(|value| !value.is_empty()) {
        return stored.to_string();
    }
    if matches!(provider.unwrap_or(NAME), NAME | "anthropic") {
        let root = projects_root.map_or_else(default_projects_root, Path::to_path_buf);
        return root.join(slug).to_string_lossy().into_owned();
    }
    String::new()
}

fn home_dir() -> Option<PathBuf> {
    #[allow(
        deprecated,
        reason = "std::env::home_dir is the platform-correct answer on the \
        1.97.1 pin; stax-core's settings module carries the same allow"
    )]
    std::env::home_dir()
}

/// Where this adapter looks for Claude Code's data.
#[derive(Debug, Clone, Default)]
enum HomeSource {
    /// Read `$CLAUDE_CONFIG_DIR` and the home directory on every call, exactly
    /// as `_claude_home()` does.
    #[default]
    Live,
    /// Injected for tests and the parity harness.
    Injected {
        config_dir: Option<OsString>,
        home: Option<PathBuf>,
    },
}

/// The Claude Code source adapter (`ClaudeAdapter`).
#[derive(Debug, Clone, Default)]
pub struct ClaudeAdapter {
    home: HomeSource,
}

impl ClaudeAdapter {
    /// Read the live environment, as Python does.
    #[must_use]
    pub fn new() -> Self {
        Self {
            home: HomeSource::Live,
        }
    }

    /// Inject both environment inputs: `$CLAUDE_CONFIG_DIR` and the home
    /// directory.
    #[must_use]
    pub fn with_env(config_dir: Option<OsString>, home: Option<PathBuf>) -> Self {
        Self {
            home: HomeSource::Injected { config_dir, home },
        }
    }

    /// Inject a fake home directory — the equivalent of the Python suite's
    /// `set_home_env(monkeypatch, tmp_path)`: the config home becomes
    /// `<home>/.claude` and the variant homes `<home>/.claude-*`.
    #[must_use]
    pub fn with_home(home: impl Into<PathBuf>) -> Self {
        Self::with_env(None, Some(home.into()))
    }

    /// The resolved config home (`~/.claude` or `$CLAUDE_CONFIG_DIR`).
    #[must_use]
    pub fn home(&self) -> PathBuf {
        match &self.home {
            HomeSource::Live => claude_home(),
            HomeSource::Injected { config_dir, home } => {
                resolve_claude_home(config_dir.as_deref(), home.as_deref())
            }
        }
    }

    /// The resolved projects root (`<home>/projects`).
    #[must_use]
    pub fn projects_root(&self) -> PathBuf {
        self.home().join("projects")
    }

    /// The user's home directory, for the variant-home scan.
    fn user_home(&self) -> Option<PathBuf> {
        match &self.home {
            HomeSource::Live => home_dir(),
            HomeSource::Injected { home, .. } => home.clone(),
        }
    }

    /// One `SessionRef` per JSONL file in a modern project directory
    /// (`_refs_from_jsonl`).
    fn refs_from_jsonl(&self, project_dir: &Path, files: &[PathBuf], out: &mut Vec<SessionRef>) {
        for path in files {
            // Python catches FileNotFoundError only; any other stat error
            // propagates. Rust skips on every stat error — an unreadable entry
            // is not a reason to lose the other 993 sessions.
            let Some((mtime, size)) = stat_ref_fields(path) else {
                continue;
            };
            out.push(SessionRef::file(
                NAME,
                dir_name(project_dir),
                path.file_stem().unwrap_or_default().to_string_lossy(),
                path.clone(),
                mtime,
                size,
            ));
        }
    }

    /// One synthetic ref per legacy project (`_refs_from_history`).
    ///
    /// All of that project's `history.jsonl` entries are yielded by `read()` as
    /// one pseudo-session. The ref's `file_mtime` deliberately comes from the
    /// project's own `.continuation_cache.json` (falling back to the directory's
    /// mtime), *not* from the shared history file: another project writing to
    /// the centralised log must not bump this project's "last active".
    fn refs_from_history(&self, project_dir: &Path, out: &mut Vec<SessionRef>) {
        let history_file = self.home().join("history.jsonl");
        if !history_file.is_file() {
            return;
        }
        let Some((history_mtime, size)) = stat_ref_fields(&history_file) else {
            return;
        };
        let cache_file = project_dir.join(".continuation_cache.json");
        let mtime = if cache_file.is_file() {
            stat_ref_fields(&cache_file).map_or(history_mtime, |(mtime, _)| mtime)
        } else {
            // `except OSError: pass` — keep the history file's mtime.
            stat_ref_fields(project_dir).map_or(history_mtime, |(mtime, _)| mtime)
        };
        out.push(SessionRef::file(
            NAME,
            dir_name(project_dir),
            format!("legacy-{}", dir_name(project_dir)),
            history_file,
            mtime,
            size,
        ));
    }

    /// Modern JSONL read, strictly past `since_offset` (`_read_jsonl`).
    fn read_jsonl(&self, session: &SessionRef, since_offset: i64, sink: &mut dyn FnMut(Record)) {
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
            let Ok(obj) = serde_json::from_slice::<Value>(stripped) else {
                continue;
            };
            // A syntactically-valid JSON line that is not an object (bare list /
            // string / number) cannot be a session event.
            if !obj.is_object() {
                continue;
            }
            if let Some(record) = parse_line(&obj, session, line_offset) {
                sink(record);
            }
        }
    }

    /// Legacy `history.jsonl` read for one pseudo-session (`_read_history`).
    fn read_history(&self, session: &SessionRef, sink: &mut dyn FnMut(Record)) {
        if !session.file_path.is_file() {
            return;
        }
        let target_slug = session.project_slug.as_str();
        let cwd = current_dir_string();
        let mut seq = 0_i64;
        for (_line_offset, raw_line) in JsonlLines::open(&session.file_path, 0) {
            let stripped = py_bytes_strip(&raw_line);
            if stripped.is_empty() {
                continue;
            }
            let Ok(obj) = serde_json::from_slice::<Value>(stripped) else {
                continue;
            };
            let Some(map) = obj.as_object() else { continue };
            let Some(project) = map.get("project").and_then(Value::as_str) else {
                continue;
            };
            if project.is_empty() || pyval::slug_for(project, &cwd) != target_slug {
                continue;
            }
            let display = map
                .get("display")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            // History timestamps are epoch-millis ints; a malformed entry (ISO
            // string, list, …) coerces to 0 and is skipped rather than raising
            // out of the generator.
            let ts_ms = pyval::safe_int(map.get("timestamp"));
            if ts_ms == 0 {
                continue;
            }
            let ts_iso = pyval::epoch_ms_to_iso(ts_ms);
            if ts_iso.is_empty() {
                continue;
            }
            let session_id = map
                .get("sessionId")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .map_or_else(|| session.session_id.clone(), ToString::to_string);
            sink(Record {
                provider: NAME.to_string(),
                session_id,
                seq,
                timestamp: ts_iso,
                role: "user".to_string(),
                model: None,
                input_tokens: 0,
                output_tokens: 0,
                cache_create_tokens: 0,
                cache_read_tokens: 0,
                content_text: display,
                tools: Vec::new(),
                cwd: None,
                is_sidechain: false,
                uuid: String::new(),
                parent_uuid: None,
                raw: obj,
                speed: Speed::Standard,
            });
            seq += 1;
        }
    }
}

impl SourceAdapter for ClaudeAdapter {
    fn name(&self) -> &str {
        NAME
    }

    fn enumerate(&self) -> Vec<SessionRef> {
        let root = self.projects_root();
        if !root.is_dir() {
            return Vec::new();
        }
        let mut out = Vec::new();
        // DIVERGENCE (deliberate, order-only): Python iterates `root.iterdir()`
        // in readdir order, which is neither sorted nor reproducible across
        // filesystems. Sorting here yields the same *set* of refs in a
        // deterministic order — a property the parity harness needs and ingest
        // does not care about (each ref is written independently).
        let mut project_dirs = read_dir_sorted(&root);
        project_dirs.retain(|path| path.is_dir());
        for project_dir in project_dirs {
            let jsonl_files = glob_suffix(&project_dir, ".jsonl");
            if jsonl_files.is_empty() {
                if project_dir.join(".continuation_cache.json").exists() {
                    self.refs_from_history(&project_dir, &mut out);
                }
            } else {
                self.refs_from_jsonl(&project_dir, &jsonl_files, &mut out);
            }
        }
        out
    }

    fn read_into(&self, session: &SessionRef, since_offset: i64, sink: &mut dyn FnMut(Record)) {
        if session.session_id.starts_with("legacy-") {
            self.read_history(session, sink);
            return;
        }
        self.read_jsonl(session, since_offset, sink);
    }

    /// `~/.claude/projects` always, plus each existing `~/.claude-{opus,sonnet,
    /// haiku,glm}/projects` (`watch_paths`).
    ///
    /// The watcher filters again on existence before handing these to the
    /// file-watching layer, so a missing root here is a clean no-op.
    fn watch_paths(&self) -> Vec<PathBuf> {
        let mut roots = vec![self.projects_root()];
        if let Some(home) = self.user_home() {
            for variant in VARIANT_HOMES {
                let candidate = home.join(variant).join("projects");
                if candidate.is_dir() {
                    roots.push(candidate);
                }
            }
        }
        roots
    }
}

/// One JSONL line → a `Record`, or `None` for a non-conversational line
/// (`_parse_line`).
fn parse_line(obj: &Value, session: &SessionRef, seq: i64) -> Option<Record> {
    let map = obj.as_object()?;
    let msg = map.get("message").filter(|value| value.is_object());
    let role = role_from(map, msg)?;
    // `message.usage` carrying a string/list would crash the `.get` calls
    // below — treat it like a missing usage block.
    let usage = msg
        .and_then(|msg| msg.get("usage"))
        .filter(|value| value.is_object());
    let usage_get = |key: &str| usage.and_then(|usage| usage.get(key));

    let session_id = map
        .get("sessionId")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map_or_else(|| session.session_id.clone(), ToString::to_string);
    Some(Record {
        provider: NAME.to_string(),
        session_id,
        seq,
        timestamp: map.get("timestamp").map_or_else(String::new, pyval::py_str),
        role,
        model: model_from(msg),
        input_tokens: pyval::safe_int(usage_get("input_tokens")),
        output_tokens: pyval::safe_int(usage_get("output_tokens")),
        cache_create_tokens: pyval::safe_int(usage_get("cache_creation_input_tokens")),
        cache_read_tokens: pyval::safe_int(usage_get("cache_read_input_tokens")),
        content_text: text_from(msg),
        tools: tools_from(msg),
        cwd: map
            .get("cwd")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(ToString::to_string),
        is_sidechain: map.get("isSidechain").is_some_and(pyval::py_truthy),
        uuid: map
            .get("uuid")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        // An empty-string parentUuid stays `Some("")`: Python's guard is
        // `isinstance(parent, str)`, which an empty string passes.
        parent_uuid: map
            .get("parentUuid")
            .and_then(Value::as_str)
            .map(ToString::to_string),
        raw: obj.clone(),
        speed: speed_from(usage),
    })
}

/// The conversational role, or `None` when the line is not a message
/// (`_role_from`).
///
/// `summary` / `compact_summary` lines are explicitly not conversational
/// records; anything else falls back to `message.role`.
fn role_from(obj: &serde_json::Map<String, Value>, msg: Option<&Value>) -> Option<String> {
    match obj.get("type").and_then(Value::as_str) {
        Some("user") => return Some("user".to_string()),
        Some("assistant") => return Some("assistant".to_string()),
        Some("summary" | "compact_summary") => return None,
        _ => {}
    }
    match msg.and_then(|msg| msg.get("role")).and_then(Value::as_str) {
        Some(role @ ("user" | "assistant")) => Some(role.to_string()),
        _ => None,
    }
}

/// The model id, dropping Claude Code's `"<synthetic>"` sentinel (`_model_from`).
///
/// Claude Code stamps `message.model = "<synthetic>"` on locally generated
/// placeholders — API errors, invalid-request stubs, "No response requested."
/// Those rows carry zero tokens and zero cost, so propagating the literal only
/// pollutes user-facing surfaces (`stackunderflow compare` showed it as its own
/// model row). `None` makes every cost/compare path skip it the way it skips any
/// other model-less record.
fn model_from(msg: Option<&Value>) -> Option<String> {
    let raw = msg?.get("model")?.as_str()?;
    if raw.is_empty() || raw == "<synthetic>" {
        return None;
    }
    Some(raw.to_string())
}

/// Concatenated text blocks (`_text_from`).
fn text_from(msg: Option<&Value>) -> String {
    let Some(body) = msg.and_then(|msg| msg.get("content")) else {
        return String::new();
    };
    if let Some(text) = body.as_str() {
        return text.to_string();
    }
    let Some(blocks) = body.as_array() else {
        return String::new();
    };
    let mut pieces: Vec<String> = Vec::new();
    for block in blocks {
        if let Some(map) = block.as_object() {
            if map.get("type").and_then(Value::as_str) != Some("text") {
                continue;
            }
            match map.get("text") {
                // `blk.get("text", "")` — a *missing* key contributes an empty
                // piece, which still costs a "\n" in the join.
                None => pieces.push(String::new()),
                Some(Value::String(text)) => pieces.push(text.clone()),
                // DIVERGENCE (fixed-in-rust): a non-string `text` makes
                // Python's `"\n".join(pieces)` raise TypeError *out of the
                // read() generator*, aborting the whole file's ingest batch.
                // Skipping the block keeps the rest of the file ingestible.
                Some(_) => {}
            }
        } else if let Some(text) = block.as_str() {
            pieces.push(text.to_string());
        }
    }
    pieces.join("\n")
}

/// `service_tier` → the two-value speed enum (`_speed_from`).
///
/// Anthropic documents `standard` / `priority` / `batch`, and the field is
/// `null` on pre-rollout records. Anything other than `priority` is standard, so
/// we never *over*-charge: billing standard records at the ~6× Opus priority
/// rate is a far worse failure than under-reporting a priority one.
fn speed_from(usage: Option<&Value>) -> Speed {
    match usage.and_then(|usage| usage.get("service_tier")) {
        Some(Value::String(tier)) if tier == "priority" => Speed::Fast,
        _ => Speed::Standard,
    }
}

/// Tool names invoked in this turn (`_tools_from`).
fn tools_from(msg: Option<&Value>) -> Vec<String> {
    let Some(blocks) = msg
        .and_then(|msg| msg.get("content"))
        .and_then(Value::as_array)
    else {
        return Vec::new();
    };
    let mut names = Vec::new();
    for block in blocks {
        let Some(map) = block.as_object() else {
            continue;
        };
        if map.get("type").and_then(Value::as_str) != Some("tool_use") {
            continue;
        }
        match map.get("name") {
            None => {}
            // DIVERGENCE (fixed-in-rust): Python appends the raw object when it
            // is truthy, so a non-string name lands in a `tuple[str, ...]` and
            // surfaces unquoted in `tools_json`. Here it is stringified with
            // Python's own `str()` semantics.
            Some(value) if pyval::py_truthy(value) => names.push(pyval::py_str(value)),
            Some(_) => {}
        }
    }
    names
}

/// `Path.name` for a directory, as a `String`.
fn dir_name(path: &Path) -> String {
    path.file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned()
}

/// `os.getcwd()` for the slug derivation; `"/"` when the process has no cwd.
fn current_dir_string() -> String {
    std::env::current_dir().map_or_else(
        |_| "/".to_string(),
        |path| path.to_string_lossy().into_owned(),
    )
}

/// `Path.iterdir()`, sorted — see the divergence note on `enumerate`.
fn read_dir_sorted(root: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut paths: Vec<PathBuf> = entries.flatten().map(|entry| entry.path()).collect();
    paths.sort();
    paths
}

/// `Path.glob("*<suffix>")`, sorted.
///
/// pathlib's `glob` — unlike `glob.glob` — does **not** hide dotfiles, so
/// neither does this: a `.foo.jsonl` in a project directory is enumerated by
/// both implementations.
fn glob_suffix(dir: &Path, suffix: &str) -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = read_dir_sorted(dir)
        .into_iter()
        .filter(|path| {
            path.file_name()
                .is_some_and(|name| name.to_string_lossy().ends_with(suffix))
        })
        .collect();
    paths.sort();
    paths
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_dir_env_overrides_the_home_layout() {
        let home = Path::new("/home/me");
        assert_eq!(
            resolve_claude_home(None, Some(home)),
            Path::new("/home/me/.claude")
        );
        assert_eq!(
            resolve_claude_home(Some(OsStr::new("  ")), Some(home)),
            Path::new("/home/me/.claude"),
            "a blank CLAUDE_CONFIG_DIR is Python-falsy after .strip()"
        );
        assert_eq!(
            resolve_claude_home(Some(OsStr::new("/mnt/c/Users/me/.claude")), Some(home)),
            Path::new("/mnt/c/Users/me/.claude")
        );
        assert_eq!(
            resolve_claude_home(Some(OsStr::new("~/elsewhere")), Some(home)),
            Path::new("/home/me/elsewhere")
        );
    }

    #[test]
    fn legacy_log_dir_is_claude_only() {
        let root = Path::new("/root/projects");
        assert_eq!(
            resolve_legacy_log_dir(Some("claude"), None, "-a-b", Some(root)),
            "/root/projects/-a-b"
        );
        assert_eq!(
            resolve_legacy_log_dir(None, None, "-a-b", Some(root)),
            "/root/projects/-a-b",
            "a missing provider defaults to claude"
        );
        assert_eq!(
            resolve_legacy_log_dir(Some("anthropic"), None, "-a-b", Some(root)),
            "/root/projects/-a-b"
        );
        assert_eq!(
            resolve_legacy_log_dir(Some("codex"), None, "-a-b", Some(root)),
            "",
            "a non-claude project must never get an invented directory"
        );
        assert_eq!(
            resolve_legacy_log_dir(Some("codex"), Some("/stored"), "-a-b", Some(root)),
            "/stored"
        );
    }
}
