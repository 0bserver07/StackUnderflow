//! `services/playback.py` (882 ln) — the tool-call event extractor.
//!
//! One session (or one project) becomes a **sequence of state-changing
//! events**, one row per `tool_use` block, so the dashboard's Playback tab can
//! scrub a timeline. Everything is read from data the store already holds:
//!
//! * `messages.raw_json` — the transcript envelope, whose `message.content[]`
//!   carries `type == "tool_use"` (the call, on an assistant row) and
//!   `type == "tool_result"` (the result, on the following user row);
//! * `messages.timestamp` — the ordering key, and the other half of the
//!   per-tool duration;
//! * `captured_events` — the spec-05 hooks table, **if it exists**, for an
//!   authoritative failure flag.
//!
//! # Defensive parsing is the contract, not a courtesy
//!
//! Python's `_loads` swallows `JSONDecodeError` / `TypeError` / `ValueError`
//! and every consumer treats the miss as "no events". That is ported literally:
//! a `raw_json` that will not parse contributes nothing and never raises. The
//! consequence is that an *acceptance* difference between `json.loads` and
//! `serde_json` is silent rather than loud — **DIV-109's family**. Three known
//! gaps, none reachable from a Claude Code / Codex transcript:
//!
//! 1. CPython's decoder accepts the bare `NaN` / `Infinity` / `-Infinity`
//!    literals; `serde_json` rejects them, so an envelope carrying one loses
//!    its whole `tool_use` list here and keeps it in Python.
//! 2. `serde_json` caps container nesting at 128; CPython's limit is the
//!    interpreter recursion limit (~1000). A `raw_json` nested deeper than 128
//!    parses there and not here.
//! 3. An integer wider than 64 bits parses exactly in Python and widens to
//!    `f64` here (the workspace manifest has no `arbitrary_precision`), which
//!    can change a `payload_excerpt`'s rendered digits.
//!
//! # Accumulation and slicing
//!
//! `byte_count` is `len(text.encode("utf-8", errors="replace"))` — **bytes**.
//! `payload_excerpt`'s 200-char cap and every `[:60]` / `[:80]` label slice are
//! **code points** (`crate::pyops::char_prefix`). Getting those two backwards
//! is the classic silent divergence on the first non-ASCII tool argument, and
//! both directions appear within four lines of each other in the reference.

use std::collections::HashMap;

use rusqlite::Connection;
use serde_json::{Map, Value};
use stax_etl::stats::pydatetime::{PyDateTime, parse_ts};
use stax_etl::stats::pytext::{py_repr, py_str, py_strip, py_truthy};

use crate::pyops::char_prefix;
use crate::services::mart_queries::table_exists;

/// `_EXCERPT_CHARS` — the spec's "200-char excerpt".
const EXCERPT_CHARS: usize = 200;

/// `_PATH_INPUT_KEYS` — first match wins, in this order.
const PATH_INPUT_KEYS: [&str; 5] = [
    "file_path",
    "filePath",
    "notebook_path",
    "notebookPath",
    "path",
];

/// `_WRITE_CONTENT_KEYS`.
const WRITE_CONTENT_KEYS: [&str; 3] = ["content", "new_string", "new_str"];

/// `_WRITE_TOOLS` — the tools whose `byte_count` may come from the *input*.
const WRITE_TOOLS: [&str; 4] = ["Write", "Edit", "MultiEdit", "NotebookEdit"];

/// `_CAPTURED_ANCHOR_BACK_S` / `_FWD_S` — the PostToolUse anchoring window.
const CAPTURED_ANCHOR_BACK_S: f64 = 90.0;
const CAPTURED_ANCHOR_FWD_S: f64 = 2.0;

// ── the row shape both public entries read ───────────────────────────────────

/// One `messages` row, as `SELECT id, session_fk, seq, timestamp, role,
/// raw_json` hands it over.
///
/// `id` and `timestamp` stay `Option` because the reference tests both
/// (`if r["id"] is not None`, `r["timestamp"] if r["timestamp"] else None`) and
/// a partitioned-view read is not obliged to honour the base table's
/// constraints.
#[derive(Debug, Clone)]
pub struct MessageRow {
    /// `messages.id`.
    pub id: Option<i64>,
    /// `messages.session_fk`.
    pub session_fk: i64,
    /// `messages.timestamp` — an ISO-8601 string, or NULL.
    pub timestamp: Option<String>,
    /// `messages.role`.
    pub role: String,
    /// `messages.raw_json` — the transcript envelope.
    pub raw_json: Option<String>,
}

/// `SELECT id, session_fk, seq, timestamp, role, raw_json FROM messages …`.
///
/// `seq` is selected by the reference and never read: it is the `ORDER BY` key
/// and nothing else, so it is not carried on [`MessageRow`].
///
/// # Errors
/// Any SQLite failure.
pub fn read_rows(
    conn: &Connection,
    sql: &str,
    params: &[&dyn rusqlite::ToSql],
) -> rusqlite::Result<Vec<MessageRow>> {
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map(params, |row| {
        Ok(MessageRow {
            id: row.get(0)?,
            session_fk: row.get::<_, Option<i64>>(1)?.unwrap_or(0),
            timestamp: row.get(3)?,
            role: row.get::<_, Option<String>>(4)?.unwrap_or_default(),
            raw_json: row.get(5)?,
        })
    })?;
    rows.collect()
}

// ── the event ────────────────────────────────────────────────────────────────

/// `PlaybackEvent` — one tool call, the unit the scrubber steps through.
///
/// `seq` is the 0-based index within the **full** (pre-filter) stream, so a
/// filtered view's positions still line up with the unfiltered timeline.
#[derive(Debug, Clone, PartialEq)]
pub struct PlaybackEvent {
    /// 0-based index over every tool call in the stream, filter or no filter.
    pub seq: i64,
    /// The issuing message's timestamp, `str(… or "")`.
    pub ts: String,
    /// `messages.id`, or `0` when the row had none.
    pub message_id: i64,
    /// The tool name, or `"?"` for the unparseable marker.
    pub tool_name: String,
    /// The one-line human label — [`summarize_tool_call`].
    pub summary: String,
    /// The first path-ish argument, if any.
    pub target_path: Option<String>,
    /// UTF-8 **byte** length of the result text (or of what was written).
    pub byte_count: Option<i64>,
    /// `captured_events` first, then the transcript's `is_error`, else unknown.
    pub success: Option<bool>,
    /// `int((result_ts - call_ts).total_seconds() * 1000)`, when non-negative.
    pub duration_ms: Option<i64>,
    /// The blended input⇒result excerpt, capped at 200 code points.
    pub payload_excerpt: String,
    /// Which session this event belongs to — essential for the project stream.
    pub session_id: String,
}

/// `playback_event_to_dict(e)` — `dataclasses.asdict`, so the key order is the
/// **field declaration order**, not alphabetical and not the payload's.
#[must_use]
pub fn playback_event_to_dict(event: &PlaybackEvent) -> Value {
    let mut obj = Map::new();
    obj.insert("seq".to_owned(), Value::from(event.seq));
    obj.insert("ts".to_owned(), Value::from(event.ts.clone()));
    obj.insert("message_id".to_owned(), Value::from(event.message_id));
    obj.insert("tool_name".to_owned(), Value::from(event.tool_name.clone()));
    obj.insert("summary".to_owned(), Value::from(event.summary.clone()));
    obj.insert(
        "target_path".to_owned(),
        event.target_path.clone().map_or(Value::Null, Value::from),
    );
    obj.insert(
        "byte_count".to_owned(),
        event.byte_count.map_or(Value::Null, Value::from),
    );
    obj.insert(
        "success".to_owned(),
        event.success.map_or(Value::Null, Value::from),
    );
    obj.insert(
        "duration_ms".to_owned(),
        event.duration_ms.map_or(Value::Null, Value::from),
    );
    obj.insert(
        "payload_excerpt".to_owned(),
        Value::from(event.payload_excerpt.clone()),
    );
    obj.insert(
        "session_id".to_owned(),
        Value::from(event.session_id.clone()),
    );
    Value::Object(obj)
}

// ── defensive JSON helpers (shared with `playback_fs`) ───────────────────────

/// `_loads` — `json.loads`, with every failure swallowed to `None`.
///
/// `if not blob:` is Python truthiness, so an empty string never reaches the
/// decoder. See the module docs for the acceptance gap this hides.
#[must_use]
pub fn loads(blob: Option<&str>) -> Option<Value> {
    let blob = blob.filter(|text| !text.is_empty())?;
    serde_json::from_str(blob).ok()
}

/// `_envelope` — the top-level transcript object, or `{}`.
#[must_use]
pub fn envelope(raw_json: Option<&str>) -> Map<String, Value> {
    match loads(raw_json) {
        Some(Value::Object(map)) => map,
        _ => Map::new(),
    }
}

