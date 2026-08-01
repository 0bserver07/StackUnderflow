//! `hooks/handlers.py` — the dispatch point Claude Code actually runs.
//!
//! One short-lived process per fire: Claude Code spawns it, pipes the payload as
//! JSON on stdin, and (for the passive-observer hooks) ignores the result. The
//! contract is narrow and defensive — `run()` catches everything and returns
//! `0`. We are a tape recorder, not a gate.
//!
//! Nine ids arrive here. Four RECORD (`captured_events`), three INJECT
//! ([`crate::inject`]), one RECALLS ([`crate::recall`]), one NUDGES
//! ([`crate::proactive`]). Only events worth a row produce one: a `PostToolUse`
//! that *failed*, a `UserPromptSubmit` that *looked like a correction*, every
//! `Stop` and every `PreCompact`. A successful tool call or an ordinary prompt
//! is a silent no-op.
//!
//! ## The write, flagged
//!
//! **DIV-201 — the capture hooks WRITE the live store, and this port does too.**
//! Three writes per recorded fire, on the agent's critical path:
//! `store.db.connect`'s `PRAGMA journal_mode = WAL`, a `CREATE TABLE IF NOT
//! EXISTS captured_events` (+ two indexes) self-heal, and an `INSERT OR IGNORE`.
//! That is the feature working as designed — a capture hook that does not
//! capture is nothing — so it is ported **bug-for-bug**, including the schema
//! self-heal (`ensure_captured_events_table` deliberately does *not* bump
//! `user_version`, so it can race the dashboard's `v010` migration and both
//! win). It is called out because "hooks are read-only" is false for this crate
//! and the next reader must not assume it: the injection half is read-only
//! (DIV-200), the capture half is not.

use rusqlite::Connection;
use stax_core::queries::pyjson::Value;
use stax_core::queries::{pyjson, pytime};

use crate::env::{HookEnv, abspath};
use crate::inject;
use crate::pystr;
use crate::templates;

/// `event_kind` values written to `captured_events`.
pub const KIND_FAILURE: &str = "failure";
/// See [`KIND_FAILURE`].
pub const KIND_CORRECTION: &str = "correction";
/// See [`KIND_FAILURE`].
pub const KIND_BOUNDARY: &str = "boundary";
/// See [`KIND_FAILURE`].
pub const KIND_SNAPSHOT: &str = "snapshot";

/// `handlers._TRUNCATE` — how much of an error line survives into the sanitised
/// payload. Long enough to be useful, short enough that a secret pasted into a
/// prompt does not end up here.
const TRUNCATE: usize = 500;

/// The host whose hooks this module implements (`handlers._HOST_PROVIDER`).
///
/// Everything in `hooks/` is Claude Code's hook surface, so a slug collision
/// across providers resolves in the host's favour — a self-declaration passed
/// into the query as a *parameter* rather than baked into SQL.
const HOST_PROVIDER: &str = "claude";

/// What one hook fire produced. The process exit code is always 0; this is what
/// goes to stdout.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Fired {
    /// The bytes to write to stdout (already newline-terminated, or empty).
    pub stdout: String,
}

/// `handlers.run` — handle one hook fire. Always exit code `0`.
///
/// Dispatches on *hook_id*: the four capture ids record a row (or no-op), the
/// three `stackunderflow-inject-*` ids write a context-injection envelope,
/// `stackunderflow-pretool-recall` shells the `memory` CLI, and
/// `stackunderflow-posttool-nudge` consults the proactive cache. An unknown id
/// is a no-op. Any error is swallowed — neither a recorder nor an injector may
/// make Claude Code stumble.
#[must_use]
pub fn run(hook_id: &str, payload: &Value, capture_content: bool, env: &HookEnv) -> Fired {
    let payload = match payload {
        Value::Object(_) => payload.clone(),
        // `payload if isinstance(payload, dict) else {}`.
        _ => Value::Object(Vec::new()),
    };

    if templates::INJECT_HOOK_IDS.contains(&hook_id) {
        return emit(inject::build_injection(hook_id, &payload, env));
    }
    if templates::RECALL_HOOK_IDS.contains(&hook_id) {
        return emit(crate::recall::build_recall(hook_id, &payload, env));
    }
    if templates::NUDGE_HOOK_IDS.contains(&hook_id) {
        return emit(crate::proactive::build_posttool_nudge(
            hook_id, &payload, env,
        ));
    }

    let Some((kind, sanitised)) = classify(hook_id, &payload, capture_content, env) else {
        return Fired::default(); // nothing worth recording
    };
    // The reference's blanket `except Exception` — a locked store, a missing
    // `projects` table, a read-only file: all swallowed.
    let _ = write_event(hook_id, kind, &sanitised, &payload, env);
    Fired::default()
}

