//! `services/playback_fs.py` (617 ln) — playback v2, the virtual filesystem.
//!
//! v1 ([`crate::services::playback`]) answers "what did the agent do?". This
//! answers "what did the file *look like* at this moment?", by replaying the
//! session's `Read` / `Write` / `Edit` / `MultiEdit` / `NotebookEdit` calls in
//! `seq` order, up to and including the cutoff `at`.
//!
//! | Tool | Effect on the reconstructed content |
//! |---|---|
//! | `Read` | seeds the content from the matched `tool_result`, **once** |
//! | `Write` | replaces it wholesale; reconstruction becomes complete |
//! | `Edit` | `old_string → new_string`, or a warning and no change |
//! | `MultiEdit` | each `edits[i]` in order, same per-edit rules |
//! | `NotebookEdit` | accumulates a `{cell_id: source}` JSON map, never complete |
//!
//! # Three places a paraphrase silently diverges
//!
//! 1. **`_CAT_LINE_PREFIX` is a MULTILINE regex whose `\s` matches newlines.**
//!    `^\s*\d+\t` under `re.M` can therefore consume the preceding blank lines
//!    as part of the match. [`strip_read_line_numbers`] is a hand-rolled scan
//!    that reproduces the scan order (`^` asserts at 0 and after every `\n`,
//!    matches are non-overlapping and left to right) rather than an
//!    approximation of the intent.
//! 2. **`str.replace(old, new, 1)` vs `str.replace(old, new)`** is the whole
//!    `replace_all` flag, and Rust spells them `replacen(…, 1)` and `replace`.
//!    Both are left-to-right, non-overlapping, and both are no-ops on an empty
//!    `old` — which is why the empty-`old_string` guard exists upstream.
//! 3. **`json.dumps(current, indent=2, sort_keys=True)`** sorts **recursively**
//!    and by *code point*. `serde_json` is built with `preserve_order` in this
//!    workspace, so its maps are insertion-ordered and the sort has to be done
//!    explicitly — [`sort_keys_deep`].
//!
//! # Cutoff comparison is four cases, not one
//!
//! `_ts_le` compares a message timestamp with the cutoff, and CPython's
//! naive/aware rules make it a four-way branch:
//!
//! | message | cutoff | compared as |
//! |---|---|---|
//! | aware | aware | instants |
//! | naive | aware | the message is *assumed* UTC, then instants |
//! | aware | naive | the message's **wall clock**, tz discarded |
//! | naive | naive | wall clocks |
//!
//! Collapsing the third row into an instant comparison shifts every file's
//! cutoff by the offset on a store written from a non-UTC machine.

use std::collections::HashMap;

use rusqlite::Connection;
use serde_json::{Map, Value};
use stax_etl::stats::pydatetime::PyDateTime;
use stax_etl::stats::pytext::{is_py_space, py_strip, py_truthy};

use crate::services::playback::{
    MessageRow, content_blocks, envelope, loads, parse_iso, read_rows, resolve_session,
    stringify_result_content,
};

/// `_FS_TOOLS` — everything else (Bash, Glob, Grep, …) is read-only or off-FS.
const FS_TOOLS: [&str; 5] = ["Read", "Write", "Edit", "MultiEdit", "NotebookEdit"];

/// What went wrong, mapped by the route to a status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReconstructError {
    /// `UnknownSession` — the route answers `404`.
    UnknownSession(String),
    /// `FsReconstructionError` — the route answers `422`.
    Malformed(String),
}

impl ReconstructError {
    /// `str(e)` — the exception's message, which becomes the `detail`.
    #[must_use]
    pub fn detail(&self) -> &str {
        match self {
            Self::UnknownSession(text) | Self::Malformed(text) => text,
        }
    }
}

// ── reconstruction state ────────────────────────────────────────────────────

/// `_FileState` — the mutable per-file accumulator the replay updates.
#[derive(Debug, Clone)]
struct FileState {
    path: String,
    /// `None` until the first Read / Write / Edit.
    content: Option<String>,
    last_modified_ts: Option<String>,
    operations_applied: Vec<String>,
    reconstruction_complete: bool,
    /// `_call_indices` — the per-tool-name counter behind `"Edit#0"`.
    call_indices: Vec<(String, i64)>,
}

impl FileState {
    fn new(path: &str) -> Self {
        Self {
            path: path.to_owned(),
            content: None,
            last_modified_ts: None,
            operations_applied: Vec::new(),
            reconstruction_complete: false,
            call_indices: Vec::new(),
        }
    }

    /// `next_op_label` — `"<tool>#<n>"`, `n` counted per tool name *per file*.
    fn next_op_label(&mut self, tool_name: &str) -> String {
        let index = match self
            .call_indices
            .iter_mut()
            .find(|(name, _)| name == tool_name)
        {
            Some(entry) => {
                let current = entry.1;
                entry.1 += 1;
                current
            }
            None => {
                self.call_indices.push((tool_name.to_owned(), 1));
                0
            }
        };
        format!("{tool_name}#{index}")
    }
}

// ── tool-result index ───────────────────────────────────────────────────────

