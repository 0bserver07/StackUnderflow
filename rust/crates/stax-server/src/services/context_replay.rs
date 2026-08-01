//! `services/context_replay.py` — reconstruct what the model "saw" at a point
//! in a session (issue #96, spec 24).
//!
//! | Item | Python | Rust |
//! |---|---|---|
//! | the empty-but-valid shape | `empty_context` | [`empty_context`] |
//! | the heavy, cache-friendly build | `build_context_timeline` | [`build_context_timeline`] |
//! | the pure re-slice | `slice_context_timeline` | [`slice_context_timeline`] |
//! | build + slice | `reconstruct_context` | [`reconstruct_context`] |
//! | one-line tool label | `playback.summarize_tool_call` | [`summarize_tool_call`] |
//!
//! # What the endpoint answers, and what it deliberately does not
//!
//! "What the model saw at seq K" is defined by the reference as **the session's
//! own message sequence, in `seq` order, for every message with `seq <= K`**.
//! It does *not* model the harness's context-window eviction — once a real
//! session exceeds the model's limit the harness compacts and drops older
//! turns, so the live window at seq K may be a strict subset of this. That is a
//! documented MVP simplification in the Python module docstring, not an
//! oversight, and it is inherited here unchanged.
//!
//! The per-message `tokens` figure is a `chars/4` **estimate** of that
//! message's own text plus its tool-call payload — deliberately *not* the
//! stored `input_tokens`, which for an assistant turn already counts the entire
//! prior context and would multiply-count the same history once per turn.
//!
//! # The contract that shapes every line below: this never raises
//!
//! Every function here is advisory. An unknown session, a session with zero
//! messages, malformed `raw_json`, a store missing the table entirely — all of
//! them yield the empty-but-valid shape (with a `warnings` note where useful),
//! so a route or CLI can splice the output without a `try` around every field.
//! That is why **no function in this module returns `Result`**: a
//! `?`-propagated `rusqlite::Error` where Python caught it would be a
//! behaviour change visible only on a broken store, which is precisely the
//! store nobody tests on. Each swallow is marked with the Python `except` it
//! mirrors.
//!
//! One consequence worth stating: `build_context_timeline` is safe on a *miss*.
//! `routes/context_replay.rs` calls it with a session id that resolved to
//! nothing, and it must come back with the empty shape and a
//! `session not found in store: …` warning rather than doing nothing useful.
//! That is a different code path from [`empty_context`] and it is pinned by a
//! test.
//!
//! # Ported helpers that belong to `services/playback.py`
//!
//! `build_context_timeline` imports `_content_blocks`, `_envelope` and
//! `summarize_tool_call` from `services/playback.py`. `routes/playback.py` is
//! unported (its slot is a stub), so those three — plus `_short_path`,
//! `_first_command_word`, `_input_path`, `_mcp_label` and the 27-entry
//! `_SUMMARY_HANDLERS` table they hang off — are ported *here*, privately,
//! rather than reached for across a fence this batch is not allowed to cross.
//! When the playback batch lands, [`summarize_tool_call`] is the function to
//! lift out; it is `pub` for exactly that reason. Flagged for the architect's
//! dedup list.
//!
//! # Python string semantics, spelled out because Rust's differ
//!
//! * `len(text)` counts **code points**, so both the `chars/4` estimate and the
//!   240-character preview cap use `chars().count()` / `chars().take(…)`, never
//!   `str::len` or a byte slice.
//! * `str.strip()` / `str.split()` use CPython's whitespace set, which is
//!   `char::is_whitespace` **plus `U+001C..=U+001F`** (bidi class B/S). See
//!   [`py_strip`]; `trim()` alone would leave a `\x1c` behind and change a
//!   truthiness test.
//! * `str.splitlines()` breaks on eleven separators, not just `\n`. See
//!   `py_first_line`.
//! * `_safe_json` is `json.dumps(value, default=str, separators=(",", ":"))` —
//!   `ensure_ascii` **defaults to `True`**, so a non-ASCII tool argument is
//!   measured as its six-character `\uXXXX` escape. That is
//!   `pyjson::dumps_compact`, not `pyjson::dumps_http`, and the difference is
//!   worth real tokens on a tool input full of box-drawing characters.

use rusqlite::Connection;
use serde_json::{Map, Value};
use stax_memory::pyjson;

use crate::pyops::char_prefix;

/// `_PREVIEW_CHARS` — the per-message content preview cap, in code points.
const PREVIEW_CHARS: usize = 240;

/// `_PATH_INPUT_KEYS` — probed in order; the first string-and-non-blank value
/// is the call's path-ish argument.
const PATH_INPUT_KEYS: [&str; 5] = [
    "file_path",
    "filePath",
    "notebook_path",
    "notebookPath",
    "path",
];

/// The generic fallback's second probe list, after `_input_path` misses.
const GENERIC_TEXT_KEYS: [&str; 5] = ["pattern", "query", "url", "command", "description"];

/// `sudo`/`time`/… — command prefixes `_first_command_word` steps over.
const COMMAND_PREFIXES: [&str; 5] = ["sudo", "time", "env", "nice", "nohup"];

// ── CPython string primitives ────────────────────────────────────────────────

/// `str.isspace()` for one character.
///
/// CPython's whitespace set is Unicode `White_Space` **plus** `U+001C..=U+001F`
/// (the file/group/record/unit separators, whose bidirectional class is `B`/`S`).
/// `char::is_whitespace` is `White_Space` alone, so `"\u{1c}x".trim()` keeps the
/// separator where `"\x1cx".strip()` drops it — and the result is fed to a
/// truthiness test, not just to a display.
#[must_use]
pub fn py_isspace(ch: char) -> bool {
    ch.is_whitespace() || matches!(ch, '\u{1c}'..='\u{1f}')
}

/// `str.strip()` — both ends, CPython's whitespace set.
#[must_use]
pub fn py_strip(text: &str) -> &str {
    text.trim_matches(py_isspace)
}

/// `str.split()` with no argument — split on runs of whitespace, no empties.
fn py_split_whitespace(text: &str) -> impl Iterator<Item = &str> {
    text.split(py_isspace).filter(|part| !part.is_empty())
}

/// `str.splitlines()[0]` — the prefix before the first line boundary.
///
/// CPython breaks on eleven separators, not one: `\n \v \f \r \x1c \x1d \x1e
/// \x85 \u2028 \u2029` (and `\r\n` as a unit, which this does not need to
/// distinguish because it only ever wants the first line). Every caller has
/// already established the string is non-empty after [`py_strip`], which is
/// what makes Python's unguarded `[0]` safe there and here.
fn py_first_line(text: &str) -> &str {
    let boundary = |ch: char| {
        matches!(
            ch,
            '\n' | '\u{b}'
                | '\u{c}'
                | '\r'
                | '\u{1c}'
                | '\u{1d}'
                | '\u{1e}'
                | '\u{85}'
                | '\u{2028}'
                | '\u{2029}'
        )
    };
    text.find(boundary).map_or(text, |idx| &text[..idx])
}

// ── defensive JSON helpers (`services/playback.py`) ──────────────────────────

