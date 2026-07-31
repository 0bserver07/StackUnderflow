//! Cursor Agent — the port of `stackunderflow/adapters/cursor_agent.py`.
//!
//! The CLI writes transcripts under
//! `~/.cursor/projects/{project}/agent-transcripts/`, in **two formats that
//! share one adapter**:
//!
//! * **Legacy text** — one `.txt` per session, flat in `agent-transcripts/`,
//!   marked up with `user:` / `A:` / `[Thinking]` / `[Tool call]` /
//!   `[Tool result]` lines. Runs of lines group into turns and each *assistant*
//!   turn becomes one record.
//! * **Composer 2 JSONL** — `agent-transcripts/{uuid}/*.jsonl`, one
//!   `{role, message: {content: [{type, text?, name?}]}}` object per line, one
//!   record per assistant message.
//!
//! Detection is by extension, decided once at `enumerate()` time and carried in
//! the ref's `source_hint` as `{"format": "jsonl" | "text"}` so `read()` never
//! re-sniffs.
//!
//! ## Three things a naive port gets wrong
//!
//! 1. **Tokens are estimated, and the estimate is over *characters*.**
//!    `len(text) // 4` on a Python `str` counts characters, not bytes — CJK
//!    prose would estimate 3× high on `str::len`. Every record carries
//!    `raw["cost_source"] = "estimated"` so the cost layer can down-weight it.
//! 2. **`timestamp` is `datetime.now(tz=UTC)`, per record.** The source records
//!    no per-message time at all. That value cannot be diffed between two
//!    processes, so the clock is injected ([`pytime::Clock`]) and the parity
//!    harness excludes the field explicitly rather than pretending to compare
//!    it — see the note on [`CursorAgentAdapter::with_clock`].
//! 3. **The JSONL reader does not seek.** It reads from byte 0 even on a resumed
//!    read, because it has to rebuild `last_user_text` (the rolling prompt that
//!    supplies `input_tokens`) from the lines below the watermark. Handing it a
//!    seeked iterator would silently zero the input estimate of the first
//!    resumed turn — and would also apply `_streaming`'s 128 MB cap, which this
//!    adapter deliberately does not have on either side.
//!
//! ## The model comes from a side-car database
//!
//! `~/.cursor/ai-tracking/ai-code-tracking.db` holds
//! `conversation_summaries(conversationId, model, updatedAt)`, consulted once
//! per session. A missing file, a missing table, a corrupt database, or simply
//! no row: every one of them falls back to the literal `cursor-agent` and keeps
//! going ([`CursorAgentAdapter::lookup_model`]). It is opened through
//! [`crate::sqlite::open_readonly`], which carries the recorded (and
//! in-this-port's-favour) divergence about `file:` URI interpolation.

use std::io::BufReader;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::base::{Record, SessionRef, SourceAdapter, Speed, home_dir, stat_ref_fields};
use crate::{blocks, jsonl, pytime, sqlite, walk};

/// The provider key. Hyphenated, unlike the module name — it is a store column
/// value and a `capabilities.json` key, so it is spelled exactly as Python
/// spells it.
pub const NAME: &str = "cursor-agent";

/// The transcripts root (`_DEFAULT_PROJECTS_ROOT`), relative to the home
/// directory.
pub const PROJECTS_ROOT_RELATIVE: &str = ".cursor/projects";

/// The attribution database (`_DEFAULT_TRACKING_DB`), relative to the home
/// directory.
pub const TRACKING_DB_RELATIVE: &str = ".cursor/ai-tracking/ai-code-tracking.db";

/// The directory each project keeps its transcripts in.
pub const TRANSCRIPTS_DIR: &str = "agent-transcripts";

/// The model stamped when the attribution database has nothing to say.
pub const DEFAULT_MODEL: &str = "cursor-agent";

/// The tool-call block type the Composer 2 format uses.
pub const TOOL_BLOCK_TYPES: [&str; 1] = ["tool_use"];

/// The Cursor Agent source adapter (`CursorAgentAdapter`).
#[derive(Debug, Clone)]
pub struct CursorAgentAdapter {
    projects_root: PathBuf,
    tracking_db: PathBuf,
    clock: pytime::Clock,
}

impl Default for CursorAgentAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl CursorAgentAdapter {
    /// Both paths defaulted off the home directory.
    #[must_use]
    pub fn new() -> Self {
        Self::with_optional_roots(None, None)
    }

    /// Inject both paths — `CursorAgentAdapter(projects_root=…, tracking_db=…)`.
    #[must_use]
    pub fn with_roots(projects_root: impl Into<PathBuf>, tracking_db: impl Into<PathBuf>) -> Self {
        Self::with_optional_roots(Some(projects_root.into()), Some(tracking_db.into()))
    }

    /// The Python constructor exactly: each path falls back to its home default
    /// independently, so injecting one and not the other still reads the user's
    /// real tree for the other. Tests and the parity harness inject both.
    #[must_use]
    pub fn with_optional_roots(
        projects_root: Option<PathBuf>,
        tracking_db: Option<PathBuf>,
    ) -> Self {
        let home = home_dir().unwrap_or_default();
        Self {
            projects_root: projects_root.unwrap_or_else(|| home.join(PROJECTS_ROOT_RELATIVE)),
            tracking_db: tracking_db.unwrap_or_else(|| home.join(TRACKING_DB_RELATIVE)),
            clock: pytime::Clock::Live,
        }
    }