/// `_index_results` — the stripped-down twin of v1's: text only, no error flag.
///
/// Deliberately *not* the v1 function. Python declares its own here and the two
/// disagree on one thing that matters: this one stores an entry for every
/// `tool_use_id` it sees with no `is_error` bookkeeping and no timestamp, so it
/// cannot be expressed as a projection of the other without carrying dead work
/// through every fs request.
fn index_results(rows: &[MessageRow]) -> HashMap<String, String> {
    let mut out: HashMap<String, String> = HashMap::new();
    for row in rows {
        if row.role != "user" {
            continue;
        }
        let env = envelope(row.raw_json.as_deref());
        for block in content_blocks(&env) {
            let Value::Object(map) = block else { continue };
            if map.get("type").and_then(Value::as_str) != Some("tool_result") {
                continue;
            }
            let Some(tuid) = map
                .get("tool_use_id")
                .and_then(Value::as_str)
                .filter(|tuid| !tuid.is_empty())
            else {
                continue;
            };
            out.insert(
                tuid.to_owned(),
                stringify_result_content(map.get("content")),
            );
        }
    }
    out
}

/// `_strip_read_line_numbers` — drop Claude Code's `     N\t` prefix.
///
/// The Read tool returns `cat -n`-formatted text; the real file bytes carry no
/// leading `<spaces><lineno><tab>`, so stripping it lets a later `Edit` (which
/// quotes the *real* text) match the seed content.
///
/// Two details the regex hides and a rewrite would lose:
///
/// * the guard is `_CAT_LINE_PREFIX.match(text.splitlines()[0])` — the FIRST
///   line decides whether the whole blob is treated as numbered, so a file
///   whose second line happens to look numbered is left alone;
/// * `\s` in a `str` pattern is CPython's `Py_UNICODE_ISSPACE`, which includes
///   `\n` — so under `re.M` a match starting at a line boundary can swallow the
///   *following* blank lines before the digits. That is reproduced, not fixed.
#[must_use]
pub fn strip_read_line_numbers(text: &str) -> String {
    if text.is_empty() {
        return text.to_owned();
    }
    // `text.splitlines()[0]` — the first line only, and `.match` anchors at its
    // start. `\s*` cannot cross a boundary inside a single line.
    let first = first_line(text);
    if match_cat_prefix(first, 0).is_none() {
        return text.to_owned();
    }
    // `re.sub` with `re.M`: scan left to right, try only at `^` positions, take
    // non-overlapping matches.
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut index = 0;
    while index < bytes.len() {
        let at_line_start = index == 0 || bytes[index - 1] == b'\n';
        if at_line_start && let Some(end) = match_cat_prefix(text, index) {
            index = end;
            continue;
        }
        // Advance one whole character — the string is UTF-8 and a byte step
        // could split one.
        let step = text[index..].chars().next().map_or(1, |ch| ch.len_utf8());
        out.push_str(&text[index..index + step]);
        index += step;
    }
    out
}

/// `^\s*\d+\t` anchored at `start`; the byte index just past the match.
fn match_cat_prefix(text: &str, start: usize) -> Option<usize> {
    let mut cursor = start;
    // `\s*` — greedy, and whitespace and digits are disjoint so no
    // backtracking is possible.
    for ch in text[cursor..].chars() {
        if !is_py_space(ch) {
            break;
        }
        cursor += ch.len_utf8();
    }
    let digits_start = cursor;
    for ch in text[cursor..].chars() {
        if !ch.is_ascii_digit() {
            break;
        }
        cursor += ch.len_utf8();
    }
    // `\d+` needs at least one, and `\t` must follow.
    if cursor == digits_start || text.as_bytes().get(cursor) != Some(&b'\t') {
        return None;
    }
    Some(cursor + 1)
}