/// `_content_blocks` — `envelope["message"]["content"]` when both are the right
/// shape, else `[]`.
#[must_use]
pub fn content_blocks(envelope: &Map<String, Value>) -> &[Value] {
    match envelope.get("message") {
        Some(Value::Object(msg)) => match msg.get("content") {
            Some(Value::Array(items)) => items.as_slice(),
            _ => &[],
        },
        _ => &[],
    }
}

/// `_stringify_result_content` — a `tool_result` block's `content` as text.
///
/// The dict fallback is `json.dumps(content, default=str)` with **every
/// default**: `ensure_ascii=True` and the `(", ", ": ")` separators. That is
/// `pyjson::dumps_py_default`, NOT the HTTP writer — using `dumps_http` here
/// would drop the `\uXXXX` escapes and change every excerpt carrying a
/// non-ASCII tool result.
#[must_use]
pub fn stringify_result_content(content: Option<&Value>) -> String {
    // A missing key and an explicit `null` are the same `None` in Python.
    let Some(content) = content else {
        return String::new();
    };
    match content {
        Value::Null => String::new(),
        Value::String(text) => text.clone(),
        Value::Array(items) => {
            let mut parts: Vec<String> = Vec::new();
            for block in items {
                match block {
                    Value::Object(map) => {
                        // `blk.get("type") == "text" and isinstance(blk.get("text"), str)`.
                        if map.get("type").and_then(Value::as_str) == Some("text")
                            && let Some(Value::String(text)) = map.get("text")
                        {
                            parts.push(text.clone());
                        }
                    }
                    Value::String(text) => parts.push(text.clone()),
                    // Image blocks and anything else are skipped outright.
                    _ => {}
                }
            }
            parts.join("\n")
        }
        Value::Object(map) => {
            if let Some(Value::String(text)) = map.get("text") {
                return text.clone();
            }
            stax_memory::pyjson::dumps_py_default(&Value::Object(map.clone()))
        }
        // A bare number or bool: `str(content)`.
        other => py_str(other),
    }
}

// ── tool-result index ────────────────────────────────────────────────────────

/// `_ResultInfo` — a `tool_result` reduced to what an event needs.
#[derive(Debug, Clone)]
struct ResultInfo {
    text: String,
    is_error: Option<bool>,
    ts: Option<String>,
}

/// `_index_results` — every `tool_use_id` → its result.
///
/// The Claude Code `toolUseResult` field on the same user message supplies the
/// error flag **only** when the block itself carries no boolean `is_error`; the
/// block's `content` is always the canonical text.
fn index_results(rows: &[MessageRow]) -> HashMap<String, ResultInfo> {
    let mut out: HashMap<String, ResultInfo> = HashMap::new();
    for row in rows {
        if row.role != "user" {
            continue;
        }
        let env = envelope(row.raw_json.as_deref());
        // `for k in ("is_error", "isError")` — the FIRST key holding a real
        // bool wins; a non-bool under `is_error` does not stop the loop.
        let mut tur_is_error: Option<bool> = None;
        if let Some(Value::Object(tur)) = env.get("toolUseResult") {
            for key in ["is_error", "isError"] {
                if let Some(Value::Bool(flag)) = tur.get(key) {
                    tur_is_error = Some(*flag);
                    break;
                }
            }
        }
        for block in content_blocks(&env) {
            let Value::Object(map) = block else { continue };
            if map.get("type").and_then(Value::as_str) != Some("tool_result") {
                continue;
            }
            // `isinstance(tuid, str) and tuid` — a non-string or empty id is
            // unusable as a key here.
            let Some(tuid) = map
                .get("tool_use_id")
                .and_then(Value::as_str)
                .filter(|tuid| !tuid.is_empty())
            else {
                continue;
            };
            let is_error = match map.get("is_error") {
                Some(Value::Bool(flag)) => Some(*flag),
                _ => tur_is_error,
            };
            out.insert(
                tuid.to_owned(),
                ResultInfo {
                    text: stringify_result_content(map.get("content")),
                    is_error,
                    // `r["timestamp"] if r["timestamp"] else None` — an EMPTY
                    // timestamp is falsy and becomes `None`.
                    ts: row.timestamp.clone().filter(|ts| !ts.is_empty()),
                },
            );
        }
    }
    out
}

// ── captured_events (spec 05) — optional ─────────────────────────────────────

/// `_has_captured_events` — `sqlite_master WHERE type = 'table'`.
///
/// DIV-148: this is [`table_exists`], **not** `table_or_view_exists`. The
/// reference's guard says `type = 'table'`; widening it would be a different
/// question with a different answer on a store that shadowed the name.
fn has_captured_events(conn: &Connection) -> bool {
    table_exists(conn, "captured_events").unwrap_or(false)
}

/// `_captured_failure_message_ids` — the best-effort outcome overlay.
///
/// Only ever yields `false`: hooks record failures and corrections, never
/// positive confirmations, so a missing entry leaves `success` to the
/// transcript signal. Any unexpected table shape yields `{}` rather than
/// failing the request — `except sqlite3.Error: return {}`.
///
/// **Narrowing (recorded).** `(f_dt - m_dt)` raises `TypeError` in CPython when
/// exactly one of the two stamps carries a UTC offset, and that raise is
/// **not** caught — it would be a 500. `PyDateTime::sub_total_seconds` returns
/// `None` for the mixed case and this treats it as "no match", which serves a
/// 200 where the reference serves a 500. Both stamps come from ISO writers on
/// the same machine, so the mixed case has never been observed.
fn captured_failure_message_ids(
    conn: &Connection,
    session_id: &str,
    rows: &[MessageRow],
) -> HashMap<i64, bool> {
    let mut out: HashMap<i64, bool> = HashMap::new();
    if session_id.is_empty() || !has_captured_events(conn) {
        return out;
    }
    let Ok(mut stmt) = conn.prepare(
        "SELECT ts FROM captured_events \
         WHERE session_id = ? AND event_kind IN ('failure', 'correction')",
    ) else {
        return out;
    };
    let Ok(stamps) = stmt.query_map([session_id], |row| row.get::<_, Option<String>>(0)) else {
        return out;
    };
    let mut failure_ts: Vec<PyDateTime> = Vec::new();
    for stamp in stamps {
        // A row that will not read is `sqlite3.Error` — the whole overlay is
        // abandoned, not just this row.
        let Ok(stamp) = stamp else {
            return HashMap::new();
        };
        if let Some(parsed) = parse_iso(stamp.as_deref()) {
            failure_ts.push(parsed);
        }
    }
    if failure_ts.is_empty() {
        return out;
    }
    for row in rows {
        if row.role != "assistant" {
            continue;
        }
        let Some(id) = row.id else { continue };
        let Some(m_dt) = parse_iso(row.timestamp.as_deref()) else {
            continue;
        };
        for f_dt in &failure_ts {
            let Some(delta) = f_dt.sub_total_seconds(m_dt) else {
                continue;
            };
            if (-CAPTURED_ANCHOR_FWD_S..=CAPTURED_ANCHOR_BACK_S).contains(&delta) {
                out.insert(id, false);
                break;
            }
        }
    }
    out
}

// ── summary / excerpt formatting ─────────────────────────────────────────────

/// `_short_path` — trim an absolute path to its last two components.
fn short_path(path: &str) -> String {
    let norm = path.replace('\\', "/");
    // `.rstrip("/")` strips every trailing slash; the split then drops the
    // empty segments a `//` run leaves behind.
    let norm = norm.trim_end_matches('/');
    let parts: Vec<&str> = norm.split('/').filter(|part| !part.is_empty()).collect();
    if parts.len() <= 2 {
        // `"/".join(parts) if parts else path` — the ORIGINAL string comes
        // back when nothing survived, not the normalised one.
        if parts.is_empty() {
            return path.to_owned();
        }
        return parts.join("/");
    }
    parts[parts.len() - 2..].join("/")
}