    /// Pin the clock behind every record's `timestamp`.
    ///
    /// **PARITY EXCLUSION (not a divergence).** Both implementations call *now*;
    /// the source has no per-message time, so Python stamps
    /// `datetime.now(tz=UTC).isoformat()` on each record as it is built. Two
    /// processes never agree on that microsecond, which is why `timestamp` is
    /// the one field both parity harnesses replace with `<now>`
    /// (`--blank-timestamps`) instead of faking agreement on a wall clock. Every
    /// other field of every record is still diffed byte for byte, and the clock
    /// itself is pinned by `an_injected_clock_pins_the_now_fallback` below.
    ///
    /// Injection rather than a global: Rust 2024 makes `std::env::set_var`
    /// `unsafe` and the workspace forbids `unsafe`.
    #[must_use]
    pub fn with_clock(mut self, clock: pytime::Clock) -> Self {
        self.clock = clock;
        self
    }

    /// The transcripts root this adapter walks.
    #[must_use]
    pub fn projects_root(&self) -> &Path {
        &self.projects_root
    }

    /// The attribution database this adapter consults.
    #[must_use]
    pub fn tracking_db(&self) -> &Path {
        &self.tracking_db
    }

    /// The model recorded for `session_id`, or `None` (`_lookup_model`).
    ///
    /// Optional by construction. Missing file, unopenable database, missing
    /// `conversation_summaries` table, wrong columns, no row, or a NULL/empty
    /// model: all of them are `None`, and the caller substitutes
    /// [`DEFAULT_MODEL`].
    ///
    /// `str(model) if model else None` is Python's, warts included — a row whose
    /// `model` column holds the integer `7` yields the model `"7"`.
    #[must_use]
    pub fn lookup_model(&self, session_id: &str) -> Option<String> {
        if !self.tracking_db.is_file() {
            return None;
        }
        // LOG: python warns "Cannot open Cursor Agent tracking DB %s".
        let conn = sqlite::open_readonly(&self.tracking_db)?;
        // LOG: python debug-logs "conversation_summaries lookup failed".
        let mut statement = conn
            .prepare(
                "SELECT model FROM conversation_summaries \
                 WHERE conversationId = ? LIMIT 1",
            )
            .ok()?;
        let mut rows = statement.query([session_id]).ok()?;
        let row = rows.next().ok()??;
        let value = row.get_ref(0).ok()?;
        // `if model` — Python truthiness over `sqlite3`'s value mapping: NULL,
        // 0, 0.0, "" and b"" are all falsy and mean "no model recorded".
        if !truthy_column(value) {
            return None;
        }
        Some(sqlite::value_to_py_str(value))
    }
}

/// Python's `bool(v)` for a column value as `sqlite3` hands it over.
fn truthy_column(value: rusqlite::types::ValueRef<'_>) -> bool {
    use rusqlite::types::ValueRef;
    match value {
        ValueRef::Null => false,
        ValueRef::Integer(number) => number != 0,
        ValueRef::Real(number) => number != 0.0,
        ValueRef::Text(bytes) | ValueRef::Blob(bytes) => !bytes.is_empty(),
    }
}

impl SourceAdapter for CursorAgentAdapter {
    fn name(&self) -> &str {
        NAME
    }

    fn enumerate(&self) -> Vec<SessionRef> {
        let root = &self.projects_root;
        if !root.is_dir() {
            return Vec::new();
        }
        let mut out = Vec::new();
        for project_dir in walk::child_dirs(root) {
            let transcripts_dir = project_dir.join(TRANSCRIPTS_DIR);
            if !transcripts_dir.is_dir() {
                continue;
            }
            let project_slug = prettify_project_name(&walk::dir_name(&project_dir));

            // Legacy text transcripts: flat `.txt` files.
            for path in walk::glob_suffix(&transcripts_dir, ".txt") {
                if let Some(session) = self.build_ref(&path, &project_slug, None) {
                    out.push(session);
                }
            }

            // Composer 2 JSONL: one subdirectory per session, holding one or
            // more `.jsonl` files. There can be several per session directory,
            // so this yields one ref per *file* with the subdir UUID as the
            // session id — the two refs then share a session and differ by path.
            for sub in walk::child_dirs(&transcripts_dir) {
                for path in walk::glob_suffix(&sub, ".jsonl") {
                    if let Some(session) = self.build_ref(&path, &project_slug, Some(&sub)) {
                        out.push(session);
                    }
                }
            }
        }
        out
    }

    fn read_into(&self, session: &SessionRef, since_offset: i64, sink: &mut dyn FnMut(Record)) {
        let path = &session.file_path;
        if !path.is_file() {
            // LOG: python warns "Cursor Agent transcript missing at read time".
            return;
        }
        // One database round-trip per session, not per record.
        let model = self
            .lookup_model(&session.session_id)
            .unwrap_or_else(|| DEFAULT_MODEL.to_string());

        let jsonl_format = session
            .source_hint
            .as_ref()
            .and_then(|hint| hint.get("format"))
            .and_then(Value::as_str)
            == Some("jsonl");
        if jsonl_format {
            read_jsonl(path, session, &model, since_offset, self.clock, sink);
        } else {
            read_text(path, session, &model, since_offset, self.clock, sink);
        }
    }

    /// The transcripts root **and** the attribution database (`source_roots`).
    ///
    /// Two entries because the model attribution lives outside the transcript
    /// tree: a backup that copied only the transcripts would restore sessions
    /// whose model is `cursor-agent` for every turn. `watch_paths` is not
    /// declared on the Python side, so it stays empty here.
    fn source_roots(&self) -> Vec<PathBuf> {
        vec![self.projects_root.clone(), self.tracking_db.clone()]
    }
}