/// `str.splitlines()[0]` — see `playback::py_splitlines_first` for the boundary
/// set. Duplicated rather than shared because that one is private to v1 and the
/// dedup pass has not been asked to hoist it.
fn first_line(text: &str) -> &str {
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

// ── per-tool replay handlers ────────────────────────────────────────────────

/// `_apply_read` — seed the initial content from the matched `tool_result`.
///
/// A Read *after* an Edit does not re-seed: that would mask the edits already
/// replayed. Only the first Read on a path with no content is honoured.
fn apply_read(
    state: &mut FileState,
    result_text: Option<&str>,
    op_label: &str,
    ts: &str,
) -> Vec<String> {
    state.operations_applied.push(op_label.to_owned());
    state.last_modified_ts = Some(ts.to_owned());
    if state.content.is_some() {
        return Vec::new();
    }
    let Some(text) = result_text else {
        return vec![format!(
            "{}: Read result missing — no initial content captured",
            state.path
        )];
    };
    state.content = Some(strip_read_line_numbers(text));
    state.reconstruction_complete = true;
    Vec::new()
}

/// `_apply_write` — replace the whole content from `input.content`.
fn apply_write(state: &mut FileState, new_content: &str, op_label: &str, ts: &str) {
    state.content = Some(new_content.to_owned());
    state.last_modified_ts = Some(ts.to_owned());
    state.reconstruction_complete = true;
    state.operations_applied.push(op_label.to_owned());
}

/// `_apply_edit` — one substitution against the current content.
fn apply_edit(
    state: &mut FileState,
    old_string: &str,
    new_string: &str,
    op_label: &str,
    ts: &str,
    replace_all: bool,
) -> Vec<String> {
    state.operations_applied.push(op_label.to_owned());
    state.last_modified_ts = Some(ts.to_owned());
    let Some(content) = state.content.clone() else {
        // Partial reconstruction: no Read/Write was seen, so `new_string` is
        // the best-effort working content.
        state.content = Some(new_string.to_owned());
        state.reconstruction_complete = false;
        return vec![format!(
            "{}: no initial Read or Write before first Edit — \
             reconstruction is from edit deltas only",
            state.path
        )];
    };
    if old_string.is_empty() {
        // The Edit tool requires a non-empty `old_string`; treating one as a
        // wildcard would explode the content.
        return vec![format!(
            "{}: {op_label} has empty old_string — substitution skipped",
            state.path
        )];
    }
    if !content.contains(old_string) {
        return vec![format!(
            "{}: {op_label} old_string did not match — substitution skipped",
            state.path
        )];
    }
    state.content = Some(if replace_all {
        content.replace(old_string, new_string)
    } else {
        content.replacen(old_string, new_string, 1)
    });
    Vec::new()
}

/// `_apply_multi_edit` — each edit in order, warnings aggregated.
///
/// `operations_applied` gets a single `MultiEdit#N` token: the individual
/// sub-edits are not separately observable from the timeline.
fn apply_multi_edit(
    state: &mut FileState,
    edits: &[Value],
    op_label: &str,
    ts: &str,
) -> Vec<String> {
    state.operations_applied.push(op_label.to_owned());
    state.last_modified_ts = Some(ts.to_owned());
    let mut warnings: Vec<String> = Vec::new();
    for (index, edit) in edits.iter().enumerate() {
        let Value::Object(map) = edit else {
            warnings.push(format!(
                "{}: {op_label} edit[{index}] is not an object — skipped",
                state.path
            ));
            continue;
        };
        let old = map.get("old_string");
        let new = map.get("new_string");
        let (Some(Value::String(old)), Some(Value::String(new))) = (old, new) else {
            warnings.push(format!(
                "{}: {op_label} edit[{index}] missing old_string/new_string — skipped",
                state.path
            ));
            continue;
        };
        // `bool(e.get("replace_all", False))` — Python truthiness over any type.
        let replace_all = map.get("replace_all").is_some_and(py_truthy);
        let Some(content) = state.content.clone() else {
            // The first sub-edit with no prior content seeds `new_string`;
            // later sub-edits in the SAME MultiEdit then apply normally.
            state.content = Some(new.clone());
            state.reconstruction_complete = false;
            warnings.push(format!(
                "{}: no initial Read or Write before {op_label} — \
                 reconstruction is from edit deltas only",
                state.path
            ));
            continue;
        };
        if old.is_empty() {
            warnings.push(format!(
                "{}: {op_label} edit[{index}] has empty old_string — skipped",
                state.path
            ));
            continue;
        }
        if !content.contains(old.as_str()) {
            warnings.push(format!(
                "{}: {op_label} edit[{index}] old_string did not match — skipped",
                state.path
            ));
            continue;
        }
        state.content = Some(if replace_all {
            content.replace(old.as_str(), new)
        } else {
            content.replacen(old.as_str(), new, 1)
        });
    }
    warnings
}

/// `_apply_notebook_edit` — accumulate a `{cell_id: source}` map.
///
/// The notebook's true bytes are an `.ipynb` tree that never appears in the
/// transcript, so `reconstruction_complete` stays `false` forever here.
fn apply_notebook_edit(
    state: &mut FileState,
    tool_input: &Map<String, Value>,
    op_label: &str,
    ts: &str,
) -> Vec<String> {
    state.operations_applied.push(op_label.to_owned());
    state.last_modified_ts = Some(ts.to_owned());
    // `tool_input.get("cell_id") or tool_input.get("cellId") or ""`.
    let cell_id = ["cell_id", "cellId"]
        .into_iter()
        .find_map(|key| tool_input.get(key).filter(|value| py_truthy(value)));
    let new_source = ["new_source", "newSource"]
        .into_iter()
        .find_map(|key| tool_input.get(key).filter(|value| py_truthy(value)));
    let Some(Value::String(new_source)) = new_source else {
        return vec![format!(
            "{}: {op_label} missing new_source — cell content not captured",
            state.path
        )];
    };
    // `json.loads(state.content)`, with a non-dict (or a failure) resetting to
    // `{}` — the accumulated map is rebuilt from its own rendering each time.
    let mut current: Map<String, Value> = match state.content.as_deref() {
        None => Map::new(),
        Some(text) => match loads(Some(text)) {
            Some(Value::Object(map)) => map,
            _ => Map::new(),
        },
    };
    let edit_mode = ["edit_mode", "editMode"]
        .into_iter()
        .find_map(|key| tool_input.get(key).filter(|value| py_truthy(value)))
        .map_or_else(|| "replace".to_owned(), stax_etl::stats::pytext::py_str);
    // `str(cell_id) if cell_id else f"cell_{len(current)}"`.
    let key = match cell_id {
        Some(value) => stax_etl::stats::pytext::py_str(value),
        None => format!("cell_{}", current.len()),
    };
    if edit_mode == "delete" {
        current.shift_remove(&key);
    } else {
        current.insert(key, Value::String(new_source.clone()));
    }
    state.content = Some(stax_memory::pyjson::dumps_pretty(&sort_keys_deep(
        &Value::Object(current),
    )));
    state.reconstruction_complete = false;
    Vec::new()
}

/// `json.dumps(..., sort_keys=True)` — recursively, by code point.
///
/// `serde_json` is built with `preserve_order` here, so a `Map` keeps insertion
/// order and the sort must be explicit. Rust's `str` ordering is by UTF-8
/// bytes, which for valid UTF-8 is the same order as by code point — the
/// comparison CPython makes.
fn sort_keys_deep(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort_unstable();
            let mut sorted = Map::new();
            for key in keys {
                sorted.insert(key.clone(), sort_keys_deep(&map[key]));
            }
            Value::Object(sorted)
        }
        Value::Array(items) => Value::Array(items.iter().map(sort_keys_deep).collect()),
        other => other.clone(),
    }
}

// ── path extraction ─────────────────────────────────────────────────────────