/// `_first_command_word` — the first shell token, skipping plumbing.
///
/// **The `text.split()[0]` fallback cannot raise, and that was checked rather
/// than assumed.** It looks like an `IndexError` waiting to happen: it runs
/// only when `tokens` is empty, and `tokens = text.split()`. But `text` starts
/// as `cmd.strip()` and is only ever *reassigned* to `rest.strip()`, so it can
/// never be blank — and `str.split()` with no argument splits on exactly the
/// class `str.strip()` strips, so a non-blank `text` always yields at least one
/// token. Confirmed against the reference interpreter: `"cd /tmp &&  "` answers
/// `"cd"` (the peel loop breaks because `partition` leaves an empty `rest`),
/// and `"sudo time env"` answers `"sudo"` (every token skipped, so the fallback
/// fires and returns the first one). The return type is therefore a plain
/// `String`; the `except Exception` in `summarize_tool_call` is real but this
/// function is not one of the things that reaches it.
fn first_command_word(cmd: &str) -> String {
    let mut text = py_strip(cmd).to_owned();
    if text.is_empty() {
        return String::new();
    }
    // `for sep in ("&&", ";")`: peel leading `cd … &&` / `cd … ;` segments. The
    // inner `while True` runs to exhaustion for one separator before the next
    // is tried, which is why `"cd /a && cd /b; echo hi"` answers `echo`.
    for sep in ["&&", ";"] {
        while let Some(next) = peel_cd_segment(&text, sep) {
            text = next;
        }
    }
    let mut tokens: Vec<&str> = text.split_whitespace().collect();
    while let Some(first) = tokens.first().copied() {
        let is_prefix_word = matches!(first, "sudo" | "time" | "env" | "nice" | "nohup");
        // `"=" in t and not t.startswith("-") and "/" not in t.split("=")[0]`.
        let is_assignment = first.contains('=')
            && !first.starts_with('-')
            && !first.split('=').next().unwrap_or_default().contains('/');
        if !(is_prefix_word || is_assignment) {
            break;
        }
        tokens.remove(0);
    }
    match tokens.first() {
        Some(token) => (*token).to_owned(),
        // `tokens[0] if tokens else text.split()[0]` — reached when EVERY token
        // was a skippable prefix (`"sudo time env"` → `"sudo"`). See the doc
        // comment for why the `IndexError` this could raise is unreachable.
        None => text
            .split_whitespace()
            .next()
            .map_or_else(String::new, str::to_owned),
    }
}

/// One turn of `_first_command_word`'s `while True`, or `None` to break.
///
/// `head, _, rest = text.partition(sep)` then
/// `if rest and head.strip().split()[:1] == ["cd"]`. Note that `partition`
/// returns an EMPTY `rest` both when the separator is absent and when it is the
/// very last thing in the string — the two are indistinguishable here, and both
/// stop the peel.
fn peel_cd_segment(text: &str, sep: &str) -> Option<String> {
    let (head, rest) = text.split_once(sep)?;
    if rest.is_empty() {
        return None;
    }
    if py_strip(head).split_whitespace().next() != Some("cd") {
        return None;
    }
    Some(py_strip(rest).to_owned())
}

/// `_input_path` — the first `_PATH_INPUT_KEYS` entry holding a non-blank
/// string. The value returned is the **unstripped** original.
fn input_path(tool_input: &Map<String, Value>) -> Option<String> {
    for key in PATH_INPUT_KEYS {
        if let Some(Value::String(value)) = tool_input.get(key)
            && !py_strip(value).is_empty()
        {
            return Some(value.clone());
        }
    }
    None
}

/// `_mcp_label` — `mcp__github__create_pr` → `github.create_pr`.
fn mcp_label(tool_name: &str) -> String {
    let rest = &tool_name["mcp__".len()..];
    match rest.split_once("__") {
        Some((server, tool)) => format!("{server}.{tool}"),
        // `rest or tool_name` — a bare `mcp__` falls back to the whole name.
        None if rest.is_empty() => tool_name.to_owned(),
        None => rest.to_owned(),
    }
}

/// A `Map` value as a Python `str` when it is one, else `None`.
fn str_arg<'a>(input: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    match input.get(key) {
        Some(Value::String(text)) => Some(text.as_str()),
        _ => None,
    }
}

/// `summarize_tool_call` — the one-line label.
///
/// Table-driven over the tool names Claude Code emits; an unknown name falls
/// back to `"<Tool> <first path-ish arg>"`. `mcp__server__tool` collapses to
/// `server.tool`. A handler that raises yields the bare tool name — that
/// `except Exception` is load-bearing and several branches below reach it.
#[must_use]
pub fn summarize_tool_call(
    tool_name: &str,
    tool_input: Option<&Map<String, Value>>,
    tool_result_text: Option<&str>,
) -> String {
    // `if not isinstance(tool_name, str) or not tool_name` — over a decoded
    // envelope the caller has already checked the type, so only emptiness is
    // reachable from HTTP.
    if tool_name.is_empty() {
        return "(unparseable)".to_owned();
    }
    let empty = Map::new();
    let input = tool_input.unwrap_or(&empty);

    if tool_name.starts_with("mcp__") {
        return mcp_label(tool_name);
    }

    if let Some(handler) = summary_handler(tool_name) {
        // `except Exception: return name`.
        return handler(input, tool_result_text.unwrap_or_default())
            .unwrap_or_else(|| tool_name.to_owned());
    }

    if let Some(path) = input_path(input) {
        return format!("{tool_name} {}", short_path(&path));
    }
    for key in ["pattern", "query", "url", "command", "description"] {
        if let Some(value) = str_arg(input, key) {
            let stripped = py_strip(value);
            if stripped.is_empty() {
                continue;
            }
            // `v.strip().splitlines()[0]` — non-empty after the strip, so the
            // list always has a first element.
            let snippet = py_splitlines_first(stripped);
            return format!("{tool_name}: {}", char_prefix(snippet, 60));
        }
    }
    tool_name.to_owned()
}

/// `str.splitlines()[0]` for a non-empty string.
///
/// Python breaks on far more than `\n`: `\v`, `\f`, `\x1c`–`\x1e`, `\x85`,
/// `\u2028` and `\u2029` are all line boundaries. Only the first line is ever
/// wanted here, so this scans for the first boundary rather than splitting.
fn py_splitlines_first(text: &str) -> &str {
    let boundary = |c: char| {
        matches!(
            c,
            '\n' | '\r'
                | '\u{0b}'
                | '\u{0c}'
                | '\u{1c}'
                | '\u{1d}'
                | '\u{1e}'
                | '\u{85}'
                | '\u{2028}'
                | '\u{2029}'
        )
    };
    match text.find(boundary) {
        Some(index) => &text[..index],
        None => text,
    }
}

/// A summary handler: `(input, result_text) -> Option<label>`, where `None`
/// reproduces a raise.
type SummaryHandler = fn(&Map<String, Value>, &str) -> Option<String>;

/// `_SUMMARY_HANDLERS` — the dispatch table, by tool name.
fn summary_handler(name: &str) -> Option<SummaryHandler> {
    Some(match name {
        "Read" => |i: &Map<String, Value>, _r: &str| Some(file_op("Read", i)),
        "Write" => |i: &Map<String, Value>, _r: &str| Some(file_op("Write", i)),
        "Edit" => |i: &Map<String, Value>, _r: &str| Some(file_op("Edit", i)),
        "MultiEdit" => |i: &Map<String, Value>, _r: &str| Some(file_op("MultiEdit", i)),
        "NotebookRead" => |i: &Map<String, Value>, _r: &str| Some(file_op("NotebookRead", i)),
        "NotebookEdit" => sum_notebook_edit,
        "Bash" => sum_bash,
        "BashOutput" => |_i: &Map<String, Value>, _r: &str| Some("BashOutput".to_owned()),
        "KillBash" => |_i: &Map<String, Value>, _r: &str| Some("KillBash".to_owned()),
        "KillShell" => |_i: &Map<String, Value>, _r: &str| Some("KillShell".to_owned()),
        "Glob" => sum_glob,
        "Grep" => sum_grep,
        "LS" => sum_ls,
        "Task" | "Agent" => sum_task,
        "WebFetch" => sum_web_fetch,
        "WebSearch" => sum_web_search,
        "TodoWrite" => sum_todo,
        "Skill" => sum_skill,
        "ToolSearch" => sum_tool_search,
        "ExitPlanMode" => |_i: &Map<String, Value>, _r: &str| Some("ExitPlanMode".to_owned()),
        "EnterPlanMode" => |_i: &Map<String, Value>, _r: &str| Some("EnterPlanMode".to_owned()),
        "AskUserQuestion" => |_i: &Map<String, Value>, _r: &str| Some("AskUserQuestion".to_owned()),
        "TaskCreate" => sum_task_create,
        "TaskUpdate" => |_i: &Map<String, Value>, _r: &str| Some("TaskUpdate".to_owned()),
        "TaskGet" => |_i: &Map<String, Value>, _r: &str| Some("TaskGet".to_owned()),
        "TaskList" => |_i: &Map<String, Value>, _r: &str| Some("TaskList".to_owned()),
        "SendMessage" => sum_send_message,
        _ => return None,
    })
}

/// `_sum_file_op(verb)` — `"<verb> <short path>"`, or the bare verb.
fn file_op(verb: &str, input: &Map<String, Value>) -> String {
    match input_path(input) {
        Some(path) => format!("{verb} {}", short_path(&path)),
        None => verb.to_owned(),
    }
}

fn sum_bash(input: &Map<String, Value>, _res: &str) -> Option<String> {
    let cmd = str_arg(input, "command")?;
    if py_strip(cmd).is_empty() {
        return Some("Bash".to_owned());
    }
    Some(format!("Bash: {}", first_command_word(cmd)))
}

