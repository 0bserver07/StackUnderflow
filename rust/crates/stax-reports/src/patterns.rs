//! `reports/patterns.py` — cross-session recurrence mining, the "coding health"
//! report behind `GET /api/patterns`.
//!
//! | Item | Python | Reached from |
//! |---|---|---|
//! | [`mine_patterns`] | same | `routes/patterns.rs::get_patterns` |
//! | [`file_risk`] | same | the active-recall hook (campaign #5), not HTTP |
//! | [`Instant`] | the injected `now` | both of the above |
//! | [`DEFAULT_SINCE_DAYS`] / [`MAX_SINCE_DAYS`] | same | the route's validator |
//!
//! Three families are mined from one bounded collection pass: per-file risk
//! (`message_tool_mart` touches × attributed failures), recurring error
//! signatures (normalised, `>= 2` distinct sessions, with resolution hints), and
//! Bash command failure clusters (`>= 2` failures).
//!
//! # THE HEADLINE: this payload carries a wall clock, so it cannot byte-match
//!
//! `_collect` computes
//!
//! ```text
//! since_iso = (now - timedelta(days=days)).isoformat()
//! ```
//!
//! and puts that string in the response under `report.window.since`
//! (`patterns.py:655`, emitted at `:1055`). `datetime.now(UTC).isoformat()`
//! renders **microseconds**. Two servers answering the same case a few
//! milliseconds apart therefore emit two different bodies, and the *same* server
//! answering twice does too. `!PT-patterns` and `!PT-patterns-window` are
//! permanently open by construction — the `/api/compare` situation (DIV-085,
//! `generated = time.time()`) reached by a different route. The `400` leg
//! (`!PT-bad-since`) reads no clock and diffs cleanly.
//!
//! Because of that, `now` is an explicit [`Instant`] parameter here rather than
//! a global read: the unit tests below pin it, which is the only way any of this
//! arithmetic is checkable at all.
//!
//! # What a careless port gets wrong
//!
//! 1. **The table guard is view-inclusive.** `patterns.py::_table_exists` is
//!    `type IN ('table','view')`, not `type='table'` — and `messages` is a VIEW
//!    over the monthly partitions post-v008. Using
//!    [`table_exists`](super::mart_queries::table_exists) here would make every
//!    error and interruption read return nothing on a partitioned store. That is
//!    law 7 / DIV-148, and it is live in this module.
//! 2. **Every list sort is stable and the keys are *not* always total.** Two
//!    different `category` values can normalise to the same `signature`, and the
//!    signature sort key is `(-session_count, -count, signature)` — a genuine
//!    tie. Python's `list.sort` keeps the dict's insertion order there, so the
//!    aggregation maps below are insertion-ordered ([`OrderedMap`]) and every
//!    sort is `sort_by`, never `sort_unstable_by`.
//! 3. **`sum()` here is over `int`s.** `_load_interruptions` does
//!    `sum(int(r["n"] or 0) for r in rows)` — exact integer arithmetic, so this
//!    is an `i64` fold and emphatically *not* `neumaier_sum` (law 3 cuts both
//!    ways: match the operation).
//! 4. **Six regexes, no regex crate.** `stax-server` does not depend on one, so
//!    the six patterns in `_normalise_signature` / `_normalise_command` are
//!    transcribed as scanners, backtracking included where the pattern needs it
//!    (`^cd\s+\S+\s*&&\s*` does). Each has its own test.
//! 5. **Every slice is by code point.** `[:160]`, `[:200]`, `[:120]` and `[:80]`
//!    are Python slices — [`py_char_prefix`], never a byte range.
//!
//! # Advisory contract
//!
//! Nothing in this module returns an error to its caller. A missing table, a
//! malformed `raw_json`, a SQLite failure mid-scan: each degrades to an
//! empty-but-well-formed part, exactly as Python's four nested `try/except
//! Exception` blocks do. The report shape is invariant.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};

use rusqlite::Connection;
use serde_json::{Map, Value};
use stax_etl::stats::aggregator::round_py;
use stax_etl::stats::classifier::{INTERRUPT_API, INTERRUPT_PREFIX, categorise};
use stax_etl::stats::pydatetime::{civil_from_epoch, parse_ts};
use stax_etl::stats::pytext::{is_py_space, py_char_prefix, py_str, py_strip, py_truthy};

// ── tunables (`patterns.py:97..134`) ─────────────────────────────────────────

/// `DEFAULT_SINCE_DAYS = 90`.
pub const DEFAULT_SINCE_DAYS: i64 = 90;
/// `MAX_SINCE_DAYS = 365` — there is deliberately no "all time" spec.
pub const MAX_SINCE_DAYS: i64 = 365;
/// `MIN_RECURRENCE_SESSIONS = 2` — one session's flailing is a retry loop.
const MIN_RECURRENCE_SESSIONS: usize = 2;
/// `TOP_N_FILES = 20`.
const TOP_N_FILES: usize = 20;
/// `TOP_N_SIGNATURES = 20`.
const TOP_N_SIGNATURES: usize = 20;
/// `TOP_N_COMMANDS = 15`.
const TOP_N_COMMANDS: usize = 15;
/// `TOP_N_HINTS = 3`.
const TOP_N_HINTS: usize = 3;
/// `MAX_ERROR_ROWS = 20_000`.
const MAX_ERROR_ROWS: i64 = 20_000;
/// `MAX_TOOL_ROWS = 500_000`.
const MAX_TOOL_ROWS: i64 = 500_000;
/// `_ATTRIBUTION_HOPS = 5`.
const ATTRIBUTION_HOPS: i64 = 5;

/// `_WRITE_TOOLS`.
const WRITE_TOOLS: [&str; 4] = ["Edit", "Write", "MultiEdit", "NotebookEdit"];
/// `_READ_TOOLS`.
const READ_TOOLS: [&str; 1] = ["Read"];

fn is_write_tool(name: &str) -> bool {
    WRITE_TOOLS.contains(&name)
}

/// `_TOUCH_TOOLS = _WRITE_TOOLS | _READ_TOOLS`.
fn is_touch_tool(name: &str) -> bool {
    is_write_tool(name) || READ_TOOLS.contains(&name)
}

/// `_SUBCOMMAND_HEADS` — CLIs whose first subcommand is part of the identity.
const SUBCOMMAND_HEADS: [&str; 23] = [
    "apt",
    "brew",
    "bundle",
    "cargo",
    "composer",
    "docker",
    "dotnet",
    "gh",
    "git",
    "go",
    "gradle",
    "kubectl",
    "make",
    "mvn",
    "npm",
    "pip",
    "pip3",
    "pnpm",
    "poetry",
    "stackunderflow",
    "terraform",
    "uv",
    "yarn",
];
/// `_SCRIPT_HEADS` — heads whose "subcommand" is a script path.
const SCRIPT_HEADS: [&str; 7] = ["bash", "node", "npx", "python", "python3", "ruby", "sh"];

// ── the injected clock ───────────────────────────────────────────────────────

/// The instant `_collect` measures the window back from.
///
/// Python's `mine_patterns(..., now: datetime | None = None)` reads
/// `datetime.now(UTC)` when the caller passes nothing, and the route passes
/// nothing. The injection is explicit here for the same reason
/// [`scope::Instant`](super::scope::Instant) makes it explicit: it is the only
/// handle any test has on a clock-derived payload.
///
/// # Why this is not `services::scope::Instant`
///
/// It would be, except that `Instant::minus_days` is private there and `scope.rs`
/// belongs to another batch. Flagged for the dedup list rather than reached
/// across the fence; the calendar arithmetic itself is delegated to
/// [`civil_from_epoch`], which is the shared owner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Instant {
    epoch_s: i64,
    micro: i64,
}

impl Instant {
    /// `datetime.now(UTC)`.
    ///
    /// CPython rounds the clock to microseconds half-to-even before building the
    /// `datetime`; a clock the calendar arithmetic cannot represent still yields
    /// *some* instant rather than a panic, because an advisory report must not
    /// crash on the system clock.
    #[must_use]
    pub fn now_utc() -> Self {
        let (secs, nanos) = match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)
        {
            Ok(delta) => (
                i64::try_from(delta.as_secs()).unwrap_or(i64::MAX),
                i64::from(delta.subsec_nanos()),
            ),
            Err(err) => {
                let delta = err.duration();
                let secs = -i64::try_from(delta.as_secs()).unwrap_or(i64::MAX);
                let nanos = i64::from(delta.subsec_nanos());
                if nanos == 0 {
                    (secs, 0)
                } else {
                    (secs - 1, 1_000_000_000 - nanos)
                }
            }
        };
        #[allow(
            clippy::cast_precision_loss,
            clippy::cast_possible_truncation,
            reason = "the rounded value is the sub-second part only, < 1e6"
        )]
        let micro = round_py(nanos as f64 / 1000.0, 0) as i64;
        if micro >= 1_000_000 {
            Self {
                epoch_s: secs.saturating_add(1),
                micro: micro - 1_000_000,
            }
        } else {
            Self {
                epoch_s: secs,
                micro,
            }
        }
    }

    /// A pinned instant, for tests and anything that must be reproducible.
    #[must_use]
    pub const fn from_epoch(epoch_s: i64, micro: i64) -> Self {
        Self { epoch_s, micro }
    }

    /// Microseconds since the epoch — the comparison basis
    /// `routes/patterns.rs` needs for `_prune_state`'s cooldown expiry.
    #[must_use]
    pub const fn epoch_micros(self) -> i64 {
        self.epoch_s
            .saturating_mul(1_000_000)
            .saturating_add(self.micro)
    }

    /// `self - timedelta(days=n)` — the microseconds are carried, not zeroed.
    #[must_use]
    const fn minus_days(self, days: i64) -> Self {
        Self {
            epoch_s: self.epoch_s.saturating_sub(days.saturating_mul(86_400)),
            micro: self.micro,
        }
    }

    /// `datetime.isoformat()` for a UTC-aware value: the fractional part appears
    /// only when `microsecond` is non-zero, and the offset is always `+00:00`.
    #[must_use]
    pub fn isoformat(self) -> String {
        let (year, month, day, hour, minute, second) = civil_from_epoch(self.epoch_s);
        let mut out = format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}");
        if self.micro != 0 {
            out.push_str(&format!(".{:06}", self.micro));
        }
        out.push_str("+00:00");
        out
    }
}

// ── small helpers ────────────────────────────────────────────────────────────

/// `_table_exists` — `type IN ('table','view')`, and a SQLite error is `False`.
///
/// **Not** [`table_exists`](super::mart_queries::table_exists), which is
/// `type='table'`. See the module docs, point 1.
fn table_or_view_exists(conn: &Connection, name: &str) -> bool {
    let probe = || -> rusqlite::Result<bool> {
        let mut stmt = conn.prepare(
            "SELECT 1 FROM sqlite_master WHERE type IN ('table', 'view') AND name = ? LIMIT 1",
        )?;
        let mut rows = stmt.query([name])?;
        Ok(rows.next()?.is_some())
    };
    probe().unwrap_or(false)
}

/// `_ts_to_epoch` — `datetime.fromisoformat(ts.replace("Z","+00:00")).timestamp()`,
/// `0.0` on anything unparseable.
///
/// # The naive-timestamp divergence
///
/// For an **aware** value `.timestamp()` is exact instant arithmetic and this
/// matches. For a **naive** one CPython interprets the wall clock in the host's
/// *local* zone (`mktime` semantics, DST fold included); there is no timezone
/// database in this crate's dependency set, so a naive stamp is read as UTC
/// here. The value never reaches the payload — it is used only for ordering and
/// for `bisect_right` — so the blast radius is a possible reordering of
/// `last_touch_ts` / `last_failure_ts` on a store that mixes naive and aware
/// stamps within one file. Recorded, not papered over.
fn ts_to_epoch(ts: Option<&str>) -> f64 {
    let Some(ts) = ts.filter(|value| !value.is_empty()) else {
        return 0.0;
    };
    match parse_ts(&ts.replace('Z', "+00:00")) {
        Some(parsed) => {
            let instant_us = parsed.wall_us - parsed.offset_s.unwrap_or(0) * 1_000_000;
            #[allow(
                clippy::cast_precision_loss,
                reason = "matches timedelta.total_seconds(): one exact integer, one division"
            )]
            {
                instant_us as f64 / 1_000_000.0
            }
        }
        None => 0.0,
    }
}

