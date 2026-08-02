//! The `custom` history-source importer — the port of
//! `stackunderflow/adapters/custom_import.py`.
//!
//! This is the **store half** of the `stackunderflow-history-jsonl-v1` contract.
//! An external tool exports its history as a validated JSONL stream; this module
//! turns that stream into store records under one `custom` provider, and
//! persists an opaque resume cursor per source.
//!
//! ## Not a registered adapter
//!
//! `custom_import` never appears in [`crate::registry`]. Python's
//! self-discovering registry skips it because its classes do not satisfy the
//! adapter shape, and `tests/…/test_default_registry.py` lists it in
//! `_INFRA_MODULES` explicitly. Custom imports run only through the explicit
//! CLI command, so the default-registry contract is untouched.
//!
//! ## What landed here, and what belongs to RS-2-006
//!
//! The **format half** — `load_manifest`, `parse_stream`, `run_export` and the
//! guarded subprocess runner — is `adapters/custom_jsonl.py`, a separate item
//! (RS-2-006) and a separate module. What this port carries is everything on
//! the store side of that seam: the record types the stream produces
//! ([`MessageRecord`], [`FileTouchRecord`], [`ParsedStream`]) as an input
//! contract, the mapping onto [`Record`], the session planner, the cursor
//! sidecar, and the in-memory adapter shim. When RS-2-006 lands it owns
//! *producing* a [`ParsedStream`]; nothing here changes.
//!
//! The orchestration function `import_history_source` is deliberately absent for
//! the same reason plus one more: it drives `ingest.writer.ingest_file`, which
//! is a different crate and a later wave.
//!
//! ## Why the ids are content-addressed
//!
//! Two properties make a re-import a no-op and a cross-machine merge safe:
//!
//! * the store session id is `"<source_id>:<stream_session_id>"` — stable,
//!   globally distinct, reproduced identically every run;
//! * every message and file-touch uuid is a
//!   [`content_hash_id`](crate::base::content_hash_id) of its content, so an
//!   identical record hashes to an identical uuid on any machine.
//!
//! Deliberately additive: the message primary key stays the machine-local
//! integer the writer assigns, and re-import idempotency rides on the existing
//! `(session_fk, seq)` UNIQUE + `INSERT OR IGNORE`.

use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::base::{
    CONTENT_HASH_LENGTH, Record, SessionRef, SourceAdapter, SourceKind, Speed, content_hash_id,
};
use crate::jsonl;
use crate::pyval;

/// The provider every imported row is written under (`CUSTOM_PROVIDER`).
pub const CUSTOM_PROVIDER: &str = "custom";

/// The stream + manifest schema tag (`custom_jsonl.SCHEMA`).
///
/// The trailing `v1` is a maintainer-only bump — see the project version rule —
/// never widened by an agent.
pub const SCHEMA: &str = "stackunderflow-history-jsonl-v1";

/// The canonical manifest filename (`custom_jsonl.MANIFEST_FILENAME`).
pub const MANIFEST_FILENAME: &str = "stackunderflow-history-plugin.json";

/// Where the opaque per-source cursor lives, relative to the state dir
/// (`_CURSOR_SUBDIR`).
pub const CURSOR_SUBDIR: &str = "history_sources";

/// A source id is a slug *and* a filename, so it is restricted to a
/// traversal-proof charset of this length (`custom_jsonl._SOURCE_ID_MAX_LEN`).
pub const SOURCE_ID_MAX_LEN: usize = 128;

/// `file_touch.operation` → a Claude-style tool name (`_OPERATION_TOOL`).
///
/// The touch then shows up in the tools list and, through `content_text`, in
/// `find_sessions_touching_file`.
pub const OPERATION_TOOL: [(&str, &str); 7] = [
    ("read", "Read"),
    ("write", "Write"),
    ("create", "Write"),
    ("edit", "Edit"),
    ("modify", "Edit"),
    ("delete", "Edit"),
    ("append", "Edit"),
];

/// The tool name an unrecognised operation maps to (`_DEFAULT_TOUCH_TOOL`).
pub const DEFAULT_TOUCH_TOOL: &str = "Edit";

/// The prefix every content-addressed id in this module carries.
pub const ID_PREFIX: &str = "c-";

// ── the stream contract (produced by custom_jsonl, RS-2-006) ─────────────────

/// A `session` line: establishes a session and optionally its project
/// (`custom_jsonl.SessionRecord`).
#[derive(Debug, Clone, PartialEq)]
pub struct StreamSession {
    /// The stream-local session id.
    pub session_id: String,
    /// The logical project this session belongs to.
    pub project: Option<String>,
    /// The working directory, inherited by every record in the session.
    pub cwd: Option<String>,
    /// A human title.
    pub title: Option<String>,
    /// First timestamp seen.
    pub first_timestamp: Option<String>,
    /// Last timestamp seen.
    pub last_timestamp: Option<String>,
    /// The source line, verbatim.
    pub raw: Value,
}