fn sum_glob(input: &Map<String, Value>, _res: &str) -> Option<String> {
    // `f"Glob {pat}" if isinstance(pat, str) and pat else "Glob"` — TRUTHINESS
    // on the pattern, not a strip.
    let base = match str_arg(input, "pattern").filter(|pat| !pat.is_empty()) {
        Some(pat) => format!("Glob {pat}"),
        None => "Glob".to_owned(),
    };
    match str_arg(input, "path").filter(|path| !py_strip(path).is_empty()) {
        Some(path) => Some(format!("{base} in {}", short_path(path))),
        None => Some(base),
    }
}

fn sum_grep(input: &Map<String, Value>, _res: &str) -> Option<String> {
    Some(
        match str_arg(input, "pattern").filter(|pat| !pat.is_empty()) {
            Some(pat) => format!("Grep {pat}"),
            None => "Grep".to_owned(),
        },
    )
}

fn sum_ls(input: &Map<String, Value>, _res: &str) -> Option<String> {
    Some(
        match str_arg(input, "path").filter(|path| !path.is_empty()) {
            Some(path) => format!("LS {}", short_path(path)),
            None => "LS".to_owned(),
        },
    )
}

fn sum_task(input: &Map<String, Value>, _res: &str) -> Option<String> {
    if let Some(desc) = str_arg(input, "description") {
        let stripped = py_strip(desc);
        if !stripped.is_empty() {
            return Some(format!("Task: {}", char_prefix(stripped, 60)));
        }
    }
    if let Some(sub) = str_arg(input, "subagent_type") {
        let stripped = py_strip(sub);
        if !stripped.is_empty() {
            return Some(format!("Task: {stripped}"));
        }
    }
    Some("Task".to_owned())
}

fn sum_web_fetch(input: &Map<String, Value>, _res: &str) -> Option<String> {
    Some(match str_arg(input, "url").filter(|url| !url.is_empty()) {
        Some(url) => format!("WebFetch {url}"),
        None => "WebFetch".to_owned(),
    })
}

fn sum_web_search(input: &Map<String, Value>, _res: &str) -> Option<String> {
    Some(match str_arg(input, "query").filter(|q| !q.is_empty()) {
        Some(query) => format!("WebSearch: {}", char_prefix(query, 60)),
        None => "WebSearch".to_owned(),
    })
}

fn sum_todo(input: &Map<String, Value>, _res: &str) -> Option<String> {
    let count = match input.get("todos") {
        Some(Value::Array(items)) => items.len(),
        _ => 0,
    };
    let plural = if count == 1 { "" } else { "s" };
    Some(format!("TodoWrite ({count} todo{plural})"))
}

fn sum_skill(input: &Map<String, Value>, _res: &str) -> Option<String> {
    // `inp.get("skill") or inp.get("command")` — Python truthiness, so a
    // present-but-empty `skill` falls through to `command`.
    let picked = match input.get("skill") {
        Some(value) if py_truthy(value) => Some(value),
        _ => input.get("command"),
    };
    Some(match picked {
        Some(Value::String(text)) if !text.is_empty() => format!("Skill: {text}"),
        _ => "Skill".to_owned(),
    })
}

fn sum_notebook_edit(input: &Map<String, Value>, _res: &str) -> Option<String> {
    // The `or` chain walks three keys on TRUTHINESS; the winner is
    // type-checked once, afterwards.
    let picked = ["notebook_path", "notebookPath", "file_path"]
        .into_iter()
        .find_map(|key| input.get(key).filter(|value| py_truthy(value)));
    Some(match picked {
        Some(Value::String(path)) if !path.is_empty() => {
            format!("NotebookEdit {}", short_path(path))
        }
        _ => "NotebookEdit".to_owned(),
    })
}

/// `f"ToolSearch: {inp.get('query', '')[:60]}".rstrip(": ")`.
///
/// **Narrowing (recorded).** A non-string `query` is *subscripted* in Python:
/// an `int` raises `TypeError` (caught → `"ToolSearch"`), a `list` slices and
/// then renders through the f-string's `str()`. This answers `"ToolSearch"` for
/// every non-string — exact for the scalar cases, narrowed for the container
/// ones, which no transcript has ever carried.
fn sum_tool_search(input: &Map<String, Value>, _res: &str) -> Option<String> {
    let query = match input.get("query") {
        None => "",
        Some(Value::String(text)) => text.as_str(),
        Some(_) => return None,
    };
    Some(rstrip_colon_space(&format!(
        "ToolSearch: {}",
        char_prefix(query, 60)
    )))
}

/// `f"TaskCreate: {inp.get('description', '')[:60]}".rstrip(": ")` — the same
/// narrowing as [`sum_tool_search`].
fn sum_task_create(input: &Map<String, Value>, _res: &str) -> Option<String> {
    let description = match input.get("description") {
        None => "",
        Some(Value::String(text)) => text.as_str(),
        Some(_) => return None,
    };
    Some(rstrip_colon_space(&format!(
        "TaskCreate: {}",
        char_prefix(description, 60)
    )))
}

/// `f"SendMessage → {inp.get('to', '')}".rstrip("→ ")`.
///
/// The f-string takes `str(value)` for ANY type, so unlike the two above there
/// is no subscript and no raise — a numeric `to` renders as its digits.
fn sum_send_message(input: &Map<String, Value>, _res: &str) -> Option<String> {
    let to = input.get("to").map_or_else(String::new, py_str);
    let rendered = format!("SendMessage → {to}");
    Some(rendered.trim_end_matches(['→', ' ']).to_owned())
}

/// `str.rstrip(": ")` — strips any trailing run of `:` and ` `.
fn rstrip_colon_space(text: &str) -> String {
    text.trim_end_matches([':', ' ']).to_owned()
}

/// `_byte_count` — the size of the result text, else of what was written.
///
/// `len(s.encode("utf-8", errors="replace"))`: a Rust `String` is already valid
/// UTF-8, so `errors="replace"` is a no-op and `str::len()` is the same count.
fn byte_count(
    tool_name: &str,
    tool_input: &Map<String, Value>,
    result_text: Option<&str>,
) -> Option<i64> {
    // `if result_text:` — truthiness, so an EMPTY result falls through to the
    // write-side estimate rather than reporting 0.
    if let Some(text) = result_text.filter(|text| !text.is_empty()) {
        return i64::try_from(text.len()).ok();
    }
    if !WRITE_TOOLS.contains(&tool_name) {
        return None;
    }
    for key in WRITE_CONTENT_KEYS {
        if let Some(Value::String(value)) = tool_input.get(key) {
            return i64::try_from(value.len()).ok();
        }
    }
    // MultiEdit: sum the `new_string` of each edit. `seen` distinguishes "an
    // edits list with no usable entries" (→ `None`) from "a total of zero".
    if let Some(Value::Array(edits)) = tool_input.get("edits") {
        let mut total: i64 = 0;
        let mut seen = false;
        for edit in edits {
            if let Value::Object(map) = edit
                && let Some(Value::String(new_string)) = map.get("new_string")
            {
                total += i64::try_from(new_string.len()).unwrap_or(0);
                seen = true;
            }
        }
        if seen {
            return Some(total);
        }
    }
    None
}

/// `_compact_input` — a readable one-liner of the call's salient inputs.
///
/// **Narrowing (recorded).** The `Edit` / `MultiEdit` leg is
/// `f"- {(old or '')[:80]!r}\n+ {(new or '')[:80]!r}"` guarded by
/// `isinstance(old, str) or isinstance(new, str)`, so a *truthy non-string*
/// under the other key reaches `[:80]` and raises `TypeError` — uncaught, a
/// 500, since `_payload_excerpt` sits outside every `try`. This renders such a
/// value as `''`. `null` / `false` / `0` / `""` are falsy and are `''` on both
/// sides.
fn compact_input(tool_name: &str, tool_input: &Map<String, Value>) -> String {
    if tool_name == "Bash" {
        return match tool_input.get("command") {
            Some(Value::String(cmd)) => py_strip(cmd).to_owned(),
            _ => String::new(),
        };
    }
    if tool_name == "Edit" || tool_name == "MultiEdit" {
        let old = tool_input.get("old_string");
        let new = tool_input.get("new_string");
        if matches!(old, Some(Value::String(_))) || matches!(new, Some(Value::String(_))) {
            let render = |value: Option<&Value>| match value {
                Some(Value::String(text)) => char_prefix(text, 80),
                _ => String::new(),
            };
            return format!(
                "- {}\n+ {}",
                py_repr(&Value::String(render(old))),
                py_repr(&Value::String(render(new)))
            );
        }
    }
    if tool_name == "Write"
        && let Some(Value::String(content)) = tool_input.get("content")
    {
        return content.clone();
    }
    if let Some(path) = input_path(tool_input) {
        return path;
    }
    // Last resort: `json.dumps(tool_input, default=str)[: _EXCERPT_CHARS * 2]`
    // — CPython's DEFAULT separators and `ensure_ascii=True`, then a
    // 400-CODE-POINT slice.
    char_prefix(
        &stax_memory::pyjson::dumps_py_default(&Value::Object(tool_input.clone())),
        EXCERPT_CHARS * 2,
    )
}