/// `playback._loads` — `None`/`""` and any parse failure are `None`.
fn loads(blob: Option<&str>) -> Option<Value> {
    // `if not blob: return None` — Python truthiness, so the empty string is a
    // miss and never reaches the parser.
    let blob = blob.filter(|text| !text.is_empty())?;
    serde_json::from_str(blob).ok()
}

/// `playback._envelope` — the top-level transcript object, or `{}`.
fn envelope(raw_json: Option<&str>) -> Value {
    match loads(raw_json) {
        Some(value @ Value::Object(_)) => value,
        // `return obj if isinstance(obj, dict) else {}` — a bare array or
        // scalar at the top of `raw_json` is an empty envelope, not an error.
        _ => Value::Object(Map::new()),
    }
}

/// `playback._content_blocks` — `envelope["message"]["content"]` when both legs
/// are the right type, else `[]`.
fn content_blocks(env: &Value) -> &[Value] {
    static NONE: &[Value] = &[];
    env.get("message")
        .filter(|msg| msg.is_object())
        .and_then(|msg| msg.get("content"))
        .and_then(Value::as_array)
        .map_or(NONE, Vec::as_slice)
}

/// `_safe_json` — `json.dumps(value, default=str, separators=(",", ":"))`.
///
/// `ensure_ascii` is left at its default `True`, which is why this is
/// [`pyjson::dumps_compact`] (the CLI writer) and **not**
/// [`pyjson::dumps_http`]. The result is only ever *measured*, never sent, so
/// the flag shows up as a token count rather than as a response byte.
///
/// Python's `except (TypeError, ValueError)` fallback to `str(value)` is
/// unreachable from here: `value` is always a JSON object recovered from
/// `json.loads`, which has no unserialisable members and no cycles.
fn safe_json(value: &Value) -> String {
    pyjson::dumps_compact(value)
}

// ── token estimation ─────────────────────────────────────────────────────────

/// `_estimate_tokens` — `chars/4`, with a `+1` on any non-empty text.
///
/// The `+1` exists so a turn that carried real content never reports zero
/// tokens. Note that `build_context_timeline` calls this **twice** per message
/// and adds the results, so a message with both text and tool calls collects
/// the `+1` twice. Inherited as written: it is the reference's arithmetic and
/// the running total is a shape, not an invoice.
fn estimate_tokens(text: &str) -> i64 {
    if text.is_empty() {
        return 0;
    }
    // `len(text) // 4 + 1` — `len` on a `str` is code points.
    i64::try_from(text.chars().count() / 4).unwrap_or(i64::MAX) + 1
}

// ── tool-call extraction ─────────────────────────────────────────────────────

/// One `(name, input)` pair; `input` is always an object, per
/// `tinput if isinstance(tinput, dict) else {}`.
#[derive(Debug, Clone)]
struct ToolCall {
    name: String,
    input: Value,
}

/// `tinput if isinstance(tinput, dict) else {}`.
fn object_or_empty(value: Option<&Value>) -> Value {
    match value {
        Some(value @ Value::Object(_)) => value.clone(),
        _ => Value::Object(Map::new()),
    }
}

/// `_tool_calls_from_envelope` — every `tool_use` block in `raw_json`.
///
/// `raw_json` is authoritative because it carries the full tool *input*; the
/// derived `tools_json` column holds only names.
fn tool_calls_from_envelope(env: &Value) -> Vec<ToolCall> {
    let mut out = Vec::new();
    for blk in content_blocks(env) {
        // `if not isinstance(blk, dict) or blk.get("type") != "tool_use"`.
        if !blk.is_object() || blk.get("type").and_then(Value::as_str) != Some("tool_use") {
            continue;
        }
        // `if not isinstance(name, str) or not name: continue` — a non-string
        // or empty name drops the block entirely.
        let Some(name) = blk
            .get("name")
            .and_then(Value::as_str)
            .filter(|n| !n.is_empty())
        else {
            continue;
        };
        out.push(ToolCall {
            name: name.to_owned(),
            input: object_or_empty(blk.get("input")),
        });
    }
    out
}

/// `_tool_calls_from_tools_json` — the fallback, accepting both shapes the tree
/// carries: `["Edit", "Read"]` (names only) and
/// `[{"name": "Edit", "input": {…}}]` (some fixtures and adapters).
fn tool_calls_from_tools_json(tools_json: Option<&str>) -> Vec<ToolCall> {
    // `if not tools_json: return []`.
    let Some(text) = tools_json.filter(|t| !t.is_empty()) else {
        return Vec::new();
    };
    // `except (json.JSONDecodeError, TypeError, ValueError): return []`.
    let Ok(parsed) = serde_json::from_str::<Value>(text) else {
        return Vec::new();
    };
    // `if not isinstance(parsed, list): return []`.
    let Some(entries) = parsed.as_array() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries {
        match entry {
            // `if isinstance(entry, str) and entry`.
            Value::String(name) if !name.is_empty() => out.push(ToolCall {
                name: name.clone(),
                input: Value::Object(Map::new()),
            }),
            Value::Object(_) => {
                if let Some(name) = entry
                    .get("name")
                    .and_then(Value::as_str)
                    .filter(|n| !n.is_empty())
                {
                    out.push(ToolCall {
                        name: name.to_owned(),
                        input: object_or_empty(entry.get("input")),
                    });
                }
            }
            _ => {}
        }
    }
    out
}

/// `_tool_calls_for_row` — the envelope wins; `tools_json` is consulted only
/// when the envelope produced **nothing at all**.
fn tool_calls_for_row(raw_json: Option<&str>, tools_json: Option<&str>) -> Vec<ToolCall> {
    let calls = tool_calls_from_envelope(&envelope(raw_json));
    if calls.is_empty() {
        return tool_calls_from_tools_json(tools_json);
    }
    calls
}

// ── preview formatting ───────────────────────────────────────────────────────

/// `_preview` — the turn's text, or a `[Tool a, Tool b]` stand-in.
///
/// Assistant turns that are pure tool calls (and tool-result user turns) often
/// have an empty `content_text`; without the stand-in the timeline would be a
/// column of blanks.
fn preview(content: &str, tool_labels: &[String]) -> String {
    let stripped = py_strip(content);
    let text = if stripped.is_empty() && !tool_labels.is_empty() {
        format!("[{}]", tool_labels.join(", "))
    } else {
        stripped.to_owned()
    };
    // `text.replace("\r\n", "\n")` — CRLF only; a lone `\r` survives.
    let text = text.replace("\r\n", "\n");
    // `len(text) <= _PREVIEW_CHARS` — code points, and the cut keeps 239 of
    // them so the appended `…` lands the total back on 240.
    if text.chars().count() <= PREVIEW_CHARS {
        return text;
    }
    let mut out = char_prefix(&text, PREVIEW_CHARS - 1);
    out.push('…');
    out
}

// ── `playback.summarize_tool_call` and its handler table ─────────────────────

/// `_short_path` — trim a path to its last two components for display.
fn short_path(path: &str) -> String {
    let normalised = path.replace('\\', "/");
    // `.rstrip("/")` strips *every* trailing slash, not one.
    let normalised = normalised.trim_end_matches('/');
    let parts: Vec<&str> = normalised.split('/').filter(|p| !p.is_empty()).collect();
    if parts.len() <= 2 {
        // `"/".join(parts) if parts else path` — an all-separator input falls
        // back to the ORIGINAL string, slashes and all.
        if parts.is_empty() {
            return path.to_owned();
        }
        return parts.join("/");
    }
    parts[parts.len() - 2..].join("/")
}