/// A `message` line: one turn (`custom_jsonl.MessageRecord`).
///
/// `seq` is the record's stable identity within its session — unique across
/// every message *and* file touch in that session, and monotonic in emit order.
#[derive(Debug, Clone, PartialEq)]
pub struct MessageRecord {
    /// The stream-local session id.
    pub session_id: String,
    /// The session-unique sequence number.
    pub seq: i64,
    /// ISO 8601 timestamp.
    pub timestamp: String,
    /// `user` / `assistant` / `system` / `tool`.
    pub role: String,
    /// The turn's text.
    pub content: String,
    /// Model id, when the source records one.
    pub model: Option<String>,
    /// Fresh input tokens.
    pub input_tokens: i64,
    /// Billable output tokens.
    pub output_tokens: i64,
    /// Cache-read tokens.
    pub cache_read_tokens: i64,
    /// Cache-write tokens.
    pub cache_creation_tokens: i64,
    /// Tool names invoked.
    pub tools: Vec<String>,
    /// A per-message cwd override.
    pub cwd: Option<String>,
    /// The source line, verbatim.
    pub raw: Value,
}

/// A `file_touch` line: a file the agent read or wrote
/// (`custom_jsonl.FileTouchRecord`).
#[derive(Debug, Clone, PartialEq)]
pub struct FileTouchRecord {
    /// The stream-local session id.
    pub session_id: String,
    /// Shares the session's monotonic sequence with messages.
    pub seq: i64,
    /// The path touched.
    pub path: String,
    /// `read` / `write` / `edit` / …
    pub operation: String,
    /// ISO 8601 timestamp.
    pub timestamp: String,
    /// The source line, verbatim.
    pub raw: Value,
}

/// The validated result of one export run (`custom_jsonl.ParsedStream`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ParsedStream {
    /// Sessions by id, in first-seen order (Python's `dict`).
    pub sessions: Vec<StreamSession>,
    /// Every message line.
    pub messages: Vec<MessageRecord>,
    /// Every file-touch line.
    pub file_touches: Vec<FileTouchRecord>,
    /// The cursor to persist after a fully successful run.
    pub next_cursor: Option<String>,
}

impl ParsedStream {
    /// Every session id referenced anywhere in the stream, in first-seen order
    /// (`ParsedStream.session_ids`).
    ///
    /// Session lines first, then any message or touch naming a session that
    /// never got an explicit `session` line.
    #[must_use]
    pub fn session_ids(&self) -> Vec<String> {
        let mut seen: Vec<String> = Vec::new();
        let mut push = |candidate: &str| {
            if !seen.iter().any(|id| id == candidate) {
                seen.push(candidate.to_string());
            }
        };
        for session in &self.sessions {
            push(&session.session_id);
        }
        for message in &self.messages {
            push(&message.session_id);
        }
        for touch in &self.file_touches {
            push(&touch.session_id);
        }
        seen
    }

    fn session(&self, session_id: &str) -> Option<&StreamSession> {
        self.sessions
            .iter()
            .find(|session| session.session_id == session_id)
    }
}

// ── manifest resolution ──────────────────────────────────────────────────────

/// Resolve a `--history-source` *name* to a manifest path
/// (`resolve_manifest_path`).
///
/// Accepted, in order: an existing file; an existing directory holding the
/// canonical filename; a named source under one of `search_roots`
/// (`<root>/<name>/stackunderflow-history-plugin.json`).
///
/// # Errors
/// `ManifestError`'s message, verbatim, when nothing matches.
pub fn resolve_manifest_path(
    name: &str,
    search_roots: &[PathBuf],
    home: Option<&Path>,
) -> Result<PathBuf, String> {
    // Every path here goes through `py_path_str`, because every path here is
    // eventually PRINTED — the candidate in this function's own message, the
    // returned one in `parse_manifest`'s `where` — and `str(Path(...))` is
    // normalised where `PathBuf::display()` is verbatim (DIV-457).
    let candidate = py_path(&expand_user(Path::new(name), home));
    if candidate.is_file() {
        return Ok(candidate);
    }
    if candidate.is_dir() {
        let inner = py_path(&candidate.join(MANIFEST_FILENAME));
        if inner.is_file() {
            return Ok(inner);
        }
    }
    for root in search_roots {
        let inner = py_path(&root.join(name).join(MANIFEST_FILENAME));
        if inner.is_file() {
            return Ok(inner);
        }
    }
    let searched = search_roots
        .iter()
        .map(|root| py_path_str(&root.join(name).join(MANIFEST_FILENAME)))
        .collect::<Vec<_>>()
        .join(", ");
    let searched = if searched.is_empty() {
        "(no search roots)".to_string()
    } else {
        searched
    };
    // `{name!r}` is repr, not str — the quotes are part of the message. The
    // candidate is `str(Path(...))`, which is NORMALISED (DIV-457).
    Err(format!(
        "no history-source manifest for {}. Looked for a file/dir at {}, then: {searched}",
        pyval::py_repr(&Value::from(name)),
        py_path_str(&candidate)
    ))
}

/// `str(PurePosixPath(p))` — the message's candidate, normalised as pathlib
/// normalises it (DIV-457).
///
/// `Path("")` is `PosixPath('.')`, and `--history-source ''` prints that `.` on
/// the reference where `PathBuf::from("")` displays as nothing at all. Found by
/// the import leg's empty-string row, which is the `--project ''` class the
/// ledger has now caught four times: **every string option needs a row that
/// passes it the empty string.**
///
/// The rules are pathlib's and nothing more: empty and `.` components are
/// dropped, a trailing separator goes with them, `..` is KEPT (pathlib does not
/// resolve it), and a leading `//` — exactly two — is POSIX's own double-slash
/// root and survives where `///` collapses to `/`.
fn py_path(path: &Path) -> PathBuf {
    PathBuf::from(py_path_str(path))
}