/// `_payload_excerpt` — `input ⇒ result`, capped at 200 code points.
fn payload_excerpt(
    tool_name: &str,
    tool_input: &Map<String, Value>,
    result_text: Option<&str>,
) -> String {
    let left = py_strip(&compact_input(tool_name, tool_input)).to_owned();
    let right = py_strip(result_text.unwrap_or_default()).to_owned();
    let blended = if !left.is_empty() && !right.is_empty() {
        format!("{left}\n⇒ {right}")
    } else if left.is_empty() {
        right
    } else {
        left
    };
    let blended = blended.replace("\r\n", "\n");
    // `len(blended)` is a CODE POINT count, and so is the slice.
    if blended.chars().count() <= EXCERPT_CHARS {
        return blended;
    }
    format!("{}…", char_prefix(&blended, EXCERPT_CHARS - 1))
}

// ── timestamps ───────────────────────────────────────────────────────────────

/// `_parse_iso` — `datetime.fromisoformat(ts.strip())` with a **trailing** `Z`
/// rewritten to `+00:00`.
///
/// [`parse_ts`] is the deduped owner of the `fromisoformat` grammar, and it
/// applies `.replace("Z", "+00:00")` to EVERY `Z` rather than a trailing one.
/// The strip and the trailing-`Z` rewrite both happen here, so the only
/// residual difference is a string with an *interior* `Z` — which fails the
/// date grammar on both sides.
#[must_use]
pub fn parse_iso(ts: Option<&str>) -> Option<PyDateTime> {
    let text = py_strip(ts?);
    if text.is_empty() {
        return None;
    }
    let normalised = match text.strip_suffix('Z') {
        Some(head) => format!("{head}+00:00"),
        None => text.to_owned(),
    };
    parse_ts(&normalised)
}

/// `_duration_ms` — `int((b - a).total_seconds() * 1000)`, dropped when
/// negative.
///
/// **Narrowing (recorded).** `b - a` raises `TypeError` for a naive/aware mix
/// and the reference catches only `(OverflowError, ValueError)`, so that case
/// is a 500 there and a `None` here. Same trade as
/// `captured_failure_message_ids`.
fn duration_ms(call_ts: Option<&str>, result_ts: Option<&str>) -> Option<i64> {
    let a = parse_iso(call_ts)?;
    let b = parse_iso(result_ts)?;
    let seconds = b.sub_total_seconds(a)?;
    // `int(x)` truncates toward zero.
    let delta_ms = (seconds * 1000.0).trunc();
    if !delta_ms.is_finite() {
        return None;
    }
    #[allow(
        clippy::cast_possible_truncation,
        reason = "both operands came from parsed timestamps and stay far inside i64"
    )]
    let delta_ms = delta_ms as i64;
    (delta_ms >= 0).then_some(delta_ms)
}

// ── core builder ─────────────────────────────────────────────────────────────

/// Where each event's `session_id` comes from: one id for the whole stream, or
/// a `{session_fk: session_id}` map for the cross-session path.
enum SessionIdFor<'a> {
    One(&'a str),
    ByFk(&'a HashMap<i64, String>),
}

impl SessionIdFor<'_> {
    fn resolve(&self, row: &MessageRow) -> String {
        match self {
            Self::One(sid) => (*sid).to_owned(),
            // `.get(fk, "")` — an orphan row yields the empty string.
            Self::ByFk(map) => map.get(&row.session_fk).cloned().unwrap_or_default(),
        }
    }
}

/// `_build_events` — the seq/ts-ordered message list as an event stream.
///
/// Returns `(events, truncated)`. The reference's control flow is a `for … else`
/// with a trailing `break`: the inner block loop `break`s when `limit` is hit,
/// and that `break` propagates out of the OUTER loop too. Every other exit from
/// the inner loop falls into `else: continue` and the next message is read.
/// `'rows` reproduces that exactly.
fn build_events(
    rows: &[MessageRow],
    session_id_for: &SessionIdFor<'_>,
    tool_filter: Option<&[String]>,
    limit: i64,
    include_payload: bool,
    captured_success: &HashMap<i64, bool>,
) -> (Vec<PlaybackEvent>, bool) {
    let results = index_results(rows);
    let mut events: Vec<PlaybackEvent> = Vec::new();
    let mut global_idx: i64 = 0;
    let mut truncated = false;
    let limit = usize::try_from(limit).unwrap_or(0);

    'rows: for row in rows {
        if row.role != "assistant" {
            continue;
        }
        let env = envelope(row.raw_json.as_deref());
        for block in content_blocks(&env) {
            let Value::Object(map) = block else { continue };
            if map.get("type").and_then(Value::as_str) != Some("tool_use") {
                continue;
            }
            let this_idx = global_idx;
            global_idx += 1;

            // `if not isinstance(tname, str) or not tname` — a recoverable
            // envelope with a bad inner shape.
            let Some(tname) = map
                .get("name")
                .and_then(Value::as_str)
                .filter(|name| !name.is_empty())
            else {
                // The marker is suppressed entirely when a filter is active —
                // it has no tool name to match.
                if tool_filter.is_some() {
                    continue;
                }
                if events.len() >= limit {
                    truncated = true;
                    break 'rows;
                }
                events.push(PlaybackEvent {
                    seq: this_idx,
                    ts: row.timestamp.clone().unwrap_or_default(),
                    message_id: row.id.unwrap_or(0),
                    tool_name: "?".to_owned(),
                    summary: "(unparseable)".to_owned(),
                    target_path: None,
                    byte_count: None,
                    success: None,
                    duration_ms: None,
                    payload_excerpt: String::new(),
                    session_id: session_id_for.resolve(row),
                });
                continue;
            };

            if tool_filter.is_some_and(|filter| !filter.iter().any(|want| want == tname)) {
                continue;
            }
            if events.len() >= limit {
                truncated = true;
                break 'rows;
            }

            let empty = Map::new();
            let tinput = match map.get("input") {
                Some(Value::Object(obj)) => obj,
                _ => &empty,
            };
            let res = map
                .get("id")
                .and_then(Value::as_str)
                .and_then(|tuid| results.get(tuid));
            let result_text = res.map(|info| info.text.as_str());

            let mid = row.id.unwrap_or(0);
            // captured_events (authoritative) > transcript is_error > unknown.
            let success = if let Some(flag) = captured_success.get(&mid) {
                Some(*flag)
            } else {
                res.and_then(|info| info.is_error).map(|is_error| !is_error)
            };

            events.push(PlaybackEvent {
                seq: this_idx,
                ts: row.timestamp.clone().unwrap_or_default(),
                message_id: mid,
                tool_name: tname.to_owned(),
                summary: summarize_tool_call(tname, Some(tinput), result_text),
                target_path: input_path(tinput),
                byte_count: byte_count(tname, tinput, result_text),
                success,
                duration_ms: duration_ms(
                    row.timestamp.as_deref(),
                    res.and_then(|info| info.ts.as_deref()),
                ),
                payload_excerpt: if include_payload {
                    payload_excerpt(tname, tinput, result_text)
                } else {
                    String::new()
                },
                session_id: session_id_for.resolve(row),
            });
        }
    }

    (events, truncated)
}

/// `_norm_filter` — the stripped, de-duplicated tool names, or `None`.
///
/// Python builds a `set`; only membership is ever asked, so a de-duplicated
/// `Vec` answers identically with a deterministic cost and no hashing.
#[must_use]
pub fn norm_filter(tool_filter: Option<&[String]>) -> Option<Vec<String>> {
    let raw = tool_filter.filter(|raw| !raw.is_empty())?;
    let mut cleaned: Vec<String> = Vec::new();
    for name in raw {
        let stripped = py_strip(name);
        if stripped.is_empty() {
            continue;
        }
        if !cleaned.iter().any(|seen| seen == stripped) {
            cleaned.push(stripped.to_owned());
        }
    }
    (!cleaned.is_empty()).then_some(cleaned)
}

// ── session-id resolution ────────────────────────────────────────────────────