impl CursorAgentAdapter {
    /// One transcript path → a `SessionRef` (`_build_ref`).
    fn build_ref(
        &self,
        path: &Path,
        project_slug: &str,
        session_dir: Option<&Path>,
    ) -> Option<SessionRef> {
        // Python warns and returns None on a stat failure.
        let (mtime, size) = stat_ref_fields(path)?;
        let format = if suffix_lower(path) == ".jsonl" {
            "jsonl"
        } else {
            "text"
        };
        let mut hint = Map::new();
        hint.insert("format".to_string(), Value::from(format));
        let mut session = SessionRef::file(
            NAME,
            project_slug,
            session_id_for(path, session_dir),
            path,
            mtime,
            size,
        );
        session.source_hint = Some(hint);
        Some(session)
    }
}

// ── format readers ───────────────────────────────────────────────────────────

/// Composer 2 JSONL: one record per assistant message (`_read_jsonl`).
///
/// Reads from byte 0 regardless of the watermark — see the module note. Lines at
/// or below the watermark are parsed only far enough to keep `last_user_text`
/// current, so the first resumed assistant turn still attaches the right prompt.
fn read_jsonl(
    path: &Path,
    session: &SessionRef,
    model: &str,
    since_offset: i64,
    clock: pytime::Clock,
    sink: &mut dyn FnMut(Record),
) {
    // LOG: python warns "Cursor Agent JSONL read failed on %s".
    let Ok(file) = std::fs::File::open(path) else {
        return;
    };
    let mut reader = BufReader::new(file);
    let mut last_user_text = String::new();
    let mut offset = 0_i64;

    loop {
        let mut raw_line = Vec::new();
        match read_until_newline(&mut reader, &mut raw_line) {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
        let line_offset = offset;
        offset += i64::try_from(raw_line.len()).unwrap_or(i64::MAX);

        // `if since_offset and line_offset <= since_offset` — a *negative*
        // watermark is truthy in Python and no line offset is ever below it, so
        // the comparison, not the truthiness, is what decides.
        if since_offset != 0 && line_offset <= since_offset {
            if let Some(parsed) = safe_jsonl_loads(&raw_line)
                && parsed.get("role").and_then(Value::as_str) == Some("user")
            {
                let text = jsonl_message_text(&parsed);
                if !text.is_empty() {
                    last_user_text = text;
                }
            }
            continue;
        }

        let Some(parsed) = safe_jsonl_loads(&raw_line) else {
            continue;
        };
        // A non-string role is schema drift, not a user turn: skip the line.
        let Some(role) = parsed.get("role").and_then(Value::as_str) else {
            continue;
        };
        let text = jsonl_message_text(&parsed);
        if role == "user" {
            // `last_user_text = text or last_user_text` — an empty prompt does
            // not clear the running one.
            if !text.is_empty() {
                last_user_text = text;
            }
            continue;
        }
        if role != "assistant" {
            continue;
        }

        let tools = jsonl_message_tools(&parsed);
        // An assistant turn with no text of its own is attributed the prompt it
        // answered, so the record is never blank in the UI.
        let content_text = if text.is_empty() {
            last_user_text.clone()
        } else {
            text.clone()
        };
        let mut raw = parsed;
        raw.insert("cost_source".to_string(), Value::from("estimated"));

        sink(Record {
            provider: NAME.to_string(),
            session_id: session.session_id.clone(),
            seq: line_offset,
            timestamp: clock.now_iso(),
            role: "assistant".to_string(),
            model: Some(model.to_string()),
            input_tokens: estimate_tokens(&last_user_text),
            output_tokens: estimate_tokens(&text),
            cache_create_tokens: 0,
            cache_read_tokens: 0,
            content_text,
            tools,
            cwd: None,
            is_sidechain: false,
            uuid: format!("{}:{line_offset}", session.session_id),
            parent_uuid: None,
            raw: Value::Object(raw),
            speed: Speed::Standard,
        });
    }
}

/// The legacy marker-line format: one record per assistant turn (`_read_text`).
///
/// A turn opens at a `user:` or `A:` line and runs until the next one; the
/// record's `seq` is the byte offset of the opening marker. `[Thinking]` and
/// `[Tool result]` lines join the current assistant turn's text, `[Tool call]`
/// contributes a tool name, and any other non-empty line is a continuation of
/// whichever turn is open.
fn read_text(
    path: &Path,
    session: &SessionRef,
    model: &str,
    since_offset: i64,
    clock: pytime::Clock,
    sink: &mut dyn FnMut(Record),
) {
    // LOG: python warns "Cursor Agent text read failed on %s".
    let Ok(raw) = std::fs::read(path) else {
        return;
    };

    let mut last_user_text = String::new();
    let mut current_role: Option<&'static str> = None;
    let mut current_offset: Option<i64> = None;
    let mut current_text: Vec<String> = Vec::new();
    let mut current_tools: Vec<String> = Vec::new();
    let mut offset = 0_i64;

    for line_bytes in jsonl::splitlines_keepends(&raw) {
        let line_offset = offset;
        offset += i64::try_from(line_bytes.len()).unwrap_or(i64::MAX);
        // `errors="replace"` cannot raise, so Python's UnicodeDecodeError guard
        // here is unreachable on both sides.
        let decoded = String::from_utf8_lossy(line_bytes);
        let line = decoded.trim_end_matches(['\r', '\n']);

        match classify_text_line(line) {
            Some("user") => {
                if current_role == Some("assistant")
                    && past_resume_floor(since_offset, current_offset)
                    && let Some(record) = build_text_record(
                        session,
                        model,
                        clock,
                        current_role,
                        current_offset,
                        &current_text,
                        &current_tools,
                        &last_user_text,
                    )
                {
                    sink(record);
                }
                current_role = Some("user");
                current_offset = Some(line_offset);
                current_text = vec![py_strip(strip_prefix(line, "user:")).to_string()];
                current_tools = Vec::new();
            }
            Some("assistant") => {
                if current_role == Some("user") {
                    last_user_text = py_strip(&current_text.join("\n")).to_string();
                } else if current_role == Some("assistant")
                    && past_resume_floor(since_offset, current_offset)
                    && let Some(record) = build_text_record(
                        session,
                        model,
                        clock,
                        current_role,
                        current_offset,
                        &current_text,
                        &current_tools,
                        &last_user_text,
                    )
                {
                    sink(record);
                }
                current_role = Some("assistant");
                current_offset = Some(line_offset);
                current_text = vec![py_strip(strip_prefix(line, "A:")).to_string()];
                current_tools = Vec::new();
            }
            Some("thinking" | "tool_result") => {
                if current_role == Some("assistant") {
                    current_text.push(line.to_string());
                }
            }
            Some("tool_call") => {
                if let Some(tool) = parse_tool_call_name(line)
                    && current_role == Some("assistant")
                {
                    current_tools.push(tool);
                }
            }
            _ => {
                // A continuation line joins whichever turn is open; a blank one
                // is dropped (`if current_role is not None and line`).
                if current_role.is_some() && !line.is_empty() {
                    current_text.push(line.to_string());
                }
            }
        }
    }

    // Flush the trailing turn.
    if current_role == Some("assistant")
        && past_resume_floor(since_offset, current_offset)
        && let Some(record) = build_text_record(
            session,
            model,
            clock,
            current_role,
            current_offset,
            &current_text,
            &current_tools,
            &last_user_text,
        )
    {
        sink(record);
    }
}

/// Whether a buffered turn is past the resume watermark.
///
/// **Ported bug-for-bug.** Python writes `(current_offset or -1) > since_offset`,
/// and `0 or -1` is `-1` — so an assistant turn that opens at byte **0** is
/// treated as "before the floor" on every resumed read and never re-emitted,
/// even though a turn at offset 0 with a watermark of 0 would be. A file whose
/// very first line is `A:` is the only shape that reaches it.
fn past_resume_floor(since_offset: i64, current_offset: Option<i64>) -> bool {
    since_offset == 0 || or_minus_one(current_offset) > since_offset
}

/// `current_offset or -1`.
const fn or_minus_one(current_offset: Option<i64>) -> i64 {
    match current_offset {
        Some(value) if value != 0 => value,
        _ => -1,
    }
}

/// One buffered assistant turn → a `Record` (`_emit`).
#[allow(
    clippy::too_many_arguments,
    reason = "the Python original is a closure over eight locals; passing them \
    explicitly is what makes the emit points readable in Rust, where a closure \
    borrowing them would fight the loop's mutation"
)]
fn build_text_record(
    session: &SessionRef,
    model: &str,
    clock: pytime::Clock,
    current_role: Option<&str>,
    current_offset: Option<i64>,
    current_text: &[String],
    current_tools: &[String],
    last_user_text: &str,
) -> Option<Record> {
    if current_role != Some("assistant") {
        return None;
    }
    let seq = current_offset?;
    let text = py_strip(&current_text.join("\n")).to_string();
    // The fixed two-key envelope Python builds inline; key order is the
    // literal's, and `preserve_order` keeps it through `raw_json`.
    let mut raw = Map::new();
    raw.insert("format".to_string(), Value::from("text"));
    raw.insert("cost_source".to_string(), Value::from("estimated"));

    Some(Record {
        provider: NAME.to_string(),
        session_id: session.session_id.clone(),
        seq,
        timestamp: clock.now_iso(),
        role: "assistant".to_string(),
        model: Some(model.to_string()),
        input_tokens: estimate_tokens(last_user_text),
        output_tokens: estimate_tokens(&text),
        cache_create_tokens: 0,
        cache_read_tokens: 0,
        content_text: text,
        tools: current_tools.to_vec(),
        cwd: None,
        is_sidechain: false,
        uuid: format!("{}:{seq}", session.session_id),
        parent_uuid: None,
        raw: Value::Object(raw),
        speed: Speed::Standard,
    })
}