/// `_first_command_word` — the operation a `Bash` call actually runs.
///
/// `cd /tmp && pytest -q` → `pytest`; `FOO=1 sudo make` → `make`. Best effort
/// and never raising, exactly as the reference advertises.
fn first_command_word(cmd: &str) -> String {
    let mut text = py_strip(cmd).to_owned();
    if text.is_empty() {
        return String::new();
    }
    // Skip leading `cd … &&` / `cd … ;` segments — plumbing, not the operation.
    for sep in ["&&", ";"] {
        // `while True: head, _, rest = text.partition(sep)` — an absent
        // separator gives `rest == ""`, which fails `if rest and …` and breaks,
        // so "no separator left" and "nothing after it" leave by the same door.
        while let Some((head, rest)) = text.split_once(sep) {
            // `head.strip().split()[:1] == ["cd"]`.
            if rest.is_empty() || py_split_whitespace(head).next() != Some("cd") {
                break;
            }
            let remainder = py_strip(rest).to_owned();
            text = remainder;
        }
    }
    let mut tokens: Vec<&str> = py_split_whitespace(&text).collect();
    while let Some(first) = tokens.first().copied() {
        // `VAR=value` env assignment, or a known no-op prefix.
        let is_assignment = first.contains('=')
            && !first.starts_with('-')
            && !first.split('=').next().unwrap_or_default().contains('/');
        if !COMMAND_PREFIXES.contains(&first) && !is_assignment {
            break;
        }
        tokens.remove(0);
    }
    // `tokens[0] if tokens else text.split()[0]` — the fallback re-splits the
    // ORIGINAL text, so a command that is nothing but env assignments reports
    // the first assignment rather than an empty string. Bug-for-bug.
    tokens.first().map_or_else(
        || {
            py_split_whitespace(&text)
                .next()
                .unwrap_or_default()
                .to_owned()
        },
        |token| (*token).to_owned(),
    )
}

/// `_input_path` — the first `_PATH_INPUT_KEYS` entry holding a non-blank string.
fn input_path(inp: &Map<String, Value>) -> Option<&str> {
    for key in PATH_INPUT_KEYS {
        if let Some(value) = inp.get(key).and_then(Value::as_str)
            && !py_strip(value).is_empty()
        {
            return Some(value);
        }
    }
    None
}

/// `_mcp_label` — `mcp__github__create_pr` → `github.create_pr`.
fn mcp_label(rest: &str, tool_name: &str) -> String {
    match rest.split_once("__") {
        // `rest.split("__", 1)` with maxsplit=1 — only the FIRST separator.
        Some((server, tool)) => format!("{server}.{tool}"),
        // `return rest or tool_name` — a bare `mcp__` falls back to the name.
        None if rest.is_empty() => tool_name.to_owned(),
        None => rest.to_owned(),
    }
}

/// `a or b or c` over dict lookups, with Python truthiness: `""` falls through.
///
/// The last operand is returned even when it too is falsy, which is what makes
/// `inp.get("skill") or inp.get("command")` yield `""` (not `None`) when both
/// keys are present and empty. The callers then re-test with `isinstance`, so
/// the distinction never escapes — but reproducing it is cheaper than proving
/// it cannot matter.
fn or_nonempty<'a>(inp: &'a Map<String, Value>, keys: &[&str]) -> Option<&'a Value> {
    for key in keys {
        if let Some(value) = inp.get(*key)
            && !is_falsy(value)
        {
            return Some(value);
        }
    }
    keys.last().and_then(|key| inp.get(*key))
}

/// Python truthiness for the JSON values `or` can see here.
fn is_falsy(value: &Value) -> bool {
    match value {
        Value::Null => true,
        Value::Bool(flag) => !flag,
        Value::Number(number) => number.as_f64() == Some(0.0),
        Value::String(text) => text.is_empty(),
        Value::Array(items) => items.is_empty(),
        Value::Object(members) => members.is_empty(),
    }
}

/// `_SUMMARY_HANDLERS.get(name)` plus the `try/except` wrapped around calling it.
enum Summary {
    /// The handler returned this label.
    Label(String),
    /// The handler raised — `except Exception: return name`.
    Raised,
    /// `.get(name)` was `None`; fall through to the generic tail.
    NoHandler,
}

/// A string argument sliced to 60 code points, or the `except` branch.
///
/// Two of the table's lambdas index straight into `inp.get(k, "")` with no
/// `isinstance` guard (`ToolSearch`, `TaskCreate`). CPython's behaviour splits
/// by JSON type:
///
/// * a string slices, and an absent key yields `""` — both reproduced;
/// * a number / bool / null / object raises `TypeError` on `[:60]`, caught by
///   `summarize_tool_call`'s `except Exception`, yielding the bare tool name —
///   [`Summary::Raised`];
/// * an **array** slices successfully and interpolates CPython's `repr` of the
///   sliced list. Not reproduced — see DIV-108. The port takes the same
///   `Raised` branch as the other non-strings.
fn sliced_str_arg(inp: &Map<String, Value>, key: &str) -> Result<String, Summary> {
    match inp.get(key) {
        None => Ok(String::new()),
        Some(Value::String(text)) => Ok(char_prefix(text, 60)),
        Some(_) => Err(Summary::Raised),
    }
}