/// `_clamp_days` — `max(1, min(days, MAX_SINCE_DAYS))`.
fn clamp_days(since_days: i64) -> i64 {
    since_days.clamp(1, MAX_SINCE_DAYS)
}

/// `_basename` — `path.replace("\\","/").rstrip("/").rsplit("/", 1)[-1]`.
///
/// `rstrip("/")` strips *every* trailing slash, so `"/a/b//"` is `"b"` and
/// `"///"` is `""`.
fn basename(path: &str) -> String {
    let normalised = path.replace('\\', "/");
    let trimmed = normalised.trim_end_matches('/');
    match trimmed.rsplit_once('/') {
        Some((_, tail)) => tail.to_owned(),
        None => trimmed.to_owned(),
    }
}

/// `str.splitlines()` — the boundaries CPython actually breaks a `str` on.
///
/// Wider than `str::lines()`: `\v`, `\f`, `\x1c`–`\x1e`, `\x85`, `\u2028` and
/// `\u2029` all split, and `\r\n` is one boundary. Reached from
/// `_normalise_signature` (first meaningful line) and `_extract_error_events`
/// (the `example` field), so a `\x0c` inside an error body changes which line is
/// the "first" one.
fn py_splitlines(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let bytes = text.as_bytes();
    let mut start = 0usize;
    let mut idx = 0usize;
    while idx < text.len() {
        let Some(ch) = text[idx..].chars().next() else {
            break;
        };
        let width = ch.len_utf8();
        let is_break = matches!(
            ch,
            '\n' | '\r'
                | '\u{0b}'
                | '\u{0c}'
                | '\u{1c}'
                | '\u{1d}'
                | '\u{1e}'
                | '\u{85}'
                | '\u{2028}'
                | '\u{2029}'
        );
        if is_break {
            out.push(&text[start..idx]);
            // `\r\n` is a single boundary.
            if ch == '\r' && bytes.get(idx + 1) == Some(&b'\n') {
                idx += 2;
            } else {
                idx += width;
            }
            start = idx;
            continue;
        }
        idx += width;
    }
    if start < text.len() {
        out.push(&text[start..]);
    }
    out
}

/// The first non-empty `strip()`ped line, or `""` — the loop both
/// `_normalise_signature` and `_extract_error_events` open with.
fn first_meaningful_line(text: &str) -> &str {
    for candidate in py_splitlines(text) {
        let stripped = py_strip(candidate);
        if !stripped.is_empty() {
            return stripped;
        }
    }
    ""
}

// ── the six regexes, transcribed ─────────────────────────────────────────────

/// `_PATH_RE`'s character class `[^\s'\":,)\]]`. Note `/` is **in** the class,
/// which is why a run swallows its own separators and the `{2,}` backtracks.
fn is_path_char(c: char) -> bool {
    !(is_py_space(c) || matches!(c, '\'' | '"' | ':' | ',' | ')' | ']'))
}

/// `(?:/[^\s'\":,)\]]+){2,}` anchored at `at`, returning the end index.
///
/// The greedy `+` consumes the whole run (`/` is a member of the class), then
/// the `{2,}` backtracks looking for a slash it can start a second repetition
/// at. Equivalent closed form, case-checked in the tests: the match is the whole
/// maximal run `R` starting at `at`, and it succeeds iff `R` holds a slash at
/// some relative index `p` with `2 <= p <= len(R) - 2`.
fn path_run_at(chars: &[char], at: usize) -> Option<usize> {
    if chars.get(at) != Some(&'/') {
        return None;
    }
    let mut end = at;
    while end < chars.len() && is_path_char(chars[end]) {
        end += 1;
    }
    let len = end - at;
    let ok = (2..len.saturating_sub(1)).any(|rel| chars[at + rel] == '/');
    ok.then_some(end)
}

/// `_PATH_RE.sub(lambda m: _basename(m.group(0)), line)`.
fn sub_paths(line: &str) -> String {
    let chars: Vec<char> = line.chars().collect();
    let mut out = String::with_capacity(line.len());
    let mut idx = 0usize;
    while idx < chars.len() {
        // `(?:[A-Za-z]:)?` is greedy: try WITH the drive prefix first. If the run
        // after it fails, the backtrack requires `/` at `idx`, which is a letter
        // here — so the whole position fails.
        let with_drive = (chars[idx].is_ascii_alphabetic() && chars.get(idx + 1) == Some(&':'))
            .then(|| path_run_at(&chars, idx + 2))
            .flatten();
        let matched_end = match with_drive {
            Some(end) => Some(end),
            None => path_run_at(&chars, idx),
        };
        if let Some(end) = matched_end {
            let text: String = chars[idx..end].iter().collect();
            out.push_str(&basename(&text));
            idx = end;
            continue;
        }
        out.push(chars[idx]);
        idx += 1;
    }
    out
}

/// `\w` for a `str` pattern — Unicode alphanumeric plus `_`. Used only for the
/// `\b` assertions around `_HEX_RE`.
fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// `_HEX_RE.sub("<hex>", line)` — `\b[0-9a-f]{8,}\b`, `re.I`.
///
/// The two `\b`s mean the run must be a WHOLE word: `deadbeef` matches,
/// `xdeadbeef` does not (there is no boundary inside a word), and `deadbeef.log`
/// does (the `.` is a boundary). So the closed form is "replace any maximal
/// word-run that is entirely hex and at least eight characters long".
fn sub_hex(line: &str) -> String {
    let chars: Vec<char> = line.chars().collect();
    let mut out = String::with_capacity(line.len());
    let mut idx = 0usize;
    while idx < chars.len() {
        if !is_word_char(chars[idx]) {
            out.push(chars[idx]);
            idx += 1;
            continue;
        }
        let mut end = idx;
        while end < chars.len() && is_word_char(chars[end]) {
            end += 1;
        }
        let run = &chars[idx..end];
        // `[0-9a-f]` under `re.I` is exactly `[0-9a-fA-F]`, which is what
        // `is_ascii_hexdigit` tests — nothing wider.
        if run.len() >= 8 && run.iter().all(char::is_ascii_hexdigit) {
            out.push_str("<hex>");
        } else {
            out.extend(run.iter());
        }
        idx = end;
    }
    out
}

/// `_NUM_RE.sub("<n>", line)` — `\d+`.
///
/// DIVERGENCE, recorded: Python's `\d` on a `str` pattern is the Unicode `Nd`
/// category, not ASCII. Rust's std has no `Nd` predicate (`char::is_numeric` is
/// the wider `Nd | Nl | No`), so this narrows to ASCII. Between an over-match and
/// an under-match the under-match is the conservative direction — a Devanagari
/// digit survives into the signature instead of being collapsed — and no store
/// has produced one in an error body.
fn sub_nums(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars().peekable();
    while let Some(ch) = chars.next() {
        if !ch.is_ascii_digit() {
            out.push(ch);
            continue;
        }
        while chars.peek().is_some_and(char::is_ascii_digit) {
            chars.next();
        }
        out.push_str("<n>");
    }
    out
}

/// `_WS_RE.sub(" ", line)` — `\s+`, which for a `str` pattern is
/// [`is_py_space`]'s set (the `\x1c`–`\x1f` run included).
fn collapse_ws(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars().peekable();
    while let Some(ch) = chars.next() {
        if !is_py_space(ch) {
            out.push(ch);
            continue;
        }
        while chars.peek().copied().is_some_and(is_py_space) {
            chars.next();
        }
        out.push(' ');
    }
    out
}

/// `_normalise_signature` — collapse an error body into a stable cross-session key.
fn normalise_signature(text: &str) -> String {
    let line = first_meaningful_line(text);
    let line = sub_paths(line);
    let line = sub_hex(&line);
    let line = sub_nums(&line);
    let line = collapse_ws(&line);
    let line = py_strip(&line);
    if line.is_empty() {
        "<empty error body>".to_owned()
    } else {
        py_char_prefix(line, 160).to_owned()
    }
}

/// `_ENV_ASSIGN_RE.sub("", s)` — `^[A-Za-z_][A-Za-z0-9_]*=\S*\s+`, anchored, so
/// at most one replacement.
///
/// No backtracking is needed: `[A-Za-z0-9_]*` cannot cross an `=`, so the greedy
/// run ends exactly where the `=` must be, and `\S*` cannot cross whitespace, so
/// it ends exactly where `\s+` must start.
fn strip_env_assign(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let Some(&first) = chars.first() else {
        return s.to_owned();
    };
    if !(first.is_ascii_alphabetic() || first == '_') {
        return s.to_owned();
    }
    let mut idx = 1usize;
    while idx < chars.len() && (chars[idx].is_ascii_alphanumeric() || chars[idx] == '_') {
        idx += 1;
    }
    if chars.get(idx) != Some(&'=') {
        return s.to_owned();
    }
    idx += 1;
    while idx < chars.len() && !is_py_space(chars[idx]) {
        idx += 1;
    }
    if !chars.get(idx).copied().is_some_and(is_py_space) {
        return s.to_owned();
    }
    while idx < chars.len() && is_py_space(chars[idx]) {
        idx += 1;
    }
    chars[idx..].iter().collect()
}

/// `_CD_PREFIX_RE.sub("", s)` — `^cd\s+\S+\s*&&\s*`.
///
/// This one DOES backtrack: `\S+` is greedy and `&&` has no whitespace
/// requirement around it, so `cd /x&&ls` matches only after the greedy
/// `\S+ = "/x&&ls"` fails. The loop below walks the repetition lengths
/// longest-first, which is the engine's order.
fn strip_cd_prefix(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() < 2 || chars[0] != 'c' || chars[1] != 'd' {
        return s.to_owned();
    }
    let mut idx = 2usize;
    let ws_start = idx;
    while idx < chars.len() && is_py_space(chars[idx]) {
        idx += 1;
    }
    if idx == ws_start {
        return s.to_owned(); // `\s+` needs at least one
    }
    let token_start = idx;
    let mut token_end = idx;
    while token_end < chars.len() && !is_py_space(chars[token_end]) {
        token_end += 1;
    }
    let mut split = token_end;
    while split > token_start {
        let mut probe = split;
        while probe < chars.len() && is_py_space(chars[probe]) {
            probe += 1;
        }
        if chars.get(probe) == Some(&'&') && chars.get(probe + 1) == Some(&'&') {
            let mut tail = probe + 2;
            while tail < chars.len() && is_py_space(chars[tail]) {
                tail += 1;
            }
            return chars[tail..].iter().collect();
        }
        split -= 1;
    }
    s.to_owned()
}

/// `str.split()` with no argument — split on runs of whitespace, discarding the
/// leading and trailing ones.
fn py_split_whitespace(s: &str) -> Vec<&str> {
    s.split(is_py_space).filter(|tok| !tok.is_empty()).collect()
}

/// `_normalise_command` — reduce a Bash line to its cluster key.
fn normalise_command(cmd: &str) -> String {
    let mut s = py_strip(cmd).to_owned();
    for _ in 0..3 {
        // bounded prefix stripping; malformed input cannot loop
        let stripped = strip_cd_prefix(&strip_env_assign(&s));
        let new = py_strip(&stripped).to_owned();
        if new == s {
            break;
        }
        s = new;
    }
    let tokens = py_split_whitespace(&s);
    let Some(first) = tokens.first() else {
        return "<empty>".to_owned();
    };
    let head = basename(first);
    let is_script = SCRIPT_HEADS.contains(&head.as_str());
    let mut sub = String::new();
    if SUBCOMMAND_HEADS.contains(&head.as_str()) || is_script {
        for token in &tokens[1..] {
            if token.starts_with('-') {
                continue;
            }
            sub = if is_script {
                basename(token)
            } else {
                (*token).to_owned()
            };
            break;
        }
    }
    let joined = format!("{head} {sub}");
    py_char_prefix(py_strip(&joined), 80).to_owned()
}

// ── insertion-ordered map ────────────────────────────────────────────────────

/// A `dict` whose iteration order is first-insertion order.
///
/// Used wherever Python iterates an aggregation dict and the downstream sort key
/// can tie — see the module docs, point 2. A `HashMap` would be randomised per
/// process and would silently reorder those ties.
struct OrderedMap<V> {
    keys: Vec<String>,
    values: Vec<V>,
    index: HashMap<String, usize>,
}