/// `sys.stdout.write(output if output.endswith("\n") else output + "\n")`, and
/// nothing at all for an empty output.
fn emit(output: String) -> Fired {
    if output.is_empty() {
        return Fired::default();
    }
    let stdout = if output.ends_with('\n') {
        output
    } else {
        format!("{output}\n")
    };
    Fired { stdout }
}

// ── classification ──────────────────────────────────────────────────────────

/// `handlers._classify` — `(event_kind, payload_to_store)`, or `None` for
/// "don't record this one".
fn classify(
    hook_id: &str,
    payload: &Value,
    capture_content: bool,
    env: &HookEnv,
) -> Option<(&'static str, Value)> {
    let stored = |meta: Vec<(String, Value)>| {
        if capture_content {
            payload.clone()
        } else {
            drop_none(meta)
        }
    };

    match hook_id {
        "stackunderflow-post-tool-use" => {
            // `Err` is the reference raising inside `_tool_call_failed` — the
            // fire is over, no row, no output.
            if !tool_call_failed(payload).ok()? {
                return None;
            }
            let mut meta = vec![
                (
                    "hook_event_name".into(),
                    default_str(payload, "hook_event_name", "PostToolUse"),
                ),
                ("tool_name".into(), cloned(payload, "tool_name")),
                (
                    "exit_code".into(),
                    extract_exit_code(payload)
                        .ok()?
                        .map_or(Value::Null, Value::Int),
                ),
                ("cwd".into(), cloned(payload, "cwd")),
            ];
            if let Some(err) = extract_error_summary(payload) {
                meta.push(("error_summary".into(), Value::Str(err)));
            }
            Some((KIND_FAILURE, stored(meta)))
        }

        "stackunderflow-user-prompt" => {
            let prompt = payload.get("prompt").and_then(Value::as_str)?;
            let matched = correction_match(prompt)?;
            let meta = vec![
                (
                    "hook_event_name".into(),
                    default_str(payload, "hook_event_name", "UserPromptSubmit"),
                ),
                ("matched_keyword".into(), Value::Str(matched)),
                (
                    "prompt_length".into(),
                    Value::Int(pystr::len_chars(prompt) as i64),
                ),
                ("cwd".into(), cloned(payload, "cwd")),
            ];
            Some((KIND_CORRECTION, stored(meta)))
        }

        "stackunderflow-stop" => {
            let meta = vec![
                (
                    "hook_event_name".into(),
                    default_str(payload, "hook_event_name", "Stop"),
                ),
                (
                    "stop_hook_active".into(),
                    cloned(payload, "stop_hook_active"),
                ),
                ("cwd".into(), cloned(payload, "cwd")),
                (
                    "session_totals".into(),
                    session_totals(payload.get("session_id"), env),
                ),
            ];
            Some((KIND_BOUNDARY, stored(meta)))
        }

        "stackunderflow-pre-compact" => {
            let meta = vec![
                (
                    "hook_event_name".into(),
                    default_str(payload, "hook_event_name", "PreCompact"),
                ),
                ("trigger".into(), cloned(payload, "trigger")),
                ("cwd".into(), cloned(payload, "cwd")),
                (
                    "session_totals".into(),
                    session_totals(payload.get("session_id"), env),
                ),
            ];
            Some((KIND_SNAPSHOT, stored(meta)))
        }

        _ => None, // unknown hook id
    }
}

fn cloned(payload: &Value, key: &str) -> Value {
    payload.get(key).cloned().unwrap_or(Value::Null)
}

/// `payload.get(key, default)` where the default is a literal string.
fn default_str(payload: &Value, key: &str, fallback: &str) -> Value {
    payload
        .get(key)
        .cloned()
        .unwrap_or_else(|| Value::Str(fallback.to_string()))
}

/// `handlers._drop_none` — strip `None`-valued keys so the stored metadata stays
/// tidy. Note `session_totals` is a dict and always survives.
fn drop_none(entries: Vec<(String, Value)>) -> Value {
    Value::Object(
        entries
            .into_iter()
            .filter(|(_, value)| !matches!(value, Value::Null))
            .collect(),
    )
}