/// The 27 entries of `_SUMMARY_HANDLERS`, as one `match`.
fn summary_handler(name: &str, inp: &Map<String, Value>) -> Summary {
    // `_sum_file_op(verb)` — `f"{verb} {_short_path(p)}" if p else verb`.
    let file_op = |verb: &str| {
        input_path(inp).map_or_else(
            || verb.to_owned(),
            |path| format!("{verb} {}", short_path(path)),
        )
    };
    // `isinstance(v, str) and v` — a non-empty string under `key`.
    let nonempty_str = |key: &str| {
        inp.get(key)
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
    };
    let label = match name {
        "Read" | "Write" | "Edit" | "MultiEdit" | "NotebookRead" => file_op(name),
        "NotebookEdit" => {
            // `inp.get("notebook_path") or inp.get("notebookPath") or inp.get("file_path")`.
            let picked = or_nonempty(inp, &["notebook_path", "notebookPath", "file_path"]);
            match picked.and_then(Value::as_str).filter(|p| !p.is_empty()) {
                Some(path) => format!("NotebookEdit {}", short_path(path)),
                None => "NotebookEdit".to_owned(),
            }
        }
        "Bash" => match inp.get("command").and_then(Value::as_str) {
            // `if not isinstance(cmd, str) or not cmd.strip(): return "Bash"`.
            Some(cmd) if !py_strip(cmd).is_empty() => {
                format!("Bash: {}", first_command_word(cmd))
            }
            _ => "Bash".to_owned(),
        },
        "BashOutput" | "KillBash" | "KillShell" | "ExitPlanMode" | "EnterPlanMode"
        | "AskUserQuestion" | "TaskUpdate" | "TaskGet" | "TaskList" => name.to_owned(),
        "Glob" => {
            let base =
                nonempty_str("pattern").map_or_else(|| "Glob".to_owned(), |p| format!("Glob {p}"));
            // `if isinstance(p, str) and p.strip()` — the *blank* test gates,
            // but the UNTRIMMED value is what `_short_path` receives.
            match inp.get("path").and_then(Value::as_str) {
                Some(path) if !py_strip(path).is_empty() => {
                    format!("{base} in {}", short_path(path))
                }
                _ => base,
            }
        }
        "Grep" => {
            nonempty_str("pattern").map_or_else(|| "Grep".to_owned(), |p| format!("Grep {p}"))
        }
        "LS" => nonempty_str("path").map_or_else(
            || "LS".to_owned(),
            |path| format!("LS {}", short_path(path)),
        ),
        // `"Task": _sum_task, "Agent": _sum_task` — one handler, two names.
        "Task" | "Agent" => {
            let described = inp
                .get("description")
                .and_then(Value::as_str)
                .map(py_strip)
                .filter(|desc| !desc.is_empty())
                .map(|desc| format!("Task: {}", char_prefix(desc, 60)));
            let subagent = || {
                inp.get("subagent_type")
                    .and_then(Value::as_str)
                    .map(py_strip)
                    .filter(|sub| !sub.is_empty())
                    // Note the asymmetry: `description` is sliced to 60 and
                    // `subagent_type` is NOT. Inherited.
                    .map(|sub| format!("Task: {sub}"))
            };
            described
                .or_else(subagent)
                .unwrap_or_else(|| "Task".to_owned())
        }
        "WebFetch" => nonempty_str("url")
            .map_or_else(|| "WebFetch".to_owned(), |url| format!("WebFetch {url}")),
        "WebSearch" => nonempty_str("query").map_or_else(
            || "WebSearch".to_owned(),
            // The conditional expression evaluates `isinstance(q, str) and q`
            // FIRST, so unlike `ToolSearch` this one can never raise.
            |query| format!("WebSearch: {}", char_prefix(query, 60)),
        ),
        "TodoWrite" => {
            let count = inp
                .get("todos")
                .and_then(Value::as_array)
                .map_or(0, Vec::len);
            let plural = if count == 1 { "" } else { "s" };
            format!("TodoWrite ({count} todo{plural})")
        }
        "Skill" => {
            // `inp.get("skill") or inp.get("command")`.
            let picked = or_nonempty(inp, &["skill", "command"]);
            match picked.and_then(Value::as_str).filter(|s| !s.is_empty()) {
                Some(skill) => format!("Skill: {skill}"),
                None => "Skill".to_owned(),
            }
        }
        "ToolSearch" => match sliced_str_arg(inp, "query") {
            // `.rstrip(": ")` strips any trailing run of `:` and space, so an
            // absent query collapses to the bare tool name.
            Ok(query) => rstrip_set(&format!("ToolSearch: {query}"), &[':', ' ']),
            Err(raised) => return raised,
        },
        "TaskCreate" => match sliced_str_arg(inp, "description") {
            Ok(description) => rstrip_set(&format!("TaskCreate: {description}"), &[':', ' ']),
            Err(raised) => return raised,
        },
        "SendMessage" => match inp.get("to") {
            // No slice here, so CPython never raises — but also no `repr` port
            // for a non-string `to`. DIV-108.
            None => rstrip_set("SendMessage → ", &['→', ' ']),
            Some(Value::String(to)) => rstrip_set(&format!("SendMessage → {to}"), &['→', ' ']),
            Some(_) => return Summary::Raised,
        },
        _ => return Summary::NoHandler,
    };
    Summary::Label(label)
}

/// `str.rstrip(chars)` — strip any trailing run of the given characters.
fn rstrip_set(text: &str, chars: &[char]) -> String {
    text.trim_end_matches(|ch| chars.contains(&ch)).to_owned()
}

/// `playback.summarize_tool_call(tool_name, tool_input)` — a one-line,
/// human-readable label for a tool call.
///
/// Table-driven over the names Claude Code emits; an unknown name falls back to
/// `"<Tool> <first path-ish arg>"` so newly-added tools still read sensibly, and
/// `mcp__server__tool` collapses to `server.tool`. Never raises.
///
/// The reference's third parameter (`tool_result_text`) is omitted: this
/// module's only call site passes it as `None`, no shipped handler reads it,
/// and carrying a dead parameter through the ported signature would be
/// inventing API.
#[must_use]
pub fn summarize_tool_call(tool_name: &str, tool_input: &Value) -> String {
    // `if not isinstance(tool_name, str) or not tool_name` — the non-string leg
    // is unreachable through a `&str`; the empty leg is not.
    if tool_name.is_empty() {
        return "(unparseable)".to_owned();
    }
    let empty = Map::new();
    let inp = tool_input.as_object().unwrap_or(&empty);

    if let Some(rest) = tool_name.strip_prefix("mcp__") {
        return mcp_label(rest, tool_name);
    }

    match summary_handler(tool_name, inp) {
        Summary::Label(label) => return label,
        // `except Exception: return name` — defensive, never crash a row.
        Summary::Raised => return tool_name.to_owned(),
        Summary::NoHandler => {}
    }

    // Generic fallback: surface a path-ish argument if there is one.
    if let Some(path) = input_path(inp) {
        return format!("{tool_name} {}", short_path(path));
    }
    for key in GENERIC_TEXT_KEYS {
        if let Some(value) = inp.get(key).and_then(Value::as_str) {
            let stripped = py_strip(value);
            if !stripped.is_empty() {
                // `v.strip().splitlines()[0]` then `[:60]`.
                let snippet = py_first_line(stripped);
                return format!("{tool_name}: {}", char_prefix(snippet, 60));
            }
        }
    }
    tool_name.to_owned()
}

// ── session resolution ───────────────────────────────────────────────────────

/// `_resolve_session` — `session_id` → `(session_fk, session_id)`, most recent.
///
/// `session_id` is unique per *project*, not globally, so the newest row wins.
/// `NULLS LAST` is load-bearing and SQLite honours it: a session that never got
/// a `last_ts` must not outrank one that has one. The SQL is the reference
/// statement verbatim (LAW 5).
///
/// `except sqlite3.Error: return None` — every driver exception derives from
/// `sqlite3.Error`, so this swallow is total in practice. The port swallows the
/// column conversions too; Python performs those *outside* the `try` and would
/// raise on a NULL `id`, which `INTEGER PRIMARY KEY` makes unreachable.
fn resolve_session(conn: &Connection, session_id: &str) -> Option<(i64, String)> {
    conn.query_row(
        "SELECT id, session_id FROM sessions WHERE session_id = ? \
         ORDER BY last_ts DESC NULLS LAST, id DESC LIMIT 1",
        [session_id],
        |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
    )
    .ok()
}

// ── public API ───────────────────────────────────────────────────────────────