impl<V> OrderedMap<V> {
    fn new() -> Self {
        Self {
            keys: Vec::new(),
            values: Vec::new(),
            index: HashMap::new(),
        }
    }

    /// `d.setdefault(key, default())`, returning the slot.
    fn entry(&mut self, key: &str, default: impl FnOnce() -> V) -> &mut V {
        let slot = match self.index.get(key) {
            Some(&slot) => slot,
            None => {
                let slot = self.values.len();
                self.keys.push(key.to_owned());
                self.values.push(default());
                self.index.insert(key.to_owned(), slot);
                slot
            }
        };
        &mut self.values[slot]
    }

    fn get(&self, key: &str) -> Option<&V> {
        self.index.get(key).map(|&slot| &self.values[slot])
    }

    fn iter(&self) -> impl Iterator<Item = (&String, &V)> {
        self.keys.iter().zip(self.values.iter())
    }
}

/// `collections.Counter` over string keys.
///
/// A plain `HashMap` is safe here and only here: every consumer applies a sort
/// whose key ends in the *counted key itself*, which is unique, so the map's own
/// order is never observable.
type Counter = HashMap<String, i64>;

fn bump(counter: &mut Counter, key: &str) {
    *counter.entry(key.to_owned()).or_insert(0) += 1;
}

/// `sorted(counter.items(), key=lambda kv: (-kv[1], kv[0]))` — count descending,
/// then key ascending. A total order, so stability is moot.
fn by_count_then_key(counter: &Counter) -> Vec<(&String, i64)> {
    let mut pairs: Vec<(&String, i64)> = counter.iter().map(|(k, v)| (k, *v)).collect();
    pairs.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
    pairs
}

/// `dict(sorted(counter.items()))` — the categories block, keyed ascending.
fn counter_to_sorted_object(counter: &Counter) -> Value {
    let mut keys: Vec<&String> = counter.keys().collect();
    keys.sort();
    let mut obj = Map::new();
    for key in keys {
        obj.insert(key.clone(), Value::from(counter[key]));
    }
    Value::Object(obj)
}

// ── internal row shapes ──────────────────────────────────────────────────────

/// `@dataclass(frozen=True, slots=True) class _ToolCall`.
struct ToolCall {
    session_id: String,
    ts: String,
    epoch: f64,
    tool_name: String,
    file_path: Option<String>,
}

/// `@dataclass(frozen=True, slots=True) class _ErrorEvent`.
struct ErrorEvent {
    session_id: String,
    ts: String,
    epoch: f64,
    category: String,
    signature: String,
    example: String,
    tool_name: Option<String>,
    file_path: Option<String>,
    command: Option<String>,
}

/// `@dataclass(slots=True) class _Collected`.
struct Collected {
    since_iso: String,
    since_days: i64,
    mart_available: bool,
    tool_calls: Vec<ToolCall>,
    errors: Vec<ErrorEvent>,
    interruption_count: i64,
    interruption_sessions: HashSet<String>,
}

impl Collected {
    fn empty(since_iso: String, since_days: i64) -> Self {
        Self {
            since_iso,
            since_days,
            mart_available: false,
            tool_calls: Vec::new(),
            errors: Vec::new(),
            interruption_count: 0,
            interruption_sessions: HashSet::new(),
        }
    }
}

// ── data sourcing (all bounded) ──────────────────────────────────────────────

/// `_project_filter` — `("", [])` for `None` **and** for an empty list, because
/// Python gates on truthiness. The empty-list case is intercepted by [`collect`]
/// before any SQL is built.
fn project_filter(column: &str, project_ids: Option<&[i64]>) -> String {
    match project_ids.filter(|ids| !ids.is_empty()) {
        None => String::new(),
        Some(ids) => format!("AND {column} IN ({}) ", vec!["?"; ids.len()].join(",")),
    }
}

fn project_params(project_ids: Option<&[i64]>) -> Vec<i64> {
    project_ids
        .filter(|ids| !ids.is_empty())
        .map(<[i64]>::to_vec)
        .unwrap_or_default()
}

/// `_load_tool_calls` — `(calls, mart_available)`.
///
/// `mart_available == false` means the touch denominators are untracked, and the
/// caller reports `failure_rate` as `null` rather than a misleading `1.0`.
fn load_tool_calls(
    conn: &Connection,
    since_day: &str,
    project_ids: Option<&[i64]>,
) -> (Vec<ToolCall>, bool) {
    if !table_or_view_exists(conn, "message_tool_mart") {
        return (Vec::new(), false);
    }
    let sql = format!(
        "SELECT session_id, ts, tool_name, file_path \
         FROM message_tool_mart \
         WHERE day >= ? {}\
         ORDER BY session_id, ts, message_id, call_index LIMIT ?",
        project_filter("project_id", project_ids)
    );
    let mut params: Vec<Box<dyn rusqlite::ToSql>> = vec![Box::new(since_day.to_owned())];
    for id in project_params(project_ids) {
        params.push(Box::new(id));
    }
    params.push(Box::new(MAX_TOOL_ROWS));

    let read = || -> rusqlite::Result<Vec<ToolCall>> {
        let mut stmt = conn.prepare(&sql)?;
        let refs: Vec<&dyn rusqlite::ToSql> =
            params.iter().map(std::convert::AsRef::as_ref).collect();
        let mut rows = stmt.query(refs.as_slice())?;
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            let ts: Option<String> = row.get(1)?;
            out.push(ToolCall {
                session_id: row.get::<_, Option<String>>(0)?.unwrap_or_default(),
                epoch: ts_to_epoch(ts.as_deref()),
                ts: ts.unwrap_or_default(),
                tool_name: row.get::<_, Option<String>>(2)?.unwrap_or_default(),
                file_path: row.get::<_, Option<String>>(3)?,
            });
        }
        Ok(out)
    };
    // `except sqlite3.Error: return [], False` — note the `False`: a mid-scan
    // failure reports the mart as UNAVAILABLE, not as present-but-empty.
    read().map_or_else(|_| (Vec::new(), false), |calls| (calls, true))
}

/// One pre-screened `messages` row — the shape `_load_error_rows` hands on.
struct ErrorRow {
    session_fk: i64,
    session_id: String,
    seq: i64,
    timestamp: String,
    raw_json: Option<String>,
}

/// `_load_error_rows` — windowed rows whose `raw_json` *can* hold an errored
/// `tool_result`.
///
/// The LIKE screen matches both JSON spacings. `_` is a single-character LIKE
/// wildcard, so the screen is slightly wider than the literal; the JSON parse in
/// [`extract_error_events`] is the authoritative filter.
fn load_error_rows(
    conn: &Connection,
    since_iso: &str,
    project_ids: Option<&[i64]>,
) -> Vec<ErrorRow> {
    if !(table_or_view_exists(conn, "messages") && table_or_view_exists(conn, "sessions")) {
        return Vec::new();
    }
    let sql = format!(
        "SELECT m.session_fk AS session_fk, s.session_id AS session_id, \
                m.seq AS seq, m.timestamp AS timestamp, m.raw_json AS raw_json \
         FROM messages m \
         JOIN sessions s ON s.id = m.session_fk \
         WHERE m.timestamp >= ? \
           AND (m.raw_json LIKE '%\"is_error\": true%' \
                OR m.raw_json LIKE '%\"is_error\":true%') {}\
         ORDER BY s.session_id, m.seq LIMIT ?",
        project_filter("s.project_id", project_ids)
    );
    let mut params: Vec<Box<dyn rusqlite::ToSql>> = vec![Box::new(since_iso.to_owned())];
    for id in project_params(project_ids) {
        params.push(Box::new(id));
    }
    params.push(Box::new(MAX_ERROR_ROWS));

    let read = || -> rusqlite::Result<Vec<ErrorRow>> {
        let mut stmt = conn.prepare(&sql)?;
        let refs: Vec<&dyn rusqlite::ToSql> =
            params.iter().map(std::convert::AsRef::as_ref).collect();
        let mut rows = stmt.query(refs.as_slice())?;
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            out.push(ErrorRow {
                // `int(row["session_fk"] or 0)` / `int(row["seq"] or 0)`.
                session_fk: row.get::<_, Option<i64>>(0)?.unwrap_or(0),
                session_id: row.get::<_, Option<String>>(1)?.unwrap_or_default(),
                seq: row.get::<_, Option<i64>>(2)?.unwrap_or(0),
                timestamp: row.get::<_, Option<String>>(3)?.unwrap_or_default(),
                raw_json: row.get::<_, Option<String>>(4)?,
            });
        }
        Ok(out)
    };
    read().unwrap_or_default()
}

/// `_load_interruptions` — `(total, sessions)` from the classifier's markers.
///
/// The two markers contain no `%` and no `_`, so appending `%` is a pure prefix
/// LIKE with no accidental wildcard.
fn load_interruptions(
    conn: &Connection,
    since_iso: &str,
    project_ids: Option<&[i64]>,
) -> (i64, HashSet<String>) {
    if !(table_or_view_exists(conn, "messages") && table_or_view_exists(conn, "sessions")) {
        return (0, HashSet::new());
    }
    let sql = format!(
        "SELECT s.session_id AS session_id, COUNT(*) AS n \
         FROM messages m \
         JOIN sessions s ON s.id = m.session_fk \
         WHERE m.timestamp >= ? \
           AND (m.content_text LIKE ? OR m.content_text LIKE ?) {}\
         GROUP BY s.session_id",
        project_filter("s.project_id", project_ids)
    );
    let mut params: Vec<Box<dyn rusqlite::ToSql>> = vec![
        Box::new(since_iso.to_owned()),
        Box::new(format!("{INTERRUPT_PREFIX}%")),
        Box::new(format!("{INTERRUPT_API}%")),
    ];
    for id in project_params(project_ids) {
        params.push(Box::new(id));
    }

    let read = || -> rusqlite::Result<(i64, HashSet<String>)> {
        let mut stmt = conn.prepare(&sql)?;
        let refs: Vec<&dyn rusqlite::ToSql> =
            params.iter().map(std::convert::AsRef::as_ref).collect();
        let mut rows = stmt.query(refs.as_slice())?;
        // `sum(int(r["n"] or 0) for r in rows)` — INTEGER arithmetic. Not
        // `neumaier_sum`; law 3 says match the operation, and this one is exact.
        let mut total: i64 = 0;
        let mut sessions = HashSet::new();
        while let Some(row) = rows.next()? {
            let session_id: Option<String> = row.get(0)?;
            total += row.get::<_, Option<i64>>(1)?.unwrap_or(0);
            if let Some(id) = session_id.filter(|value| !value.is_empty()) {
                sessions.insert(id);
            }
        }
        Ok((total, sessions))
    };
    read().unwrap_or_else(|_| (0, HashSet::new()))
}

// ── error extraction + attribution ───────────────────────────────────────────

/// `_error_bodies` — `[(tool_use_id, error_text), ...]` per errored `tool_result`.
///
/// DEVIATION, recorded: when a `content` list element carries a non-string
/// `text`, CPython's `str.join` raises `TypeError`, which unwinds all the way to
/// `_collect`'s advisory `except` and drops **every** error event for the whole
/// window. Reproducing a whole-pass wipeout from one malformed block is not a
/// behaviour worth transliterating, so [`py_str`] renders the element instead.
/// No store has produced one.
fn error_bodies(payload: &Value) -> Vec<(Option<String>, String)> {
    let Some(message) = payload.get("message").filter(|value| value.is_object()) else {
        return Vec::new();
    };
    let Some(content) = message.get("content").and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for block in content {
        let Some(block) = block.as_object() else {
            continue;
        };
        if block.get("type").and_then(Value::as_str) != Some("tool_result") {
            continue;
        }
        // `not block.get("is_error")` — truthiness, so `0`, `""` and `[]` are all
        // "not an error" alongside `false` and a missing key.
        if !block.get("is_error").is_some_and(py_truthy) {
            continue;
        }
        let raw_body = block
            .get("content")
            .cloned()
            .unwrap_or_else(|| Value::String(String::new()));
        let body = match raw_body.as_array() {
            Some(parts) => parts
                .iter()
                .filter(|part| part.is_object())
                .map(|part| match part.get("text") {
                    Some(Value::String(text)) => text.clone(),
                    Some(other) => py_str(other),
                    None => String::new(),
                })
                .collect::<Vec<_>>()
                .join(" "),
            // `str(body)` — a dict body renders as a Python repr, not as JSON.
            None => py_str(&raw_body),
        };
        let tid = block
            .get("tool_use_id")
            .and_then(Value::as_str)
            .map(str::to_owned);
        out.push((tid, body));
    }
    out
}