/// `str(PurePosixPath(p))` — see [`py_path`].
fn py_path_str(path: &Path) -> String {
    let raw = path.to_string_lossy();
    let root = if raw.starts_with("//") && !raw.starts_with("///") {
        "//"
    } else if raw.starts_with('/') {
        "/"
    } else {
        ""
    };
    let parts: Vec<&str> = raw
        .split('/')
        .filter(|part| !part.is_empty() && *part != ".")
        .collect();
    if parts.is_empty() {
        return if root.is_empty() {
            ".".to_string()
        } else {
            root.to_string()
        };
    }
    format!("{root}{}", parts.join("/"))
}

/// Expand a leading `~` against `home`, as `Path.expanduser()` does.
fn expand_user(path: &Path, home: Option<&Path>) -> PathBuf {
    let Some(home) = home else {
        return path.to_path_buf();
    };
    let mut parts = path.components();
    match parts.next() {
        Some(std::path::Component::Normal(first)) if first == std::ffi::OsStr::new("~") => {
            home.join(parts.as_path())
        }
        _ => path.to_path_buf(),
    }
}

// ── cursor persistence (sidecar) ─────────────────────────────────────────────

/// A filename- and slug-safe, traversal-proof source id
/// (`custom_jsonl.is_safe_source_id`).
///
/// A non-empty run of `[A-Za-z0-9._-]` that is not `.` or `..`. `isalnum()` is
/// Unicode-aware in Python and `char::is_alphanumeric` is Unicode-aware here,
/// so a non-ASCII letter passes in both.
#[must_use]
pub fn is_safe_source_id(source_id: &str) -> bool {
    if source_id.is_empty() || source_id.chars().count() > SOURCE_ID_MAX_LEN {
        return false;
    }
    if source_id == "." || source_id == ".." {
        return false;
    }
    source_id
        .chars()
        .all(|ch| ch.is_alphanumeric() || ch == '.' || ch == '_' || ch == '-')
}

/// `<state_dir>/history_sources/<source_id>.cursor.json` (`_cursor_path`).
///
/// # Errors
/// The defensive `ManifestError` message when `source_id` is unsafe. The
/// manifest loader already checked; this is the second lock on the door,
/// because the value becomes a filename.
pub fn cursor_path(state_dir: &Path, source_id: &str) -> Result<PathBuf, String> {
    if !is_safe_source_id(source_id) {
        return Err(format!(
            "unsafe source_id for cursor storage: {}",
            pyval::py_str(&Value::from(source_id))
        ));
    }
    Ok(state_dir
        .join(CURSOR_SUBDIR)
        .join(format!("{source_id}.cursor.json")))
}

/// The stored cursor for `source_id`, or `None` (`load_cursor`).
///
/// A missing, unreadable or corrupt sidecar is "start fresh", not a failure:
/// the cursor is regenerable, and the worst case is a replay from the manifest
/// seed, which is idempotent.
#[must_use]
pub fn load_cursor(state_dir: &Path, source_id: &str) -> Option<String> {
    let path = cursor_path(state_dir, source_id).ok()?;
    let text = std::fs::read_to_string(path).ok()?;
    let data: Value = jsonl::parse_json(text.as_bytes())?;
    match data.get("cursor") {
        Some(Value::String(cursor)) => Some(cursor.clone()),
        _ => None,
    }
}

/// Persist `cursor` for `source_id` atomically (`store_cursor`).
///
/// Write-temp-then-rename, so a crash mid-write leaves the previous cursor
/// intact rather than a truncated one. `now_seconds` is `time.time()`,
/// injected — the campaign forbids freezing the process clock.
///
/// # Errors
/// An unsafe source id, or any filesystem failure.
pub fn store_cursor(
    state_dir: &Path,
    source_id: &str,
    cursor: &str,
    now_seconds: f64,
) -> Result<(), String> {
    let path = cursor_path(state_dir, source_id)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|err| err.to_string())?;
    }
    // `json.dumps(payload, indent=2)` — two-space indent, `": "` separators,
    // and the float rendered by Python's repr.
    let body = format!(
        "{{\n  \"schema\": {},\n  \"source_id\": {},\n  \"cursor\": {},\n  \"updated_at\": {}\n}}",
        json_string(SCHEMA),
        json_string(source_id),
        json_string(cursor),
        pyval::py_float_str(now_seconds),
    );
    // `path.with_suffix(".json.tmp")` on `<id>.cursor.json` replaces the last
    // suffix: `<id>.cursor.json.tmp`.
    let tmp = crate::walk::with_suffix(&path, ".json.tmp");
    std::fs::write(&tmp, body).map_err(|err| err.to_string())?;
    std::fs::rename(&tmp, &path).map_err(|err| err.to_string())
}

/// One JSON string literal, escaped as `json.dumps` escapes it.
fn json_string(text: &str) -> String {
    Value::from(text).to_string()
}

// ── store mapping ────────────────────────────────────────────────────────────