/// `empty_context` — the canonical empty-but-valid reconstruction.
///
/// Every producer (missing session, out-of-scope fence, empty session) emits
/// THIS shape and this key order, so a consumer can rely on the keys
/// unconditionally. `total_tokens` is Python's `int` `0`, which renders as `0`
/// and not `0.0` (LAW 3).
#[must_use]
pub fn empty_context(session_id: &str, at_seq: Option<i64>, warnings: &[String]) -> Value {
    let mut obj = Map::new();
    obj.insert("session_id".to_owned(), Value::from(session_id));
    obj.insert("at_seq".to_owned(), at_seq.map_or(Value::Null, Value::from));
    obj.insert("message_count".to_owned(), Value::from(0));
    obj.insert("total_tokens".to_owned(), Value::from(0));
    obj.insert("events".to_owned(), Value::Array(Vec::new()));
    obj.insert(
        "warnings".to_owned(),
        Value::Array(warnings.iter().map(|w| Value::from(w.clone())).collect()),
    );
    Value::Object(obj)
}

/// One row of the message sweep.
struct MessageRow {
    seq: i64,
    role: Option<String>,
    content_text: Option<String>,
    tools_json: Option<String>,
    raw_json: Option<String>,
}

/// The reference statement, verbatim (LAW 5).
///
/// `messages` is a UNION-ALL VIEW over the monthly partitions. A direct
/// `session_fk = ?` predicate pushes into each arm's `(session_fk, seq)` index;
/// a join against `sessions` would materialise the whole view, which is the
/// July hang. Nothing here needs a join, and nothing here should grow one.
///
/// # Errors
/// Whatever SQLite rejects — the caller turns it into a `warnings` note rather
/// than letting it out.
fn read_messages(conn: &Connection, session_fk: i64) -> rusqlite::Result<Vec<MessageRow>> {
    let mut stmt = conn.prepare(
        "SELECT seq, role, content_text, tools_json, raw_json \
         FROM messages WHERE session_fk = ? ORDER BY seq",
    )?;
    let rows = stmt.query_map([session_fk], |row| {
        Ok(MessageRow {
            seq: row.get(0)?,
            role: row.get(1)?,
            content_text: row.get(2)?,
            tools_json: row.get(3)?,
            raw_json: row.get(4)?,
        })
    })?;
    rows.collect()
}

/// `build_context_timeline` — the full (uncut) reconstruction for `session_id`.
///
/// The heavy, cache-friendly unit: walk the session's messages in `seq` order,
/// one event per message, with a running token total. **Safe on a miss** — an
/// unknown session or an unreadable store yields [`empty_context`] with a note,
/// which is the branch `routes/context_replay.rs` relies on for its
/// unknown-session path.
#[must_use]
pub fn build_context_timeline(conn: &Connection, session_id: &str) -> Value {
    let Some((session_fk, sid)) = resolve_session(conn, session_id) else {
        return empty_context(
            session_id,
            None,
            &[format!("session not found in store: {session_id}")],
        );
    };

    let rows = match read_messages(conn, session_fk) {
        Ok(rows) => rows,
        // `except sqlite3.Error as exc:` — the driver's message is INTERPOLATED
        // into the warning. rusqlite prints the same SQLite string as CPython
        // for a `SqliteFailure` (both forward the engine's own text), which is
        // the only kind this statement can raise on a real store.
        Err(err) => return empty_context(&sid, None, &[format!("could not read messages: {err}")]),
    };

    let mut events: Vec<Value> = Vec::with_capacity(rows.len());
    // `cumulative = 0` then `cumulative += tokens` — an int accumulator, so no
    // compensated summation applies (LAW 3 is about float `sum()`).
    let mut cumulative: i64 = 0;
    for row in &rows {
        // `r["content_text"] or ""` — NULL and "" are the same empty string.
        let content = row.content_text.as_deref().unwrap_or_default();
        let calls = tool_calls_for_row(row.raw_json.as_deref(), row.tools_json.as_deref());
        let tool_labels: Vec<String> = calls
            .iter()
            .map(|call| summarize_tool_call(&call.name, &call.input))
            .collect();
        // `"".join(name + _safe_json(inp) for name, inp in calls)`.
        let mut tool_payload = String::new();
        for call in &calls {
            tool_payload.push_str(&call.name);
            tool_payload.push_str(&safe_json(&call.input));
        }
        // TWO estimates added, so a turn with text AND tools collects the `+1`
        // twice. The reference's arithmetic, character for character.
        let tokens = estimate_tokens(content) + estimate_tokens(&tool_payload);
        cumulative += tokens;

        let mut event = Map::new();
        event.insert("seq".to_owned(), Value::from(row.seq));
        event.insert(
            "role".to_owned(),
            Value::from(row.role.clone().unwrap_or_default()),
        );
        event.insert(
            "content_preview".to_owned(),
            Value::from(preview(content, &tool_labels)),
        );
        event.insert("tokens".to_owned(), Value::from(tokens));
        event.insert("cumulative_tokens".to_owned(), Value::from(cumulative));
        event.insert(
            "tool_calls".to_owned(),
            Value::Array(tool_labels.into_iter().map(Value::from).collect()),
        );
        events.push(Value::Object(event));
    }

    let mut obj = Map::new();
    obj.insert("session_id".to_owned(), Value::from(sid));
    // The full build always reports `at_seq: None`; the slice stamps the real
    // cutoff.
    obj.insert("at_seq".to_owned(), Value::Null);
    obj.insert(
        "message_count".to_owned(),
        Value::from(i64::try_from(events.len()).unwrap_or(i64::MAX)),
    );
    obj.insert("total_tokens".to_owned(), Value::from(cumulative));
    obj.insert("events".to_owned(), Value::Array(events));
    obj.insert("warnings".to_owned(), Value::Array(Vec::new()));
    Value::Object(obj)
}

/// `slice_context_timeline` — cut a full timeline to `seq <= at_seq` and retotal.
///
/// `at_seq is None` returns the whole timeline. Because events are `seq`-ordered
/// and each carries its own prefix-sum `cumulative_tokens`, the slice's
/// `total_tokens` is just the **last retained event's cumulative** — there is
/// no re-summation, which is also why a non-monotonic `seq` ordering would give
/// a surprising total. Pure: the returned value is fresh.
#[must_use]
pub fn slice_context_timeline(full: &Value, at_seq: Option<i64>) -> Value {
    // `full.get("events") or []` — missing, null and non-list all become [].
    let events = full.get("events").and_then(Value::as_array);
    let kept: Vec<Value> = match (events, at_seq) {
        (None, _) => Vec::new(),
        (Some(events), None) => events.clone(),
        (Some(events), Some(cutoff)) => events
            .iter()
            // `int(e.get("seq", 0)) <= at_seq` — an event with no `seq` sorts
            // as 0 and is kept for any non-negative cutoff.
            .filter(|event| event.get("seq").and_then(Value::as_i64).unwrap_or(0) <= cutoff)
            .cloned()
            .collect(),
    };
    let total = kept
        .last()
        .and_then(|event| event.get("cumulative_tokens"))
        .cloned()
        // `kept[-1]["cumulative_tokens"] if kept else 0` — the int `0`.
        .unwrap_or_else(|| Value::from(0));

    let mut obj = Map::new();
    obj.insert(
        "session_id".to_owned(),
        // `full.get("session_id", "")` — the default is the empty *string*.
        full.get("session_id")
            .cloned()
            .unwrap_or_else(|| Value::from("")),
    );
    obj.insert("at_seq".to_owned(), at_seq.map_or(Value::Null, Value::from));
    obj.insert(
        "message_count".to_owned(),
        Value::from(i64::try_from(kept.len()).unwrap_or(i64::MAX)),
    );
    obj.insert("total_tokens".to_owned(), total);
    obj.insert("events".to_owned(), Value::Array(kept));
    obj.insert(
        "warnings".to_owned(),
        // `list(full.get("warnings") or [])`.
        full.get("warnings")
            .and_then(Value::as_array)
            .map_or_else(|| Value::Array(Vec::new()), |w| Value::Array(w.clone())),
    );
    Value::Object(obj)
}