/// One entry from an assistant message's `tools_json`.
#[derive(Clone)]
struct ResolvedCall {
    name: String,
    input: Value,
}

/// `_CallResolver` — match a `tool_use_id` back to its call, memoising parses.
///
/// Parallel tool calls share one assistant message, so the memo makes a burst of
/// sibling errors cost a single parse.
struct CallResolver<'a> {
    conn: &'a Connection,
    parsed: HashMap<(i64, i64), HashMap<String, ResolvedCall>>,
}

impl<'a> CallResolver<'a> {
    fn new(conn: &'a Connection) -> Self {
        Self {
            conn,
            parsed: HashMap::new(),
        }
    }

    fn resolve(
        &mut self,
        session_fk: i64,
        seq: i64,
        tool_use_id: Option<&str>,
    ) -> Option<ResolvedCall> {
        // `if not tool_use_id: return None` — truthiness, so `""` too.
        let tool_use_id = tool_use_id.filter(|id| !id.is_empty())?;
        for hop in 0..ATTRIBUTION_HOPS {
            let (row_seq, tools_json) = self.hop(session_fk, seq, hop).ok()??;
            let calls = self.calls_for(session_fk, row_seq, tools_json.as_deref());
            if let Some(hit) = calls.get(tool_use_id) {
                return Some(hit.clone());
            }
        }
        None
    }

    /// One `LIMIT 1 OFFSET hop` step back through the assistant messages that
    /// actually carry tools. `Ok(None)` is Python's `row is None` (stop);
    /// `Err(_)` is its `except sqlite3.Error: return None`.
    fn hop(
        &self,
        session_fk: i64,
        seq: i64,
        hop: i64,
    ) -> rusqlite::Result<Option<(i64, Option<String>)>> {
        let mut stmt = self.conn.prepare(
            "SELECT seq, tools_json FROM messages \
             WHERE session_fk = ? AND seq < ? AND role = 'assistant' \
               AND tools_json != '[]' \
             ORDER BY seq DESC LIMIT 1 OFFSET ?",
        )?;
        let mut rows = stmt.query(rusqlite::params![session_fk, seq, hop])?;
        match rows.next()? {
            Some(row) => Ok(Some((
                row.get::<_, Option<i64>>(0)?.unwrap_or(0),
                row.get::<_, Option<String>>(1)?,
            ))),
            None => Ok(None),
        }
    }

    fn calls_for(
        &mut self,
        session_fk: i64,
        seq: i64,
        tools_json: Option<&str>,
    ) -> &HashMap<String, ResolvedCall> {
        self.parsed.entry((session_fk, seq)).or_insert_with(|| {
            let entries = tools_json
                .filter(|text| !text.is_empty())
                .and_then(|text| serde_json::from_str::<Value>(text).ok())
                .unwrap_or(Value::Null);
            let mut calls = HashMap::new();
            let Some(entries) = entries.as_array() else {
                return calls;
            };
            for entry in entries {
                let Some(entry) = entry.as_object() else {
                    continue;
                };
                let Some(tid) = entry.get("id").and_then(Value::as_str) else {
                    continue;
                };
                if tid.is_empty() {
                    continue;
                }
                calls.insert(
                    tid.to_owned(),
                    ResolvedCall {
                        // `t.get("name") or "Unknown"` — falsy becomes the
                        // placeholder. A truthy non-string is kept by Python as
                        // the raw object and is rendered here instead; it can
                        // only ever be compared against a tool-name literal.
                        name: match entry.get("name") {
                            Some(Value::String(name)) if !name.is_empty() => name.clone(),
                            Some(other) if py_truthy(other) => py_str(other),
                            _ => "Unknown".to_owned(),
                        },
                        input: entry
                            .get("input")
                            .filter(|value| value.is_object())
                            .cloned()
                            .unwrap_or_else(|| Value::Object(Map::new())),
                    },
                );
            }
            calls
        })
    }
}

/// `_file_path_from_input` — the first of three keys holding a non-empty string.
fn file_path_from_input(tool_input: &Value) -> Option<String> {
    for key in ["file_path", "path", "notebook_path"] {
        if let Some(value) = tool_input.get(key).and_then(Value::as_str)
            && !value.is_empty()
        {
            return Some(value.to_owned());
        }
    }
    None
}

/// `_extract_error_events` — parse pre-screened rows into attributed events.
fn extract_error_events(conn: &Connection, rows: &[ErrorRow]) -> Vec<ErrorEvent> {
    let mut resolver = CallResolver::new(conn);
    let mut out = Vec::new();
    for row in rows {
        // `json.loads(row["raw_json"]) if row["raw_json"] else {}`, then
        // `except JSONDecodeError: continue`, then the `isinstance(dict)` gate.
        let payload = row
            .raw_json
            .as_deref()
            .filter(|text| !text.is_empty())
            .map_or_else(
                || Some(Value::Object(Map::new())),
                |text| serde_json::from_str::<Value>(text).ok(),
            );
        let Some(payload) = payload.filter(Value::is_object) else {
            continue;
        };
        let bodies = error_bodies(&payload);
        if bodies.is_empty() {
            continue; // LIKE screen false positive (literal text in a body)
        }
        let epoch = ts_to_epoch(Some(row.timestamp.as_str()));
        for (tool_use_id, text) in bodies {
            let call = resolver.resolve(row.session_fk, row.seq, tool_use_id.as_deref());
            let tool_name = call.as_ref().map(|call| call.name.clone());
            let empty_input = Value::Object(Map::new());
            let tool_input = call.as_ref().map_or(&empty_input, |call| &call.input);
            let file_path = match tool_name.as_deref() {
                Some(name) if is_touch_tool(name) => file_path_from_input(tool_input),
                _ => None,
            };
            let command = if tool_name.as_deref() == Some("Bash") {
                tool_input
                    .get("command")
                    .and_then(Value::as_str)
                    .map(py_strip)
                    .filter(|cmd| !cmd.is_empty())
                    .map(str::to_owned)
            } else {
                None
            };
            out.push(ErrorEvent {
                session_id: row.session_id.clone(),
                ts: row.timestamp.clone(),
                epoch,
                category: categorise(&text),
                signature: normalise_signature(&text),
                example: py_char_prefix(first_meaningful_line(&text), 200).to_owned(),
                tool_name,
                file_path,
                command,
            });
        }
    }
    out
}

// ── collection pass ──────────────────────────────────────────────────────────

/// `_collect` — every bounded read for one window.
fn collect(
    conn: &Connection,
    since_days: i64,
    project_ids: Option<&[i64]>,
    now: Instant,
) -> Collected {
    let days = clamp_days(since_days);
    let since_iso = now.minus_days(days).isoformat();
    // `since_iso[:10]` — the mart's `day` column. ASCII by construction.
    let since_day: String = since_iso.chars().take(10).collect();

    let mut collected = Collected::empty(since_iso.clone(), days);

    // `project_ids == []` means "a filter was requested and matched nothing" —
    // scope to nothing rather than silently widening to the whole store.
    if project_ids.is_some_and(<[i64]>::is_empty) {
        return collected;
    }

    let (calls, mart_available) = load_tool_calls(conn, &since_day, project_ids);
    collected.tool_calls = calls;
    collected.mart_available = mart_available;

    let rows = load_error_rows(conn, &since_iso, project_ids);
    collected.errors = extract_error_events(conn, &rows);

    let (count, sessions) = load_interruptions(conn, &since_iso, project_ids);
    collected.interruption_count = count;
    collected.interruption_sessions = sessions;

    collected
}

// ── mining ───────────────────────────────────────────────────────────────────

/// `@dataclass(slots=True) class _FileAgg`.
#[derive(Default)]
struct FileAgg {
    touch_count: i64,
    edit_count: i64,
    read_count: i64,
    touch_sessions: HashSet<String>,
    failure_count: i64,
    failure_sessions: HashSet<String>,
    interruption_count: i64,
    last_touch: Option<(f64, String)>,
    last_failure: Option<(f64, String)>,
    categories: Counter,
}

fn build_file_map(collected: &Collected) -> OrderedMap<FileAgg> {
    let mut files: OrderedMap<FileAgg> = OrderedMap::new();
    for call in &collected.tool_calls {
        // `if not call.file_path or call.tool_name not in _TOUCH_TOOLS: continue`.
        let Some(path) = call.file_path.as_deref().filter(|p| !p.is_empty()) else {
            continue;
        };
        if !is_touch_tool(&call.tool_name) {
            continue;
        }
        let agg = files.entry(path, FileAgg::default);
        agg.touch_count += 1;
        if is_write_tool(&call.tool_name) {
            agg.edit_count += 1;
        } else {
            agg.read_count += 1;
        }
        if !call.session_id.is_empty() {
            agg.touch_sessions.insert(call.session_id.clone());
        }
        // Strictly `>`, so the FIRST call at a tied epoch keeps the `ts`.
        if agg
            .last_touch
            .as_ref()
            .is_none_or(|last| call.epoch > last.0)
        {
            agg.last_touch = Some((call.epoch, call.ts.clone()));
        }
    }
    for err in &collected.errors {
        let Some(path) = err.file_path.as_deref().filter(|p| !p.is_empty()) else {
            continue;
        };
        let agg = files.entry(path, FileAgg::default);
        agg.failure_count += 1;
        if !err.session_id.is_empty() {
            agg.failure_sessions.insert(err.session_id.clone());
        }
        bump(&mut agg.categories, &err.category);
        if err.category == "User Interruption" {
            agg.interruption_count += 1;
        }
        if agg
            .last_failure
            .as_ref()
            .is_none_or(|last| err.epoch > last.0)
        {
            agg.last_failure = Some((err.epoch, err.ts.clone()));
        }
    }
    files
}

/// The rendered `file_risk` entry plus the fields the sort reads.
struct FileRisk {
    failure_session_count: i64,
    failure_rate: Option<f64>,
    failure_count: i64,
    path: String,
    payload: Value,
}

/// `_file_risk_entry` — the union denominator is defensive against mart lag, so
/// the rate can never exceed `1.0`.
fn file_risk_entry(path: &str, agg: &FileAgg) -> FileRisk {
    let denom = agg.touch_sessions.union(&agg.failure_sessions).count();
    let failure_sessions = agg.failure_sessions.len();
    let rate = if agg.touch_count > 0 && denom > 0 {
        #[allow(
            clippy::cast_precision_loss,
            reason = "Python's int/int is true division; both counts are far below 2^53"
        )]
        Some(round_py(failure_sessions as f64 / denom as f64, 4))
    } else {
        None
    };
    let reason = file_reason(agg, rate, denom);

    // `asdict()` follows the dataclass declaration order — that IS the key order.
    let mut obj = Map::new();
    obj.insert("path".to_owned(), Value::from(path));
    obj.insert("touch_count".to_owned(), Value::from(agg.touch_count));
    obj.insert("edit_count".to_owned(), Value::from(agg.edit_count));
    obj.insert("read_count".to_owned(), Value::from(agg.read_count));
    obj.insert(
        "touch_session_count".to_owned(),
        Value::from(i64::try_from(denom).unwrap_or(i64::MAX)),
    );
    obj.insert("failure_count".to_owned(), Value::from(agg.failure_count));
    obj.insert(
        "failure_session_count".to_owned(),
        Value::from(i64::try_from(failure_sessions).unwrap_or(i64::MAX)),
    );
    obj.insert(
        "failure_rate".to_owned(),
        rate.map_or(Value::Null, Value::from),
    );
    obj.insert(
        "interruption_count".to_owned(),
        Value::from(agg.interruption_count),
    );
    obj.insert(
        "last_touch_ts".to_owned(),
        agg.last_touch
            .as_ref()
            .map_or(Value::Null, |last| Value::from(last.1.clone())),
    );
    obj.insert(
        "last_failure_ts".to_owned(),
        agg.last_failure
            .as_ref()
            .map_or(Value::Null, |last| Value::from(last.1.clone())),
    );
    obj.insert(
        "categories".to_owned(),
        counter_to_sorted_object(&agg.categories),
    );
    obj.insert("reason".to_owned(), Value::from(reason));

    FileRisk {
        failure_session_count: i64::try_from(failure_sessions).unwrap_or(i64::MAX),
        failure_rate: rate,
        failure_count: agg.failure_count,
        path: path.to_owned(),
        payload: Value::Object(obj),
    }
}