/// `_tool_file_path` — the path the tool call operated on.
///
/// NOT `playback::_input_path`: the key list differs (`NotebookEdit` looks at
/// `filePath` too, and neither list contains the other's order), so the two
/// stay separate exactly as they are in Python.
fn tool_file_path(tool_name: &str, tool_input: &Map<String, Value>) -> Option<String> {
    let keys: &[&str] = if tool_name == "NotebookEdit" {
        &["notebook_path", "notebookPath", "file_path", "filePath"]
    } else {
        &["file_path", "filePath", "path"]
    };
    for key in keys {
        if let Some(Value::String(value)) = tool_input.get(*key)
            && !py_strip(value).is_empty()
        {
            return Some(value.clone());
        }
    }
    None
}

// ── core replay ─────────────────────────────────────────────────────────────

/// `_ts_le` — is the message timestamp `<= cutoff`? See the module docs for the
/// four naive/aware cases this branches on.
fn ts_le(ts: Option<&str>, cutoff: PyDateTime) -> bool {
    // `if not ts: return False` — an EMPTY timestamp excludes the message.
    let Some(ts) = ts.filter(|ts| !ts.is_empty()) else {
        return false;
    };
    let Some(dt) = parse_iso(Some(ts)) else {
        return false;
    };
    match (dt.offset_s, cutoff.offset_s) {
        // Naive message, aware cutoff: `dt.replace(tzinfo=UTC)`.
        (None, Some(_)) => PyDateTime {
            wall_us: dt.wall_us,
            offset_s: Some(0),
        }
        .cmp_instant(cutoff)
        .is_some_and(std::cmp::Ordering::is_le),
        // Aware message, naive cutoff: compare the WALL clocks, tz discarded.
        (Some(_), None) => dt.wall_us <= cutoff.wall_us,
        // Both naive or both aware: the plain `dt <= cutoff`.
        _ => dt
            .cmp_instant(cutoff)
            .is_some_and(std::cmp::Ordering::is_le),
    }
}

/// `_replay_session` — walk seq-ordered messages and build the per-file states.
///
/// Returns `(states in first-touch order, warnings in emission order)`.
fn replay_session(
    rows: &[MessageRow],
    cutoff: PyDateTime,
    path_filter: Option<&[String]>,
) -> (Vec<FileState>, Vec<String>) {
    let results = index_results(rows);
    // `states` is a plain dict: insertion-ordered by first touch, and that
    // order becomes the `files` object's key order in the response.
    let mut states: Vec<FileState> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();

    for row in rows {
        if row.role != "assistant" {
            continue;
        }
        if !ts_le(row.timestamp.as_deref(), cutoff) {
            continue;
        }
        let env = envelope(row.raw_json.as_deref());
        let ts = row.timestamp.clone().unwrap_or_default();
        for block in content_blocks(&env) {
            let Value::Object(map) = block else { continue };
            if map.get("type").and_then(Value::as_str) != Some("tool_use") {
                continue;
            }
            let Some(tname) = map.get("name").and_then(Value::as_str) else {
                continue;
            };
            if !FS_TOOLS.contains(&tname) {
                continue;
            }
            let Some(Value::Object(tinput)) = map.get("input") else {
                continue;
            };
            // `if not path: continue` — truthiness, so a blank path is skipped.
            let Some(path) = tool_file_path(tname, tinput) else {
                continue;
            };
            // `if path_filter is not None and path not in path_filter` — an
            // EXACT match against the tool's path argument, not a prefix test.
            if path_filter.is_some_and(|filter| !filter.contains(&path)) {
                continue;
            }

            let index = match states.iter().position(|state| state.path == path) {
                Some(index) => index,
                None => {
                    states.push(FileState::new(&path));
                    states.len() - 1
                }
            };
            let op_label = states[index].next_op_label(tname);
            let state = &mut states[index];

            // `results.get(tuid) if isinstance(tuid, str) and tuid else None`.
            let result_text = map
                .get("id")
                .and_then(Value::as_str)
                .filter(|tuid| !tuid.is_empty())
                .and_then(|tuid| results.get(tuid))
                .map(String::as_str);

            match tname {
                "Read" => warnings.extend(apply_read(state, result_text, &op_label, &ts)),
                "Write" => match tinput.get("content") {
                    Some(Value::String(content)) => {
                        apply_write(state, content, &op_label, &ts);
                    }
                    _ => {
                        warnings.push(format!("{path}: Write missing content string — skipped"));
                        // The op is still recorded, for visibility.
                        state.operations_applied.push(op_label.clone());
                        state.last_modified_ts = Some(ts.clone());
                    }
                },
                "Edit" => {
                    let old = tinput.get("old_string");
                    let new = tinput.get("new_string");
                    // `bool(tinput.get("replace_all", False))` — evaluated
                    // BEFORE the type check, though it cannot matter.
                    let replace_all = tinput.get("replace_all").is_some_and(py_truthy);
                    if let (Some(Value::String(old)), Some(Value::String(new))) = (old, new) {
                        warnings.extend(apply_edit(state, old, new, &op_label, &ts, replace_all));
                    } else {
                        warnings.push(format!(
                            "{path}: {op_label} missing old_string/new_string — skipped"
                        ));
                        state.operations_applied.push(op_label.clone());
                        state.last_modified_ts = Some(ts.clone());
                    }
                }
                "MultiEdit" => match tinput.get("edits") {
                    Some(Value::Array(edits)) => {
                        warnings.extend(apply_multi_edit(state, edits, &op_label, &ts));
                    }
                    _ => {
                        warnings.push(format!("{path}: {op_label} missing edits list — skipped"));
                        state.operations_applied.push(op_label.clone());
                        state.last_modified_ts = Some(ts.clone());
                    }
                },
                "NotebookEdit" => {
                    warnings.extend(apply_notebook_edit(state, tinput, &op_label, &ts));
                }
                _ => {}
            }
        }
    }

    (states, warnings)
}