/// `_resolve_session` — `session_id` → `(session_fk, session_id)`.
///
/// `session_id` is unique per project, not globally, so the most recently
/// active match wins. `NULLS LAST` is load-bearing: SQLite's default under
/// `DESC` puts NULLs FIRST, and a session that never recorded a `last_ts` would
/// otherwise outrank every session that did.
///
/// # Errors
/// Any SQLite failure. The reference has no `try` here either.
pub fn resolve_session(
    conn: &Connection,
    session_id: &str,
) -> rusqlite::Result<Option<(i64, String)>> {
    let mut stmt = conn.prepare(
        "SELECT id, session_id FROM sessions WHERE session_id = ? \
         ORDER BY last_ts DESC NULLS LAST, id DESC LIMIT 1",
    )?;
    let mut rows = stmt.query([session_id])?;
    match rows.next()? {
        Some(row) => Ok(Some((
            row.get::<_, i64>(0)?,
            row.get::<_, Option<String>>(1)?.unwrap_or_default(),
        ))),
        None => Ok(None),
    }
}

// ── public API ───────────────────────────────────────────────────────────────

/// `session_playback_page` — `(events, truncated)`, or `None` when the session
/// is not in the store.
///
/// The `None` is what lets the route tell "wrong session id" (404) from
/// "session with no tool calls" (200 and an empty list).
///
/// # Errors
/// Any SQLite failure.
pub fn session_playback_page(
    conn: &Connection,
    session_id: &str,
    tool_filter: Option<&[String]>,
    limit: i64,
    include_payload: bool,
) -> rusqlite::Result<Option<(Vec<PlaybackEvent>, bool)>> {
    let Some((session_fk, sid)) = resolve_session(conn, session_id)? else {
        return Ok(None);
    };
    let rows = read_rows(
        conn,
        "SELECT id, session_fk, seq, timestamp, role, raw_json \
         FROM messages WHERE session_fk = ? ORDER BY seq",
        &[&session_fk],
    )?;
    let captured = captured_failure_message_ids(conn, &sid, &rows);
    Ok(Some(build_events(
        &rows,
        &SessionIdFor::One(&sid),
        norm_filter(tool_filter).as_deref(),
        // `max(0, int(limit))`.
        limit.max(0),
        include_payload,
        &captured,
    )))
}