/// `_file_reason`.
///
/// `f"{rate * 100:.0f}"` is CPython's `%.0f`: the exact binary value, correctly
/// rounded, ties to even. Rust's `{:.0}` applies the same rule to the same value.
fn file_reason(agg: &FileAgg, rate: Option<f64>, denom: usize) -> String {
    let name_part = if denom > 0 {
        format!(
            "Failed in {} of {denom} sessions that touched it",
            agg.failure_sessions.len()
        )
    } else {
        format!("{} failures recorded", agg.failure_count)
    };
    let pct = if let Some(rate) = rate {
        format!(" ({:.0}%)", rate * 100.0)
    } else if agg.failure_count != 0 {
        " (touch history untracked — rate unknown)".to_owned()
    } else {
        String::new()
    };
    let sample = if denom > 0 && denom < 3 {
        " — small sample"
    } else {
        ""
    };
    format!("{name_part}{pct}{sample}.")
}

/// `@dataclass(slots=True) class _SigAgg`.
struct SigAgg {
    category: String,
    count: i64,
    sessions: HashSet<String>,
    first: Option<(f64, String)>,
    last: Option<(f64, String)>,
    tools: Counter,
    files: Counter,
    example: String,
    last_by_session: HashMap<String, f64>,
}

/// `_session_timeline` — `session_id -> [(epoch, tool_name, file_path_or_empty)]`,
/// sorted. Plain strings in every slot so tuple ordering never compares `None`.
fn session_timeline(collected: &Collected) -> HashMap<String, Vec<(f64, String, String)>> {
    let mut timeline: HashMap<String, Vec<(f64, String, String)>> = HashMap::new();
    for call in &collected.tool_calls {
        if call.session_id.is_empty() {
            continue;
        }
        timeline.entry(call.session_id.clone()).or_default().push((
            call.epoch,
            call.tool_name.clone(),
            call.file_path.clone().unwrap_or_default(),
        ));
    }
    for events in timeline.values_mut() {
        // `events.sort()` — tuple order, and `list.sort` is stable.
        events.sort_by(|a, b| {
            a.0.partial_cmp(&b.0)
                .unwrap_or(Ordering::Equal)
                .then_with(|| a.1.cmp(&b.1))
                .then_with(|| a.2.cmp(&b.2))
        });
    }
    timeline
}

/// `bisect_right(events, (last_epoch, "\uffff", "\uffff"))`.
///
/// The `\uffff` sentinels make the key sort after every realistic
/// `(tool_name, file_path)` at the same epoch, so the answer is the first call
/// strictly after the signature's last occurrence in that session.
fn bisect_right_after(events: &[(f64, String, String)], key_epoch: f64) -> usize {
    const SENTINEL: &str = "\u{ffff}";
    let greater = |event: &(f64, String, String)| -> bool {
        match event.0.partial_cmp(&key_epoch).unwrap_or(Ordering::Equal) {
            Ordering::Greater => true,
            Ordering::Less => false,
            Ordering::Equal => match event.1.as_str().cmp(SENTINEL) {
                Ordering::Greater => true,
                Ordering::Less => false,
                Ordering::Equal => event.2.as_str() > SENTINEL,
            },
        }
    };
    let mut lo = 0usize;
    let mut hi = events.len();
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        if greater(&events[mid]) {
            hi = mid;
        } else {
            lo = mid + 1;
        }
    }
    lo
}

/// `_hint_action`.
fn hint_action(tool_name: &str, file_path: &str) -> String {
    if file_path.is_empty() {
        tool_name.to_owned()
    } else {
        format!("{tool_name} {}", basename(file_path))
    }
}

/// The rendered `error_signatures` entry plus the three fields the sort reads.
struct SignatureRow {
    session_count: i64,
    count: i64,
    signature: String,
    payload: Value,
}

fn build_signatures(
    collected: &Collected,
    timeline: &HashMap<String, Vec<(f64, String, String)>>,
) -> Vec<SignatureRow> {
    // Keyed on `(category, signature)`; the joiner is `\u{0}` because neither
    // component can contain it (a category is a taxonomy literal and a signature
    // is a collapsed single line).
    let mut sigs: OrderedMap<SigAgg> = OrderedMap::new();
    for err in &collected.errors {
        let key = format!("{}\u{0}{}", err.category, err.signature);
        let agg = sigs.entry(&key, || SigAgg {
            category: err.category.clone(),
            count: 0,
            sessions: HashSet::new(),
            first: None,
            last: None,
            tools: HashMap::new(),
            files: HashMap::new(),
            example: String::new(),
            last_by_session: HashMap::new(),
        });
        agg.count += 1;
        if !err.session_id.is_empty() {
            agg.sessions.insert(err.session_id.clone());
            let prev = agg
                .last_by_session
                .get(&err.session_id)
                .copied()
                .unwrap_or(0.0);
            if err.epoch >= prev {
                agg.last_by_session
                    .insert(err.session_id.clone(), err.epoch);
            }
        }
        if agg.first.as_ref().is_none_or(|first| err.epoch < first.0) {
            agg.first = Some((err.epoch, err.ts.clone()));
        }
        if agg.last.as_ref().is_none_or(|last| err.epoch > last.0) {
            agg.last = Some((err.epoch, err.ts.clone()));
        }
        if let Some(tool) = err.tool_name.as_deref().filter(|name| !name.is_empty()) {
            bump(&mut agg.tools, tool);
        }
        if let Some(path) = err.file_path.as_deref().filter(|p| !p.is_empty()) {
            bump(&mut agg.files, path);
        }
        if agg.example.is_empty() {
            agg.example = err.example.clone();
        }
    }

    let mut out: Vec<SignatureRow> = Vec::new();
    for (key, agg) in sigs.iter() {
        if agg.sessions.len() < MIN_RECURRENCE_SESSIONS {
            continue;
        }
        let signature = key.split_once('\u{0}').map_or("", |(_, sig)| sig);
        let mut resolved: i64 = 0;
        let mut hints: Counter = HashMap::new();
        // `for session_id, last_epoch in agg.last_by_session.items()` — a dict
        // walk whose only outputs are a COUNT and a Counter, both order-free.
        for (session_id, last_epoch) in &agg.last_by_session {
            let Some(events) = timeline.get(session_id).filter(|ev| !ev.is_empty()) else {
                continue;
            };
            let idx = bisect_right_after(events, *last_epoch);
            if idx < events.len() {
                resolved += 1;
                let (_, tool_name, file_path) = &events[idx];
                bump(&mut hints, &hint_action(tool_name, file_path));
            }
        }
        let top_hints: Vec<(String, i64)> = by_count_then_key(&hints)
            .into_iter()
            .take(TOP_N_HINTS)
            .map(|(action, n)| (action.clone(), n))
            .collect();
        let reason = signature_reason(agg, resolved, top_hints.first());

        let mut obj = Map::new();
        obj.insert("signature".to_owned(), Value::from(signature));
        obj.insert("category".to_owned(), Value::from(agg.category.clone()));
        obj.insert("count".to_owned(), Value::from(agg.count));
        obj.insert(
            "session_count".to_owned(),
            Value::from(i64::try_from(agg.sessions.len()).unwrap_or(i64::MAX)),
        );
        obj.insert("resolved_session_count".to_owned(), Value::from(resolved));
        obj.insert(
            "first_ts".to_owned(),
            agg.first
                .as_ref()
                .map_or(Value::Null, |first| Value::from(first.1.clone())),
        );
        obj.insert(
            "last_ts".to_owned(),
            agg.last
                .as_ref()
                .map_or(Value::Null, |last| Value::from(last.1.clone())),
        );
        obj.insert(
            "top_tools".to_owned(),
            Value::Array(
                by_count_then_key(&agg.tools)
                    .into_iter()
                    .take(3)
                    .map(|(name, _)| Value::from(name.clone()))
                    .collect(),
            ),
        );
        obj.insert(
            "top_files".to_owned(),
            Value::Array(
                by_count_then_key(&agg.files)
                    .into_iter()
                    .take(3)
                    .map(|(name, _)| Value::from(name.clone()))
                    .collect(),
            ),
        );
        obj.insert(
            "resolution_hints".to_owned(),
            Value::Array(
                top_hints
                    .iter()
                    .map(|(action, n)| {
                        let mut hint = Map::new();
                        hint.insert("action".to_owned(), Value::from(action.clone()));
                        hint.insert("count".to_owned(), Value::from(*n));
                        Value::Object(hint)
                    })
                    .collect(),
            ),
        );
        obj.insert("example".to_owned(), Value::from(agg.example.clone()));
        obj.insert("reason".to_owned(), Value::from(reason));

        out.push(SignatureRow {
            session_count: i64::try_from(agg.sessions.len()).unwrap_or(i64::MAX),
            count: agg.count,
            signature: signature.to_owned(),
            payload: Value::Object(obj),
        });
    }
    // `out.sort(key=lambda s: (-s.session_count, -s.count, s.signature))`.
    // NOT a total order: two categories can share one signature. Python's sort is
    // stable and keeps the dict's insertion order there — `sort_by` does too,
    // `sort_unstable_by` would not.
    out.sort_by(|a, b| {
        b.session_count
            .cmp(&a.session_count)
            .then_with(|| b.count.cmp(&a.count))
            .then_with(|| a.signature.cmp(&b.signature))
    });
    out
}

/// `_signature_reason`.
fn signature_reason(agg: &SigAgg, resolved: i64, top_hint: Option<&(String, i64)>) -> String {
    let base = format!(
        "Recurred in {} sessions ({} occurrences).",
        agg.sessions.len(),
        agg.count
    );
    match (resolved, top_hint) {
        (0, _) => format!("{base} No session in window is known to have moved past it."),
        (_, Some((action, _))) => {
            format!("{base} {resolved} moved past it — most often the next step was {action}.")
        }
        (_, None) => format!("{base} {resolved} moved past it."),
    }
}

/// `@dataclass(slots=True) class _CmdAgg`.
#[derive(Default)]
struct CmdAgg {
    failure_count: i64,
    sessions: HashSet<String>,
    categories: Counter,
    last: Option<(f64, String)>,
    example: String,
}

/// The rendered `command_clusters` entry plus the fields the sort reads.
struct ClusterRow {
    failure_count: i64,
    session_count: i64,
    command: String,
    payload: Value,
}

fn build_command_clusters(collected: &Collected) -> Vec<ClusterRow> {
    let mut cmds: OrderedMap<CmdAgg> = OrderedMap::new();
    for err in &collected.errors {
        if err.tool_name.as_deref() != Some("Bash") {
            continue;
        }
        let Some(command) = err.command.as_deref().filter(|cmd| !cmd.is_empty()) else {
            continue;
        };
        let key = normalise_command(command);
        let agg = cmds.entry(&key, CmdAgg::default);
        agg.failure_count += 1;
        if !err.session_id.is_empty() {
            agg.sessions.insert(err.session_id.clone());
        }
        bump(&mut agg.categories, &err.category);
        if agg.last.as_ref().is_none_or(|last| err.epoch > last.0) {
            agg.last = Some((err.epoch, err.ts.clone()));
        }
        if agg.example.is_empty() {
            agg.example = py_char_prefix(command, 120).to_owned();
        }
    }

    let mut out = Vec::new();
    for (command, agg) in cmds.iter() {
        if agg.failure_count < 2 {
            continue; // a single failure isn't a cluster
        }
        // `min(items, key=lambda kv: (-kv[1], kv[0]))` — the same total order the
        // `sorted(...)[0]` elsewhere uses, spelled as a min.
        let top_cat = by_count_then_key(&agg.categories)
            .first()
            .map_or_else(|| "Other".to_owned(), |(name, _)| (*name).clone());
        let plural = if agg.sessions.len() == 1 { "" } else { "s" };
        let reason = format!(
            "{} failures across {} session{plural}; mostly {top_cat}.",
            agg.failure_count,
            agg.sessions.len()
        );

        let mut obj = Map::new();
        obj.insert("command".to_owned(), Value::from(command.clone()));
        obj.insert("failure_count".to_owned(), Value::from(agg.failure_count));
        obj.insert(
            "session_count".to_owned(),
            Value::from(i64::try_from(agg.sessions.len()).unwrap_or(i64::MAX)),
        );
        obj.insert(
            "categories".to_owned(),
            counter_to_sorted_object(&agg.categories),
        );
        obj.insert(
            "last_failure_ts".to_owned(),
            agg.last
                .as_ref()
                .map_or(Value::Null, |last| Value::from(last.1.clone())),
        );
        obj.insert("example".to_owned(), Value::from(agg.example.clone()));
        obj.insert("reason".to_owned(), Value::from(reason));

        out.push(ClusterRow {
            failure_count: agg.failure_count,
            session_count: i64::try_from(agg.sessions.len()).unwrap_or(i64::MAX),
            command: command.clone(),
            payload: Value::Object(obj),
        });
    }
    out.sort_by(|a, b| {
        b.failure_count
            .cmp(&a.failure_count)
            .then_with(|| b.session_count.cmp(&a.session_count))
            .then_with(|| a.command.cmp(&b.command))
    });
    out
}