// ── failure detection ───────────────────────────────────────────────────────

/// `handlers._EXIT_CODE_KEYS`, checked case-insensitively.
const EXIT_CODE_KEYS: [&str; 7] = [
    "exit_code",
    "exitcode",
    "exit",
    "returncode",
    "return_code",
    "code",
    "status",
];
/// `handlers._ERROR_FLAG_KEYS`.
const ERROR_FLAG_KEYS: [&str; 4] = ["is_error", "error", "iserror", "failed"];

/// `handlers._tool_call_failed`.
///
/// The `tool_response` shape is not stable across Claude Code versions, so this
/// probes several plausible spots. When none is present the call is *not* a
/// failure — no false-positive rows. Stdout-scanning for "ERROR" is deliberately
/// out: that is exactly the heuristic this replaces.
/// The reference raised here and `run`'s blanket `except` swallowed it, so the
/// fire produces no row and no output. See [`py_int_str`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Aborted;

/// `handlers._tool_call_failed`.
///
/// The `tool_response` shape is not stable across Claude Code versions, so this
/// probes several plausible spots. When none is present the call is *not* a
/// failure — no false-positive rows. `Err` is the reference raising; see
/// [`Aborted`].
#[must_use = "an Err means the reference aborted the whole fire"]
pub fn tool_call_failed(payload: &Value) -> Result<bool, Aborted> {
    for key in ["tool_response", "tool_input"] {
        match payload.get(key) {
            Some(blob @ Value::Object(_)) => {
                if dict_signals_failure(blob)? {
                    return Ok(true);
                }
            }
            Some(Value::Array(items)) => {
                for item in items {
                    if matches!(item, Value::Object(_)) && dict_signals_failure(item)? {
                        return Ok(true);
                    }
                }
            }
            _ => {}
        }
    }
    Ok(false)
}

/// A case-insensitive key lookup that reproduces `{str(k).lower(): v for ...}` —
/// including its collision rule, where the **last** duplicate wins.
fn lower_get<'a>(blob: &'a Value, key: &str) -> Option<&'a Value> {
    let Value::Object(entries) = blob else {
        return None;
    };
    entries
        .iter()
        .rfind(|(name, _)| name.to_lowercase() == key)
        .map(|(_, value)| value)
}

fn dict_signals_failure(blob: &Value) -> Result<bool, Aborted> {
    for key in EXIT_CODE_KEYS {
        if let Some(value) = lower_get(blob, key) {
            match value {
                // `isinstance(v, bool)` is checked FIRST, and `bool` is a
                // subclass of `int` in Python — a `True` exit_code is skipped,
                // not treated as 1.
                Value::Bool(_) => continue,
                Value::Int(number) if *number != 0 => return Ok(true),
                Value::Str(text) if py_int_str(text)?.is_some_and(|number| number != 0) => {
                    return Ok(true);
                }
                // A FLOAT is not an int: `isinstance(1.0, int)` is False, so a
                // float exit code is silently ignored. Bug-for-bug.
                _ => {}
            }
        }
    }
    for key in ERROR_FLAG_KEYS {
        if let Some(value) = lower_get(blob, key) {
            match value {
                Value::Bool(true) => return Ok(true),
                Value::Str(text) if !text.trim().is_empty() => return Ok(true),
                _ => {}
            }
        }
    }
    Ok(matches!(
        lower_get(blob, "success"),
        Some(Value::Bool(false))
    ))
}

/// `v.strip().lstrip("-").isdigit()` and then, only if that passed, `int(v)`.
///
/// Two things about that pair are load-bearing and neither is obvious:
///
/// * **The guard and the parse disagree.** `lstrip("-")` removes *every* leading
///   `-`, so `"--5"` passes `isdigit()` and then `int("--5")` **raises**. The
///   exception escapes `_tool_call_failed`, escapes `_classify`, and is caught by
///   `run`'s blanket `except` — so the fire records nothing and prints nothing.
///   That is [`Aborted`]: not "this key is not a number", but "this whole hook
///   fire is over". Returning `None` instead would record a row Python never
///   records.
/// * **`str.isdigit()` is Unicode-aware** (True for `"١٢٣"`) and `int()` accepts
///   those digits too, so the reference parses non-ASCII numerals. This narrows
///   to ASCII: Claude Code writes exit codes as ASCII, so the difference is
///   unreachable rather than a behaviour change — recorded because "ASCII
///   digits" is a decision, not an accident.
///
/// `Ok(None)` = not an integer-shaped string at all (the ordinary case).
fn py_int_str(text: &str) -> Result<Option<i64>, Aborted> {
    let stripped = text.trim();
    let body = stripped.trim_start_matches('-');
    if body.is_empty() || !body.chars().all(|c| c.is_ascii_digit()) {
        return Ok(None); // `isdigit()` said no — the key is simply skipped
    }
    // `isdigit()` said yes, so `int()` runs. It accepts at most one sign.
    let signs = stripped.len() - body.len();
    if signs > 1 {
        return Err(Aborted);
    }
    let mut value: i64 = 0;
    for ch in body.chars() {
        // A digit run longer than i64 is a Python int in the reference and
        // never zero, so saturating keeps the `!= 0` answer exact.
        value = value
            .saturating_mul(10)
            .saturating_add(i64::from(ch.to_digit(10).unwrap_or(0)));
    }
    Ok(Some(if signs == 1 { -value } else { value }))
}