// ── helpers ──────────────────────────────────────────────────────────────────

/// `max(len(text) // 4, 0)` — the length estimate, over **characters**.
///
/// Python's `len()` counts characters, not bytes: a transcript of CJK prose
/// estimates 3× high on `str::len`. The `max(_, 0)` is Python's and is
/// unreachable — a length is never negative — but it is what the source says.
#[must_use]
pub fn estimate_tokens(text: &str) -> i64 {
    i64::try_from(text.chars().count() / 4).unwrap_or(i64::MAX)
}

/// `BufRead::read_until` without making the caller import the trait.
fn read_until_newline(
    reader: &mut BufReader<std::fs::File>,
    out: &mut Vec<u8>,
) -> std::io::Result<usize> {
    use std::io::BufRead;
    reader.read_until(b'\n', out)
}

/// Parse one JSONL line, or `None` (`_safe_jsonl_loads`).
///
/// The bytes are decoded with `errors="replace"` **before** parsing, exactly as
/// Python does here — so an invalid UTF-8 byte inside a JSON string becomes
/// `U+FFFD` and the line still parses, where a bytes-level parse would reject
/// the whole line. A document that is not an object is discarded: every caller
/// indexes it as a mapping.
///
/// **DIVERGENCE (recorded).** Three, all of them properties of the two parsers
/// rather than of this adapter; the measurement and the cross-cutting scope are
/// written up once on [`crate::hermes`], because `cursor_agent.py` is one of the
/// 18 stdlib-`json` adapters this port parses with orjson's rules:
///
/// * nesting 1025–9997 deep parses in Python and is dropped (and counted) here;
/// * `NaN` / `Infinity` / `-Infinity` / `1e999` parse in Python and drop the
///   whole line here;
/// * `String::from_utf8_lossy` and CPython's `errors="replace"` both emit one
///   `U+FFFD` per maximal invalid subpart, so they agree on every sequence
///   either has been seen to produce — but they are two implementations of a
///   recommendation, not one, and only a malformed byte *inside* a JSON string
///   could ever show a difference.
fn safe_jsonl_loads(line: &[u8]) -> Option<Map<String, Value>> {
    if line.is_empty() {
        return None;
    }
    let text = String::from_utf8_lossy(line);
    match jsonl::parse_json(text.as_bytes()) {
        Some(Value::Object(map)) => Some(map),
        _ => None,
    }
}