// ── public entry points ──────────────────────────────────────────────────────

/// `mine_patterns(conn, since_days=…, project_ids=…, now=…)`.
///
/// `project_ids`: `None` is the whole store, `Some(&[])` is "a filter was
/// requested and matched nothing" — an empty report, never a silent widening.
///
/// Never fails. Every SQL access is `sqlite_master`-guarded and every read
/// swallows its own errors, so a bare or corrupt store yields the empty-but-
/// well-formed shape rather than a 500.
#[must_use]
pub fn mine_patterns(
    conn: &Connection,
    since_days: i64,
    project_ids: Option<&[i64]>,
    now: Instant,
) -> Value {
    let collected = collect(conn, since_days, project_ids, now);
    assemble(&collected, TOP_N_FILES, TOP_N_SIGNATURES, TOP_N_COMMANDS)
}

/// `_empty_totals` — the eight keys, in the dict literal's order.
///
/// Python reaches this only from `mine_patterns`'s outer `except`, the degraded
/// branch that fires when `_assemble` itself raises. Nothing in this port can
/// raise there — every read already swallows its own errors — so the branch is
/// unreachable and is **recorded rather than wired in**, the disposition
/// `routes/cost.rs` gives `_convert_in_place`. It stays as the test oracle for
/// the eight-key shape, which the live path must independently produce.
#[cfg(test)]
fn empty_totals() -> Value {
    let mut obj = Map::new();
    for key in [
        "tool_call_count",
        "error_count",
        "attributed_error_count",
        "interruption_count",
        "interruption_session_count",
        "session_count",
        "sessions_with_failures",
        "files_touched",
    ] {
        obj.insert(key.to_owned(), Value::from(0));
    }
    Value::Object(obj)
}

/// `_assemble` — the `PatternsReport` dataclass, in declaration order.
fn assemble(
    collected: &Collected,
    top_files: usize,
    top_signatures: usize,
    top_commands: usize,
) -> Value {
    let files = build_file_map(collected);
    let timeline = session_timeline(collected);
    let signatures = build_signatures(collected, &timeline);
    let clusters = build_command_clusters(collected);

    let mut risk_entries: Vec<FileRisk> = files
        .iter()
        .filter(|(_, agg)| agg.failure_count > 0)
        .map(|(path, agg)| file_risk_entry(path, agg))
        .collect();
    // `key=(-failure_session_count, -(failure_rate or 0.0), -failure_count, path)`
    // — note `or 0.0`, which maps both `None` and a real `0.0` to zero.
    risk_entries.sort_by(|a, b| {
        b.failure_session_count
            .cmp(&a.failure_session_count)
            .then_with(|| {
                b.failure_rate
                    .unwrap_or(0.0)
                    .partial_cmp(&a.failure_rate.unwrap_or(0.0))
                    .unwrap_or(Ordering::Equal)
            })
            .then_with(|| b.failure_count.cmp(&a.failure_count))
            .then_with(|| a.path.cmp(&b.path))
    });

    let mut sessions_seen: HashSet<&str> = HashSet::new();
    for call in &collected.tool_calls {
        if !call.session_id.is_empty() {
            sessions_seen.insert(&call.session_id);
        }
    }
    for err in &collected.errors {
        if !err.session_id.is_empty() {
            sessions_seen.insert(&err.session_id);
        }
    }
    for session in &collected.interruption_sessions {
        sessions_seen.insert(session);
    }
    let sessions_with_failures: HashSet<&str> = collected
        .errors
        .iter()
        .filter(|err| !err.session_id.is_empty())
        .map(|err| err.session_id.as_str())
        .collect();

    let count = |n: usize| Value::from(i64::try_from(n).unwrap_or(i64::MAX));

    let mut totals = Map::new();
    totals.insert(
        "tool_call_count".to_owned(),
        count(collected.tool_calls.len()),
    );
    totals.insert("error_count".to_owned(), count(collected.errors.len()));
    totals.insert(
        "attributed_error_count".to_owned(),
        count(
            collected
                .errors
                .iter()
                .filter(|err| err.tool_name.is_some())
                .count(),
        ),
    );
    totals.insert(
        "interruption_count".to_owned(),
        Value::from(collected.interruption_count),
    );
    totals.insert(
        "interruption_session_count".to_owned(),
        count(collected.interruption_sessions.len()),
    );
    totals.insert("session_count".to_owned(), count(sessions_seen.len()));
    totals.insert(
        "sessions_with_failures".to_owned(),
        count(sessions_with_failures.len()),
    );
    totals.insert(
        "files_touched".to_owned(),
        count(files.iter().filter(|(_, agg)| agg.touch_count > 0).count()),
    );

    let mut window = Map::new();
    window.insert("since".to_owned(), Value::from(collected.since_iso.clone()));
    window.insert("days".to_owned(), Value::from(collected.since_days));

    let mut sources = Map::new();
    sources.insert(
        "message_tool_mart".to_owned(),
        Value::Bool(collected.mart_available),
    );

    let mut report = Map::new();
    report.insert("window".to_owned(), Value::Object(window));
    report.insert("sources".to_owned(), Value::Object(sources));
    report.insert("totals".to_owned(), Value::Object(totals));
    report.insert(
        "file_risk".to_owned(),
        Value::Array(
            risk_entries
                .into_iter()
                .take(top_files)
                .map(|entry| entry.payload)
                .collect(),
        ),
    );
    report.insert(
        "error_signatures".to_owned(),
        Value::Array(
            signatures
                .into_iter()
                .take(top_signatures)
                .map(|row| row.payload)
                .collect(),
        ),
    );
    report.insert(
        "command_clusters".to_owned(),
        Value::Array(
            clusters
                .into_iter()
                .take(top_commands)
                .map(|row| row.payload)
                .collect(),
        ),
    );
    Value::Object(report)
}

/// `file_risk(conn, path, …)` — the per-file lookup campaign #5's hook calls.
///
/// Not reachable from any endpoint (the hooks package imports it directly), so it
/// carries no case row. Ported here because it is the second half of the module's
/// public API and shares every helper above; wave 8's hook port would otherwise
/// fork it.
///
/// Resolution order: exact path, then a **unique** suffix match. No match is a
/// well-formed zero entry, not an error.
#[must_use]
pub fn file_risk(
    conn: &Connection,
    path: &str,
    since_days: i64,
    project_ids: Option<&[i64]>,
    now: Instant,
) -> Value {
    let collected = collect(conn, since_days, project_ids, now);
    let files = build_file_map(&collected);
    if let Some(agg) = files.get(path) {
        return file_risk_entry(path, agg).payload;
    }
    if !path.is_empty() {
        let suffix = format!("/{}", path.trim_start_matches('/'));
        let mut candidates: Vec<&String> = files
            .iter()
            .map(|(key, _)| key)
            .filter(|key| key.ends_with(&suffix))
            .collect();
        candidates.sort();
        if candidates.len() == 1 {
            let key = candidates[0].clone();
            if let Some(agg) = files.get(&key) {
                return file_risk_entry(&key, agg).payload;
            }
        }
    }
    zero_file_risk(path)
}

/// `FileRisk(path=path, reason="No activity recorded in window.")` — every other
/// field at its dataclass default.
fn zero_file_risk(path: &str) -> Value {
    let agg = FileAgg::default();
    let mut entry = file_risk_entry(path, &agg);
    if let Value::Object(obj) = &mut entry.payload {
        obj.insert(
            "reason".to_owned(),
            Value::from("No activity recorded in window."),
        );
    }
    entry.payload
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, reason = "test code")]
mod tests {
    use super::*;

    /// 2026-07-31T12:34:56.789012+00:00.
    fn pinned() -> Instant {
        Instant::from_epoch(1_785_501_296, 789_012)
    }

    // ── the clock ────────────────────────────────────────────────────────────

    #[test]
    fn the_pinned_instant_renders_cpythons_isoformat() {
        assert_eq!(pinned().isoformat(), "2026-07-31T12:34:56.789012+00:00");
        // `microsecond == 0` drops the fractional part entirely.
        assert_eq!(
            Instant::from_epoch(1_785_501_296, 0).isoformat(),
            "2026-07-31T12:34:56+00:00"
        );
    }

    #[test]
    fn minus_days_carries_the_microseconds_and_crosses_months() {
        assert_eq!(
            pinned().minus_days(90).isoformat(),
            "2026-05-02T12:34:56.789012+00:00"
        );
        assert_eq!(
            pinned().minus_days(7).isoformat(),
            "2026-07-24T12:34:56.789012+00:00"
        );
    }

    /// The headline finding, asserted rather than asserted-about: the window
    /// bound the payload carries is a microsecond-resolution wall clock, so two
    /// servers cannot render the same bytes.
    #[test]
    fn the_window_bound_is_a_microsecond_wall_clock() {
        let a = Instant::from_epoch(1_785_501_296, 1)
            .minus_days(90)
            .isoformat();
        let b = Instant::from_epoch(1_785_501_296, 2)
            .minus_days(90)
            .isoformat();
        assert_ne!(a, b);
        assert!(a.ends_with("+00:00") && a.contains('.'));
    }

    // ── helpers ──────────────────────────────────────────────────────────────

    #[test]
    fn basename_strips_every_trailing_slash_and_normalises_backslashes() {
        assert_eq!(basename("/a/b/c.py"), "c.py");
        assert_eq!(basename("/a/b//"), "b");
        assert_eq!(basename("///"), "");
        assert_eq!(basename(r"C:\a\b.py"), "b.py");
        assert_eq!(basename("bare.py"), "bare.py");
    }

    #[test]
    fn clamp_days_pins_both_ends() {
        assert_eq!(clamp_days(0), 1);
        assert_eq!(clamp_days(-5), 1);
        assert_eq!(clamp_days(90), 90);
        assert_eq!(clamp_days(9_999), MAX_SINCE_DAYS);
    }

    #[test]
    fn splitlines_breaks_on_the_wide_cpython_set() {
        assert_eq!(py_splitlines("a\r\nb\rc\nd"), vec!["a", "b", "c", "d"]);
        assert_eq!(py_splitlines("a\u{0c}b"), vec!["a", "b"]);
        assert_eq!(py_splitlines("a\u{2028}b"), vec!["a", "b"]);
        assert_eq!(py_splitlines(""), Vec::<&str>::new());
        // A trailing boundary does NOT produce a trailing empty element.
        assert_eq!(py_splitlines("a\n"), vec!["a"]);
    }

    // ── the six regexes ──────────────────────────────────────────────────────

    #[test]
    fn path_sub_needs_two_separators_and_replaces_with_the_basename() {
        assert_eq!(
            sub_paths("File /a/b/foo.py not found"),
            "File foo.py not found"
        );
        // One separator: `{2,}` is unsatisfiable, so the text survives.
        assert_eq!(sub_paths("File /foo.py here"), "File /foo.py here");
        // A trailing separator with nothing after it also fails the second rep.
        assert_eq!(sub_paths("at /a/ end"), "at /a/ end");
        // ...but a run ending in a separator that has content before it matches.
        assert_eq!(sub_paths("at /ab/c/ end"), "at c end");
        // The optional drive prefix is consumed into the match.
        assert_eq!(sub_paths("C:/a/b.py bad"), "b.py bad");
        // A URL is where the drive-letter branch misfires, and it does so in
        // BOTH implementations: the scan reaches `p`, `(?:[A-Za-z]:)?` claims
        // `p:`, and the match is `p://x/y` whose basename is `y`. So `http://`
        // loses its last scheme letter. Bug-for-bug, and worth seeing spelled
        // out — it is why URLs in an error body normalise oddly.
        assert_eq!(sub_paths("see http://x/y now"), "see htty now");
    }