/// `handlers._extract_exit_code`.
fn extract_exit_code(payload: &Value) -> Result<Option<i64>, Aborted> {
    for key in ["tool_response", "tool_input"] {
        let Some(blob @ Value::Object(_)) = payload.get(key) else {
            continue;
        };
        for candidate in EXIT_CODE_KEYS {
            if let Some(value) = lower_get(blob, candidate) {
                match value {
                    Value::Bool(_) => continue,
                    Value::Int(number) => return Ok(Some(*number)),
                    Value::Str(text) => {
                        if let Some(number) = py_int_str(text)? {
                            return Ok(Some(number));
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    Ok(None)
}

/// `handlers._extract_error_summary` — a short, single-line excerpt, *not* full
/// stdout/stderr.
fn extract_error_summary(payload: &Value) -> Option<String> {
    let blob @ Value::Object(_) = payload.get("tool_response")? else {
        return None;
    };
    for key in ["error", "message", "stderr"] {
        if let Some(Value::Str(text)) = lower_get(blob, key)
            && !text.trim().is_empty()
        {
            // `v.strip().splitlines()[0].strip()`.
            let line = text.trim().split('\n').next().unwrap_or("").trim();
            // `line[:500] + ("…" if len(line) > 500 else "")` — note this is
            // NOT `pystr::clip`: the ellipsis is *appended* to a full-width
            // 500-character slice, so the result can be 501 characters.
            return Some(if pystr::len_chars(line) > TRUNCATE {
                format!("{}…", pystr::head(line, TRUNCATE))
            } else {
                line.to_string()
            });
        }
    }
    None
}

// ── correction heuristic ────────────────────────────────────────────────────

/// `handlers._CORRECTION_OPENERS` — bare lowercase tokens matched only at the
/// *start* of the prompt and only on a word boundary, so "no" fires on "no, …"
/// but not on "nobody" or "now". Order is the literal's: the first match wins.
const CORRECTION_OPENERS: [&str; 27] = [
    "no",
    "nope",
    "nah",
    "stop",
    "stop it",
    "undo",
    "revert",
    "rollback",
    "wait",
    "hold on",
    "hold up",
    "don't",
    "dont",
    "do not",
    "that's not",
    "thats not",
    "that is not",
    "that's wrong",
    "thats wrong",
    "not what i",
    "not quite",
    "go back",
    "back up",
    "scratch that",
    "cancel that",
    "never mind",
    "nevermind",
];

/// `handlers._CORRECTION_PHRASES` — unambiguous phrases matched anywhere.
///
/// The *pattern string itself* is what gets stored in `matched_keyword`
/// (`return pat.pattern`), so these literals are a wire contract, not an
/// implementation detail: change one and last month's rows stop grouping with
/// this month's.
const CORRECTION_PHRASES: [&str; 8] = [
    r"\bundo (that|the |what)",
    r"\brevert (that|the |what)",
    r"\broll ?back\b",
    r"\bthat'?s (not right|wrong|incorrect)\b",
    r"\bnot what i (wanted|asked|meant)\b",
    r"\bdon'?t (do|change|touch|edit|modify|add|remove|delete)\b",
    r"\bstop (doing|editing|changing|adding)\b",
    r"\bgo back to\b",
];

static CORRECTION_RES: std::sync::LazyLock<Vec<regex::Regex>> = std::sync::LazyLock::new(|| {
    CORRECTION_PHRASES
        .iter()
        .map(|pattern| {
            // `re.I` — the patterns are authored lowercase and matched
            // case-insensitively.
            regex::Regex::new(&format!("(?i){pattern}"))
                .expect("the correction patterns are literals and compile")
        })
        .collect()
});

/// `handlers._correction_match` — the keyword/phrase that flagged *prompt*.
#[must_use]
pub fn correction_match(prompt: &str) -> Option<String> {
    let text = prompt.trim();
    if text.is_empty() {
        return None;
    }
    let low = text.to_lowercase();
    for opener in &CORRECTION_OPENERS {
        if low == *opener {
            return Some((*opener).to_string());
        }
        if let Some(rest) = low.strip_prefix(opener) {
            // `nxt = low[len(opener):len(opener)+1]` — one CHARACTER, and the
            // boundary test is `nxt == "" or not nxt.isalnum()`.
            match rest.chars().next() {
                None => return Some((*opener).to_string()),
                Some(ch) if !ch.is_alphanumeric() => return Some((*opener).to_string()),
                Some(_) => {}
            }
        }
    }
    for (index, re) in CORRECTION_RES.iter().enumerate() {
        if re.is_match(text) {
            // `pat.pattern` — the raw pattern, WITHOUT the `(?i)` this port
            // prepends to carry `re.I`.
            return Some(CORRECTION_PHRASES[index].to_string());
        }
    }
    None
}

// ── session totals snapshot (boundary / pre-compact) ────────────────────────

/// `handlers._session_totals` — a cheap best-effort per-session rollup.
///
/// Reads the *real* store; the JSONL for this very session may not have landed
/// yet, in which case the counts are whatever is there so far. Any failure →
/// `{"available": False}`.
fn session_totals(session_id: Option<&Value>, env: &HookEnv) -> Value {
    let unavailable = || Value::Object(vec![("available".into(), Value::Bool(false))]);
    let Some(session_id) = session_id.and_then(Value::as_str).filter(|s| !s.is_empty()) else {
        return unavailable();
    };
    match session_totals_inner(session_id, env) {
        Ok(value) => value,
        Err(_) => unavailable(),
    }
}

fn session_totals_inner(session_id: &str, env: &HookEnv) -> anyhow::Result<Value> {
    // `db.connect` — read-WRITE, like the reference (DIV-201). The totals query
    // itself is a read, but the reference reaches it through the same
    // connect-and-WAL helper the writer uses.
    let conn = open_store(env)?;
    let row = conn.query_row(
        "SELECT
                    COUNT(*)                          AS message_count,
                    COALESCE(SUM(m.input_tokens), 0)         AS input_tokens,
                    COALESCE(SUM(m.output_tokens), 0)        AS output_tokens,
                    COALESCE(SUM(m.cache_read_tokens), 0)    AS cache_read_tokens,
                    COALESCE(SUM(m.cache_create_tokens), 0)  AS cache_create_tokens
                FROM messages m
                JOIN sessions s ON s.id = m.session_fk
                WHERE s.session_id = ?",
        [session_id],
        |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, i64>(4)?,
            ))
        },
    )?;

    let mut totals = vec![
        ("available".to_string(), Value::Bool(true)),
        ("message_count".to_string(), Value::Int(row.0)),
        ("input_tokens".to_string(), Value::Int(row.1)),
        ("output_tokens".to_string(), Value::Int(row.2)),
        ("cache_read_tokens".to_string(), Value::Int(row.3)),
        ("cache_create_tokens".to_string(), Value::Int(row.4)),
    ];
    if let Some(cost) = session_cost(&conn, session_id) {
        totals.push(("cost_usd".to_string(), Value::Float(cost)));
    }
    Ok(Value::Object(totals))
}

/// `handlers._session_cost` — `session_mart.cost_usd`, or `None` when the mart
/// does not exist yet or has no row.
fn session_cost(conn: &Connection, session_id: &str) -> Option<f64> {
    conn.query_row(
        "SELECT cost_usd FROM session_mart WHERE session_id = ?",
        [session_id],
        |row| row.get::<_, Option<f64>>(0),
    )
    .ok()
    .flatten()
}

// ── write path ──────────────────────────────────────────────────────────────

/// `store.db.connect` — read-write, creating the file if missing, with the
/// standard PRAGMAs. See DIV-201: this is the reference's helper, ported.
fn open_store(env: &HookEnv) -> anyhow::Result<Connection> {
    if let Some(parent) = env.store_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let conn = Connection::open(&env.store_path)?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    Ok(conn)
}

/// `handlers.ensure_captured_events_table` — the hook path's self-heal.
///
/// Never bumps `user_version`: the dashboard's `schema.apply` owns the versioned
/// `v010_captured_events.sql` migration, both create the identical shape, and
/// both use `IF NOT EXISTS`, so the two never collide.
///
/// # Errors
/// When the script cannot run (locked store, read-only file).
pub fn ensure_captured_events_table(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS captured_events (
            id              INTEGER PRIMARY KEY,
            ts              TEXT NOT NULL,
            project_id      INTEGER,
            session_id      TEXT,
            hook_id         TEXT NOT NULL,
            event_kind      TEXT NOT NULL,
            payload_json    TEXT NOT NULL,
            UNIQUE (ts, hook_id, session_id)
        );
        CREATE INDEX IF NOT EXISTS idx_captured_events_session ON captured_events(session_id);
        CREATE INDEX IF NOT EXISTS idx_captured_events_kind    ON captured_events(event_kind, ts);
        ",
    )
}

/// `handlers._write_event` — one `INSERT OR IGNORE` into the real store.
///
/// `project_id` is resolved best-effort from the payload's `cwd`; `session_id`
/// comes straight from the payload. The `UNIQUE (ts, hook_id, session_id)` index
/// makes a re-fire of the same hook a no-op.
fn write_event(
    hook_id: &str,
    event_kind: &str,
    stored_payload: &Value,
    payload: &Value,
    env: &HookEnv,
) -> anyhow::Result<()> {
    let ts = pytime::isoformat_utc(env.now_micros);
    let session_id = payload
        .get("session_id")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty());

    let conn = open_store(env)?;
    // Don't make a hot-path hook block forever on a busy writer (the watcher),
    // but don't drop the event at the first contention either.
    conn.busy_timeout(std::time::Duration::from_millis(3_000))?;
    ensure_captured_events_table(&conn)?;
    let project_id = resolve_project_id(&conn, payload.get("cwd"), HOST_PROVIDER, env);
    conn.execute(
        "INSERT OR IGNORE INTO captured_events \
         (ts, project_id, session_id, hook_id, event_kind, payload_json) \
         VALUES (?, ?, ?, ?, ?, ?)",
        rusqlite::params![
            ts,
            project_id,
            session_id,
            hook_id,
            event_kind,
            // `json.dumps(stored_payload, default=str)` — bare defaults, so
            // `", "` / `": "` and `ensure_ascii=True`. `default=str` cannot
            // fire here: every value came out of `json.loads`.
            pyjson::dumps_default(stored_payload),
        ],
    )?;
    Ok(())
}

/// `handlers._resolve_project_id` — a hook's `cwd` → `projects.id`, if the store
/// already knows it.
///
/// When several providers share the slug the row belonging to *prefer_provider*
/// — the host this hook fires under — wins; otherwise any provider with that
/// slug, oldest row for determinism. `None` when the project isn't in the store
/// yet, and `None` (quietly) when `projects` is somehow absent.
fn resolve_project_id(
    conn: &Connection,
    cwd: Option<&Value>,
    prefer_provider: &str,
    env: &HookEnv,
) -> Option<i64> {
    let cwd = cwd?.as_str()?;
    if cwd.is_empty() {
        return None;
    }
    let slug = inject::slugify(&abspath(cwd, &env.cwd));
    conn.query_row(
        "SELECT id FROM projects WHERE slug = ? ORDER BY (provider = ?) DESC, id LIMIT 1",
        rusqlite::params![slug, prefer_provider],
        |row| row.get::<_, i64>(0),
    )
    .ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn env() -> HookEnv {
        HookEnv {
            store_path: PathBuf::from("/nonexistent/deep/store.db"),
            app_dir: PathBuf::from("/nonexistent/deep"),
            weights: (0.5, 0.2, 0.3),
            now_micros: 1_785_456_000_000_000,
            cwd: PathBuf::from("/home/u/proj"),
            config: None,
            proactive_disabled: None,
            recall_timeout: None,
            memory_bin: "stackunderflow".into(),
            proactive: crate::env::ProactiveSettings::default(),
        }
    }

    fn obj(pairs: &[(&str, Value)]) -> Value {
        Value::Object(
            pairs
                .iter()
                .map(|(k, v)| ((*k).to_string(), v.clone()))
                .collect(),
        )
    }

    #[test]
    fn a_successful_tool_call_records_nothing() {
        let payload = obj(&[(
            "tool_response",
            obj(&[
                ("exit_code", Value::Int(0)),
                ("stdout", Value::Str("ok".into())),
            ]),
        )]);
        assert!(classify("stackunderflow-post-tool-use", &payload, false, &env()).is_none());
    }

    #[test]
    fn a_bool_exit_code_is_skipped_not_coerced() {
        // `isinstance(v, bool): continue` runs BEFORE the int test, and in
        // Python `True` IS an int. Recording it as 1 would be wrong.
        let payload = obj(&[("tool_response", obj(&[("exit_code", Value::Bool(true))]))]);
        assert_eq!(tool_call_failed(&payload), Ok(false));
        assert_eq!(extract_exit_code(&payload), Ok(None));
    }

    #[test]
    fn a_float_exit_code_is_invisible() {
        // `isinstance(1.0, int)` is False. Bug-for-bug.
        let payload = obj(&[("tool_response", obj(&[("exit_code", Value::Float(1.0))]))]);
        assert_eq!(tool_call_failed(&payload), Ok(false));
    }

    #[test]
    fn failure_is_detected_through_every_documented_shape() {
        assert_eq!(
            tool_call_failed(&obj(&[(
                "tool_response",
                obj(&[("exitCode", Value::Int(2))])
            )])),
            Ok(true)
        );
        assert_eq!(
            tool_call_failed(&obj(&[(
                "tool_response",
                obj(&[("returncode", Value::Str("-1".into()))])
            )])),
            Ok(true)
        );
        assert_eq!(
            tool_call_failed(&obj(&[(
                "tool_response",
                obj(&[("is_error", Value::Bool(true))])
            )])),
            Ok(true)
        );
        assert_eq!(
            tool_call_failed(&obj(&[(
                "tool_response",
                obj(&[("error", Value::Str("boom".into()))])
            )])),
            Ok(true)
        );
        assert_eq!(
            tool_call_failed(&obj(&[(
                "tool_response",
                obj(&[("success", Value::Bool(false))])
            )])),
            Ok(true)
        );
        // A LIST of blocks, one of which errored.
        assert_eq!(
            tool_call_failed(&obj(&[(
                "tool_response",
                Value::Array(vec![
                    obj(&[("text", Value::Str("fine".into()))]),
                    obj(&[("is_error", Value::Bool(true))]),
                ])
            )])),
            Ok(true)
        );
        // An empty error string is NOT a failure.
        assert_eq!(
            tool_call_failed(&obj(&[(
                "tool_response",
                obj(&[("error", Value::Str("   ".into()))])
            )])),
            Ok(false)
        );
    }

    #[test]
    fn the_correction_heuristic_needs_a_word_boundary() {
        assert_eq!(
            correction_match("no, do it the other way").as_deref(),
            Some("no")
        );
        assert_eq!(correction_match("no").as_deref(), Some("no"));
        assert_eq!(correction_match("nobody knows"), None);
        assert_eq!(correction_match("now do the thing"), None);
        assert_eq!(correction_match("I have no idea how this works"), None);
        assert_eq!(correction_match("   "), None);
    }

    #[test]
    fn a_matched_phrase_stores_its_own_pattern() {
        // `matched_keyword` is the PATTERN string — a wire contract.
        assert_eq!(
            correction_match("please undo that change").as_deref(),
            Some(r"\bundo (that|the |what)")
        );
        assert_eq!(
            correction_match("could you ROLL BACK the migration").as_deref(),
            Some(r"\broll ?back\b")
        );
        // Not "that is not …" — that string starts with the `that is not`
        // OPENER, which wins before any phrase is tried.
        assert_eq!(
            correction_match("well, not what i wanted").as_deref(),
            Some(r"\bnot what i (wanted|asked|meant)\b")
        );
        // A contraction is not the phrase, on either side: `isn't` has no `not`.
        assert_eq!(correction_match("that isn't what i wanted at all"), None);
    }

    #[test]
    fn openers_win_over_phrases_and_the_first_opener_wins() {
        // "stop" precedes "stop it" in the tuple, and both match.
        assert_eq!(correction_match("stop it now").as_deref(), Some("stop"));
    }

    #[test]
    fn metadata_only_is_the_default_and_drops_nulls() {
        let payload = obj(&[
            ("tool_response", obj(&[("exit_code", Value::Int(3))])),
            ("tool_name", Value::Str("Bash".into())),
            // No `cwd` at all → the key must not appear in the stored metadata.
        ]);
        let (kind, stored) =
            classify("stackunderflow-post-tool-use", &payload, false, &env()).expect("failure row");
        assert_eq!(kind, KIND_FAILURE);
        assert_eq!(
            pyjson::dumps_default(&stored),
            r#"{"hook_event_name": "PostToolUse", "tool_name": "Bash", "exit_code": 3}"#
        );
    }

    #[test]
    fn capture_content_stores_the_whole_payload_verbatim() {
        let payload = obj(&[
            ("tool_response", obj(&[("exit_code", Value::Int(3))])),
            ("prompt", Value::Str("secret".into())),
        ]);
        let (_, stored) =
            classify("stackunderflow-post-tool-use", &payload, true, &env()).expect("failure row");
        assert_eq!(
            pyjson::dumps_default(&stored),
            pyjson::dumps_default(&payload)
        );
    }

    #[test]
    fn prompt_length_counts_characters() {
        let payload = obj(&[("prompt", Value::Str("no ❤❤❤".into()))]);
        let (kind, stored) =
            classify("stackunderflow-user-prompt", &payload, false, &env()).expect("correction");
        assert_eq!(kind, KIND_CORRECTION);
        assert_eq!(
            pyjson::dumps_default(&stored),
            r#"{"hook_event_name": "UserPromptSubmit", "matched_keyword": "no", "prompt_length": 6}"#
        );
    }

    #[test]
    fn a_boundary_row_carries_unavailable_totals_without_a_store() {
        let payload = obj(&[("session_id", Value::Str("s-1".into()))]);
        let (kind, stored) =
            classify("stackunderflow-stop", &payload, false, &env()).expect("boundary");
        assert_eq!(kind, KIND_BOUNDARY);
        assert_eq!(
            pyjson::dumps_default(&stored),
            r#"{"hook_event_name": "Stop", "session_totals": {"available": false}}"#
        );
    }

    #[test]
    fn an_unknown_id_is_a_silent_noop() {
        assert_eq!(
            run("stackunderflow-nope", &Value::Object(vec![]), false, &env()),
            Fired::default()
        );
        // A non-object payload becomes `{}` rather than an error.
        assert_eq!(
            run(
                "stackunderflow-stop",
                &Value::Str("garbage".into()),
                false,
                &env()
            )
            .stdout,
            ""
        );
    }

    #[test]
    fn the_error_summary_takes_the_first_line_and_truncates_at_501() {
        let long = "x".repeat(600);
        let payload = obj(&[(
            "tool_response",
            obj(&[("stderr", Value::Str(format!("{long}\nsecond line")))]),
        )]);
        let summary = extract_error_summary(&payload).expect("summary");
        assert_eq!(
            pystr::len_chars(&summary),
            501,
            "500 chars plus the ellipsis"
        );
        assert!(summary.ends_with('…'));

        let payload = obj(&[(
            "tool_response",
            obj(&[("error", Value::Str("  boom\nrest  ".into()))]),
        )]);
        assert_eq!(extract_error_summary(&payload).as_deref(), Some("boom"));
    }

    #[test]
    fn a_double_signed_exit_code_aborts_the_fire_like_pythons_int_does() {
        // `"--5".lstrip("-").isdigit()` is True and `int("--5")` then RAISES.
        // `run`'s blanket except swallows it: no row, no output.
        assert_eq!(py_int_str("--5"), Err(Aborted));
        assert_eq!(py_int_str("-5"), Ok(Some(-5)));
        assert_eq!(py_int_str(" 7 "), Ok(Some(7)));
        assert_eq!(py_int_str("7a"), Ok(None));
        assert_eq!(py_int_str("-"), Ok(None));
        let payload = obj(&[(
            "tool_response",
            obj(&[("exit_code", Value::Str("--5".into()))]),
        )]);
        assert_eq!(tool_call_failed(&payload), Err(Aborted));
        assert!(classify("stackunderflow-post-tool-use", &payload, false, &env()).is_none());
    }

    #[test]
    fn the_case_insensitive_lookup_lets_the_last_duplicate_win() {
        // `{str(k).lower(): v for k, v in d.items()}` — a later key overwrites.
        let blob = obj(&[("Code", Value::Int(0)), ("code", Value::Int(7))]);
        assert_eq!(dict_signals_failure(&blob), Ok(true));
        let blob = obj(&[("code", Value::Int(7)), ("CODE", Value::Int(0))]);
        assert_eq!(dict_signals_failure(&blob), Ok(false));
    }
}