/// The text of a Composer 2 message (`_jsonl_message_text`).
///
/// **Not** [`crate::blocks::message_text`], and the difference is load-bearing
/// in both directions: this one *requires* `type == "text"` on a block, and it
/// appends an empty `text` (costing a newline) where the shared helper drops it.
#[must_use]
pub fn jsonl_message_text(parsed: &Map<String, Value>) -> String {
    let Some(message) = parsed.get("message").and_then(Value::as_object) else {
        return String::new();
    };
    let Some(content) = message.get("content") else {
        return String::new();
    };
    if let Some(text) = content.as_str() {
        return text.to_string();
    }
    let Some(items) = content.as_array() else {
        return String::new();
    };
    let mut pieces: Vec<&str> = Vec::new();
    for block in items {
        if let Some(map) = block.as_object() {
            if map.get("type").and_then(Value::as_str) == Some("text")
                && let Some(text) = map.get("text").and_then(Value::as_str)
            {
                pieces.push(text);
            }
        } else if let Some(text) = block.as_str() {
            pieces.push(text);
        }
    }
    pieces.join("\n")
}

/// Tool names from a Composer 2 message (`_jsonl_message_tools`).
#[must_use]
pub fn jsonl_message_tools(parsed: &Map<String, Value>) -> Vec<String> {
    let Some(message) = parsed.get("message").and_then(Value::as_object) else {
        return Vec::new();
    };
    blocks::tool_names(message.get("content"), &TOOL_BLOCK_TYPES)
}

/// Classify one legacy-format line (`_classify_text_line`).
#[must_use]
pub fn classify_text_line(line: &str) -> Option<&'static str> {
    if line.starts_with("user:") {
        return Some("user");
    }
    if line.starts_with("A:") {
        return Some("assistant");
    }
    if line.starts_with("[Thinking]") {
        return Some("thinking");
    }
    if line.starts_with("[Tool call]") {
        return Some("tool_call");
    }
    if line.starts_with("[Tool result]") {
        return Some("tool_result");
    }
    None
}

/// The tool name on a `[Tool call]` line (`_parse_tool_call_name`).
///
/// Accepts both `[Tool call] name` and `[Tool call] name args=…`: the first
/// whitespace-separated token wins, splitting on Python's whitespace set.
#[must_use]
pub fn parse_tool_call_name(line: &str) -> Option<String> {
    let rest = py_strip(strip_prefix(line, "[Tool call]"));
    if rest.is_empty() {
        return None;
    }
    let name = rest.split(py_is_space).next().unwrap_or("");
    (!name.is_empty()).then(|| name.to_string())
}

/// `str.removeprefix` — the prefix is dropped only when it is there.
fn strip_prefix<'a>(line: &'a str, prefix: &str) -> &'a str {
    line.strip_prefix(prefix).unwrap_or(line)
}

/// Python's `str.strip()` (no argument): both ends, Python's whitespace set.
///
/// `char::is_whitespace` is the Unicode `White_Space` property;
/// `str.isspace()` adds the four C0 separators `\x1c`–`\x1f`, which is the only
/// difference and the only reason this is not `str::trim`.
#[must_use]
pub fn py_strip(text: &str) -> &str {
    text.trim_matches(py_is_space)
}

/// Whether `ch` is whitespace to Python's `str.isspace()`.
fn py_is_space(ch: char) -> bool {
    ch.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&ch)
}

/// `Path.suffix`, lowercased — `""` when the name has none.
///
/// `pathlib` does not treat a leading dot as a suffix boundary, so `.jsonl` has
/// no suffix at all; `Path::extension` agrees, but returns the text without the
/// dot, and the comparison this feeds is against `".jsonl"`.
fn suffix_lower(path: &Path) -> String {
    let name = walk::dir_name(path);
    match name.rfind('.') {
        Some(index) if index > 0 => name[index..].to_lowercase(),
        _ => String::new(),
    }
}

/// A session id for a transcript path (`_session_id_for`).
///
/// The Composer 2 session directory's name wins when it is UUID-shaped, then the
/// file stem when *it* is, and finally a SHA-1 of the absolute path — which is
/// what keeps two identically-named legacy transcripts in different projects
/// from colliding.
#[must_use]
pub fn session_id_for(path: &Path, session_dir: Option<&Path>) -> String {
    if let Some(dir) = session_dir {
        let name = walk::dir_name(dir);
        if is_uuid_shaped(&name) {
            return name;
        }
    }
    let stem = walk::file_stem(path);
    if is_uuid_shaped(&stem) {
        return stem;
    }
    sha1_hex(path.to_string_lossy().as_bytes())
}