    #[test]
    fn hex_sub_needs_a_whole_word_of_eight_or_more() {
        assert_eq!(sub_hex("sha deadbeef here"), "sha <hex> here");
        assert_eq!(sub_hex("sha DEADBEEF here"), "sha <hex> here");
        // Seven is short.
        assert_eq!(sub_hex("sha deadbee here"), "sha deadbee here");
        // No word boundary inside a word.
        assert_eq!(sub_hex("xdeadbeefcafe"), "xdeadbeefcafe");
        // A `.` IS a boundary.
        assert_eq!(sub_hex("deadbeef.log"), "<hex>.log");
        // `g` is not a hex digit, so the whole run is disqualified.
        assert_eq!(sub_hex("deadbeefg"), "deadbeefg");
    }

    #[test]
    fn number_and_whitespace_subs() {
        assert_eq!(sub_nums("line 212 of 7"), "line <n> of <n>");
        assert_eq!(collapse_ws("a  \t\n b"), "a b");
    }

    #[test]
    fn normalise_signature_is_the_full_pipeline() {
        assert_eq!(
            normalise_signature("File /a/b/foo.py:212 not found"),
            "File foo.py:<n> not found"
        );
        assert_eq!(
            normalise_signature("File /x/y/foo.py:7 not found"),
            "File foo.py:<n> not found"
        );
        // Only the first meaningful line, and leading blank lines are skipped.
        assert_eq!(
            normalise_signature("\n\n  boom happened  \ntrailing"),
            "boom happened"
        );
        assert_eq!(normalise_signature("   \n  "), "<empty error body>");
        assert_eq!(normalise_signature(""), "<empty error body>");
        // The 160-cap is by code point.
        let long = "é".repeat(300);
        assert_eq!(normalise_signature(&long).chars().count(), 160);
    }

    #[test]
    fn env_assign_prefix_needs_trailing_whitespace() {
        assert_eq!(strip_env_assign("FOO=bar  baz qux"), "baz qux");
        assert_eq!(strip_env_assign("FOO=bar"), "FOO=bar");
        assert_eq!(strip_env_assign("_A1=x y"), "y");
        // A leading digit is not a valid identifier start.
        assert_eq!(strip_env_assign("1FOO=bar baz"), "1FOO=bar baz");
        assert_eq!(strip_env_assign("no-equals here"), "no-equals here");
    }

    #[test]
    fn cd_prefix_backtracks_over_the_greedy_token() {
        assert_eq!(strip_cd_prefix("cd /x && ls -la"), "ls -la");
        // The greedy `\S+` swallows `&&ls` first and must give it back.
        assert_eq!(strip_cd_prefix("cd /x&&ls"), "ls");
        assert_eq!(strip_cd_prefix("cd /x ls"), "cd /x ls");
        assert_eq!(strip_cd_prefix("cdx /a && b"), "cdx /a && b");
    }

    #[test]
    fn normalise_command_keys_on_head_plus_subcommand() {
        assert_eq!(normalise_command("npm install --save x"), "npm install");
        assert_eq!(normalise_command("git push origin main"), "git push");
        // Flags are skipped when looking for the subcommand.
        assert_eq!(normalise_command("git -C /tmp status"), "git /tmp");
        // A script head keeps only the script's basename.
        assert_eq!(normalise_command("python /a/b/run.py --x"), "python run.py");
        // An unknown head carries no subcommand at all.
        assert_eq!(normalise_command("pytest -q tests/"), "pytest");
        assert_eq!(normalise_command("/usr/bin/pytest -q"), "pytest");
        // Both prefix strippers, applied to a fixpoint.
        assert_eq!(
            normalise_command("cd /repo && FOO=1 npm run build"),
            "npm run"
        );
        assert_eq!(normalise_command("   "), "<empty>");
        assert_eq!(normalise_command(""), "<empty>");
    }

    // ── the mining arithmetic ────────────────────────────────────────────────

    #[test]
    fn ts_to_epoch_reads_both_suffixes_and_fails_soft() {
        assert!((ts_to_epoch(Some("2026-07-31T12:34:56Z")) - 1_785_501_296.0).abs() < 1e-6);
        assert!((ts_to_epoch(Some("2026-07-31T12:34:56+00:00")) - 1_785_501_296.0).abs() < 1e-6);
        assert!(ts_to_epoch(Some("not a stamp")).abs() < f64::EPSILON);
        assert!(ts_to_epoch(None).abs() < f64::EPSILON);
        assert!(ts_to_epoch(Some("")).abs() < f64::EPSILON);
    }

    #[test]
    fn bisect_right_finds_the_first_call_after_the_epoch() {
        let events = vec![
            (1.0, "Read".to_owned(), String::new()),
            (2.0, "Edit".to_owned(), "/a".to_owned()),
            (2.0, "Write".to_owned(), "/b".to_owned()),
            (3.0, "Bash".to_owned(), String::new()),
        ];
        // The `\uffff` sentinels put the key after every same-epoch entry.
        assert_eq!(bisect_right_after(&events, 2.0), 3);
        assert_eq!(bisect_right_after(&events, 0.0), 0);
        assert_eq!(bisect_right_after(&events, 3.0), 4);
    }

    #[test]
    fn file_reason_renders_the_percentage_with_cpythons_rule() {
        let mut agg = FileAgg {
            failure_count: 2,
            ..FileAgg::default()
        };
        agg.failure_sessions.insert("s1".to_owned());
        agg.failure_sessions.insert("s2".to_owned());
        assert_eq!(
            file_reason(&agg, Some(0.4), 5),
            "Failed in 2 of 5 sessions that touched it (40%)."
        );
        // `0 < denom < 3` appends the small-sample note.
        assert_eq!(
            file_reason(&agg, Some(1.0), 2),
            "Failed in 2 of 2 sessions that touched it (100%) — small sample."
        );
        // No denominator at all: the count phrasing, and the em-dashed note.
        assert_eq!(
            file_reason(&agg, None, 0),
            "2 failures recorded (touch history untracked — rate unknown)."
        );
    }

    #[test]
    fn signature_reason_has_three_shapes() {
        let agg = SigAgg {
            category: "Other".to_owned(),
            count: 5,
            sessions: ["a".to_owned(), "b".to_owned()].into_iter().collect(),
            first: None,
            last: None,
            tools: HashMap::new(),
            files: HashMap::new(),
            example: String::new(),
            last_by_session: HashMap::new(),
        };
        assert_eq!(
            signature_reason(&agg, 0, None),
            "Recurred in 2 sessions (5 occurrences). No session in window is known to have moved past it."
        );
        assert_eq!(
            signature_reason(&agg, 1, None),
            "Recurred in 2 sessions (5 occurrences). 1 moved past it."
        );
        assert_eq!(
            signature_reason(&agg, 2, Some(&("Read x.py".to_owned(), 2))),
            "Recurred in 2 sessions (5 occurrences). 2 moved past it — most often the next step was Read x.py."
        );
    }

    #[test]
    fn counter_orderings_are_count_desc_then_key_asc() {
        let mut counter: Counter = HashMap::new();
        counter.insert("b".to_owned(), 2);
        counter.insert("a".to_owned(), 2);
        counter.insert("c".to_owned(), 9);
        let ordered: Vec<&str> = by_count_then_key(&counter)
            .into_iter()
            .map(|(k, _)| k.as_str())
            .collect();
        assert_eq!(ordered, vec!["c", "a", "b"]);
        // `dict(sorted(...))` is key order, and the rendered object keeps it.
        assert_eq!(
            stax_memory::pyjson::dumps_http(&counter_to_sorted_object(&counter)),
            r#"{"a":2,"b":2,"c":9}"#
        );
    }