/// `reconstruct_context` — build, then slice.
///
/// The composed entry point direct callers (the CLI, wave 8's
/// `stackunderflow context-replay`) use. The route splits the two so it can
/// cache the build and re-slice per scrub tick.
#[must_use]
pub fn reconstruct_context(conn: &Connection, session_id: &str, at_seq: Option<i64>) -> Value {
    slice_context_timeline(&build_context_timeline(conn, session_id), at_seq)
}

#[cfg(test)]
mod tests {
    use super::*;
    use stax_memory::pyjson::dumps_http;

    /// Two sessions plus the messages the assertions below read. `messages` is
    /// a plain table here: the statement under test cannot tell the partitioned
    /// view from a table, and a fixture needing sixteen monthly partitions to
    /// answer a unit test is a fixture nobody reads.
    fn seed(conn: &Connection) {
        conn.execute_batch(
            r#"CREATE TABLE projects (id INTEGER PRIMARY KEY, slug TEXT);
             CREATE TABLE sessions (
                 id INTEGER PRIMARY KEY, project_id INTEGER NOT NULL,
                 session_id TEXT NOT NULL, last_ts TEXT);
             CREATE TABLE messages (
                 id INTEGER PRIMARY KEY, session_fk INTEGER NOT NULL, seq INTEGER NOT NULL,
                 timestamp TEXT, role TEXT NOT NULL DEFAULT '',
                 content_text TEXT NOT NULL DEFAULT '',
                 tools_json TEXT NOT NULL DEFAULT '[]', raw_json TEXT NOT NULL DEFAULT '');
             INSERT INTO projects (id, slug) VALUES (1, '-p-one'), (2, '-p-two');
             INSERT INTO sessions (id, project_id, session_id, last_ts) VALUES
                 (10, 1, 'sess-a', '2026-01-01T00:00:00Z'),
                 (11, 2, 'sess-b', NULL);
             INSERT INTO messages (session_fk, seq, role, content_text, tools_json, raw_json) VALUES
                 (10, 1, 'user', 'hello there', '[]', '{}'),
                 (10, 2, 'assistant', '', '["Edit"]',
                  '{"message":{"content":[{"type":"tool_use","name":"Edit","input":{"file_path":"/a/b/routes/cost.py"}}]}}'),
                 (10, 3, 'user', 'and then', '[]', 'not json at all');"#,
        )
        .expect("seed");
    }

    fn store() -> Connection {
        let conn = Connection::open_in_memory().expect("open");
        seed(&conn);
        conn
    }

    #[test]
    fn an_unknown_session_is_the_empty_shape_carrying_the_requested_id_and_a_warning() {
        let conn = store();
        let built = build_context_timeline(&conn, "no-such-session");
        assert_eq!(
            dumps_http(&built),
            r#"{"session_id":"no-such-session","at_seq":null,"message_count":0,"total_tokens":0,"events":[],"warnings":["session not found in store: no-such-session"]}"#
        );
    }

    #[test]
    fn a_store_with_no_tables_at_all_swallows_the_sqlite_error_instead_of_propagating() {
        // The route drops `schema.apply(conn)` (DIV-106); this is the branch
        // that makes the omission payload-neutral. A `?` here would be a 500
        // where Python answers 200.
        let conn = Connection::open_in_memory().expect("open");
        let built = build_context_timeline(&conn, "sess-a");
        assert_eq!(
            dumps_http(&built),
            r#"{"session_id":"sess-a","at_seq":null,"message_count":0,"total_tokens":0,"events":[],"warnings":["session not found in store: sess-a"]}"#
        );
    }

    #[test]
    fn a_session_with_no_messages_is_zero_events_and_no_warning_at_all() {
        let conn = store();
        let built = build_context_timeline(&conn, "sess-b");
        assert_eq!(
            dumps_http(&built),
            r#"{"session_id":"sess-b","at_seq":null,"message_count":0,"total_tokens":0,"events":[],"warnings":[]}"#
        );
    }

    #[test]
    fn the_full_build_renders_the_dict_literals_key_order_on_both_levels() {
        let conn = store();
        let built = build_context_timeline(&conn, "sess-a");
        let keys: Vec<&String> = built.as_object().expect("object").keys().collect();
        assert_eq!(
            keys,
            vec![
                "session_id",
                "at_seq",
                "message_count",
                "total_tokens",
                "events",
                "warnings"
            ]
        );
        let event = &built["events"][0];
        let keys: Vec<&String> = event.as_object().expect("object").keys().collect();
        assert_eq!(
            keys,
            vec![
                "seq",
                "role",
                "content_preview",
                "tokens",
                "cumulative_tokens",
                "tool_calls"
            ]
        );
    }

    #[test]
    fn cumulative_tokens_is_a_prefix_sum_and_total_tokens_is_its_last_entry() {
        let conn = store();
        let built = build_context_timeline(&conn, "sess-a");
        let events = built["events"].as_array().expect("array");
        assert_eq!(events.len(), 3);
        let mut running = 0;
        for event in events {
            running += event["tokens"].as_i64().expect("int");
            assert_eq!(event["cumulative_tokens"].as_i64(), Some(running));
        }
        assert_eq!(built["total_tokens"].as_i64(), Some(running));
        assert_eq!(built["message_count"].as_i64(), Some(3));
    }

    #[test]
    fn the_token_estimate_adds_a_separate_plus_one_for_the_text_and_the_tool_payload() {
        // seq 1: text "hello there" (11 chars), no tools → 11/4 + 1 = 3.
        let conn = store();
        let built = build_context_timeline(&conn, "sess-a");
        assert_eq!(built["events"][0]["tokens"].as_i64(), Some(3));

        // seq 2: no text, so only the tool payload is measured — `Edit` plus
        // the compact JSON of its input.
        let payload = format!(
            "Edit{}",
            safe_json(&serde_json::json!({"file_path": "/a/b/routes/cost.py"}))
        );
        assert_eq!(payload.chars().count(), 39);
        assert_eq!(built["events"][1]["tokens"].as_i64(), Some(39 / 4 + 1));
    }