/// `_UUID_RE.match(value)` — the canonical 8-4-4-4-12 hex shape.
///
/// `re.match` anchors at the start and the pattern ends in `$`, which in
/// non-`MULTILINE` mode also matches immediately before a trailing newline —
/// hence the strip. Nothing else is accepted: no braces, no `urn:uuid:`, no
/// case-folding beyond hex's own.
#[must_use]
pub fn is_uuid_shaped(value: &str) -> bool {
    let value = value.strip_suffix('\n').unwrap_or(value);
    let groups = [8_usize, 4, 4, 4, 12];
    let mut parts = value.split('-');
    for expected in groups {
        let Some(part) = parts.next() else {
            return false;
        };
        if part.len() != expected || !part.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return false;
        }
    }
    parts.next().is_none()
}

/// Prettify a raw project-directory name (`_prettify_project_name`).
///
/// Strips leading path separators (`^[-_/]+`) and a trailing ISO-ish timestamp,
/// so `-Users-yad-myproj-2025-04-01T10-30-00` becomes `Users-yad-myproj`. The
/// result is a slug, not a path — and an empty result falls back to the original
/// name, which is why a directory named only `--` keeps its name.
#[must_use]
pub fn prettify_project_name(name: &str) -> String {
    let stripped = name.trim_start_matches(['-', '_', '/']);
    let out = strip_timestamp_tail(stripped);
    if out.is_empty() {
        name.to_string()
    } else {
        out
    }
}

/// `_TIMESTAMP_TAIL_RE.sub("", name)` — hand-matched rather than regex-driven.
///
/// The pattern is `[-_]?\d{4}-?\d{2}-?\d{2}[Tt _]?\d{2}.*$`, and it is small
/// enough that a matcher is cheaper than a regex crate in the shared lock. The
/// two optional hyphens and the optional separator are enumerated rather than
/// backtracked; `.` does not match a newline and `$` matches before a trailing
/// one, so a name that ends in `\n` keeps it.
fn strip_timestamp_tail(name: &str) -> String {
    let bytes = name.as_bytes();
    for start in 0..=bytes.len() {
        if let Some(end) = timestamp_tail_at(bytes, start) {
            let mut out = String::with_capacity(bytes.len());
            out.push_str(&name[..start]);
            out.push_str(&name[end..]);
            return out;
        }
    }
    name.to_string()
}

/// The end offset of the timestamp pattern matched at `start`, or `None`.
fn timestamp_tail_at(bytes: &[u8], start: usize) -> Option<usize> {
    // `[-_]?` is greedy, so the "present" alternative is tried first; either
    // way the match *starts* at `start`, which is what the caller cuts on.
    if matches!(bytes.get(start), Some(b'-' | b'_'))
        && let Some(end) = date_time_at(bytes, start + 1)
    {
        return Some(end);
    }
    date_time_at(bytes, start)
}

/// `\d{4}-?\d{2}-?\d{2}[Tt _]?\d{2}.*$` anchored at `start`.
fn date_time_at(bytes: &[u8], start: usize) -> Option<usize> {
    if !digits_at(bytes, start, 4) {
        return None;
    }
    for first_dash in [true, false] {
        for second_dash in [true, false] {
            let mut index = start + 4;
            if first_dash {
                if bytes.get(index) != Some(&b'-') {
                    continue;
                }
                index += 1;
            }
            if !digits_at(bytes, index, 2) {
                continue;
            }
            index += 2;
            if second_dash {
                if bytes.get(index) != Some(&b'-') {
                    continue;
                }
                index += 1;
            }
            if !digits_at(bytes, index, 2) {
                continue;
            }
            index += 2;
            for separator in [true, false] {
                let mut tail = index;
                if separator {
                    if !matches!(bytes.get(tail), Some(b'T' | b't' | b' ' | b'_')) {
                        continue;
                    }
                    tail += 1;
                }
                if !digits_at(bytes, tail, 2) {
                    continue;
                }
                tail += 2;
                if let Some(end) = dot_star_dollar(bytes, tail) {
                    return Some(end);
                }
            }
        }
    }
    None
}

/// `.*$` from `start`: everything to the end, stopping before a final newline.
fn dot_star_dollar(bytes: &[u8], start: usize) -> Option<usize> {
    let rest = bytes.get(start..)?;
    let body = rest.strip_suffix(b"\n").unwrap_or(rest);
    if body.contains(&b'\n') {
        return None;
    }
    Some(start + body.len())
}

/// Whether `count` ASCII digits start at `index`.
fn digits_at(bytes: &[u8], index: usize, count: usize) -> bool {
    bytes
        .get(index..index + count)
        .is_some_and(|slice| slice.iter().all(u8::is_ascii_digit))
}