/// Namespace a stream project under its source id (`_project_slug`).
///
/// A source exporting one logical project lands at `<source_id>`; a
/// multi-project export disambiguates with `<source_id>--<project>`, with every
/// character outside `[alnum]._-` rewritten to `-` and the result stripped of
/// leading and trailing dashes.
#[must_use]
pub fn project_slug(source_id: &str, project: Option<&str>) -> String {
    let Some(project) = project.filter(|project| !project.is_empty()) else {
        return source_id.to_string();
    };
    let mapped: String = project
        .chars()
        .map(|ch| {
            if ch.is_alphanumeric() || ch == '.' || ch == '_' || ch == '-' {
                ch
            } else {
                '-'
            }
        })
        .collect();
    let safe = mapped.trim_matches('-');
    if safe.is_empty() {
        source_id.to_string()
    } else {
        format!("{source_id}--{safe}")
    }
}

/// `"<source_id>:<stream_session_id>"` (`_store_session_id`).
#[must_use]
pub fn store_session_id(source_id: &str, stream_session_id: &str) -> String {
    format!("{source_id}:{stream_session_id}")
}

/// One stream message → one store [`Record`] (`_message_to_record`).
#[must_use]
pub fn message_to_record(
    message: &MessageRecord,
    source_id: &str,
    store_session_id: &str,
    session_cwd: Option<&str>,
) -> Record {
    let uuid = content_hash_id(
        &[
            Some(CUSTOM_PROVIDER.to_string()),
            Some(source_id.to_string()),
            Some(store_session_id.to_string()),
            Some(message.seq.to_string()),
            Some("message".to_string()),
            Some(message.role.clone()),
            Some(message.timestamp.clone()),
            // `msg.model or ""` — a missing model hashes as the empty string,
            // not as the None sentinel.
            Some(message.model.clone().unwrap_or_default()),
            Some(message.content.clone()),
        ],
        ID_PREFIX,
        CONTENT_HASH_LENGTH,
    );
    Record {
        provider: CUSTOM_PROVIDER.to_string(),
        session_id: store_session_id.to_string(),
        seq: message.seq,
        timestamp: message.timestamp.clone(),
        role: message.role.clone(),
        model: message.model.clone(),
        input_tokens: message.input_tokens,
        output_tokens: message.output_tokens,
        cache_create_tokens: message.cache_creation_tokens,
        cache_read_tokens: message.cache_read_tokens,
        content_text: message.content.clone(),
        tools: message.tools.clone(),
        // `msg.cwd or cwd` — a blank per-message cwd falls back to the
        // session's, which may itself be absent.
        cwd: message
            .cwd
            .clone()
            .filter(|cwd| !cwd.is_empty())
            .or_else(|| session_cwd.map(ToString::to_string)),
        is_sidechain: false,
        uuid,
        parent_uuid: None,
        raw: message.raw.clone(),
        speed: Speed::Standard,
    }
}

/// One stream file touch → one store [`Record`] (`_file_touch_to_record`).
///
/// The path goes into `content_text` because `find_sessions_touching_file`
/// scans that column for a mention; the operation is recorded as a tool name so
/// the touch shows up in the tools list.
#[must_use]
pub fn file_touch_to_record(
    touch: &FileTouchRecord,
    source_id: &str,
    store_session_id: &str,
    session_cwd: Option<&str>,
) -> Record {
    let lowered = touch.operation.to_lowercase();
    let tool = OPERATION_TOOL
        .iter()
        .find(|(operation, _)| *operation == lowered)
        .map_or(DEFAULT_TOUCH_TOOL, |(_, tool)| *tool);
    let uuid = content_hash_id(
        &[
            Some(CUSTOM_PROVIDER.to_string()),
            Some(source_id.to_string()),
            Some(store_session_id.to_string()),
            Some(touch.seq.to_string()),
            Some("file_touch".to_string()),
            Some(touch.operation.clone()),
            Some(touch.path.clone()),
        ],
        ID_PREFIX,
        CONTENT_HASH_LENGTH,
    );
    Record {
        provider: CUSTOM_PROVIDER.to_string(),
        session_id: store_session_id.to_string(),
        seq: touch.seq,
        timestamp: touch.timestamp.clone(),
        role: "assistant".to_string(),
        model: None,
        input_tokens: 0,
        output_tokens: 0,
        cache_create_tokens: 0,
        cache_read_tokens: 0,
        content_text: format!("{} {}", touch.operation, touch.path),
        tools: vec![tool.to_string()],
        cwd: session_cwd.map(ToString::to_string),
        is_sidechain: false,
        uuid,
        parent_uuid: None,
        raw: touch.raw.clone(),
        speed: Speed::Standard,
    }
}

/// One session's worth of planned store writes (`_SessionPlan`).
#[derive(Debug, Clone, PartialEq)]
pub struct SessionPlan {
    /// `"<source_id>:<stream_session_id>"`.
    pub store_session_id: String,
    /// The namespaced project slug.
    pub project_slug: String,
    /// The session's working directory, when it declared one.
    pub cwd: Option<String>,
    /// Every record for this session, ordered by `seq`.
    pub records: Vec<Record>,
}