    #[test]
    fn the_whole_fixture_renders_the_bytes_the_reference_renders() {
        // The oracle: this literal is `json.dumps(..., ensure_ascii=False,
        // separators=(",", ":"))` of `build_context_timeline` run against this
        // same fixture under CPython 3.12 / the checked-in
        // `services/context_replay.py`, pasted verbatim. Every number in it —
        // the two `+1`s inside the tokens, the prefix sums, the `_short_path`
        // label, the bracketed preview — is a place a plausible port drifts.
        let conn = store();
        let built = build_context_timeline(&conn, "sess-a");
        assert_eq!(
            dumps_http(&built),
            r#"{"session_id":"sess-a","at_seq":null,"message_count":3,"total_tokens":16,"events":[{"seq":1,"role":"user","content_preview":"hello there","tokens":3,"cumulative_tokens":3,"tool_calls":[]},{"seq":2,"role":"assistant","content_preview":"[Edit routes/cost.py]","tokens":10,"cumulative_tokens":13,"tool_calls":["Edit routes/cost.py"]},{"seq":3,"role":"user","content_preview":"and then","tokens":3,"cumulative_tokens":16,"tool_calls":[]}],"warnings":[]}"#
        );
        assert_eq!(
            dumps_http(&slice_context_timeline(&built, Some(2))),
            r#"{"session_id":"sess-a","at_seq":2,"message_count":2,"total_tokens":13,"events":[{"seq":1,"role":"user","content_preview":"hello there","tokens":3,"cumulative_tokens":3,"tool_calls":[]},{"seq":2,"role":"assistant","content_preview":"[Edit routes/cost.py]","tokens":10,"cumulative_tokens":13,"tool_calls":["Edit routes/cost.py"]}],"warnings":[]}"#
        );
    }

    #[test]
    fn a_turn_with_no_text_previews_its_tool_labels_in_brackets() {
        let conn = store();
        let built = build_context_timeline(&conn, "sess-a");
        // `_short_path` keeps the last two components.
        assert_eq!(
            built["events"][1]["content_preview"].as_str(),
            Some("[Edit routes/cost.py]")
        );
        assert_eq!(
            built["events"][1]["tool_calls"],
            serde_json::json!(["Edit routes/cost.py"])
        );
    }

    #[test]
    fn malformed_raw_json_is_an_empty_envelope_and_never_an_error() {
        let conn = store();
        let built = build_context_timeline(&conn, "sess-a");
        assert_eq!(built["events"][2]["tool_calls"], serde_json::json!([]));
        assert_eq!(
            built["events"][2]["content_preview"].as_str(),
            Some("and then")
        );
    }