/// `_normalize_paths` — the stripped, de-duplicated restriction set, or `None`.
fn normalize_paths(paths: Option<&[String]>) -> Option<Vec<String>> {
    let raw = paths.filter(|raw| !raw.is_empty())?;
    let mut cleaned: Vec<String> = Vec::new();
    for path in raw {
        let stripped = py_strip(path);
        if stripped.is_empty() {
            continue;
        }
        if !cleaned.iter().any(|seen| seen == stripped) {
            cleaned.push(stripped.to_owned());
        }
    }
    (!cleaned.is_empty()).then_some(cleaned)
}

// ── public entry ────────────────────────────────────────────────────────────

/// `reconstruct_fs_at` — the whole response body, shaped for the route.
///
/// ```text
/// {"session_id", "snapshot_ts", "files": {<path>: {…}}, "warnings": [...]}
/// ```
///
/// `snapshot_ts` is `str(at)` — the raw query value, **unstripped and
/// un-normalised**, so `?at=%20 2026-01-01 %20` echoes its own spaces back even
/// though the cutoff parsed fine. That is a wall-clock-free field: no `now()`
/// touches this payload, which is what makes a byte-for-byte case row possible
/// at all.
///
/// # Errors
/// [`ReconstructError::Malformed`] when `at` will not parse (422),
/// [`ReconstructError::UnknownSession`] when the session is absent (404).
///
/// # Panics
/// Never: `store_error` maps every SQLite failure onto an error value.
pub fn reconstruct_fs_at(
    conn: &Connection,
    session_id: &str,
    at: &str,
    paths: Option<&[String]>,
    include_content: bool,
) -> Result<Value, ReconstructError> {
    // ── parse cutoff ────────────────────────────────────────────────────
    let Some(cutoff) = parse_iso(Some(at)) else {
        // `f"…: {at!r}"` — a Python `repr` of the raw string, quotes and all.
        return Err(ReconstructError::Malformed(format!(
            "Could not parse 'at' as ISO-8601 / RFC-3339: {}",
            stax_core::queries::paths::py_repr(at)
        )));
    };
    let cutoff_str = at.to_owned();

    // ── resolve session ─────────────────────────────────────────────────
    let resolved = resolve_session(conn, session_id).map_err(store_error)?;
    let Some((session_fk, sid)) = resolved else {
        return Err(ReconstructError::UnknownSession(format!(
            "Session not found in store: {session_id}"
        )));
    };

    // ── load + replay ───────────────────────────────────────────────────
    let rows = read_rows(
        conn,
        "SELECT id, session_fk, seq, timestamp, role, raw_json \
         FROM messages WHERE session_fk = ? ORDER BY seq",
        &[&session_fk],
    )
    .map_err(store_error)?;
    let path_filter = normalize_paths(paths);
    let (states, warnings) = replay_session(&rows, cutoff, path_filter.as_deref());

    // ── pack response ───────────────────────────────────────────────────
    let mut files = Map::new();
    for state in states {
        // `content = st.content or ""` — truthiness, so `None` and `""` are the
        // same empty body and both report `byte_count: 0`.
        let content = state.content.unwrap_or_default();
        let mut entry = Map::new();
        entry.insert(
            "byte_count".to_owned(),
            Value::from(i64::try_from(content.len()).unwrap_or(i64::MAX)),
        );
        entry.insert(
            "last_modified_ts".to_owned(),
            state.last_modified_ts.map_or(Value::Null, Value::from),
        );
        entry.insert(
            "operations_applied".to_owned(),
            Value::Array(
                state
                    .operations_applied
                    .into_iter()
                    .map(Value::from)
                    .collect(),
            ),
        );
        entry.insert(
            "reconstruction_complete".to_owned(),
            Value::Bool(state.reconstruction_complete),
        );
        if include_content {
            entry.insert("content".to_owned(), Value::from(content));
        }
        files.insert(state.path, Value::Object(entry));
    }

    let mut payload = Map::new();
    payload.insert("session_id".to_owned(), Value::from(sid));
    payload.insert("snapshot_ts".to_owned(), Value::from(cutoff_str));
    payload.insert("files".to_owned(), Value::Object(files));
    payload.insert(
        "warnings".to_owned(),
        Value::Array(warnings.into_iter().map(Value::from).collect()),
    );
    Ok(Value::Object(payload))
}