/// Group a validated stream into per-session, seq-ordered store records
/// (`_plan_sessions`).
///
/// The sort is stable, as Python's is: a message and a file touch that share a
/// `seq` keep their emit order (messages first, since the message loop runs
/// first).
#[must_use]
pub fn plan_sessions(stream: &ParsedStream, source_id: &str) -> Vec<SessionPlan> {
    let ids = stream.session_ids();
    let mut records_by_sid: Vec<(String, Vec<Record>)> =
        ids.iter().map(|sid| (sid.clone(), Vec::new())).collect();
    let index_of =
        |sid: &str, table: &[(String, Vec<Record>)]| table.iter().position(|(key, _)| key == sid);

    for message in &stream.messages {
        let store_sid = store_session_id(source_id, &message.session_id);
        let cwd = stream
            .session(&message.session_id)
            .and_then(|session| session.cwd.clone());
        if let Some(index) = index_of(&message.session_id, &records_by_sid) {
            records_by_sid[index].1.push(message_to_record(
                message,
                source_id,
                &store_sid,
                cwd.as_deref(),
            ));
        }
    }
    for touch in &stream.file_touches {
        let store_sid = store_session_id(source_id, &touch.session_id);
        let cwd = stream
            .session(&touch.session_id)
            .and_then(|session| session.cwd.clone());
        if let Some(index) = index_of(&touch.session_id, &records_by_sid) {
            records_by_sid[index].1.push(file_touch_to_record(
                touch,
                source_id,
                &store_sid,
                cwd.as_deref(),
            ));
        }
    }

    ids.iter()
        .map(|sid| {
            let mut records = index_of(sid, &records_by_sid)
                .map(|index| records_by_sid[index].1.clone())
                .unwrap_or_default();
            records.sort_by_key(|record| record.seq);
            let session = stream.session(sid);
            SessionPlan {
                store_session_id: store_session_id(source_id, sid),
                project_slug: project_slug(
                    source_id,
                    session.and_then(|session| session.project.as_deref()),
                ),
                cwd: session.and_then(|session| session.cwd.clone()),
                records,
            }
        })
        .collect()
}

/// The synthetic path a `custom` import's refs point at (`synthetic_path`).
#[must_use]
pub fn synthetic_path(source_id: &str) -> PathBuf {
    PathBuf::from(format!("custom-history:{source_id}"))
}

/// The `SessionRef` the writer is handed for one plan.
///
/// `source_kind` is `database` and `file_size` is the record *count*: there is
/// no file, and the two fields are the ingest layer's dedup key and change
/// signal rather than filesystem facts.
#[must_use]
pub fn session_ref(plan: &SessionPlan, source_id: &str, mtime: f64) -> SessionRef {
    let mut hint = Map::new();
    hint.insert("source_id".to_string(), Value::from(source_id));
    hint.insert("schema".to_string(), Value::from(SCHEMA));
    hint.insert("kind".to_string(), Value::from("history-plugin"));
    SessionRef {
        provider: CUSTOM_PROVIDER.to_string(),
        project_slug: plan.project_slug.clone(),
        session_id: plan.store_session_id.clone(),
        file_path: synthetic_path(source_id),
        file_mtime: mtime,
        file_size: u64::try_from(plan.records.len()).unwrap_or(u64::MAX),
        source_kind: SourceKind::Database,
        source_hint: Some(hint),
    }
}

// ── in-memory adapter shim ───────────────────────────────────────────────────

/// A minimal [`SourceAdapter`] fed from an already-validated in-memory stream
/// (`_StreamAdapter`).
///
/// It exists only so the import path can reuse the shared transactional writer
/// (which pulls records through `adapter.read(ref)`) instead of re-implementing
/// message partitioning, id assignment and idempotent upsert. It is **never
/// registered**.
///
/// ## The one contract deviation, ported deliberately
///
/// Every other adapter treats `since_offset` as an exclusive watermark
/// (`seq > since_offset`). This one is **inclusive** (`seq >= since_offset`),
/// because the importer always calls it with `since_offset = 0` and relies on
/// the writer's `INSERT OR IGNORE` for idempotency rather than on the
/// watermark. Ported as written; the conformance harness is therefore not run
/// against it, since invariant 6 ("a resumed read yields strictly fewer
/// records") is the one thing this shim does not promise.
#[derive(Debug, Clone, Default)]
pub struct StreamAdapter {
    by_session: Vec<(String, Vec<Record>)>,
}

impl StreamAdapter {
    /// Build a shim over one session's records.
    #[must_use]
    pub fn new(by_session: Vec<(String, Vec<Record>)>) -> Self {
        Self { by_session }
    }

    /// Build a shim over one plan, as the importer does per session.
    #[must_use]
    pub fn for_plan(plan: &SessionPlan) -> Self {
        Self::new(vec![(plan.store_session_id.clone(), plan.records.clone())])
    }
}

impl SourceAdapter for StreamAdapter {
    fn name(&self) -> &str {
        CUSTOM_PROVIDER
    }

    /// Always empty: custom imports run only through the explicit CLI command.
    fn enumerate(&self) -> Vec<SessionRef> {
        Vec::new()
    }

    fn read_into(&self, session: &SessionRef, since_offset: i64, sink: &mut dyn FnMut(Record)) {
        let Some((_, records)) = self
            .by_session
            .iter()
            .find(|(sid, _)| *sid == session.session_id)
        else {
            return;
        };
        for record in records {
            // `>=`, not `>` — see the type's docs.
            if record.seq >= since_offset {
                sink(record.clone());
            }
        }
    }
}