    #[test]
    fn the_envelope_wins_and_tools_json_is_read_only_when_it_yields_nothing() {
        // seq 2 has BOTH a `tool_use` block and a `tools_json` of ["Edit"]; the
        // envelope's version carries the input, so the label has the path.
        let conn = store();
        let built = build_context_timeline(&conn, "sess-a");
        assert_eq!(
            built["events"][1]["tool_calls"],
            serde_json::json!(["Edit routes/cost.py"])
        );

        // Names-only fallback, from `tools_json` alone.
        let calls = tool_calls_for_row(Some("{}"), Some(r#"["Edit","Read"]"#));
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].name, "Edit");
        assert_eq!(calls[0].input, serde_json::json!({}));

        // The object-array shape some adapters carry, including a non-dict
        // `input` that collapses to `{}`, an entry with no name, and a scalar.
        let calls = tool_calls_for_row(
            None,
            Some(r#"[{"name":"Edit","input":{"path":"x"}},{"name":"Bad"},{"name":""},7]"#),
        );
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].input, serde_json::json!({"path": "x"}));
        assert_eq!(calls[1].input, serde_json::json!({}));
    }

    #[test]
    fn the_resolver_prefers_the_newest_last_ts_and_sorts_nulls_last() {
        let conn = store();
        conn.execute_batch(
            "INSERT INTO sessions (id, project_id, session_id, last_ts) VALUES
                 (12, 2, 'sess-a', NULL),
                 (13, 2, 'sess-a', '2026-02-02T00:00:00Z');",
        )
        .expect("insert");
        // Without NULLS LAST, SQLite sorts NULL FIRST under DESC and id 12 wins.
        assert_eq!(
            resolve_session(&conn, "sess-a"),
            Some((13, "sess-a".to_owned()))
        );
    }

    #[test]
    fn a_cutoff_keeps_the_prefix_and_reuses_the_last_kept_events_cumulative() {
        let conn = store();
        let full = build_context_timeline(&conn, "sess-a");
        let sliced = slice_context_timeline(&full, Some(2));
        assert_eq!(sliced["message_count"].as_i64(), Some(2));
        assert_eq!(sliced["at_seq"].as_i64(), Some(2));
        assert_eq!(
            sliced["total_tokens"],
            full["events"][1]["cumulative_tokens"]
        );
    }

    #[test]
    fn a_cutoff_below_every_seq_keeps_nothing_and_totals_pythons_int_zero() {
        let conn = store();
        let full = build_context_timeline(&conn, "sess-a");
        let sliced = slice_context_timeline(&full, Some(0));
        // `0` and not `0.0` — the int/float split is visible in the bytes.
        assert_eq!(
            dumps_http(&sliced),
            r#"{"session_id":"sess-a","at_seq":0,"message_count":0,"total_tokens":0,"events":[],"warnings":[]}"#
        );
    }

    #[test]
    fn a_cutoff_of_none_is_the_whole_timeline_with_at_seq_left_null() {
        let conn = store();
        let full = build_context_timeline(&conn, "sess-a");
        let sliced = slice_context_timeline(&full, None);
        assert_eq!(sliced["events"], full["events"]);
        assert_eq!(sliced["at_seq"], Value::Null);
        assert_eq!(sliced["total_tokens"], full["total_tokens"]);
    }

    #[test]
    fn a_negative_cutoff_is_legal_and_keeps_nothing_rather_than_erroring() {
        // FastAPI coerces `?at=-3` to a real int and hands it straight through
        // — measured, not assumed.
        let conn = store();
        let sliced = reconstruct_context(&conn, "sess-a", Some(-3));
        assert_eq!(sliced["message_count"].as_i64(), Some(0));
        assert_eq!(sliced["at_seq"].as_i64(), Some(-3));
    }

    #[test]
    fn slicing_an_unknown_sessions_empty_build_keeps_the_warning_and_stamps_at_seq() {
        // The route's unknown-session path, end to end: no cache, no fence,
        // `build_context_timeline` on an id that resolves to nothing.
        let conn = store();
        let body = reconstruct_context(&conn, "ghost", Some(9));
        assert_eq!(
            dumps_http(&body),
            r#"{"session_id":"ghost","at_seq":9,"message_count":0,"total_tokens":0,"events":[],"warnings":["session not found in store: ghost"]}"#
        );
    }

    #[test]
    fn the_preview_cuts_at_two_hundred_thirty_nine_code_points_plus_an_ellipsis() {
        let long = "é".repeat(500);
        let cut = preview(&long, &[]);
        // Code points, not bytes: a byte slice would cut mid-character.
        assert_eq!(cut.chars().count(), PREVIEW_CHARS);
        assert!(cut.ends_with('…'));
        assert_eq!(
            cut.chars().filter(|ch| *ch == 'é').count(),
            PREVIEW_CHARS - 1
        );

        // Exactly at the cap is a no-op.
        let exact = "a".repeat(PREVIEW_CHARS);
        assert_eq!(preview(&exact, &[]), exact);
    }

    #[test]
    fn the_preview_folds_crlf_but_leaves_a_lone_carriage_return_alone() {
        assert_eq!(preview("a\r\nb", &[]), "a\nb");
        assert_eq!(preview("a\rb", &[]), "a\rb");
        // Whitespace-only content with tools still shows the tools.
        assert_eq!(preview("   ", &["Bash: ls".to_owned()]), "[Bash: ls]");
        // …and with no tools it is simply empty.
        assert_eq!(preview("   ", &[]), "");
    }

    #[test]
    fn an_mcp_name_collapses_to_server_dot_tool_and_ignores_the_handler_table() {
        assert_eq!(
            summarize_tool_call("mcp__github__create_pr", &serde_json::json!({})),
            "github.create_pr"
        );
        // Only the FIRST separator splits.
        assert_eq!(
            summarize_tool_call("mcp__a__b__c", &serde_json::json!({})),
            "a.b__c"
        );
        assert_eq!(
            summarize_tool_call("mcp__", &serde_json::json!({})),
            "mcp__"
        );
        assert_eq!(
            summarize_tool_call("", &serde_json::json!({})),
            "(unparseable)"
        );
    }

    #[test]
    fn a_bash_command_reports_the_first_real_word_past_cd_and_env_plumbing() {
        let bash = |cmd: &str| summarize_tool_call("Bash", &serde_json::json!({"command": cmd}));
        assert_eq!(bash("cd /tmp && pytest -q"), "Bash: pytest");
        assert_eq!(bash("pytest tests/"), "Bash: pytest");
        assert_eq!(bash("FOO=1 sudo make install"), "Bash: make");
        // A `/` before the `=` means it is a path, not an assignment.
        assert_eq!(bash("/usr/bin/x=y run"), "Bash: /usr/bin/x=y");
        assert_eq!(bash("   "), "Bash");
        assert_eq!(bash("cd /tmp;ls -la"), "Bash: ls");
        // Nothing but assignments: `tokens` empties and the fallback re-splits
        // the original text, so the first assignment comes back. Bug-for-bug.
        assert_eq!(bash("A=1 B=2"), "Bash: A=1");
    }

    #[test]
    fn the_unknown_tool_fallback_prefers_a_path_then_a_first_line_snippet() {
        assert_eq!(
            summarize_tool_call("Brandnew", &serde_json::json!({"path": "/x/y/z/f.rs"})),
            "Brandnew z/f.rs"
        );
        // `v.strip().splitlines()[0]` — the SECOND line is never shown.
        assert_eq!(
            summarize_tool_call("Brandnew", &serde_json::json!({"query": "  one\ntwo  "})),
            "Brandnew: one"
        );
        assert_eq!(
            summarize_tool_call("Brandnew", &serde_json::json!({})),
            "Brandnew"
        );
        // A non-object `tool_input` is `{}` — never a panic.
        assert_eq!(summarize_tool_call("Brandnew", &Value::Null), "Brandnew");
    }

    #[test]
    fn the_rstrip_lambdas_collapse_to_the_bare_name_when_the_argument_is_absent() {
        assert_eq!(
            summarize_tool_call("ToolSearch", &serde_json::json!({})),
            "ToolSearch"
        );
        assert_eq!(
            summarize_tool_call("ToolSearch", &serde_json::json!({"query": "select:Read"})),
            "ToolSearch: select:Read"
        );
        assert_eq!(
            summarize_tool_call("TaskCreate", &serde_json::json!({})),
            "TaskCreate"
        );
        assert_eq!(
            summarize_tool_call("SendMessage", &serde_json::json!({})),
            "SendMessage"
        );
        assert_eq!(
            summarize_tool_call("SendMessage", &serde_json::json!({"to": "ctxreplay"})),
            "SendMessage → ctxreplay"
        );
        // A non-string, non-array argument raises inside the lambda and the
        // `except Exception` hands back the bare tool name.
        assert_eq!(
            summarize_tool_call("ToolSearch", &serde_json::json!({"query": 7})),
            "ToolSearch"
        );
    }

    #[test]
    fn the_handler_table_covers_the_shapes_a_transliteration_would_get_wrong() {
        let call = |name: &str, input: Value| summarize_tool_call(name, &input);
        // `or` chains fall through an EMPTY string, not just a missing key.
        assert_eq!(
            call(
                "NotebookEdit",
                serde_json::json!({"notebook_path": "", "file_path": "/a/b/c.ipynb"})
            ),
            "NotebookEdit b/c.ipynb"
        );
        assert_eq!(
            call("Skill", serde_json::json!({"skill": "", "command": "run"})),
            "Skill: run"
        );
        // Singular vs plural, and a non-list `todos` counting as zero.
        assert_eq!(
            call("TodoWrite", serde_json::json!({"todos": [1]})),
            "TodoWrite (1 todo)"
        );
        assert_eq!(
            call("TodoWrite", serde_json::json!({"todos": "nope"})),
            "TodoWrite (0 todos)"
        );
        // Glob's `path` gates on the STRIPPED value but shortens the raw one.
        assert_eq!(
            call(
                "Glob",
                serde_json::json!({"pattern": "*.rs", "path": "/x/y/z"})
            ),
            "Glob *.rs in y/z"
        );
        assert_eq!(call("Glob", serde_json::json!({"path": "  "})), "Glob");
        // `description` is sliced to 60; `subagent_type` is not.
        assert_eq!(
            call("Task", serde_json::json!({"description": "  ship it  "})),
            "Task: ship it"
        );
        assert_eq!(
            call("Agent", serde_json::json!({"subagent_type": "Explore"})),
            "Task: Explore"
        );
        // The zero-argument entries.
        assert_eq!(call("KillShell", serde_json::json!({"x": 1})), "KillShell");
        assert_eq!(call("TaskList", serde_json::json!({})), "TaskList");
    }

    #[test]
    fn short_path_keeps_two_components_and_falls_back_to_the_raw_string() {
        assert_eq!(short_path("/Users/x/repo/routes/cost.py"), "routes/cost.py");
        assert_eq!(short_path("routes/cost.py"), "routes/cost.py");
        assert_eq!(short_path("cost.py"), "cost.py");
        assert_eq!(short_path("C:\\a\\b\\c.txt"), "b/c.txt");
        assert_eq!(short_path("/a/b/"), "a/b");
        // No components at all: the ORIGINAL string, separators intact.
        assert_eq!(short_path("///"), "///");
    }

    #[test]
    fn pythons_whitespace_set_includes_the_separators_rusts_trim_leaves_behind() {
        // `"\x1c".isspace()` is True in CPython and `char::is_whitespace` is
        // false, so `trim()` alone would call this string non-blank.
        assert_eq!(py_strip("\u{1c}\u{1f} x \u{1e}"), "x");
        assert!(!"\u{1c}".trim().is_empty());
        assert!(py_strip("\u{1c}").is_empty());
    }

    #[test]
    fn splitlines_breaks_on_more_than_a_newline() {
        assert_eq!(py_first_line("one\u{2028}two"), "one");
        assert_eq!(py_first_line("one\u{b}two"), "one");
        assert_eq!(py_first_line("one two"), "one two");
    }

    #[test]
    fn the_tool_payload_is_measured_with_ensure_ascii_true_not_the_http_writer() {
        // `_safe_json` is `json.dumps(...)` with the DEFAULT `ensure_ascii`, so
        // a single `é` costs six characters, not two bytes. Using the HTTP
        // writer here would undercount every non-ASCII tool argument.
        let value = serde_json::json!({"q": "é"});
        assert_eq!(safe_json(&value), r#"{"q":"\u00e9"}"#);
        assert_eq!(pyjson::dumps_http(&value), "{\"q\":\"é\"}");
    }
}