/// `hashlib.sha1(data).hexdigest()`.
///
/// Hand-rolled rather than pulled from crates.io on purpose: it is FIPS 180-4
/// in forty lines, it is pinned against CPython's own output by the tests below,
/// and this crate's manifest already records why a shared `Cargo.lock` in a
/// many-agent campaign is worth keeping small. Not a security primitive here —
/// it is the identity of a transcript path, and a colliding session id would
/// need two paths chosen adversarially by the user's own filesystem.
#[must_use]
pub fn sha1_hex(data: &[u8]) -> String {
    let mut state: [u32; 5] = [
        0x6745_2301,
        0xEFCD_AB89,
        0x98BA_DCFE,
        0x1032_5476,
        0xC3D2_E1F0,
    ];
    let mut message = data.to_vec();
    let bit_length = (data.len() as u64).wrapping_mul(8);
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend_from_slice(&bit_length.to_be_bytes());

    for chunk in message.chunks_exact(64) {
        let mut schedule = [0_u32; 80];
        for (index, word) in schedule.iter_mut().take(16).enumerate() {
            let start = index * 4;
            *word = u32::from_be_bytes([
                chunk[start],
                chunk[start + 1],
                chunk[start + 2],
                chunk[start + 3],
            ]);
        }
        for index in 16..80 {
            schedule[index] = (schedule[index - 3]
                ^ schedule[index - 8]
                ^ schedule[index - 14]
                ^ schedule[index - 16])
                .rotate_left(1);
        }

        let [mut a, mut b, mut c, mut d, mut e] = state;
        for (index, word) in schedule.iter().enumerate() {
            let (mixed, constant) = match index {
                0..=19 => ((b & c) | ((!b) & d), 0x5A82_7999_u32),
                20..=39 => (b ^ c ^ d, 0x6ED9_EBA1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1B_BCDC),
                _ => (b ^ c ^ d, 0xCA62_C1D6),
            };
            let next = a
                .rotate_left(5)
                .wrapping_add(mixed)
                .wrapping_add(e)
                .wrapping_add(constant)
                .wrapping_add(*word);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = next;
        }
        for (slot, value) in state.iter_mut().zip([a, b, c, d, e]) {
            *slot = slot.wrapping_add(value);
        }
    }

    state.iter().map(|word| format!("{word:08x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn sha1_matches_cpythons_hashlib() {
        // Every literal is `hashlib.sha1(b"…").hexdigest()` under the
        // campaign's interpreter — the point of a hand-rolled digest is that it
        // agrees with the one it replaces, so the values are independent.
        assert_eq!(sha1_hex(b""), "da39a3ee5e6b4b0d3255bfef95601890afd80709");
        assert_eq!(sha1_hex(b"abc"), "a9993e364706816aba3e25717850c26c9cd0d89d");
        assert_eq!(
            sha1_hex(b"/Users/yad/.cursor/projects/proj/agent-transcripts/legacy.txt"),
            "cb08b99167738e0a65fe4fda7431b2197d337fb8"
        );
        // The block boundary: 55, 56 and 64 bytes exercise all three padding
        // paths (fits, spills into a second block, exact multiple).
        assert_eq!(
            sha1_hex(&b"a".repeat(55)),
            "c1c8bbdc22796e28c0e15163d20899b65621d65a"
        );
        assert_eq!(
            sha1_hex(&b"a".repeat(56)),
            "c2db330f6083854c99d4b5bfb6e8f29f201be699"
        );
        assert_eq!(
            sha1_hex(&b"a".repeat(64)),
            "0098ba824b5c16427bd7a1122a5a442a25ec644d"
        );
    }

    #[test]
    fn uuid_shapes_are_the_regex_and_nothing_else() {
        assert!(is_uuid_shaped("11111111-2222-3333-4444-555555555555"));
        assert!(is_uuid_shaped("AAAAAAAA-bbbb-CCCC-dddd-EEEEEEEEEEEE"));
        // `$` matches before a trailing newline, so `re.match` accepts this.
        assert!(is_uuid_shaped("11111111-2222-3333-4444-555555555555\n"));
        assert!(!is_uuid_shaped("11111111-2222-3333-4444-55555555555"));
        assert!(!is_uuid_shaped("11111111-2222-3333-4444-5555555555555"));
        assert!(!is_uuid_shaped("{11111111-2222-3333-4444-555555555555}"));
        assert!(!is_uuid_shaped("11111111222233334444555555555555"));
        assert!(!is_uuid_shaped("gggggggg-2222-3333-4444-555555555555"));
        assert!(!is_uuid_shaped(""));
        assert!(!is_uuid_shaped("session"));
    }

    #[test]
    fn project_names_lose_their_prefix_and_their_timestamp() {
        assert_eq!(
            prettify_project_name("-Users-yad-myproj-2025-04-01T10-30-00"),
            "Users-yad-myproj"
        );
        // Basic-format dates and every optional piece of the pattern.
        assert_eq!(prettify_project_name("proj_20250401T1030"), "proj");
        assert_eq!(prettify_project_name("proj 2025-04-01 10:30"), "proj ");
        assert_eq!(prettify_project_name("proj2025040110"), "proj");
        // Nothing timestamp-shaped: only the leading separators go.
        assert_eq!(prettify_project_name("_/-plain-name"), "plain-name");
        assert_eq!(prettify_project_name("myproj"), "myproj");
        // A four-digit run that is not a date is left alone.
        assert_eq!(prettify_project_name("release-2024"), "release-2024");
        // An empty result falls back to the original.
        assert_eq!(prettify_project_name("---"), "---");
        assert_eq!(prettify_project_name("2025-04-01T10"), "2025-04-01T10");
        assert_eq!(prettify_project_name(""), "");
    }

    #[test]
    fn text_lines_classify_and_tool_names_take_the_first_token() {
        assert_eq!(classify_text_line("user: hi"), Some("user"));
        assert_eq!(classify_text_line("A: hello"), Some("assistant"));
        assert_eq!(classify_text_line("[Thinking] hmm"), Some("thinking"));
        assert_eq!(classify_text_line("[Tool call] Read"), Some("tool_call"));
        assert_eq!(classify_text_line("[Tool result] ok"), Some("tool_result"));
        assert_eq!(classify_text_line("plain continuation"), None);
        assert_eq!(classify_text_line(""), None);
        // The marker match is a prefix, not a word: `A:` inside a line is not
        // a turn boundary, but `Answering:` starts with neither marker.
        assert_eq!(classify_text_line("say A: to start"), None);

        assert_eq!(
            parse_tool_call_name("[Tool call] Read path=foo.py"),
            Some("Read".to_string())
        );
        assert_eq!(
            parse_tool_call_name("[Tool call]   Bash"),
            Some("Bash".to_string())
        );
        assert_eq!(parse_tool_call_name("[Tool call]"), None);
        assert_eq!(parse_tool_call_name("[Tool call]    "), None);
    }

    #[test]
    fn composer_text_requires_the_type_key_and_keeps_empty_pieces() {
        let parsed = json!({"message": {"content": [
            {"type": "text", "text": "a"},
            {"type": "text", "text": ""},
            {"type": "tool_use", "name": "Read", "text": "not counted"},
            {"text": "no type"},
            "bare",
            7,
        ]}});
        let map = parsed.as_object().expect("object").clone();
        // "a", "" and "bare" — the empty piece costs a newline here, unlike the
        // shared `blocks::message_text`.
        assert_eq!(jsonl_message_text(&map), "a\n\nbare");
        assert_eq!(jsonl_message_tools(&map), vec!["Read".to_string()]);

        let plain = json!({"message": {"content": "just a string"}});
        assert_eq!(
            jsonl_message_text(plain.as_object().expect("object")),
            "just a string"
        );
        let neither = json!({"message": {"content": 42}});
        assert_eq!(jsonl_message_text(neither.as_object().expect("object")), "");
        let no_message = json!({"role": "assistant"});
        assert_eq!(
            jsonl_message_text(no_message.as_object().expect("object")),
            ""
        );
        assert!(jsonl_message_tools(no_message.as_object().expect("object")).is_empty());
    }

    #[test]
    fn tokens_are_estimated_over_characters_not_bytes() {
        assert_eq!(estimate_tokens("abcdefgh"), 2);
        assert_eq!(estimate_tokens("abc"), 0);
        assert_eq!(estimate_tokens(""), 0);
        // Eight CJK characters are 24 bytes and 2 estimated tokens.
        assert_eq!(estimate_tokens("你好世界你好世界"), 2);
    }

    #[test]
    fn the_zero_offset_resume_quirk_is_preserved() {
        // `(current_offset or -1)`: a turn opening at byte 0 reads as -1.
        assert!(
            past_resume_floor(0, Some(0)),
            "a fresh read emits everything"
        );
        assert!(!past_resume_floor(5, Some(0)));
        assert!(!past_resume_floor(5, Some(5)));
        assert!(past_resume_floor(5, Some(6)));
        assert!(!past_resume_floor(5, None));
    }

    #[test]
    fn strip_is_pythons_whitespace_set() {
        assert_eq!(py_strip("  a b \n"), "a b");
        assert_eq!(
            py_strip("\u{1c}x\u{1f}"),
            "x",
            "the C0 separators strip too"
        );
        assert_eq!(py_strip(""), "");
        assert_eq!(py_strip("   "), "");
    }

    #[test]
    fn suffixes_follow_pathlib_not_the_first_dot() {
        assert_eq!(suffix_lower(Path::new("/a/session.JSONL")), ".jsonl");
        assert_eq!(suffix_lower(Path::new("/a/foo.bar.txt")), ".txt");
        assert_eq!(suffix_lower(Path::new("/a/.jsonl")), "");
        assert_eq!(suffix_lower(Path::new("/a/plain")), "");
    }

    #[test]
    fn an_absent_projects_root_enumerates_empty_rather_than_failing() {
        let adapter = CursorAgentAdapter::with_roots(
            "/nonexistent/stax/cursor-projects",
            "/nonexistent/stax/tracking.db",
        );
        assert!(adapter.enumerate().is_empty());
        assert_eq!(adapter.name(), NAME);
        // A missing database is "no model recorded", not an error.
        assert_eq!(adapter.lookup_model("any"), None);
        // `source_roots` is declared and `watch_paths` is not.
        assert_eq!(adapter.source_roots().len(), 2);
        assert!(adapter.watch_paths().is_empty());
    }

    #[test]
    fn the_defaults_are_the_two_home_paths() {
        let adapter = CursorAgentAdapter::new();
        assert!(adapter.projects_root().ends_with("projects"));
        assert!(adapter.tracking_db().ends_with("ai-code-tracking.db"));
    }

    #[test]
    fn an_injected_clock_pins_the_now_fallback() {
        use std::time::{Duration, UNIX_EPOCH};
        // The one field the parity harness cannot diff, pinned here instead.
        let dir = std::env::temp_dir().join(format!("stax-cursor-agent-{}", std::process::id()));
        let transcripts = dir.join("projects/proj/agent-transcripts");
        std::fs::create_dir_all(&transcripts).expect("scratch");
        std::fs::write(transcripts.join("legacy.txt"), "user: hi\nA: hello there\n")
            .expect("transcript");

        let adapter = CursorAgentAdapter::with_roots(dir.join("projects"), dir.join("no.db"))
            .with_clock(pytime::Clock::Fixed(
                UNIX_EPOCH + Duration::new(1_745_596_801, 123_456_000),
            ));
        let refs = adapter.enumerate();
        assert_eq!(refs.len(), 1);
        let records = adapter.read(&refs[0], 0);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].timestamp, "2025-04-25T16:00:01.123456+00:00");
        assert_eq!(records[0].model.as_deref(), Some(DEFAULT_MODEL));
        assert_eq!(records[0].content_text, "hello there");
        // "hi" is 2 characters, "hello there" is 11.
        assert_eq!(records[0].input_tokens, 0);
        assert_eq!(records[0].output_tokens, 2);
        // A non-UUID stem falls back to the path digest.
        assert_eq!(
            refs[0].session_id,
            sha1_hex(transcripts.join("legacy.txt").to_string_lossy().as_bytes())
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