/// The outcome of one import run (`ImportResult`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportResult {
    /// The source id from the manifest.
    pub source_id: String,
    /// Always [`CUSTOM_PROVIDER`].
    pub provider: String,
    /// Distinct project slugs written, sorted.
    pub projects: Vec<String>,
    /// Sessions the stream described.
    pub sessions_seen: usize,
    /// Rows the writer actually inserted.
    pub messages_ingested: i64,
    /// File-touch lines in the stream.
    pub file_touches_seen: usize,
    /// Messages plus file touches validated.
    pub records_validated: usize,
    /// The cursor before the run.
    pub cursor_before: Option<String>,
    /// The cursor after the run.
    pub cursor_after: Option<String>,
    /// Whether the cursor moved.
    pub cursor_advanced: bool,
}

impl ImportResult {
    /// The summary an import produces, given its plans and the stream that made
    /// them.
    ///
    /// The `messages_ingested` count is the writer's (an `after - before` delta
    /// over `SELECT COUNT(*) FROM messages`), so it is a parameter here rather
    /// than something this module can compute.
    #[must_use]
    pub fn build(
        source_id: &str,
        plans: &[SessionPlan],
        stream: &ParsedStream,
        messages_ingested: i64,
        cursor_before: Option<String>,
    ) -> Self {
        let mut projects: Vec<String> =
            plans.iter().map(|plan| plan.project_slug.clone()).collect();
        projects.sort();
        projects.dedup();
        // The cursor advances last, only after every row has committed. A
        // stream carrying no cursor record leaves it exactly as it was.
        let (cursor_after, cursor_advanced) = match &stream.next_cursor {
            Some(next) => (Some(next.clone()), Some(next) != cursor_before.as_ref()),
            None => (cursor_before.clone(), false),
        };
        Self {
            source_id: source_id.to_string(),
            provider: CUSTOM_PROVIDER.to_string(),
            projects,
            sessions_seen: plans.len(),
            messages_ingested,
            file_touches_seen: stream.file_touches.len(),
            records_validated: stream.messages.len() + stream.file_touches.len(),
            cursor_before,
            cursor_after,
            cursor_advanced,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn message(session_id: &str, seq: i64, role: &str, content: &str) -> MessageRecord {
        MessageRecord {
            session_id: session_id.to_string(),
            seq,
            timestamp: "2026-04-25T14:00:00+00:00".to_string(),
            role: role.to_string(),
            content: content.to_string(),
            model: Some("gpt-5".to_string()),
            input_tokens: 10,
            output_tokens: 20,
            cache_read_tokens: 1,
            cache_creation_tokens: 2,
            tools: vec!["Bash".to_string()],
            cwd: None,
            raw: json!({"type": "message", "seq": seq}),
        }
    }

    fn touch(session_id: &str, seq: i64, operation: &str, path: &str) -> FileTouchRecord {
        FileTouchRecord {
            session_id: session_id.to_string(),
            seq,
            path: path.to_string(),
            operation: operation.to_string(),
            timestamp: "2026-04-25T14:00:01+00:00".to_string(),
            raw: json!({"type": "file_touch", "seq": seq}),
        }
    }

    struct Scratch(PathBuf);

    impl Scratch {
        fn new(tag: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "stax-custom-import-{tag}-{}-{:?}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |delta| delta.subsec_nanos())
            ));
            std::fs::create_dir_all(&path).expect("scratch");
            Self(path)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn ids_are_content_addressed_and_reproducible() {
        let record = message_to_record(&message("s", 0, "user", "hi"), "src", "src:s", None);
        let again = message_to_record(&message("s", 0, "user", "hi"), "src", "src:s", None);
        assert_eq!(record.uuid, again.uuid, "the same content is the same id");
        assert!(record.uuid.starts_with(ID_PREFIX));
        assert_eq!(record.uuid.len(), ID_PREFIX.len() + CONTENT_HASH_LENGTH);
        // Different content, different id.
        let other = message_to_record(&message("s", 0, "user", "ho"), "src", "src:s", None);
        assert_ne!(record.uuid, other.uuid);
        // A missing model hashes as "", not as the None sentinel.
        let mut model_less = message("s", 0, "user", "hi");
        model_less.model = None;
        let mut empty_model = message("s", 0, "user", "hi");
        empty_model.model = Some(String::new());
        assert_eq!(
            message_to_record(&model_less, "src", "src:s", None).uuid,
            message_to_record(&empty_model, "src", "src:s", None).uuid
        );
    }

    #[test]
    fn a_file_touch_becomes_a_searchable_assistant_record() {
        let record = file_touch_to_record(
            &touch("s", 3, "Write", "/a/b.py"),
            "src",
            "src:s",
            Some("/a"),
        );
        assert_eq!(record.role, "assistant");
        assert_eq!(record.model, None);
        // The operation maps case-insensitively; the path is in content_text so
        // find_sessions_touching_file can see it.
        assert_eq!(record.tools, vec!["Write"]);
        assert_eq!(record.content_text, "Write /a/b.py");
        assert_eq!(record.cwd.as_deref(), Some("/a"));
        // An unrecognised operation falls back to Edit.
        let odd = file_touch_to_record(&touch("s", 4, "frobnicate", "/x"), "src", "src:s", None);
        assert_eq!(odd.tools, vec![DEFAULT_TOUCH_TOOL]);
    }

    #[test]
    fn the_composed_ids_match_the_python_importer_exactly() {
        // Hashing the right bytes is only half the contract; hashing them in
        // the right *order*, with the right parts, is the other half. Both
        // literals below come from `stackunderflow.adapters.custom_import`'s
        // own `content_hash_id` call, run under the campaign's interpreter —
        // an id minted here must be the id an existing store already holds, or
        // a re-import duplicates every row it was designed to deduplicate.
        let record = message_to_record(&message("s", 0, "user", "hi"), "src", "src:s", None);
        assert_eq!(record.uuid, "c-fedd172e308e54df612d3e60fc332a4c");
        let touched =
            file_touch_to_record(&touch("s", 3, "Write", "/a/b.py"), "src", "src:s", None);
        assert_eq!(touched.uuid, "c-960b9ec4da0351228cdd881ac91d9c52");
    }

    #[test]
    fn project_slugs_are_namespaced_and_sanitised() {
        assert_eq!(project_slug("src", None), "src");
        assert_eq!(project_slug("src", Some("")), "src");
        assert_eq!(project_slug("src", Some("app")), "src--app");
        // Everything outside [alnum]._- becomes a dash, then dashes are
        // stripped from both ends.
        assert_eq!(
            project_slug("src", Some("/Users/me/app")),
            "src--Users-me-app"
        );
        assert_eq!(project_slug("src", Some("///")), "src");
        assert_eq!(project_slug("src", Some("a b.c_d-e")), "src--a-b.c_d-e");
    }

    #[test]
    fn planning_groups_by_session_and_orders_by_seq() {
        let stream = ParsedStream {
            sessions: vec![StreamSession {
                session_id: "s1".into(),
                project: Some("app".into()),
                cwd: Some("/work".into()),
                title: None,
                first_timestamp: None,
                last_timestamp: None,
                raw: json!({}),
            }],
            messages: vec![
                message("s1", 2, "assistant", "b"),
                message("s2", 0, "user", "orphan"),
            ],
            file_touches: vec![touch("s1", 1, "read", "/f")],
            next_cursor: Some("cursor-2".into()),
        };
        let plans = plan_sessions(&stream, "src");
        // The declared session comes first, then the one only a message named.
        assert_eq!(plans.len(), 2);
        assert_eq!(plans[0].store_session_id, "src:s1");
        assert_eq!(plans[0].project_slug, "src--app");
        assert_eq!(plans[1].store_session_id, "src:s2");
        // A session with no `session` line has no project and no cwd.
        assert_eq!(plans[1].project_slug, "src");
        // Records are seq-ordered, so the touch at seq 1 precedes the message
        // at seq 2 even though messages are collected first.
        assert_eq!(
            plans[0].records.iter().map(|r| r.seq).collect::<Vec<_>>(),
            vec![1, 2]
        );
        // The session cwd is inherited by both record kinds.
        assert!(
            plans[0]
                .records
                .iter()
                .all(|r| r.cwd.as_deref() == Some("/work"))
        );

        let result = ImportResult::build("src", &plans, &stream, 3, Some("cursor-1".into()));
        assert_eq!(result.projects, vec!["src", "src--app"]);
        assert_eq!(result.sessions_seen, 2);
        assert_eq!(result.file_touches_seen, 1);
        assert_eq!(result.records_validated, 3);
        assert_eq!(result.cursor_after.as_deref(), Some("cursor-2"));
        assert!(result.cursor_advanced);
        // A stream with no cursor record leaves the cursor untouched.
        let no_cursor = ParsedStream {
            next_cursor: None,
            ..stream.clone()
        };
        let result = ImportResult::build("src", &plans, &no_cursor, 0, Some("cursor-1".into()));
        assert_eq!(result.cursor_after.as_deref(), Some("cursor-1"));
        assert!(!result.cursor_advanced);
    }

    #[test]
    fn the_stream_shim_resumes_inclusively_unlike_every_other_adapter() {
        let records: Vec<Record> = (0..3)
            .map(|seq| message_to_record(&message("s", seq, "user", "x"), "src", "src:s", None))
            .collect();
        let adapter = StreamAdapter::new(vec![("src:s".to_string(), records)]);
        let plan = SessionPlan {
            store_session_id: "src:s".into(),
            project_slug: "src".into(),
            cwd: None,
            records: Vec::new(),
        };
        let session = session_ref(&plan, "src", 1.5);
        assert_eq!(adapter.read(&session, 0).len(), 3);
        // `>=` keeps the record AT the watermark — the deviation this shim
        // carries on purpose.
        assert_eq!(adapter.read(&session, 1).len(), 2);
        assert_eq!(adapter.read(&session, 3).len(), 0);
        assert!(adapter.enumerate().is_empty());
        assert!(adapter.watch_paths().is_empty());
        // An unknown session id reads as empty, not as a panic.
        let mut other = session.clone();
        other.session_id = "nope".into();
        assert!(adapter.read(&other, 0).is_empty());
    }

    #[test]
    fn the_synthetic_ref_carries_the_plugin_provenance() {
        let plan = SessionPlan {
            store_session_id: "src:s".into(),
            project_slug: "src--app".into(),
            cwd: Some("/work".into()),
            records: vec![message_to_record(
                &message("s", 0, "user", "x"),
                "src",
                "src:s",
                None,
            )],
        };
        let session = session_ref(&plan, "src", 1.5);
        assert_eq!(session.provider, CUSTOM_PROVIDER);
        assert_eq!(session.file_path, Path::new("custom-history:src"));
        assert_eq!(session.source_kind, SourceKind::Database);
        assert_eq!(session.file_size, 1, "file_size is the record count");
        let hint = session.source_hint.expect("hint");
        assert_eq!(hint.get("schema").and_then(Value::as_str), Some(SCHEMA));
        assert_eq!(
            hint.get("kind").and_then(Value::as_str),
            Some("history-plugin")
        );
    }

    #[test]
    fn source_ids_are_filename_safe_and_traversal_proof() {
        assert!(is_safe_source_id("amp"));
        assert!(is_safe_source_id("my-source_1.0"));
        assert!(!is_safe_source_id(""));
        assert!(!is_safe_source_id("."));
        assert!(!is_safe_source_id(".."));
        assert!(!is_safe_source_id("a/b"));
        assert!(!is_safe_source_id("a\\b"));
        assert!(!is_safe_source_id(&"a".repeat(SOURCE_ID_MAX_LEN + 1)));
        assert!(cursor_path(Path::new("/state"), "../etc").is_err());
        assert_eq!(
            cursor_path(Path::new("/state"), "amp").expect("safe"),
            Path::new("/state/history_sources/amp.cursor.json")
        );
    }

    #[test]
    fn the_cursor_sidecar_round_trips_and_tolerates_corruption() {
        let scratch = Scratch::new("cursor");
        assert_eq!(load_cursor(&scratch.0, "amp"), None, "no sidecar yet");
        store_cursor(&scratch.0, "amp", "page-2", 1_745_596_800.5).expect("stored");
        assert_eq!(load_cursor(&scratch.0, "amp").as_deref(), Some("page-2"));

        let path = cursor_path(&scratch.0, "amp").expect("path");
        let body = std::fs::read_to_string(&path).expect("read");
        // Byte-for-byte `json.dumps(payload, indent=2)`, float repr included —
        // the sidecar is read back by the Python importer on a mixed-binary
        // machine, so its shape is a contract and not a detail.
        assert_eq!(
            body,
            concat!(
                "{\n",
                "  \"schema\": \"stackunderflow-history-jsonl-v1\",\n",
                "  \"source_id\": \"amp\",\n",
                "  \"cursor\": \"page-2\",\n",
                "  \"updated_at\": 1745596800.5\n",
                "}"
            )
        );
        // The temp file is renamed away, never left behind.
        assert!(!crate::walk::with_suffix(&path, ".json.tmp").exists());

        // A corrupt sidecar is "start fresh", not a failure.
        std::fs::write(&path, "{not json").expect("write");
        assert_eq!(load_cursor(&scratch.0, "amp"), None);
        std::fs::write(&path, r#"{"cursor": 7}"#).expect("write");
        assert_eq!(load_cursor(&scratch.0, "amp"), None);
    }

    #[test]
    fn manifest_resolution_walks_file_then_dir_then_search_roots() {
        let scratch = Scratch::new("manifest");
        let roots = vec![scratch.0.join("plugins")];
        // Nothing anywhere: the error names every path it tried.
        let err = resolve_manifest_path("amp", &roots, None).expect_err("missing");
        assert!(
            err.starts_with("no history-source manifest for 'amp'."),
            "{err}"
        );
        assert!(
            err.contains("plugins/amp/stackunderflow-history-plugin.json"),
            "{err}"
        );

        // A named source under a search root.
        let nested = roots[0].join("amp");
        std::fs::create_dir_all(&nested).expect("mkdir");
        let manifest = nested.join(MANIFEST_FILENAME);
        std::fs::write(&manifest, "{}").expect("write");
        assert_eq!(
            resolve_manifest_path("amp", &roots, None).expect("found"),
            manifest
        );
        // A directory holding the canonical filename.
        let direct = nested.to_string_lossy().into_owned();
        assert_eq!(
            resolve_manifest_path(&direct, &[], None).expect("found"),
            manifest
        );
        // An explicit file path.
        assert_eq!(
            resolve_manifest_path(&manifest.to_string_lossy(), &[], None).expect("found"),
            manifest
        );
        // With no search roots the message says so rather than trailing off.
        let err = resolve_manifest_path("nope", &[], None).expect_err("missing");
        assert!(err.ends_with("(no search roots)"), "{err}");

        // DIV-457: the candidate is `str(Path(name))`, and pathlib normalises.
        // `--history-source ''` prints `.`, not nothing; every row below is
        // transcribed from `str(Path(s))` under the campaign's interpreter.
        let err = resolve_manifest_path("", &[], None).expect_err("missing");
        assert!(err.contains("Looked for a file/dir at ., then:"), "{err}");
        for (input, expected) in [
            ("", "."),
            (".", "."),
            ("./a", "a"),
            ("a//b", "a/b"),
            ("/a/", "/a"),
            ("//a", "//a"),
            ("///a", "/a"),
            ("../a", "../a"),
            ("a/.", "a"),
            ("./", "."),
            ("a/./b/", "a/b"),
            ("x/..", "x/.."),
        ] {
            assert_eq!(py_path_str(Path::new(input)), expected, "input {input:?}");
        }
    }
}