/// `project_timeline_page` — the cross-session stream for one project.
///
/// The `captured_events` join is deliberately skipped here: the reference
/// passes `captured_success={}` with the comment "not worth it for v1", so
/// every event's `success` comes from the transcript alone even on a store
/// where the hooks table exists.
///
/// # Errors
/// Any SQLite failure.
pub fn project_timeline_page(
    conn: &Connection,
    project_id: i64,
    since: Option<&str>,
    tool_filter: Option<&[String]>,
    limit: i64,
    include_payload: bool,
) -> rusqlite::Result<(Vec<PlaybackEvent>, bool)> {
    let mut sid_by_fk: HashMap<i64, String> = HashMap::new();
    {
        let mut stmt = conn.prepare("SELECT id, session_id FROM sessions WHERE project_id = ?")?;
        let mut rows = stmt.query([project_id])?;
        while let Some(row) = rows.next()? {
            sid_by_fk.insert(
                row.get::<_, i64>(0)?,
                row.get::<_, Option<String>>(1)?.unwrap_or_default(),
            );
        }
    }
    // `if not sid_by_fk:` — a project with no sessions short-circuits before
    // the message sweep, and reports `truncated=False` regardless of `limit`.
    if sid_by_fk.is_empty() {
        return Ok((Vec::new(), false));
    }

    let mut sql = String::from(
        "SELECT m.id, m.session_fk, m.seq, m.timestamp, m.role, m.raw_json \
         FROM messages m JOIN sessions s ON s.id = m.session_fk \
         WHERE s.project_id = ?",
    );
    let mut params: Vec<Box<dyn rusqlite::ToSql>> = vec![Box::new(project_id)];
    // `if since:` — Python truthiness, so an EMPTY `since` adds no clause.
    if let Some(bound) = since.filter(|value| !value.is_empty()) {
        sql.push_str(" AND m.timestamp >= ?");
        params.push(Box::new(bound.to_owned()));
    }
    sql.push_str(" ORDER BY m.timestamp, m.session_fk, m.seq");
    let refs: Vec<&dyn rusqlite::ToSql> = params.iter().map(std::convert::AsRef::as_ref).collect();
    let rows = read_rows(conn, &sql, refs.as_slice())?;

    Ok(build_events(
        &rows,
        &SessionIdFor::ByFk(&sid_by_fk),
        norm_filter(tool_filter).as_deref(),
        limit.max(0),
        include_payload,
        &HashMap::new(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn map(value: Value) -> Map<String, Value> {
        match value {
            Value::Object(obj) => obj,
            _ => panic!("not an object"),
        }
    }

    #[test]
    fn short_path_keeps_the_last_two_components_and_falls_back_to_the_original() {
        assert_eq!(short_path("/Users/x/repo/routes/cost.py"), "routes/cost.py");
        assert_eq!(short_path("routes/cost.py"), "routes/cost.py");
        assert_eq!(short_path("cost.py"), "cost.py");
        assert_eq!(short_path("C:\\a\\b\\c.py"), "b/c.py");
        // `"/".join(parts) if parts else path` — the UNNORMALISED original.
        assert_eq!(short_path("///"), "///");
        assert_eq!(short_path(""), "");
    }

    #[test]
    fn the_first_command_word_skips_cd_prefixes_env_assignments_and_sudo() {
        let word = first_command_word;
        assert_eq!(word("cd /tmp && pytest -q"), "pytest");
        assert_eq!(word("pytest tests/"), "pytest");
        assert_eq!(word("cd /a && cd /b && make"), "make");
        assert_eq!(word("sudo apt install x"), "apt");
        assert_eq!(word("FOO=1 BAR=2 cargo test"), "cargo");
        // `"/" in t.split("=")[0]` keeps a path that happens to contain `=`.
        assert_eq!(word("./a=b/run.sh"), "./a=b/run.sh");
        assert_eq!(word("--flag=x"), "--flag=x");
        assert_eq!(word("cd /tmp; ls"), "ls");
        // The `;` peel runs only after the `&&` peel is exhausted.
        assert_eq!(word("cd /a && cd /b; echo hi"), "echo");
    }

    /// The near-miss that looks like an `IndexError` and is not. `"cd /tmp &&  "`
    /// strips to `"cd /tmp &&"`, whose `partition("&&")` leaves an EMPTY `rest`,
    /// so the peel loop breaks immediately and the answer is the literal `cd`.
    /// Measured against the reference interpreter, not reasoned about.
    #[test]
    fn a_trailing_separator_leaves_cd_as_the_command_rather_than_raising() {
        assert_eq!(first_command_word("cd /tmp &&  "), "cd");
        let input = map(json!({"command": "cd /tmp &&  "}));
        assert_eq!(summarize_tool_call("Bash", Some(&input), None), "Bash: cd");
        // Every token skippable: the fallback returns the first one.
        assert_eq!(first_command_word("sudo time env"), "sudo");
        // A separator with no `cd` head is not peeled either.
        assert_eq!(first_command_word("cd /a && \t && x"), "&&");
    }

    #[test]
    fn mcp_names_collapse_to_server_dot_tool() {
        assert_eq!(
            summarize_tool_call("mcp__github__create_pr", None, None),
            "github.create_pr"
        );
        // No second `__`: the remainder alone.
        assert_eq!(summarize_tool_call("mcp__solo", None, None), "solo");
        // `rest or tool_name` — a bare prefix falls back to the whole name.
        assert_eq!(summarize_tool_call("mcp__", None, None), "mcp__");
    }

    #[test]
    fn the_summary_table_covers_every_named_tool() {
        let cases: Vec<(&str, Value, &str)> = vec![
            ("Read", json!({"file_path": "/a/b/c.py"}), "Read b/c.py"),
            ("Write", json!({}), "Write"),
            ("Edit", json!({"filePath": "/x/y.py"}), "Edit x/y.py"),
            ("Glob", json!({"pattern": "*.py"}), "Glob *.py"),
            (
                "Glob",
                json!({"pattern": "*.py", "path": "/a/b/c"}),
                "Glob *.py in b/c",
            ),
            ("Grep", json!({"pattern": "TODO"}), "Grep TODO"),
            ("Grep", json!({}), "Grep"),
            ("LS", json!({"path": "/a/b/c"}), "LS b/c"),
            ("Task", json!({"description": " ship it "}), "Task: ship it"),
            ("Task", json!({"subagent_type": "explore"}), "Task: explore"),
            ("Task", json!({}), "Task"),
            ("Agent", json!({"description": "go"}), "Task: go"),
            (
                "WebFetch",
                json!({"url": "https://x"}),
                "WebFetch https://x",
            ),
            ("WebSearch", json!({"query": "rust"}), "WebSearch: rust"),
            ("TodoWrite", json!({"todos": [1]}), "TodoWrite (1 todo)"),
            ("TodoWrite", json!({"todos": []}), "TodoWrite (0 todos)"),
            ("TodoWrite", json!({}), "TodoWrite (0 todos)"),
            ("Skill", json!({"skill": "run"}), "Skill: run"),
            // `skill or command` — an empty `skill` falls through.
            ("Skill", json!({"skill": "", "command": "x"}), "Skill: x"),
            ("Skill", json!({}), "Skill"),
            ("ToolSearch", json!({"query": "grep"}), "ToolSearch: grep"),
            // `.rstrip(": ")` eats the separator when the argument is blank.
            ("ToolSearch", json!({}), "ToolSearch"),
            ("TaskCreate", json!({}), "TaskCreate"),
            ("TaskUpdate", json!({}), "TaskUpdate"),
            ("SendMessage", json!({"to": "alice"}), "SendMessage → alice"),
            ("SendMessage", json!({}), "SendMessage"),
            (
                "NotebookEdit",
                json!({"notebookPath": "/n/b.ipynb"}),
                "NotebookEdit n/b.ipynb",
            ),
            ("BashOutput", json!({}), "BashOutput"),
            ("ExitPlanMode", json!({}), "ExitPlanMode"),
            ("KillShell", json!({}), "KillShell"),
        ];
        for (name, input, want) in cases {
            let input = map(input);
            assert_eq!(
                summarize_tool_call(name, Some(&input), None),
                want,
                "{name} {input:?}"
            );
        }
    }

    #[test]
    fn an_unknown_tool_falls_back_to_a_path_then_to_a_text_argument() {
        let input = map(json!({"path": "/a/b/c"}));
        assert_eq!(
            summarize_tool_call("Frobnicate", Some(&input), None),
            "Frobnicate b/c"
        );
        let input = map(json!({"query": "  line one\nline two  "}));
        assert_eq!(
            summarize_tool_call("Frobnicate", Some(&input), None),
            "Frobnicate: line one"
        );
        assert_eq!(summarize_tool_call("Frobnicate", None, None), "Frobnicate");
        // An empty name is the "(unparseable)" marker.
        assert_eq!(summarize_tool_call("", None, None), "(unparseable)");
    }

    #[test]
    fn byte_count_is_bytes_while_every_label_slice_is_code_points() {
        let input = map(json!({"content": "café"}));
        // Five bytes, four characters.
        assert_eq!(byte_count("Write", &input, None), Some(5));
        assert_eq!(byte_count("Write", &input, Some("ab")), Some(2));
        // An EMPTY result text is falsy and falls through to the input.
        assert_eq!(byte_count("Write", &input, Some("")), Some(5));
        // A read-style tool with no result reports nothing.
        assert_eq!(byte_count("Read", &input, None), None);
        // MultiEdit sums `new_string`; an edits list with none is `None`.
        let edits = map(json!({"edits": [{"new_string": "ab"}, {"new_string": "é"}]}));
        assert_eq!(byte_count("MultiEdit", &edits, None), Some(4));
        let empty = map(json!({"edits": [{"nope": 1}]}));
        assert_eq!(byte_count("MultiEdit", &empty, None), None);
    }

    #[test]
    fn the_excerpt_caps_at_two_hundred_code_points_with_an_ellipsis() {
        let long = "é".repeat(500);
        let input = map(json!({ "content": long }));
        let excerpt = payload_excerpt("Write", &input, None);
        assert_eq!(excerpt.chars().count(), EXCERPT_CHARS);
        assert!(excerpt.ends_with('…'));
        // Exactly at the cap: no ellipsis, nothing dropped.
        let exact = "x".repeat(EXCERPT_CHARS);
        let input = map(json!({ "content": exact.clone() }));
        assert_eq!(payload_excerpt("Write", &input, None), exact);
    }

    #[test]
    fn the_excerpt_blends_input_and_result_and_normalises_crlf() {
        let input = map(json!({"command": "ls -la"}));
        assert_eq!(
            payload_excerpt("Bash", &input, Some("a\r\nb")),
            "ls -la\n⇒ a\nb"
        );
        // An EMPTY input dict is NOT "no left side": `_compact_input` falls all
        // the way through to `json.dumps({})`, which is the two characters
        // `{}`. Measured — the intuitive `"out"` is wrong.
        let empty = Map::new();
        assert_eq!(payload_excerpt("Read", &empty, Some("out")), "{}\n⇒ out");
    }

    #[test]
    fn the_edit_excerpt_is_a_python_repr_pair() {
        let input = map(json!({"old_string": "a'b", "new_string": "c"}));
        // `repr("a'b")` picks double quotes; `repr("c")` keeps single.
        assert_eq!(compact_input("Edit", &input), "- \"a'b\"\n+ 'c'");
        // Only one side a string: the other renders as `''`.
        let input = map(json!({"new_string": "c"}));
        assert_eq!(compact_input("Edit", &input), "- ''\n+ 'c'");
    }

    #[test]
    fn the_last_resort_excerpt_is_a_bare_json_dumps_not_the_http_writer() {
        // `json.dumps` defaults: `", "` / `": "` and ensure_ascii=True.
        let input = map(json!({"a": 1, "b": "café"}));
        assert_eq!(
            compact_input("Frobnicate", &input),
            r#"{"a": 1, "b": "caf\u00e9"}"#
        );
    }

    #[test]
    fn result_content_normalises_every_wire_shape() {
        assert_eq!(stringify_result_content(None), "");
        assert_eq!(stringify_result_content(Some(&json!(null))), "");
        assert_eq!(stringify_result_content(Some(&json!("txt"))), "txt");
        assert_eq!(
            stringify_result_content(Some(&json!([
                {"type": "text", "text": "a"},
                {"type": "image"},
                "bare",
                {"type": "text", "text": 5}
            ]))),
            "a\nbare"
        );
        assert_eq!(
            stringify_result_content(Some(&json!({"text": "inner"}))),
            "inner"
        );
        // No `text` key: the bare `json.dumps` layout.
        assert_eq!(
            stringify_result_content(Some(&json!({"stdout": "x"}))),
            r#"{"stdout": "x"}"#
        );
        assert_eq!(stringify_result_content(Some(&json!(5))), "5");
        assert_eq!(stringify_result_content(Some(&json!(true))), "True");
    }

    #[test]
    fn parse_iso_strips_and_rewrites_only_a_trailing_z() {
        assert!(parse_iso(Some("  2026-01-01T00:00:00Z  ")).is_some());
        assert!(parse_iso(Some("2026-01-01")).is_some());
        assert!(parse_iso(Some("")).is_none());
        assert!(parse_iso(Some("   ")).is_none());
        assert!(parse_iso(None).is_none());
        assert!(parse_iso(Some("not-a-date")).is_none());
        // The trailing `Z` becomes an offset, so the value is AWARE.
        assert_eq!(
            parse_iso(Some("2026-01-01T00:00:00Z"))
                .expect("parsed")
                .offset_s,
            Some(0)
        );
        // No `Z`, no offset: naive.
        assert_eq!(
            parse_iso(Some("2026-01-01T00:00:00"))
                .expect("parsed")
                .offset_s,
            None
        );
    }

    #[test]
    fn duration_is_dropped_when_negative_or_unparseable() {
        assert_eq!(
            duration_ms(Some("2026-01-01T00:00:00Z"), Some("2026-01-01T00:00:01Z")),
            Some(1000)
        );
        // The result landed BEFORE the call — the reference drops it.
        assert_eq!(
            duration_ms(Some("2026-01-01T00:00:01Z"), Some("2026-01-01T00:00:00Z")),
            None
        );
        assert_eq!(duration_ms(Some("2026-01-01T00:00:00Z"), None), None);
        // The mixed naive/aware narrowing: `None`, not a panic.
        assert_eq!(
            duration_ms(Some("2026-01-01T00:00:00"), Some("2026-01-01T00:00:01Z")),
            None
        );
    }

    #[test]
    fn the_tool_filter_is_stripped_deduplicated_and_none_when_empty() {
        assert_eq!(norm_filter(None), None);
        assert_eq!(norm_filter(Some(&[])), None);
        assert_eq!(norm_filter(Some(&["  ".to_owned()])), None);
        assert_eq!(
            norm_filter(Some(&[
                " Edit ".to_owned(),
                "Edit".to_owned(),
                "Write".to_owned()
            ])),
            Some(vec!["Edit".to_owned(), "Write".to_owned()])
        );
    }

    // ── the builder, against a seeded store ─────────────────────────────────

    fn seeded() -> Connection {
        let conn = Connection::open_in_memory().expect("open");
        conn.execute_batch(
            "CREATE TABLE projects (id INTEGER PRIMARY KEY, slug TEXT);
             CREATE TABLE sessions (id INTEGER PRIMARY KEY, project_id INTEGER,
                 session_id TEXT, last_ts TEXT);
             CREATE TABLE messages (id INTEGER PRIMARY KEY, session_fk INTEGER,
                 seq INTEGER, timestamp TEXT, role TEXT, raw_json TEXT);
             INSERT INTO projects (id, slug) VALUES (1, '-p');
             INSERT INTO sessions (id, project_id, session_id, last_ts)
                 VALUES (10, 1, 'sess', '2026-01-01T00:00:00Z');",
        )
        .expect("schema");
        let call = json!({
            "message": {"content": [
                {"type": "tool_use", "id": "t1", "name": "Read",
                 "input": {"file_path": "/a/b/c.py"}},
                {"type": "tool_use", "id": "t2", "name": "Bash",
                 "input": {"command": "cd /x && pytest"}}
            ]}
        });
        let result = json!({
            "message": {"content": [
                {"type": "tool_result", "tool_use_id": "t1", "content": "file body"},
                {"type": "tool_result", "tool_use_id": "t2", "content": "boom",
                 "is_error": true}
            ]}
        });
        conn.execute(
            "INSERT INTO messages (id, session_fk, seq, timestamp, role, raw_json)
             VALUES (1, 10, 1, '2026-01-01T00:00:00Z', 'assistant', ?)",
            [call.to_string()],
        )
        .expect("insert call");
        conn.execute(
            "INSERT INTO messages (id, session_fk, seq, timestamp, role, raw_json)
             VALUES (2, 10, 2, '2026-01-01T00:00:02Z', 'user', ?)",
            [result.to_string()],
        )
        .expect("insert result");
        conn
    }

    #[test]
    fn a_session_page_pairs_calls_with_results_and_keeps_the_global_seq() {
        let conn = seeded();
        let (events, truncated) = session_playback_page(&conn, "sess", None, 1000, true)
            .expect("query")
            .expect("session");
        assert!(!truncated);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].seq, 0);
        assert_eq!(events[0].summary, "Read b/c.py");
        assert_eq!(events[0].byte_count, Some(9));
        assert_eq!(events[0].duration_ms, Some(2000));
        // No `is_error` on the block and no `toolUseResult` → unknown.
        assert_eq!(events[0].success, None);
        assert_eq!(events[1].summary, "Bash: pytest");
        assert_eq!(events[1].success, Some(false));
        assert_eq!(events[1].session_id, "sess");
    }

    #[test]
    fn a_filter_narrows_the_list_but_never_renumbers_it() {
        let conn = seeded();
        let (events, _) =
            session_playback_page(&conn, "sess", Some(&["Bash".to_owned()]), 1000, true)
                .expect("query")
                .expect("session");
        assert_eq!(events.len(), 1);
        // The Bash call is the SECOND tool call in the stream — the filtered
        // view keeps its position so a deep link still lines up.
        assert_eq!(events[0].seq, 1);
    }

    #[test]
    fn the_limit_truncates_and_says_so() {
        let conn = seeded();
        let (events, truncated) = session_playback_page(&conn, "sess", None, 1, true)
            .expect("query")
            .expect("session");
        assert_eq!(events.len(), 1);
        assert!(truncated);
        // A limit of zero yields nothing at all, and still reports truncation.
        let (events, truncated) = session_playback_page(&conn, "sess", None, 0, true)
            .expect("query")
            .expect("session");
        assert!(events.is_empty());
        assert!(truncated);
    }

    #[test]
    fn an_unknown_session_is_none_and_a_known_one_with_no_tools_is_empty() {
        let conn = seeded();
        assert!(
            session_playback_page(&conn, "ghost", None, 10, true)
                .expect("query")
                .is_none()
        );
        conn.execute(
            "INSERT INTO sessions (id, project_id, session_id, last_ts)
             VALUES (11, 1, 'quiet', '2026-01-01T00:00:00Z')",
            [],
        )
        .expect("insert");
        let (events, truncated) = session_playback_page(&conn, "quiet", None, 10, true)
            .expect("query")
            .expect("session");
        assert!(events.is_empty());
        assert!(!truncated);
    }

    #[test]
    fn include_payload_off_blanks_the_excerpt_and_nothing_else() {
        let conn = seeded();
        let (events, _) = session_playback_page(&conn, "sess", None, 10, false)
            .expect("query")
            .expect("session");
        assert_eq!(events[0].payload_excerpt, "");
        assert_eq!(events[0].byte_count, Some(9));
    }

    #[test]
    fn a_malformed_raw_json_contributes_nothing_and_never_raises() {
        let conn = seeded();
        conn.execute(
            "INSERT INTO messages (id, session_fk, seq, timestamp, role, raw_json)
             VALUES (3, 10, 3, '2026-01-01T00:00:03Z', 'assistant', '{not json')",
            [],
        )
        .expect("insert");
        let (events, _) = session_playback_page(&conn, "sess", None, 10, true)
            .expect("query")
            .expect("session");
        assert_eq!(events.len(), 2);
    }

    #[test]
    fn a_tool_use_block_with_no_name_becomes_the_unparseable_marker() {
        let conn = seeded();
        let bad = json!({"message": {"content": [{"type": "tool_use", "id": "t9"}]}});
        conn.execute(
            "INSERT INTO messages (id, session_fk, seq, timestamp, role, raw_json)
             VALUES (4, 10, 4, '2026-01-01T00:00:04Z', 'assistant', ?)",
            [bad.to_string()],
        )
        .expect("insert");
        let (events, _) = session_playback_page(&conn, "sess", None, 10, true)
            .expect("query")
            .expect("session");
        assert_eq!(events.len(), 3);
        assert_eq!(events[2].tool_name, "?");
        assert_eq!(events[2].summary, "(unparseable)");
        assert_eq!(events[2].seq, 2);
        // A filter suppresses the marker entirely — it has no name to match.
        let (events, _) =
            session_playback_page(&conn, "sess", Some(&["Read".to_owned()]), 10, true)
                .expect("query")
                .expect("session");
        assert_eq!(events.len(), 1);
    }

    #[test]
    fn the_project_timeline_interleaves_sessions_and_stamps_each_events_own_id() {
        let conn = seeded();
        conn.execute(
            "INSERT INTO sessions (id, project_id, session_id, last_ts)
             VALUES (11, 1, 'other', '2026-01-01T00:00:00Z')",
            [],
        )
        .expect("insert");
        let call = json!({"message": {"content": [
            {"type": "tool_use", "id": "z", "name": "Grep", "input": {"pattern": "x"}}]}});
        conn.execute(
            "INSERT INTO messages (id, session_fk, seq, timestamp, role, raw_json)
             VALUES (5, 11, 1, '2026-01-01T00:00:01Z', 'assistant', ?)",
            [call.to_string()],
        )
        .expect("insert");
        let (events, _) = project_timeline_page(&conn, 1, None, None, 5000, false).expect("query");
        // `ORDER BY m.timestamp` puts the 00:00:00 pair first, then 00:00:01.
        assert_eq!(events.len(), 3);
        assert_eq!(events[2].session_id, "other");
        assert_eq!(events[2].summary, "Grep x");
        // A `since` above every stamp empties the stream.
        let (events, _) =
            project_timeline_page(&conn, 1, Some("2027-01-01"), None, 5000, false).expect("query");
        assert!(events.is_empty());
        // A project with no sessions short-circuits.
        let (events, truncated) =
            project_timeline_page(&conn, 99, None, None, 5000, false).expect("query");
        assert!(events.is_empty());
        assert!(!truncated);
    }

    #[test]
    fn captured_events_override_the_transcript_signal() {
        let conn = seeded();
        conn.execute_batch(
            "CREATE TABLE captured_events (session_id TEXT, event_kind TEXT, ts TEXT);
             INSERT INTO captured_events (session_id, event_kind, ts)
                 VALUES ('sess', 'failure', '2026-01-01T00:00:01Z');",
        )
        .expect("hooks table");
        let (events, _) = session_playback_page(&conn, "sess", None, 10, true)
            .expect("query")
            .expect("session");
        // Message 1 is within the [-2s, +90s] window of the hook event, so BOTH
        // its tool calls are marked failed — even the Read, whose transcript
        // result carried no error at all.
        assert_eq!(events[0].success, Some(false));
        assert_eq!(events[1].success, Some(false));
    }

    #[test]
    fn the_serialised_event_keeps_the_dataclass_field_order() {
        let event = PlaybackEvent {
            seq: 0,
            ts: "t".to_owned(),
            message_id: 1,
            tool_name: "Read".to_owned(),
            summary: "Read a".to_owned(),
            target_path: None,
            byte_count: None,
            success: None,
            duration_ms: None,
            payload_excerpt: String::new(),
            session_id: "s".to_owned(),
        };
        assert_eq!(
            stax_memory::pyjson::dumps_http(&playback_event_to_dict(&event)),
            r#"{"seq":0,"ts":"t","message_id":1,"tool_name":"Read","summary":"Read a","target_path":null,"byte_count":null,"success":null,"duration_ms":null,"payload_excerpt":"","session_id":"s"}"#
        );
    }
}