    #[test]
    fn ordered_map_iterates_in_first_insertion_order() {
        let mut map: OrderedMap<i64> = OrderedMap::new();
        *map.entry("z", || 0) += 1;
        *map.entry("a", || 0) += 1;
        *map.entry("z", || 0) += 1;
        let keys: Vec<&str> = map.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, vec!["z", "a"]);
        assert_eq!(map.get("z"), Some(&2));
    }

    // ── the assembled report ─────────────────────────────────────────────────

    fn empty_collected() -> Collected {
        Collected::empty(pinned().minus_days(90).isoformat(), 90)
    }

    #[test]
    fn the_empty_report_has_the_full_shape_in_declaration_order() {
        let report = assemble(
            &empty_collected(),
            TOP_N_FILES,
            TOP_N_SIGNATURES,
            TOP_N_COMMANDS,
        );
        assert_eq!(
            stax_memory::pyjson::dumps_http(&report),
            concat!(
                r#"{"window":{"since":"2026-05-02T12:34:56.789012+00:00","days":90},"#,
                r#""sources":{"message_tool_mart":false},"#,
                r#""totals":{"tool_call_count":0,"error_count":0,"attributed_error_count":0,"#,
                r#""interruption_count":0,"interruption_session_count":0,"session_count":0,"#,
                r#""sessions_with_failures":0,"files_touched":0},"#,
                r#""file_risk":[],"error_signatures":[],"command_clusters":[]}"#
            )
        );
        // The degraded path emits the same eight totals keys, all zero.
        assert_eq!(
            empty_totals(),
            report.get("totals").cloned().expect("totals present")
        );
    }

    /// A file with two touch sessions and one failure session renders every
    /// `file_risk` key in the dataclass's order, `failure_rate` included.
    #[test]
    fn a_file_risk_entry_renders_the_dataclass_order() {
        let mut agg = FileAgg {
            touch_count: 4,
            edit_count: 3,
            read_count: 1,
            failure_count: 2,
            ..FileAgg::default()
        };
        agg.touch_sessions.insert("s1".to_owned());
        agg.touch_sessions.insert("s2".to_owned());
        agg.failure_sessions.insert("s1".to_owned());
        bump(&mut agg.categories, "File Not Found");
        bump(&mut agg.categories, "Access Denied");
        agg.last_touch = Some((2.0, "2026-07-30T00:00:00+00:00".to_owned()));
        agg.last_failure = Some((1.0, "2026-07-29T00:00:00+00:00".to_owned()));

        let entry = file_risk_entry("/repo/auth.py", &agg);
        assert_eq!(
            stax_memory::pyjson::dumps_http(&entry.payload),
            concat!(
                r#"{"path":"/repo/auth.py","touch_count":4,"edit_count":3,"read_count":1,"#,
                r#""touch_session_count":2,"failure_count":2,"failure_session_count":1,"#,
                r#""failure_rate":0.5,"interruption_count":0,"#,
                r#""last_touch_ts":"2026-07-30T00:00:00+00:00","#,
                r#""last_failure_ts":"2026-07-29T00:00:00+00:00","#,
                r#""categories":{"Access Denied":1,"File Not Found":1},"#,
                r#""reason":"Failed in 1 of 2 sessions that touched it (50%) — small sample."}"#
            )
        );
    }

    /// The union denominator is what keeps the rate at or below `1.0` when the
    /// mart has not caught up with a failure's session.
    #[test]
    fn the_denominator_is_the_union_so_the_rate_cannot_exceed_one() {
        let mut agg = FileAgg {
            touch_count: 1,
            read_count: 1,
            failure_count: 3,
            ..FileAgg::default()
        };
        agg.touch_sessions.insert("s1".to_owned());
        agg.failure_sessions.insert("s2".to_owned());
        let entry = file_risk_entry("/x.py", &agg);
        assert_eq!(entry.failure_rate, Some(0.5));
        assert_eq!(entry.payload["touch_session_count"], Value::from(2));
    }

    /// `touch_count == 0` (a failure with no mart row) leaves the rate NULL
    /// rather than reporting a misleading 100%.
    #[test]
    fn an_untracked_file_reports_a_null_rate() {
        let mut agg = FileAgg {
            failure_count: 1,
            ..FileAgg::default()
        };
        agg.failure_sessions.insert("s1".to_owned());
        let entry = file_risk_entry("/x.py", &agg);
        assert_eq!(entry.failure_rate, None);
        assert_eq!(entry.payload["failure_rate"], Value::Null);
        assert!(
            entry.payload["reason"]
                .as_str()
                .expect("reason is a string")
                .contains("touch history untracked")
        );
    }

    #[test]
    fn the_zero_file_risk_entry_is_well_formed() {
        assert_eq!(
            stax_memory::pyjson::dumps_http(&zero_file_risk("src/x.py")),
            concat!(
                r#"{"path":"src/x.py","touch_count":0,"edit_count":0,"read_count":0,"#,
                r#""touch_session_count":0,"failure_count":0,"failure_session_count":0,"#,
                r#""failure_rate":null,"interruption_count":0,"last_touch_ts":null,"#,
                r#""last_failure_ts":null,"categories":{},"#,
                r#""reason":"No activity recorded in window."}"#
            )
        );
    }

    // ── the collection pass against a real store ─────────────────────────────

    /// A store with the four relations `patterns.py` reads, `messages` created as
    /// a VIEW so the law-7 guard is actually exercised.
    fn store() -> Connection {
        let conn = Connection::open_in_memory().expect("in-memory store");
        conn.execute_batch(
            "CREATE TABLE projects (id INTEGER PRIMARY KEY, slug TEXT);
             CREATE TABLE sessions (id INTEGER PRIMARY KEY, session_id TEXT, project_id INTEGER);
             CREATE TABLE messages_part (
                 session_fk INTEGER, seq INTEGER, timestamp TEXT, role TEXT,
                 raw_json TEXT, tools_json TEXT, content_text TEXT);
             CREATE VIEW messages AS SELECT * FROM messages_part;
             CREATE TABLE message_tool_mart (
                 project_id INTEGER, session_id TEXT, day TEXT, ts TEXT,
                 tool_name TEXT, file_path TEXT, message_id TEXT, call_index INTEGER);
             INSERT INTO projects VALUES (1, 'proj');
             INSERT INTO sessions VALUES (10, 'sess-a', 1), (11, 'sess-b', 1);",
        )
        .expect("schema");
        conn
    }

    fn tool_result_error(tool_use_id: &str, body: &str) -> String {
        serde_json::json!({
            "message": {"content": [
                {"type": "tool_result", "tool_use_id": tool_use_id,
                 "is_error": true, "content": body}
            ]}
        })
        .to_string()
    }

    #[test]
    fn the_view_guard_lets_the_partitioned_messages_object_through() {
        let conn = store();
        assert!(table_or_view_exists(&conn, "messages"));
        assert!(table_or_view_exists(&conn, "sessions"));
        assert!(!table_or_view_exists(&conn, "no_such_relation"));
        // The table-only guard would have hidden it — this is DIV-148, live.
        assert!(!crate::services::mart_queries::table_exists(&conn, "messages").expect("probe"));
    }

    #[test]
    fn an_empty_store_yields_the_empty_report_not_an_error() {
        let conn = Connection::open_in_memory().expect("bare store");
        let report = mine_patterns(&conn, 90, None, pinned());
        assert_eq!(report["sources"]["message_tool_mart"], Value::Bool(false));
        assert_eq!(report["totals"], empty_totals());
        assert_eq!(report["window"]["days"], Value::from(90));
    }

    #[test]
    fn an_empty_project_filter_scopes_to_nothing_rather_than_widening() {
        let conn = store();
        conn.execute(
            "INSERT INTO message_tool_mart VALUES (1,'sess-a','2026-07-30','2026-07-30T00:00:00+00:00','Edit','/a.py','m1',0)",
            [],
        )
        .expect("mart row");
        let all = mine_patterns(&conn, 90, None, pinned());
        assert_eq!(all["totals"]["tool_call_count"], Value::from(1));
        // `project_ids == []` — an empty report, and `mart_available` stays false
        // because the read never happened.
        let none = mine_patterns(&conn, 90, Some(&[]), pinned());
        assert_eq!(none["totals"]["tool_call_count"], Value::from(0));
        assert_eq!(none["sources"]["message_tool_mart"], Value::Bool(false));
    }

    #[test]
    fn errors_are_attributed_back_to_their_tool_call_and_clustered() {
        let conn = store();
        // Two sessions, each: an assistant Bash call, then its errored result.
        for (fk, session) in [(10, "sess-a"), (11, "sess-b")] {
            conn.execute(
                "INSERT INTO messages_part VALUES (?, 1, '2026-07-30T00:00:00+00:00', 'assistant', NULL, ?, NULL)",
                rusqlite::params![
                    fk,
                    serde_json::json!([{
                        "id": format!("tu-{session}"), "name": "Bash",
                        "input": {"command": "cd /repo && npm install --save x"}
                    }])
                    .to_string()
                ],
            )
            .expect("assistant row");
            conn.execute(
                "INSERT INTO messages_part VALUES (?, 2, '2026-07-30T00:01:00+00:00', 'user', ?, '[]', NULL)",
                rusqlite::params![
                    fk,
                    tool_result_error(
                        &format!("tu-{session}"),
                        "ENOENT: no such file or directory, open '/repo/pkg/402'"
                    )
                ],
            )
            .expect("result row");
        }

        let report = mine_patterns(&conn, 90, Some(&[1]), pinned());
        assert_eq!(report["totals"]["error_count"], Value::from(2));
        assert_eq!(report["totals"]["attributed_error_count"], Value::from(2));
        assert_eq!(report["totals"]["sessions_with_failures"], Value::from(2));

        // One cluster, keyed on the normalised head+subcommand.
        let clusters = report["command_clusters"].as_array().expect("clusters");
        assert_eq!(clusters.len(), 1);
        assert_eq!(clusters[0]["command"], Value::from("npm install"));
        assert_eq!(clusters[0]["failure_count"], Value::from(2));
        assert_eq!(clusters[0]["session_count"], Value::from(2));
        assert_eq!(
            clusters[0]["example"],
            Value::from("cd /repo && npm install --save x")
        );

        // One recurring signature: two sessions clears MIN_RECURRENCE_SESSIONS,
        // and the number is normalised away.
        let sigs = report["error_signatures"].as_array().expect("signatures");
        assert_eq!(sigs.len(), 1);
        assert_eq!(sigs[0]["session_count"], Value::from(2));
        assert_eq!(sigs[0]["count"], Value::from(2));
        assert_eq!(sigs[0]["top_tools"], serde_json::json!(["Bash"]));
        assert!(
            sigs[0]["signature"]
                .as_str()
                .expect("signature")
                .contains("<n>")
        );
    }

    #[test]
    fn a_signature_in_one_session_only_is_not_recurring() {
        let conn = store();
        conn.execute(
            "INSERT INTO messages_part VALUES (10, 1, '2026-07-30T00:00:00+00:00', 'assistant', NULL, ?, NULL)",
            rusqlite::params![
                serde_json::json!([{"id": "tu-1", "name": "Read", "input": {"file_path": "/a.py"}}])
                    .to_string()
            ],
        )
        .expect("assistant row");
        conn.execute(
            "INSERT INTO messages_part VALUES (10, 2, '2026-07-30T00:01:00+00:00', 'user', ?, '[]', NULL)",
            rusqlite::params![tool_result_error("tu-1", "boom")],
        )
        .expect("result row");

        let report = mine_patterns(&conn, 90, None, pinned());
        assert_eq!(report["totals"]["error_count"], Value::from(1));
        assert_eq!(report["error_signatures"], serde_json::json!([]));
        // ...but the file-risk row is still produced, with a NULL rate (the mart
        // has no touch for it).
        let risk = report["file_risk"].as_array().expect("file_risk");
        assert_eq!(risk.len(), 1);
        assert_eq!(risk[0]["path"], Value::from("/a.py"));
        assert_eq!(risk[0]["failure_rate"], Value::Null);
    }

    #[test]
    fn the_like_screen_false_positive_is_dropped_by_the_json_parse() {
        let conn = store();
        // `raw_json` contains the literal text but no errored tool_result block.
        conn.execute(
            "INSERT INTO messages_part VALUES (10, 1, '2026-07-30T00:00:00+00:00', 'user', ?, '[]', NULL)",
            rusqlite::params![
                r#"{"message":{"content":[{"type":"text","text":"the flag \"is_error\": true means"}]}}"#
            ],
        )
        .expect("row");
        let report = mine_patterns(&conn, 90, None, pinned());
        assert_eq!(report["totals"]["error_count"], Value::from(0));
    }

    #[test]
    fn interruptions_are_counted_from_the_classifiers_markers() {
        let conn = store();
        conn.execute(
            "INSERT INTO messages_part VALUES (10, 1, '2026-07-30T00:00:00+00:00', 'user', NULL, '[]', ?)",
            rusqlite::params![format!("{INTERRUPT_PREFIX} and then some")],
        )
        .expect("interrupt row");
        conn.execute(
            "INSERT INTO messages_part VALUES (11, 1, '2026-07-30T00:00:00+00:00', 'user', NULL, '[]', ?)",
            rusqlite::params![INTERRUPT_API],
        )
        .expect("abort row");
        let report = mine_patterns(&conn, 90, None, pinned());
        assert_eq!(report["totals"]["interruption_count"], Value::from(2));
        assert_eq!(
            report["totals"]["interruption_session_count"],
            Value::from(2)
        );
        // Interruption sessions count towards `session_count` too.
        assert_eq!(report["totals"]["session_count"], Value::from(2));
    }

    /// The two reads bound the window at DIFFERENT granularities, and that is
    /// Python's shape, not an oversight of this port.
    ///
    /// `message_tool_mart` is filtered by `day >= since_iso[:10]`, a truncated
    /// calendar day, so a `1d` window taken at 12:34 pulls in the *whole* of the
    /// preceding day — up to 24 extra hours. `messages` is filtered by
    /// `timestamp >= since_iso`, the full microsecond instant. The same request
    /// therefore sees a wider touch window than error window. (Structurally the
    /// note `routes/cost.rs` records for `week` and the day-aligned mart.)
    #[test]
    fn the_mart_window_is_day_truncated_and_the_messages_window_is_not() {
        let conn = store();
        for (day, ts) in [
            // `now` is 2026-07-31T12:34:56Z, so `1d` back is 2026-07-30T12:34:56
            // and `since_day` is "2026-07-30".
            ("2026-07-31", "2026-07-31T09:00:00+00:00"),
            // BEFORE the instant, but on the boundary day — the mart keeps it.
            ("2026-07-30", "2026-07-30T00:00:00+00:00"),
            ("2026-07-29", "2026-07-29T23:59:59+00:00"),
            ("2020-01-01", "2020-01-01T00:00:00+00:00"),
        ] {
            conn.execute(
                "INSERT INTO message_tool_mart VALUES (1,'sess-a',?,?,'Read','/a.py','m1',0)",
                rusqlite::params![day, ts],
            )
            .expect("mart row");
        }
        // 90d reaches everything except the 2020 row.
        assert_eq!(
            mine_patterns(&conn, 90, None, pinned())["totals"]["tool_call_count"],
            Value::from(3)
        );
        // 1d keeps BOTH 2026-07-31 and the whole of 2026-07-30 — including the
        // 00:00:00 row, which is twelve hours older than the instant.
        assert_eq!(
            mine_patterns(&conn, 1, None, pinned())["totals"]["tool_call_count"],
            Value::from(2)
        );
    }

    #[test]
    fn file_risk_falls_back_to_a_unique_suffix_match() {
        let conn = store();
        conn.execute(
            "INSERT INTO messages_part VALUES (10, 1, '2026-07-30T00:00:00+00:00', 'assistant', NULL, ?, NULL)",
            rusqlite::params![
                serde_json::json!([{"id": "tu-1", "name": "Edit",
                                    "input": {"file_path": "/repo/src/auth.py"}}])
                    .to_string()
            ],
        )
        .expect("assistant row");
        conn.execute(
            "INSERT INTO messages_part VALUES (10, 2, '2026-07-30T00:01:00+00:00', 'user', ?, '[]', NULL)",
            rusqlite::params![tool_result_error("tu-1", "boom")],
        )
        .expect("result row");

        let exact = file_risk(&conn, "/repo/src/auth.py", 90, None, pinned());
        assert_eq!(exact["failure_count"], Value::from(1));
        let suffix = file_risk(&conn, "src/auth.py", 90, None, pinned());
        assert_eq!(suffix["path"], Value::from("/repo/src/auth.py"));
        let missing = file_risk(&conn, "nope.py", 90, None, pinned());
        assert_eq!(
            missing["reason"],
            Value::from("No activity recorded in window.")
        );
    }
}