/// A SQLite failure. Python has no `try` here, so a broken store is a 500 on
/// both sides; this carries the driver's message through the 422 channel rather
/// than panicking, and the route re-raises it as a 500.
fn store_error(err: rusqlite::Error) -> ReconstructError {
    ReconstructError::Malformed(err.to_string())
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
    fn the_cat_prefix_is_stripped_only_when_the_first_line_is_numbered() {
        assert_eq!(
            strip_read_line_numbers("     1\talpha\n     2\tbeta"),
            "alpha\nbeta"
        );
        // The first line decides: an unnumbered head leaves the blob alone.
        assert_eq!(
            strip_read_line_numbers("header\n     2\tbeta"),
            "header\n     2\tbeta"
        );
        assert_eq!(strip_read_line_numbers(""), "");
        assert_eq!(strip_read_line_numbers("plain"), "plain");
        // A digit with no tab is not a line number.
        assert_eq!(strip_read_line_numbers("  12 x"), "  12 x");
    }

    /// `\s` matches `\n`, so under `re.M` a match starting at a line boundary
    /// swallows the *following* blank lines. Reproduced, not fixed — and the
    /// exact answer was measured, because the scan order is not obvious.
    ///
    /// `"1\ta\n\n  2\tb"`: the first match is `[0,2)`. Index 3's `^` is FALSE
    /// (its predecessor is `a`), so that newline survives. Index 4's `^` is
    /// true, and there `\s*` eats `"\n  "` before the `2\t` — so the second
    /// newline and the indent vanish and the first does not.
    ///
    /// A per-line implementation would answer `"a\n\nb"`. A third blank line
    /// disappears the same way, which is what pins the spanning rather than an
    /// off-by-one.
    #[test]
    fn the_whitespace_class_spans_newlines_exactly_as_the_python_regex_does() {
        assert_eq!(strip_read_line_numbers("1\ta\n\n  2\tb"), "a\nb");
        assert_eq!(strip_read_line_numbers("1\ta\n\n\n  2\tb"), "a\nb");
    }

    #[test]
    fn the_op_label_counter_is_per_tool_and_per_file() {
        let mut state = FileState::new("/a");
        assert_eq!(state.next_op_label("Edit"), "Edit#0");
        assert_eq!(state.next_op_label("Read"), "Read#0");
        assert_eq!(state.next_op_label("Edit"), "Edit#1");
        let mut other = FileState::new("/b");
        assert_eq!(other.next_op_label("Edit"), "Edit#0");
    }

    #[test]
    fn an_edit_replaces_once_unless_replace_all_is_set() {
        let mut state = FileState::new("/a");
        state.content = Some("x x x".to_owned());
        assert!(apply_edit(&mut state, "x", "y", "Edit#0", "t", false).is_empty());
        assert_eq!(state.content.as_deref(), Some("y x x"));
        assert!(apply_edit(&mut state, "x", "z", "Edit#1", "t", true).is_empty());
        assert_eq!(state.content.as_deref(), Some("y z z"));
    }

    #[test]
    fn an_unmatched_or_empty_old_string_warns_and_changes_nothing() {
        let mut state = FileState::new("/a");
        state.content = Some("body".to_owned());
        let warnings = apply_edit(&mut state, "nope", "x", "Edit#0", "t", false);
        assert_eq!(
            warnings,
            vec!["/a: Edit#0 old_string did not match — substitution skipped"]
        );
        assert_eq!(state.content.as_deref(), Some("body"));
        let warnings = apply_edit(&mut state, "", "x", "Edit#1", "t", false);
        assert_eq!(
            warnings,
            vec!["/a: Edit#1 has empty old_string — substitution skipped"]
        );
        assert_eq!(state.content.as_deref(), Some("body"));
        // Both attempts still recorded an operation.
        assert_eq!(state.operations_applied, vec!["Edit#0", "Edit#1"]);
    }

    #[test]
    fn an_edit_with_no_prior_content_seeds_the_delta_and_marks_it_incomplete() {
        let mut state = FileState::new("/a");
        let warnings = apply_edit(&mut state, "old", "new", "Edit#0", "t", false);
        assert_eq!(state.content.as_deref(), Some("new"));
        assert!(!state.reconstruction_complete);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].ends_with("reconstruction is from edit deltas only"));
    }

    #[test]
    fn multi_edit_seeds_on_the_first_sub_edit_then_applies_the_rest_normally() {
        let mut state = FileState::new("/a");
        let edits = json!([
            {"old_string": "ignored", "new_string": "hello world"},
            {"old_string": "world", "new_string": "there"},
            {"old_string": "", "new_string": "x"},
            "not an object",
            {"old_string": "gone", "new_string": "y"},
            {"new_string": "z"}
        ]);
        let edits = match edits {
            Value::Array(items) => items,
            _ => unreachable!(),
        };
        let warnings = apply_multi_edit(&mut state, &edits, "MultiEdit#0", "t");
        assert_eq!(state.content.as_deref(), Some("hello there"));
        // One `MultiEdit#0` token for the whole call, not one per sub-edit.
        assert_eq!(state.operations_applied, vec!["MultiEdit#0"]);
        assert_eq!(warnings.len(), 5);
        assert!(warnings[1].contains("edit[2] has empty old_string"));
        assert!(warnings[2].contains("edit[3] is not an object"));
        assert!(warnings[3].contains("edit[4] old_string did not match"));
        assert!(warnings[4].contains("edit[5] missing old_string/new_string"));
    }

    #[test]
    fn a_notebook_edit_accumulates_a_sorted_two_space_json_map() {
        let mut state = FileState::new("/n.ipynb");
        let input = map(json!({"cell_id": "b", "new_source": "second"}));
        assert!(apply_notebook_edit(&mut state, &input, "NotebookEdit#0", "t").is_empty());
        let input = map(json!({"cellId": "a", "newSource": "first"}));
        assert!(apply_notebook_edit(&mut state, &input, "NotebookEdit#1", "t").is_empty());
        // `sort_keys=True` puts `a` before `b`, whatever the insertion order.
        assert_eq!(
            state.content.as_deref(),
            Some("{\n  \"a\": \"first\",\n  \"b\": \"second\"\n}")
        );
        // Never complete — the full notebook is never observed.
        assert!(!state.reconstruction_complete);
        // `delete` removes the cell.
        let input = map(json!({"cell_id": "a", "new_source": "x", "edit_mode": "delete"}));
        apply_notebook_edit(&mut state, &input, "NotebookEdit#2", "t");
        assert_eq!(state.content.as_deref(), Some("{\n  \"b\": \"second\"\n}"));
        // No `new_source` at all is a warning and no state change.
        let input = map(json!({"cell_id": "c"}));
        let warnings = apply_notebook_edit(&mut state, &input, "NotebookEdit#3", "t");
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("missing new_source"));
    }

    /// A cell with no id is keyed on the map's CURRENT size, so two of them in
    /// a row produce `cell_0` and `cell_1`.
    #[test]
    fn an_unnamed_notebook_cell_is_keyed_by_the_map_size() {
        let mut state = FileState::new("/n.ipynb");
        let input = map(json!({"new_source": "one"}));
        apply_notebook_edit(&mut state, &input, "NotebookEdit#0", "t");
        let input = map(json!({"new_source": "two"}));
        apply_notebook_edit(&mut state, &input, "NotebookEdit#1", "t");
        assert_eq!(
            state.content.as_deref(),
            Some("{\n  \"cell_0\": \"one\",\n  \"cell_1\": \"two\"\n}")
        );
    }

    #[test]
    fn the_cutoff_comparison_covers_all_four_naive_aware_cases() {
        let aware = parse_iso(Some("2026-01-01T12:00:00Z")).expect("aware");
        let naive = parse_iso(Some("2026-01-01T12:00:00")).expect("naive");
        // aware/aware — instants.
        assert!(ts_le(Some("2026-01-01T11:00:00Z"), aware));
        assert!(!ts_le(Some("2026-01-01T13:00:00Z"), aware));
        // naive message, aware cutoff — the message is assumed UTC.
        assert!(ts_le(Some("2026-01-01T11:00:00"), aware));
        // aware message, naive cutoff — the WALL clock, offset discarded. The
        // message names 20:00 UTC but reads 11:00 locally, so it is inside.
        assert!(ts_le(Some("2026-01-01T11:00:00-09:00"), naive));
        // naive/naive.
        assert!(ts_le(Some("2026-01-01T11:00:00"), naive));
        // Missing, empty and unparseable all exclude the message.
        assert!(!ts_le(None, aware));
        assert!(!ts_le(Some(""), aware));
        assert!(!ts_le(Some("junk"), aware));
    }

    #[test]
    fn the_path_key_list_differs_for_notebook_edit() {
        let input = map(json!({"filePath": "/a.ipynb"}));
        assert_eq!(
            tool_file_path("NotebookEdit", &input),
            Some("/a.ipynb".to_owned())
        );
        // `filePath` is on BOTH lists; `path` is only on the general one.
        let input = map(json!({"path": "/b"}));
        assert_eq!(tool_file_path("NotebookEdit", &input), None);
        assert_eq!(tool_file_path("Read", &input), Some("/b".to_owned()));
        // A blank path is falsy.
        let input = map(json!({"file_path": "   "}));
        assert_eq!(tool_file_path("Read", &input), None);
    }

    #[test]
    fn path_normalisation_strips_dedupes_and_collapses_to_none() {
        assert_eq!(normalize_paths(None), None);
        assert_eq!(normalize_paths(Some(&[])), None);
        assert_eq!(normalize_paths(Some(&["  ".to_owned()])), None);
        assert_eq!(
            normalize_paths(Some(&[" /a ".to_owned(), "/a".to_owned(), "/b".to_owned()])),
            Some(vec!["/a".to_owned(), "/b".to_owned()])
        );
    }

    // ── the whole entry, against a seeded store ─────────────────────────────

    fn seeded() -> Connection {
        let conn = Connection::open_in_memory().expect("open");
        conn.execute_batch(
            "CREATE TABLE sessions (id INTEGER PRIMARY KEY, project_id INTEGER,
                 session_id TEXT, last_ts TEXT);
             CREATE TABLE messages (id INTEGER PRIMARY KEY, session_fk INTEGER,
                 seq INTEGER, timestamp TEXT, role TEXT, raw_json TEXT);
             INSERT INTO sessions (id, project_id, session_id, last_ts)
                 VALUES (10, 1, 'sess', '2026-01-01T00:00:00Z');",
        )
        .expect("schema");
        let insert = |seq: i64, ts: &str, role: &str, raw: &Value| {
            conn.execute(
                "INSERT INTO messages (session_fk, seq, timestamp, role, raw_json)
                 VALUES (10, ?, ?, ?, ?)",
                rusqlite::params![seq, ts, role, raw.to_string()],
            )
            .expect("insert");
        };
        insert(
            1,
            "2026-01-01T00:00:00Z",
            "assistant",
            &json!({"message": {"content": [
                {"type": "tool_use", "id": "r1", "name": "Read",
                 "input": {"file_path": "/repo/a.py"}}]}}),
        );
        insert(
            2,
            "2026-01-01T00:00:01Z",
            "user",
            &json!({"message": {"content": [
                {"type": "tool_result", "tool_use_id": "r1",
                 "content": "     1\thello world\n     2\tsecond"}]}}),
        );
        insert(
            3,
            "2026-01-01T00:00:02Z",
            "assistant",
            &json!({"message": {"content": [
                {"type": "tool_use", "id": "e1", "name": "Edit",
                 "input": {"file_path": "/repo/a.py",
                           "old_string": "world", "new_string": "there"}}]}}),
        );
        insert(
            4,
            "2026-01-01T00:00:03Z",
            "assistant",
            &json!({"message": {"content": [
                {"type": "tool_use", "id": "w1", "name": "Write",
                 "input": {"file_path": "/repo/b.py", "content": "brand new"}}]}}),
        );
        conn
    }

    #[test]
    fn the_snapshot_replays_reads_and_edits_in_order() {
        let conn = seeded();
        let payload =
            reconstruct_fs_at(&conn, "sess", "2026-01-01T01:00:00Z", None, true).expect("ok");
        assert_eq!(
            stax_memory::pyjson::dumps_http(&payload),
            r#"{"session_id":"sess","snapshot_ts":"2026-01-01T01:00:00Z","files":{"/repo/a.py":{"byte_count":18,"last_modified_ts":"2026-01-01T00:00:02Z","operations_applied":["Read#0","Edit#0"],"reconstruction_complete":true,"content":"hello there\nsecond"},"/repo/b.py":{"byte_count":9,"last_modified_ts":"2026-01-01T00:00:03Z","operations_applied":["Write#0"],"reconstruction_complete":true,"content":"brand new"}},"warnings":[]}"#
        );
    }

    #[test]
    fn the_cutoff_excludes_later_calls_entirely() {
        let conn = seeded();
        let payload =
            reconstruct_fs_at(&conn, "sess", "2026-01-01T00:00:02Z", None, true).expect("ok");
        let files = payload["files"].as_object().expect("files");
        // `/repo/b.py` was written at 00:00:03 — after the cutoff.
        assert_eq!(files.len(), 1);
        assert_eq!(files["/repo/a.py"]["content"], json!("hello there\nsecond"));
        // A cutoff before everything yields an empty (but valid) snapshot.
        let payload =
            reconstruct_fs_at(&conn, "sess", "2025-01-01T00:00:00Z", None, true).expect("ok");
        assert_eq!(stax_memory::pyjson::dumps_http(&payload["files"]), "{}");
    }

    #[test]
    fn the_paths_filter_restricts_the_replay_not_just_the_output() {
        let conn = seeded();
        let filter = ["/repo/b.py".to_owned()];
        let payload = reconstruct_fs_at(&conn, "sess", "2026-01-01T01:00:00Z", Some(&filter), true)
            .expect("ok");
        let files = payload["files"].as_object().expect("files");
        assert_eq!(files.len(), 1);
        assert!(files.contains_key("/repo/b.py"));
    }

    #[test]
    fn include_content_false_drops_only_the_body() {
        let conn = seeded();
        let payload =
            reconstruct_fs_at(&conn, "sess", "2026-01-01T01:00:00Z", None, false).expect("ok");
        let entry = &payload["files"]["/repo/a.py"];
        assert!(entry.get("content").is_none());
        // The metadata — including the byte count of the body NOT shipped.
        assert_eq!(entry["byte_count"], json!(18));
    }

    #[test]
    fn an_unparseable_at_is_the_422_message_with_a_python_repr() {
        let conn = seeded();
        let err = reconstruct_fs_at(&conn, "sess", "not-a-time", None, true).expect_err("422");
        assert_eq!(
            err,
            ReconstructError::Malformed(
                "Could not parse 'at' as ISO-8601 / RFC-3339: 'not-a-time'".to_owned()
            )
        );
        // The cutoff is parsed BEFORE the session is resolved, so a bad `at` on
        // an unknown session is still the 422.
        let err = reconstruct_fs_at(&conn, "ghost", "", None, true).expect_err("422");
        assert!(matches!(err, ReconstructError::Malformed(_)));
    }

    #[test]
    fn an_unknown_session_is_the_404_message() {
        let conn = seeded();
        let err =
            reconstruct_fs_at(&conn, "ghost", "2026-01-01T00:00:00Z", None, true).expect_err("404");
        assert_eq!(
            err,
            ReconstructError::UnknownSession("Session not found in store: ghost".to_owned())
        );
    }

    #[test]
    fn snapshot_ts_echoes_the_raw_at_unstripped() {
        let conn = seeded();
        let payload =
            reconstruct_fs_at(&conn, "sess", "  2026-01-01T01:00:00Z  ", None, true).expect("ok");
        // The cutoff parsed (the strip is inside `_parse_iso`) but the ECHO is
        // the raw query value.
        assert_eq!(payload["snapshot_ts"], json!("  2026-01-01T01:00:00Z  "));
    }

    #[test]
    fn a_read_with_no_matching_result_warns_and_leaves_the_file_empty() {
        let conn = Connection::open_in_memory().expect("open");
        conn.execute_batch(
            "CREATE TABLE sessions (id INTEGER PRIMARY KEY, project_id INTEGER,
                 session_id TEXT, last_ts TEXT);
             CREATE TABLE messages (id INTEGER PRIMARY KEY, session_fk INTEGER,
                 seq INTEGER, timestamp TEXT, role TEXT, raw_json TEXT);
             INSERT INTO sessions (id, project_id, session_id, last_ts)
                 VALUES (10, 1, 'sess', '2026-01-01T00:00:00Z');",
        )
        .expect("schema");
        let raw = json!({"message": {"content": [
            {"type": "tool_use", "id": "r9", "name": "Read",
             "input": {"file_path": "/x.py"}}]}});
        conn.execute(
            "INSERT INTO messages (session_fk, seq, timestamp, role, raw_json)
             VALUES (10, 1, '2026-01-01T00:00:00Z', 'assistant', ?)",
            [raw.to_string()],
        )
        .expect("insert");
        let payload =
            reconstruct_fs_at(&conn, "sess", "2026-01-01T01:00:00Z", None, true).expect("ok");
        assert_eq!(
            payload["warnings"],
            json!(["/x.py: Read result missing — no initial content captured"])
        );
        assert_eq!(payload["files"]["/x.py"]["byte_count"], json!(0));
        assert_eq!(payload["files"]["/x.py"]["content"], json!(""));
        assert_eq!(
            payload["files"]["/x.py"]["reconstruction_complete"],
            json!(false)
        );
    }
}
