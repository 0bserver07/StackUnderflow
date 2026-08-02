//! The `stackunderflow-history-jsonl-v1` stream contract + plugin manifest —
//! the port of `stackunderflow/adapters/custom_jsonl.py` (RS-2-006).
//!
//! Some session sources we do not want to own forever: they are cloud-gated
//! (no local transcript on disk), or niche enough that a bespoke adapter is not
//! worth the maintenance. For those, StackUnderflow owns only a **format** and
//! a **runner** — the user supplies an export command that streams their
//! history to stdout as our JSONL, and we validate and import it under one
//! `custom` provider.
//!
//! This module is the **format half**:
//!
//! * the record types and their strict validation ([`parse_stream`]),
//! * the plugin manifest and its loader ([`load_manifest`] / [`parse_manifest`]),
//! * [`run_export`] — the guarded subprocess runner (no shell, cleared +
//!   allowlisted env, byte and wall-clock caps, non-zero exit is an error).
//!
//! The **store half** (upsert, cursor persistence, id derivation) is
//! [`crate::custom_import`], which landed first and therefore already owns the
//! record types, [`SCHEMA`], [`MANIFEST_FILENAME`] and [`is_safe_source_id`].
//! Python declares those here and imports them there; this port re-exports in
//! the other direction rather than declaring them twice, because two copies of
//! a constant that names a *file* is how a cursor sidecar goes missing.
//! Nothing here touches the database.
//!
//! # Guardrails, not a sandbox
//!
//! The export command is **the user's own code running as the user**. Running
//! it with no shell, a cleared + allowlisted environment and byte/time caps
//! removes the easy footguns (a stray `$(...)` in an argv, an env var leaking
//! into a child, a runaway process wedging the import). It is emphatically
//! **not** a security boundary — a user who points the manifest at a hostile
//! command has already lost. The reference doc says so plainly and this port
//! changes nothing about it.
//!
//! # The validation messages ARE the output
//!
//! Every failure leg of `stax import` prints `Error: {str(exc)}` and exits 1,
//! so each message here is a byte contract, not a diagnostic. That is why
//! [`crate::pydecode`] exists: two of these messages interpolate a CPython
//! decoder exception verbatim.
//!
//! # Recorded divergences
//!
//! * **DIV-452 — the timeout escalation is SIGKILL, not SIGTERM-then-SIGKILL.**
//!   `_terminate` sends `SIGTERM`, waits five seconds, then `SIGKILL`. The
//!   workspace forbids `unsafe`, `std` has no portable `SIGTERM`, and adding
//!   `libc` to reach one would put an `unsafe` block in a crate whose header
//!   says `#![forbid(unsafe_code)]`. [`terminate`] therefore does what every
//!   other subprocess site in this port does (`stax_hooks::recall`,
//!   `stax_etl::ingest::outcomes`, `stax_reports::worktrees`): `Child::kill`,
//!   which is `SIGKILL`. The *observable bytes are identical* — the timeout
//!   leg discards the child's output and raises either way — but a child with
//!   a `SIGTERM` handler loses its chance to clean up.
//! * **DIV-453 — an integer past `i64` is a float here and an int there.**
//!   `seq: 99999999999999999999` is a Python `int` (accepted, non-negative) and
//!   a `serde_json` `f64` (rejected: "'seq' must be a non-negative integer").
//!   The same class as DIV-010's `--limit` clamp, which the maintainer has
//!   already ruled on for the CLI surface.
//! * **DIV-454 — `NaN` / `Infinity` are Python JSON and not `serde_json`.**
//!   `json.loads` accepts all three literals; `serde_json` rejects them, so a
//!   line carrying one is a *parse* failure here and a *validation* failure
//!   there. [`crate::pydecode`]'s scanner agrees with CPython that the literal
//!   is well-formed, so the port would otherwise have printed no message at
//!   all; the mismatch is detected and reported rather than papered over.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::custom_import::{FileTouchRecord, MessageRecord, ParsedStream, StreamSession};
use crate::pydecode;
use crate::pyval;

pub use crate::custom_import::{MANIFEST_FILENAME, SCHEMA, SOURCE_ID_MAX_LEN, is_safe_source_id};

/// The env var the runner sets so the export command can resume from where it
/// left off (`CURSOR_ENV_VAR`).
///
/// Its value is the opaque cursor we stored last time (or the manifest's seed
/// cursor on the first run). We never interpret it.
pub const CURSOR_ENV_VAR: &str = "STACKUNDERFLOW_HISTORY_CURSOR";

/// Base environment keys forwarded to the export command (`_ENV_ALLOWLIST`).
///
/// Everything else is dropped; a manifest opts specific extra keys back in via
/// `env_passthrough` (an allowlist, never a denylist). `PATH`/`HOME` let the
/// command be found and run; the locale trio keeps its text output stable.
pub const ENV_ALLOWLIST: [&str; 6] = ["PATH", "HOME", "LANG", "LC_ALL", "LC_CTYPE", "TZ"];

/// The roles a `message` line may carry (`_ALLOWED_ROLES`), in `sorted()` order
/// — which is the order the rejection message prints them in.
pub const ALLOWED_ROLES: [&str; 4] = ["assistant", "system", "tool", "user"];

/// The line types the stream may carry (`_RECORD_TYPES`), in `sorted()` order.
pub const RECORD_TYPES: [&str; 4] = ["cursor", "file_touch", "message", "session"];

/// Default wall-clock cap on one export run (`_DEFAULT_TIMEOUT_SECONDS`).
pub const DEFAULT_TIMEOUT_SECONDS: f64 = 120.0;
/// The ceiling a manifest cannot raise the timeout past (`_MAX_TIMEOUT_SECONDS`).
pub const MAX_TIMEOUT_SECONDS: f64 = 3600.0;
/// Default cap on the bytes buffered from stdout (`_DEFAULT_MAX_OUTPUT_BYTES`).
pub const DEFAULT_MAX_OUTPUT_BYTES: i64 = 64 * 1024 * 1024;
/// The ceiling a manifest cannot raise the byte cap past (`_HARD_MAX_OUTPUT_BYTES`).
pub const HARD_MAX_OUTPUT_BYTES: i64 = 512 * 1024 * 1024;
/// How much of the child's stderr is kept for the failure message
/// (`_STDERR_CAP_BYTES`).
pub const STDERR_CAP_BYTES: i64 = 64 * 1024;
/// How long `_terminate` waits between escalations.
pub const TERMINATE_GRACE_SECONDS: f64 = 5.0;

/// The chunk size both capped readers pull with (`stream.read(65536)`).
const READ_CHUNK: usize = 65536;

// ── errors ───────────────────────────────────────────────────────────────────

/// Every history-source import failure (`HistorySourceError` and its three
/// subclasses).
///
/// All failures are **fail-closed**: the caller catches this, aborts the
/// import, and leaves the stored cursor un-advanced. The variants exist because
/// the reference's three subclasses are separately catchable, not because the
/// CLI distinguishes them — `cli.py` funnels all three into one
/// `click.ClickException(str(exc))`, so [`std::fmt::Display`] is the contract
/// and it reproduces `str(exc)` byte for byte.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HistorySourceError {
    /// `ManifestError` — the plugin manifest is missing, unreadable, or invalid.
    Manifest(String),
    /// `ExportCommandError` — the export command could not be launched, timed
    /// out, exceeded its output cap, or exited non-zero.
    ExportCommand(String),
    /// `StreamValidationError` — a stream line was not valid
    /// `stackunderflow-history-jsonl-v1`.
    StreamValidation {
        /// The message, without the line prefix.
        message: String,
        /// 1-based; `0` for whole-stream problems, which print no prefix.
        line_no: usize,
    },
}

impl std::fmt::Display for HistorySourceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Manifest(message) | Self::ExportCommand(message) => f.write_str(message),
            // `StreamValidationError.__init__` bakes the prefix into the
            // message it hands `super()`, so `str(exc)` carries it.
            Self::StreamValidation { message, line_no } => {
                if *line_no == 0 {
                    f.write_str(message)
                } else {
                    write!(f, "line {line_no}: {message}")
                }
            }
        }
    }
}

impl std::error::Error for HistorySourceError {}

impl HistorySourceError {
    /// A `StreamValidationError` at a 1-based line.
    #[must_use]
    pub fn stream(message: impl Into<String>, line_no: usize) -> Self {
        Self::StreamValidation {
            message: message.into(),
            line_no,
        }
    }
}

/// The result every function in this module returns.
pub type Result<T> = std::result::Result<T, HistorySourceError>;

// ── manifest ─────────────────────────────────────────────────────────────────

/// A parsed, validated `stackunderflow-history-plugin.json`
/// (`HistoryPluginManifest`).
#[derive(Debug, Clone, PartialEq)]
pub struct HistoryPluginManifest {
    /// Names the project and the on-disk cursor file; `[A-Za-z0-9._-]` only.
    pub source_id: String,
    /// The export command's argv. Run with **no shell**.
    pub command: Vec<String>,
    /// The seed cursor, used only until a run stores one.
    pub cursor: Option<String>,
    /// Wall-clock cap, already clamped to [`MAX_TIMEOUT_SECONDS`].
    pub timeout_seconds: f64,
    /// stdout cap, already clamped to [`HARD_MAX_OUTPUT_BYTES`].
    pub max_output_bytes: i64,
    /// Extra env keys the manifest opts back in.
    pub env_passthrough: Vec<String>,
    /// Where the manifest was read from, when it came from disk.
    pub path: Option<PathBuf>,
    /// The manifest document, verbatim.
    pub raw: Value,
}

/// Read + validate the manifest at `path` — a file, or a directory containing
/// the canonical filename (`load_manifest`).
///
/// # Errors
/// [`HistorySourceError::Manifest`] on any problem, with the reference's
/// message.
pub fn load_manifest(path: &Path) -> Result<HistoryPluginManifest> {
    let mut p = path.to_path_buf();
    if p.is_dir() {
        p = p.join(MANIFEST_FILENAME);
    }
    let bytes = std::fs::read(&p).map_err(|err| {
        if err.kind() == std::io::ErrorKind::NotFound {
            // `except FileNotFoundError` comes first and prints no errno.
            HistorySourceError::Manifest(format!("manifest not found: {}", p.display()))
        } else {
            HistorySourceError::Manifest(format!(
                "cannot read manifest {}: {}",
                p.display(),
                py_oserror(&err, Some(&p.to_string_lossy()))
            ))
        }
    })?;
    // `read_text(encoding="utf-8")` raises `UnicodeDecodeError` — a `ValueError`,
    // NOT an `OSError` — so the reference does not catch it and the CLI prints a
    // traceback. A traceback is not a byte contract, so this leg refuses to
    // invent one: the closest honest answer is the OSError funnel's shape, and
    // the divergence is recorded (DIV-455) rather than rowed.
    let text = String::from_utf8(bytes).map_err(|err| {
        HistorySourceError::Manifest(format!(
            "cannot read manifest {}: {}",
            p.display(),
            pydecode::utf8_decode_error(err.as_bytes()).unwrap_or_default()
        ))
    })?;
    let data = parse_json_like_python(&text).map_err(|message| {
        HistorySourceError::Manifest(format!(
            "manifest {} is not valid JSON: {message}",
            p.display()
        ))
    })?;
    parse_manifest(&data, Some(&p))
}

/// Validate a manifest document into a [`HistoryPluginManifest`]
/// (`parse_manifest`).
///
/// The checks run in the reference's order, because the FIRST failure is the
/// one printed: a manifest that is wrong in three ways prints exactly one
/// message, and which one is part of the contract.
///
/// # Errors
/// [`HistorySourceError::Manifest`], with the reference's message.
pub fn parse_manifest(data: &Value, path: Option<&Path>) -> Result<HistoryPluginManifest> {
    let where_ = path.map_or_else(String::new, |path| format!(" ({})", path.display()));
    let Some(map) = data.as_object() else {
        return Err(HistorySourceError::Manifest(format!(
            "manifest{where_} must be a JSON object"
        )));
    };

    // `data.get("schema")` — absent and explicit `null` are the same `None`.
    let schema = map.get("schema").filter(|value| !value.is_null());
    if let Some(schema) = schema
        && schema.as_str() != Some(SCHEMA)
    {
        return Err(HistorySourceError::Manifest(format!(
            "manifest{where_} declares schema {}; this build speaks {}",
            pyval::py_repr(schema),
            pyval::py_repr(&Value::from(SCHEMA)),
        )));
    }

    let source_id = map.get("source_id").and_then(Value::as_str);
    let Some(source_id) = source_id.filter(|id| is_safe_source_id(id)) else {
        return Err(HistorySourceError::Manifest(format!(
            "manifest{where_} 'source_id' must be a non-empty string of \
             [A-Za-z0-9._-] (it names a project + an on-disk cursor file)"
        )));
    };

    let command = map.get("command").and_then(Value::as_array);
    let command: Option<Vec<String>> = command.and_then(|items| {
        items
            .iter()
            .map(|item| item.as_str().map(ToOwned::to_owned))
            .collect()
    });
    let Some(command) = command.filter(|argv| {
        // `not command` (empty list) and `not command[0]` (empty argv[0]) are
        // both truthiness — the DIV-234 class, and both are rejections.
        !argv.is_empty() && !argv[0].is_empty()
    }) else {
        return Err(HistorySourceError::Manifest(format!(
            "manifest{where_} 'command' must be a non-empty list of strings \
             (argv, run with no shell)"
        )));
    };

    let cursor = match map.get("cursor").filter(|value| !value.is_null()) {
        None => None,
        Some(Value::String(cursor)) => Some(cursor.clone()),
        Some(_) => {
            return Err(HistorySourceError::Manifest(format!(
                "manifest{where_} 'cursor' must be a string when present"
            )));
        }
    };

    let timeout_seconds = coerce_positive_number(
        map.get("timeout_seconds"),
        DEFAULT_TIMEOUT_SECONDS,
        MAX_TIMEOUT_SECONDS,
        "timeout_seconds",
        &where_,
    )?;
    // `int(...)` truncates toward zero; the value is already clamped to the
    // hard ceiling, so the cast cannot saturate.
    let max_output_bytes = coerce_positive_number(
        map.get("max_output_bytes"),
        // The default is an int in Python and is compared against a float
        // ceiling; the `float(min(...))` at the end makes both branches float,
        // and `int()` puts it back.
        DEFAULT_MAX_OUTPUT_BYTES as f64,
        HARD_MAX_OUTPUT_BYTES as f64,
        "max_output_bytes",
        &where_,
    )? as i64;

    // `data.get("env_passthrough", [])` — a MISSING key defaults to the empty
    // list, an explicit `null` does not and is rejected by the isinstance check.
    let env_passthrough = match map.get("env_passthrough") {
        None => Vec::new(),
        Some(value) => {
            let keys: Option<Vec<String>> = value.as_array().and_then(|items| {
                items
                    .iter()
                    .map(|item| item.as_str().map(ToOwned::to_owned))
                    .collect()
            });
            keys.ok_or_else(|| {
                HistorySourceError::Manifest(format!(
                    "manifest{where_} 'env_passthrough' must be a list of strings"
                ))
            })?
        }
    };

    Ok(HistoryPluginManifest {
        source_id: source_id.to_owned(),
        command,
        cursor,
        timeout_seconds,
        max_output_bytes,
        env_passthrough,
        path: path.map(Path::to_path_buf),
        raw: data.clone(),
    })
}

/// `_coerce_positive_number` — a positive number, clamped, or the default.
fn coerce_positive_number(
    value: Option<&Value>,
    default: f64,
    maximum: f64,
    field: &str,
    where_: &str,
) -> Result<f64> {
    // `if value is None: return default` — and an absent key is `None` too.
    let Some(value) = value.filter(|value| !value.is_null()) else {
        return Ok(default);
    };
    // `isinstance(value, bool) or not isinstance(value, (int, float))` —
    // `bool` is an `int` subclass in Python, so it is refused explicitly.
    let Some(number) = value.as_f64().filter(|_| !value.is_boolean()) else {
        return Err(HistorySourceError::Manifest(format!(
            "manifest{where_} '{field}' must be a positive number"
        )));
    };
    if number <= 0.0 {
        return Err(HistorySourceError::Manifest(format!(
            "manifest{where_} '{field}' must be > 0"
        )));
    }
    Ok(number.min(maximum))
}

// ── stream parsing + validation ──────────────────────────────────────────────

/// Parse + strictly validate a whole `stackunderflow-history-jsonl-v1` stream
/// (`parse_stream`).
///
/// **Fail-closed**: the first malformed line is an `Err` and nothing is
/// returned — the caller must not write partial results or advance the cursor.
/// Validation runs over the entire buffer *before* any store write, so a bad
/// line late in the stream can never leave half an import committed.
///
/// # Errors
/// [`HistorySourceError::StreamValidation`], carrying the 1-based line number
/// (`0` for the whole-stream UTF-8 failure).
pub fn parse_stream(data: &[u8]) -> Result<ParsedStream> {
    let text = std::str::from_utf8(data).map_err(|_| {
        HistorySourceError::stream(
            format!(
                "stream is not valid UTF-8: {}",
                pydecode::utf8_decode_error(data).unwrap_or_default()
            ),
            0,
        )
    })?;

    let mut sessions: Vec<StreamSession> = Vec::new();
    let mut messages: Vec<MessageRecord> = Vec::new();
    let mut file_touches: Vec<FileTouchRecord> = Vec::new();
    let mut next_cursor: Option<String> = None;
    // `(session_id, seq) -> line number`, to catch ambiguous identity within a
    // session (which would silently drop a row on INSERT OR IGNORE). A
    // `HashMap` because Python's is a `dict`: this is looked up once per RECORD,
    // and a linear scan would make a million-line export quadratic. Iteration
    // order is never observed — only `get` and `insert` are.
    let mut seq_seen: HashMap<(String, i64), usize> = HashMap::new();

    for (index, raw_line) in py_splitlines(text).enumerate() {
        let line_no = index + 1;
        let stripped = py_strip(raw_line);
        if stripped.is_empty() {
            continue;
        }
        let obj = parse_json_like_python(stripped).map_err(|message| {
            HistorySourceError::stream(format!("not valid JSON: {message}"), line_no)
        })?;
        let Some(map) = obj.as_object() else {
            return Err(HistorySourceError::stream(
                "each line must be a JSON object",
                line_no,
            ));
        };

        let rtype = map.get("type").cloned().unwrap_or(Value::Null);
        let known = rtype.as_str().is_some_and(|t| RECORD_TYPES.contains(&t));
        if !known {
            return Err(HistorySourceError::stream(
                format!(
                    "unknown record type {}; expected one of {}",
                    pyval::py_repr(&rtype),
                    py_str_list(&RECORD_TYPES),
                ),
                line_no,
            ));
        }

        match rtype.as_str().unwrap_or_default() {
            "cursor" => {
                let Some(Value::String(cursor)) = map.get("cursor") else {
                    return Err(HistorySourceError::stream(
                        "'cursor' record must carry a string 'cursor'",
                        line_no,
                    ));
                };
                // Last cursor wins.
                next_cursor = Some(cursor.clone());
            }
            "session" => {
                let record = parse_session(&obj, line_no)?;
                // `sessions[rec.session_id] = rec` — a `dict` assignment, so a
                // repeated id REPLACES the value and KEEPS the first position.
                // A linear scan, deliberately: this runs once per SESSION line
                // (not per record), the order is the contract, and the store
                // half's `ParsedStream.sessions` is already a `Vec` for the
                // same reason.
                match sessions
                    .iter()
                    .position(|s| s.session_id == record.session_id)
                {
                    Some(at) => sessions[at] = record,
                    None => sessions.push(record),
                }
            }
            "message" => {
                let record = parse_message(&obj, line_no)?;
                reserve_seq(&mut seq_seen, &record.session_id, record.seq, line_no)?;
                messages.push(record);
            }
            // `file_touch` — the only remaining member of `_RECORD_TYPES`.
            _ => {
                let record = parse_file_touch(&obj, line_no)?;
                reserve_seq(&mut seq_seen, &record.session_id, record.seq, line_no)?;
                file_touches.push(record);
            }
        }
    }

    Ok(ParsedStream {
        sessions,
        messages,
        file_touches,
        next_cursor,
    })
}

/// `_reserve_seq` — `(session_id, seq)` may appear once per stream.
fn reserve_seq(
    seen: &mut HashMap<(String, i64), usize>,
    session_id: &str,
    seq: i64,
    line_no: usize,
) -> Result<()> {
    if let Some(prior) = seen.get(&(session_id.to_owned(), seq)) {
        return Err(HistorySourceError::stream(
            format!(
                "duplicate seq {seq} for session {} (also on line {prior}); \
                 seq must be unique within a session",
                pyval::py_repr(&Value::from(session_id)),
            ),
            line_no,
        ));
    }
    seen.insert((session_id.to_owned(), seq), line_no);
    Ok(())
}

/// `_req_str` — a string, non-empty unless `allow_empty`.
fn req_str(obj: &Value, key: &str, line_no: usize, allow_empty: bool) -> Result<String> {
    match obj.get(key) {
        Some(Value::String(value)) if allow_empty || !value.is_empty() => Ok(value.clone()),
        _ => Err(HistorySourceError::stream(
            format!(
                "'{key}' must be a {}",
                if allow_empty {
                    "string"
                } else {
                    "non-empty string"
                }
            ),
            line_no,
        )),
    }
}

/// `_opt_str` — a string when present, `None` when absent or `null`.
fn opt_str(obj: &Value, key: &str, line_no: usize) -> Result<Option<String>> {
    match obj.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.clone())),
        Some(_) => Err(HistorySourceError::stream(
            format!("'{key}' must be a string when present"),
            line_no,
        )),
    }
}

/// `_req_seq` — a non-negative `int`. `bool` is an `int` subclass in Python and
/// is refused explicitly; so is a float, which is why `1.0` is a rejection and
/// not a `1`.
fn req_seq(obj: &Value, line_no: usize) -> Result<i64> {
    match obj.get("seq") {
        Some(value) if !value.is_boolean() => value
            .as_i64()
            .filter(|seq| *seq >= 0)
            .ok_or_else(|| bad_seq(line_no)),
        _ => Err(bad_seq(line_no)),
    }
}

fn bad_seq(line_no: usize) -> HistorySourceError {
    HistorySourceError::stream("'seq' must be a non-negative integer", line_no)
}

/// `_opt_nonneg_int` — `0` when absent, else a non-negative `int`.
fn opt_nonneg_int(obj: &Value, key: &str, line_no: usize) -> Result<i64> {
    match obj.get(key) {
        None | Some(Value::Null) => Ok(0),
        Some(value) if !value.is_boolean() => value.as_i64().filter(|n| *n >= 0).ok_or_else(|| {
            HistorySourceError::stream(
                format!("'{key}' must be a non-negative integer when present"),
                line_no,
            )
        }),
        Some(_) => Err(HistorySourceError::stream(
            format!("'{key}' must be a non-negative integer when present"),
            line_no,
        )),
    }
}

/// `_parse_session` — the field order is the rejection order.
fn parse_session(obj: &Value, line_no: usize) -> Result<StreamSession> {
    Ok(StreamSession {
        session_id: req_str(obj, "session_id", line_no, false)?,
        project: opt_str(obj, "project", line_no)?,
        cwd: opt_str(obj, "cwd", line_no)?,
        title: opt_str(obj, "title", line_no)?,
        first_timestamp: opt_str(obj, "first_timestamp", line_no)?,
        last_timestamp: opt_str(obj, "last_timestamp", line_no)?,
        raw: obj.clone(),
    })
}

/// `_parse_message`.
///
/// `role`, `tools` and `content` are validated BEFORE the constructor runs, so
/// a line that is wrong in both `role` and `session_id` reports the role. The
/// remaining fields are evaluated in argument order, which is the order below.
fn parse_message(obj: &Value, line_no: usize) -> Result<MessageRecord> {
    let role = req_str(obj, "role", line_no, false)?;
    if !ALLOWED_ROLES.contains(&role.as_str()) {
        return Err(HistorySourceError::stream(
            format!(
                "'role' must be one of {}; got {}",
                py_str_list(&ALLOWED_ROLES),
                pyval::py_repr(&Value::from(role.as_str())),
            ),
            line_no,
        ));
    }
    // `obj.get("tools", [])` — absent is the empty list; an explicit `null` is
    // NOT, and fails the isinstance check.
    let tools: Vec<String> = match obj.get("tools") {
        None => Vec::new(),
        Some(value) => {
            let items: Option<Vec<String>> = value.as_array().and_then(|items| {
                items
                    .iter()
                    .map(|item| item.as_str().map(ToOwned::to_owned))
                    .collect()
            });
            items.ok_or_else(|| {
                HistorySourceError::stream("'tools' must be a list of strings", line_no)
            })?
        }
    };
    let content = match obj.get("content") {
        None => String::new(),
        Some(Value::String(content)) => content.clone(),
        Some(_) => {
            return Err(HistorySourceError::stream(
                "'content' must be a string",
                line_no,
            ));
        }
    };
    Ok(MessageRecord {
        session_id: req_str(obj, "session_id", line_no, false)?,
        seq: req_seq(obj, line_no)?,
        // `_opt_str(...) or ""` — absent, `null` and `""` all become `""`.
        timestamp: opt_str(obj, "timestamp", line_no)?.unwrap_or_default(),
        role,
        content,
        model: opt_str(obj, "model", line_no)?,
        input_tokens: opt_nonneg_int(obj, "input_tokens", line_no)?,
        output_tokens: opt_nonneg_int(obj, "output_tokens", line_no)?,
        cache_read_tokens: opt_nonneg_int(obj, "cache_read_tokens", line_no)?,
        cache_creation_tokens: opt_nonneg_int(obj, "cache_creation_tokens", line_no)?,
        tools,
        cwd: opt_str(obj, "cwd", line_no)?,
        raw: obj.clone(),
    })
}

/// `_parse_file_touch`.
fn parse_file_touch(obj: &Value, line_no: usize) -> Result<FileTouchRecord> {
    Ok(FileTouchRecord {
        session_id: req_str(obj, "session_id", line_no, false)?,
        seq: req_seq(obj, line_no)?,
        path: req_str(obj, "path", line_no, false)?,
        // `_opt_str(...) or "edit"` — an EMPTY string is falsy and becomes
        // `"edit"` too, which `custom_import`'s operation table then maps.
        operation: opt_str(obj, "operation", line_no)?
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| "edit".to_owned()),
        timestamp: opt_str(obj, "timestamp", line_no)?.unwrap_or_default(),
        raw: obj.clone(),
    })
}

// ── guarded subprocess runner ────────────────────────────────────────────────

/// Build the cleared + allowlisted environment for the export command
/// (`build_child_env`).
///
/// Starts empty; copies only [`ENV_ALLOWLIST`] keys and the manifest's explicit
/// `env_passthrough` keys that are present in `parent_env`; then sets
/// [`CURSOR_ENV_VAR`] to the opaque cursor so the command can resume.
///
/// The return type is an ordered `Vec` and not a map because Python's `dict` is
/// insertion-ordered and this value is *printed* by the argv differ: a key
/// listed in both the allowlist and `env_passthrough` keeps its FIRST position,
/// and a manifest that passes [`CURSOR_ENV_VAR`] through keeps that position
/// while the cursor assignment overwrites its value.
#[must_use]
pub fn build_child_env(
    manifest: &HistoryPluginManifest,
    cursor: Option<&str>,
    parent_env: &[(String, String)],
) -> Vec<(String, String)> {
    let mut child: Vec<(String, String)> = Vec::new();
    let mut set = |key: &str, value: String| match child.iter().position(|(k, _)| k == key) {
        Some(at) => child[at].1 = value,
        None => child.push((key.to_owned(), value)),
    };
    let keys = ENV_ALLOWLIST
        .iter()
        .map(|key| (*key).to_owned())
        .chain(manifest.env_passthrough.iter().cloned());
    for key in keys {
        if let Some((_, value)) = parent_env.iter().find(|(k, _)| *k == key) {
            set(&key, value.clone());
        }
    }
    set(CURSOR_ENV_VAR, cursor.unwrap_or_default().to_owned());
    child
}

/// This process's environment, in the shape [`build_child_env`] reads.
#[must_use]
pub fn process_env() -> Vec<(String, String)> {
    std::env::vars().collect()
}

/// Everything one export run produced, for the callers that want to see the
/// spawn without performing it.
///
/// [`run_export`] is the function the importer calls; this type is what the
/// argv differ prints, because a differ that re-derives the argv proves nothing
/// about the argv the process actually used (the `backup create` lesson).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportPlan {
    /// The argv, exactly as handed to `execve`. No shell, ever.
    pub argv: Vec<String>,
    /// The child's whole environment, in insertion order.
    pub env: Vec<(String, String)>,
    /// The child's working directory — the manifest's own directory.
    pub cwd: Option<PathBuf>,
}

impl ExportPlan {
    /// The spawn one [`run_export`] call will perform.
    #[must_use]
    pub fn new(
        manifest: &HistoryPluginManifest,
        cursor: Option<&str>,
        cwd: Option<&Path>,
        parent_env: &[(String, String)],
    ) -> Self {
        Self {
            argv: manifest.command.clone(),
            env: build_child_env(manifest, cursor, parent_env),
            cwd: cwd.map(Path::to_path_buf),
        }
    }
}

/// Run the manifest's export command and return its stdout bytes
/// (`run_export`).
///
/// No shell. Cleared + allowlisted env (see [`build_child_env`]). Output is
/// capped at `manifest.max_output_bytes` and the run at
/// `manifest.timeout_seconds`. A non-zero exit, a timeout, an over-cap stream
/// or a spawn failure are all [`HistorySourceError::ExportCommand`] — the
/// caller treats every one as fail-closed.
///
/// # Errors
/// The four failure legs above, with the reference's messages.
pub fn run_export(
    manifest: &HistoryPluginManifest,
    cursor: Option<&str>,
    cwd: Option<&Path>,
    parent_env: &[(String, String)],
) -> Result<Vec<u8>> {
    use std::process::{Command, Stdio};

    let plan = ExportPlan::new(manifest, cursor, cwd, parent_env);
    let argv0 = plan.argv.first().cloned().unwrap_or_default();
    let argv0_repr = pyval::py_repr(&Value::from(argv0.as_str()));

    let mut command = Command::new(&argv0);
    command
        .args(&plan.argv[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // `env=env` on Popen is a REPLACEMENT, not an update.
        .env_clear()
        .envs(plan.env.iter().map(|(k, v)| (k.clone(), v.clone())));
    if let Some(cwd) = &plan.cwd {
        command.current_dir(cwd);
    }
    let mut child = command.spawn().map_err(|err| {
        HistorySourceError::ExportCommand(format!(
            "could not launch export command {argv0_repr}: {}",
            py_oserror(&err, Some(&argv0))
        ))
    })?;

    // Two reader threads, exactly as `_CappedReader` is: the pipes must keep
    // draining past the cap or a chatty child blocks on a full buffer and the
    // deadline fires on a process that was never slow.
    let out_reader = spawn_capped_reader(child.stdout.take(), manifest.max_output_bytes);
    let err_reader = spawn_capped_reader(child.stderr.take(), STDERR_CAP_BYTES);

    let mut timed_out = false;
    let status = match wait_timeout(&mut child, manifest.timeout_seconds) {
        Some(status) => Some(status),
        None => {
            timed_out = true;
            terminate(&mut child);
            None
        }
    };

    let out = out_reader.join().unwrap_or_default();
    let err = err_reader.join().unwrap_or_default();

    if timed_out {
        return Err(HistorySourceError::ExportCommand(format!(
            "export command {argv0_repr} timed out after {}s",
            py_format_g(manifest.timeout_seconds)
        )));
    }
    if out.truncated {
        return Err(HistorySourceError::ExportCommand(format!(
            "export command {argv0_repr} produced more than {} bytes on stdout",
            manifest.max_output_bytes
        )));
    }
    let returncode = status.map_or(0, py_returncode);
    if returncode != 0 {
        // `errors="replace"` then `.strip()`, and an empty detail prints no
        // colon at all.
        let detail = String::from_utf8_lossy(&err.data);
        let detail = py_strip(&detail);
        let suffix = if detail.is_empty() {
            String::new()
        } else {
            format!(": {detail}")
        };
        return Err(HistorySourceError::ExportCommand(format!(
            "export command {argv0_repr} exited {returncode}{suffix}"
        )));
    }
    Ok(out.data)
}

/// What one `_CappedReader` thread produced.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct CappedOutput {
    data: Vec<u8>,
    truncated: bool,
}

/// `_CappedReader` — drain a pipe into memory up to `cap` bytes, discarding the
/// rest.
///
/// Reading past the cap is discarded (not buffered) so a runaway command cannot
/// exhaust memory, while the pipe keeps draining so the child never deadlocks
/// on a full buffer. `truncated` records that the cap was hit.
fn spawn_capped_reader<R>(stream: Option<R>, cap: i64) -> std::thread::JoinHandle<CappedOutput>
where
    R: std::io::Read + Send + 'static,
{
    std::thread::spawn(move || {
        let mut out = CappedOutput::default();
        let Some(mut stream) = stream else {
            // `if stream is None: return` — and `self.data` stays `b""`.
            return out;
        };
        let cap = usize::try_from(cap).unwrap_or(0);
        let mut buffer = vec![0_u8; READ_CHUNK];
        let mut total = 0_usize;
        loop {
            let read = match stream.read(&mut buffer) {
                Ok(0) => break,
                Ok(read) => read,
                // `except (OSError, ValueError): pass` — the partial data
                // gathered so far is what the reader returns.
                Err(_) => break,
            };
            if total >= cap {
                out.truncated = true;
                continue; // keep draining so the child doesn't block
            }
            let room = cap - total;
            if read > room {
                out.data.extend_from_slice(&buffer[..room]);
                total += room;
                out.truncated = true;
            } else {
                out.data.extend_from_slice(&buffer[..read]);
                total += read;
            }
        }
        out
    })
}

/// `proc.wait(timeout=…)` — the status, or `None` for `TimeoutExpired`.
///
/// CPython polls with an exponentially backing-off sleep (`_PopenBase._wait`:
/// 0.5 ms, doubling, capped at 50 ms and at the remaining time). Reproduced
/// rather than replaced with a one-millisecond spin, because the delay is what
/// keeps a 120-second default from costing 120,000 wakeups.
fn wait_timeout(
    child: &mut std::process::Child,
    timeout_seconds: f64,
) -> Option<std::process::ExitStatus> {
    let deadline =
        std::time::Instant::now() + std::time::Duration::from_secs_f64(timeout_seconds.max(0.0));
    let mut delay = std::time::Duration::from_micros(500);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            // A wait that errors is not a timeout; the reference would have
            // raised, and there is no status to report either way.
            Err(_) => return None,
            Ok(None) => {}
        }
        let now = std::time::Instant::now();
        if now >= deadline {
            return None;
        }
        let remaining = deadline - now;
        std::thread::sleep(delay.min(remaining));
        delay = (delay * 2).min(std::time::Duration::from_millis(50));
    }
}

/// `_terminate` — stop a runaway child we own.
///
/// **DIV-452**: the reference escalates `SIGTERM` → 5 s → `SIGKILL`; this sends
/// `SIGKILL` immediately, because `std` has no portable `SIGTERM` and the
/// workspace forbids `unsafe`. The bytes the caller prints are identical (the
/// timeout leg discards the child's output either way); what the child loses is
/// its chance to handle the signal. See the module docs.
fn terminate(child: &mut std::process::Child) {
    let _ = child.kill();
    let _ = child.wait();
}

/// `proc.returncode` — negative for a signalled child, as CPython reports it.
fn py_returncode(status: std::process::ExitStatus) -> i32 {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt as _;
        if let Some(signal) = status.signal() {
            return -signal;
        }
    }
    status.code().unwrap_or(0)
}

// ── Python string / number semantics this crate cannot import ────────────────
//
// `stax_etl::stats::pytext` already carries `is_py_space` / `py_strip`, and
// `stax_hooks` carries a second copy. This crate depends on neither and may not
// (`lib.rs`: `etl → adapters → ()` stays acyclic), so the helpers are local —
// the same deliberate duplication `pytext::py_truthy` records against
// `pyval::py_bool`. Recorded with the `pydecode` copy in DIV-451.

/// Python's `str.isspace()` — four separators wider than Unicode's
/// `White_Space`, which is why `str::trim` is not this function.
fn is_py_space(c: char) -> bool {
    matches!(c,
        '\u{09}'..='\u{0d}'
        | '\u{1c}'..='\u{1f}'
        | '\u{20}'
        | '\u{85}'
        | '\u{a0}'
        | '\u{1680}'
        | '\u{2000}'..='\u{200a}'
        | '\u{2028}'
        | '\u{2029}'
        | '\u{202f}'
        | '\u{205f}'
        | '\u{3000}'
    )
}

/// Python's `s.strip()` (no argument).
fn py_strip(s: &str) -> &str {
    s.trim_matches(is_py_space)
}

/// Python's `str.splitlines()`.
///
/// **Not** `str::lines()`. CPython splits on eight boundaries `\n` is only one
/// of — `\v`, `\f`, `\x1c`, `\x1d`, `\x1e`, `\x85`, `\u{2028}`, `\u{2029}` —
/// and a stream whose JSON carries a raw `\x1e` (a record separator, which some
/// exporters emit) is split there by the reference and would not be by
/// `lines()`. The trailing empty field after a final terminator is dropped, as
/// `splitlines` drops it; `"".splitlines()` is the empty list.
fn py_splitlines(text: &str) -> impl Iterator<Item = &str> {
    let mut out: Vec<&str> = Vec::new();
    let mut start = 0_usize;
    let mut chars = text.char_indices().peekable();
    while let Some((index, ch)) = chars.next() {
        let boundary = matches!(
            ch,
            '\n' | '\u{0b}'
                | '\u{0c}'
                | '\r'
                | '\u{1c}'
                | '\u{1d}'
                | '\u{1e}'
                | '\u{85}'
                | '\u{2028}'
                | '\u{2029}'
        );
        if !boundary {
            continue;
        }
        out.push(&text[start..index]);
        let mut end = index + ch.len_utf8();
        // `\r\n` is ONE boundary.
        if ch == '\r' && chars.peek().is_some_and(|(_, next)| *next == '\n') {
            chars.next();
            end += 1;
        }
        start = end;
    }
    if start < text.len() {
        out.push(&text[start..]);
    }
    out.into_iter()
}

/// `repr(sorted(frozenset))` for a list of string literals — `['a', 'b']`.
fn py_str_list(items: &[&str]) -> String {
    let values: Vec<Value> = items.iter().map(|item| Value::from(*item)).collect();
    pyval::py_repr(&Value::Array(values))
}

/// `f"{x:g}"` — C's `%g` at the default precision of 6.
///
/// `120.0` prints as `120`, not `120.0`; `0.5` as `0.5`; `1e-05` in the
/// exponent form with a two-digit exponent. This is the timeout message's
/// format and nothing else uses it.
fn py_format_g(value: f64) -> String {
    const PRECISION: i32 = 6;
    if value.is_nan() {
        return "nan".to_owned();
    }
    if value.is_infinite() {
        return if value < 0.0 { "-inf" } else { "inf" }.to_owned();
    }
    if value == 0.0 {
        return "0".to_owned();
    }
    // The decimal exponent `%e` would use.
    let exponent = format!("{:e}", value.abs())
        .split_once('e')
        .and_then(|(_, exp)| exp.parse::<i32>().ok())
        .unwrap_or(0);
    if !(-4..PRECISION).contains(&exponent) {
        let mantissa = trim_trailing_zeros(&format!(
            "{:.*}",
            usize::try_from(PRECISION - 1).unwrap_or(0),
            value / 10_f64.powi(exponent)
        ));
        let sign = if exponent < 0 { '-' } else { '+' };
        return format!("{mantissa}e{sign}{:02}", exponent.abs());
    }
    let decimals = usize::try_from(PRECISION - 1 - exponent).unwrap_or(0);
    trim_trailing_zeros(&format!("{value:.decimals$}"))
}

/// `%g`'s trailing-zero removal: only inside a fractional part, and the point
/// goes with them.
fn trim_trailing_zeros(text: &str) -> String {
    if !text.contains('.') {
        return text.to_owned();
    }
    text.trim_end_matches('0').trim_end_matches('.').to_owned()
}

/// `str(OSError)` — `[Errno N] strerror: 'filename'`.
///
/// CPython appends `: {filename!r}` when the exception carries one, which
/// `Popen` and `Path.read_text` both set. Rust's `io::Error` renders the same
/// `strerror` text with a ` (os error N)` tail instead, so the tail is stripped
/// and the errno re-attached in Python's position.
fn py_oserror(err: &std::io::Error, filename: Option<&str>) -> String {
    let rendered = err.to_string();
    let message = rendered
        .rsplit_once(" (os error ")
        .map_or(rendered.as_str(), |(head, _)| head)
        .to_owned();
    let code = err.raw_os_error().unwrap_or(0);
    match filename {
        Some(name) => format!(
            "[Errno {code}] {message}: {}",
            pyval::py_repr(&Value::from(name))
        ),
        None => format!("[Errno {code}] {message}"),
    }
}

/// `json.loads`, with CPython's message on failure.
///
/// The decode itself is [`crate::jsonl::parse_json`] — the depth-tolerant
/// parser the adapters already share. The message comes from
/// [`crate::pydecode`], which agrees with CPython on what is malformed. When
/// the two disagree the input is one CPython accepts and `serde_json` does not
/// (`NaN` / `Infinity`, DIV-454), and saying so is better than printing an
/// empty reason.
fn parse_json_like_python(text: &str) -> std::result::Result<Value, String> {
    if let Some(value) = crate::jsonl::parse_json(text.as_bytes()) {
        return Ok(value);
    }
    Err(pydecode::json_decode_error(text)
        .unwrap_or_else(|| "Expecting value: line 1 column 1 (char 0)".to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn manifest_value() -> Value {
        json!({
            "schema": SCHEMA,
            "source_id": "amp",
            "command": ["./export.sh", "--since", "yesterday"],
        })
    }

    #[test]
    fn a_minimal_manifest_takes_every_default() {
        let manifest = parse_manifest(&manifest_value(), None).expect("valid");
        assert_eq!(manifest.source_id, "amp");
        assert_eq!(manifest.command, ["./export.sh", "--since", "yesterday"]);
        assert_eq!(manifest.cursor, None);
        assert!((manifest.timeout_seconds - DEFAULT_TIMEOUT_SECONDS).abs() < f64::EPSILON);
        assert_eq!(manifest.max_output_bytes, DEFAULT_MAX_OUTPUT_BYTES);
        assert!(manifest.env_passthrough.is_empty());
        assert_eq!(manifest.path, None);
        // A manifest with NO schema key is accepted: the field is optional and
        // only a *disagreeing* value is refused.
        let mut bare = manifest_value();
        bare.as_object_mut().expect("object").remove("schema");
        assert!(parse_manifest(&bare, None).is_ok());
    }

    #[test]
    fn the_caps_clamp_but_never_raise() {
        let mut data = manifest_value();
        {
            let map = data.as_object_mut().expect("object");
            map.insert("timeout_seconds".into(), json!(99999));
            map.insert("max_output_bytes".into(), json!(9_999_999_999_i64));
        }
        let manifest = parse_manifest(&data, None).expect("valid");
        assert!((manifest.timeout_seconds - MAX_TIMEOUT_SECONDS).abs() < f64::EPSILON);
        assert_eq!(manifest.max_output_bytes, HARD_MAX_OUTPUT_BYTES);
        // A lower value is honoured, and `int()` truncates toward zero.
        {
            let map = data.as_object_mut().expect("object");
            map.insert("timeout_seconds".into(), json!(1.5));
            map.insert("max_output_bytes".into(), json!(1024.9));
        }
        let manifest = parse_manifest(&data, None).expect("valid");
        assert!((manifest.timeout_seconds - 1.5).abs() < f64::EPSILON);
        assert_eq!(manifest.max_output_bytes, 1024);
    }

    /// Every message here is the reference's, and the ORDER matters as much as
    /// the text: a manifest wrong in several ways prints exactly one message.
    #[test]
    fn every_manifest_rejection_prints_the_reference_message() {
        let cases: [(Value, &str); 12] = [
            (json!([]), "manifest must be a JSON object"),
            (json!("nope"), "manifest must be a JSON object"),
            (
                json!({"schema": "other-v9", "source_id": "a", "command": ["x"]}),
                "manifest declares schema 'other-v9'; this build speaks \
                 'stackunderflow-history-jsonl-v1'",
            ),
            (
                json!({"schema": 7, "source_id": "a", "command": ["x"]}),
                "manifest declares schema 7; this build speaks \
                 'stackunderflow-history-jsonl-v1'",
            ),
            (
                json!({"command": ["x"]}),
                "manifest 'source_id' must be a non-empty string of [A-Za-z0-9._-] \
                 (it names a project + an on-disk cursor file)",
            ),
            (
                json!({"source_id": "../etc", "command": ["x"]}),
                "manifest 'source_id' must be a non-empty string of [A-Za-z0-9._-] \
                 (it names a project + an on-disk cursor file)",
            ),
            (
                json!({"source_id": "a"}),
                "manifest 'command' must be a non-empty list of strings \
                 (argv, run with no shell)",
            ),
            (
                json!({"source_id": "a", "command": []}),
                "manifest 'command' must be a non-empty list of strings \
                 (argv, run with no shell)",
            ),
            (
                // `not command[0]` — the empty argv0 is the truthiness leg.
                json!({"source_id": "a", "command": [""]}),
                "manifest 'command' must be a non-empty list of strings \
                 (argv, run with no shell)",
            ),
            (
                json!({"source_id": "a", "command": ["x"], "cursor": 7}),
                "manifest 'cursor' must be a string when present",
            ),
            (
                json!({"source_id": "a", "command": ["x"], "timeout_seconds": true}),
                "manifest 'timeout_seconds' must be a positive number",
            ),
            (
                json!({"source_id": "a", "command": ["x"], "timeout_seconds": 0}),
                "manifest 'timeout_seconds' must be > 0",
            ),
        ];
        for (data, expected) in cases {
            let err = parse_manifest(&data, None).expect_err("rejected");
            assert_eq!(err.to_string(), expected, "input {data}");
            assert!(matches!(err, HistorySourceError::Manifest(_)));
        }
        // `where` is the manifest path, in parentheses, when there is one.
        let err = parse_manifest(&json!([]), Some(Path::new("/p/m.json"))).expect_err("rejected");
        assert_eq!(
            err.to_string(),
            "manifest (/p/m.json) must be a JSON object"
        );
        // `env_passthrough` is the last field checked, so it needs a manifest
        // that is otherwise valid to be reached at all.
        let err = parse_manifest(
            &json!({"source_id": "a", "command": ["x"], "env_passthrough": "TOKEN"}),
            None,
        )
        .expect_err("rejected");
        assert_eq!(
            err.to_string(),
            "manifest 'env_passthrough' must be a list of strings"
        );
    }

    #[test]
    fn a_valid_stream_parses_into_the_store_halfs_types() {
        let stream = concat!(
            r#"{"type":"session","session_id":"s1","project":"app","cwd":"/w"}"#,
            "\n",
            r#"{"type":"message","session_id":"s1","seq":0,"role":"user","content":"hi"}"#,
            "\n",
            "\n",
            r#"   {"type":"file_touch","session_id":"s1","seq":1,"path":"/a.py","operation":"Write"}   "#,
            "\n",
            r#"{"type":"cursor","cursor":"page-1"}"#,
            "\n",
            r#"{"type":"cursor","cursor":"page-2"}"#,
            "\n",
        );
        let parsed = parse_stream(stream.as_bytes()).expect("valid");
        assert_eq!(parsed.sessions.len(), 1);
        assert_eq!(parsed.sessions[0].project.as_deref(), Some("app"));
        assert_eq!(parsed.messages.len(), 1);
        assert_eq!(parsed.messages[0].role, "user");
        // Defaults: no timestamp is `""`, no tokens are `0`, no tools is empty.
        assert_eq!(parsed.messages[0].timestamp, "");
        assert_eq!(parsed.messages[0].input_tokens, 0);
        assert!(parsed.messages[0].tools.is_empty());
        assert_eq!(parsed.file_touches.len(), 1);
        assert_eq!(parsed.file_touches[0].operation, "Write");
        // Last cursor wins, and a blank line is skipped rather than rejected.
        assert_eq!(parsed.next_cursor.as_deref(), Some("page-2"));
        assert_eq!(parsed.session_ids(), vec!["s1"]);
    }

    #[test]
    fn a_repeated_session_line_replaces_in_place() {
        // Python's `sessions[id] = rec` keeps the FIRST position and the LAST
        // value; a `Vec` that pushed would have reported two sessions.
        let stream = concat!(
            r#"{"type":"session","session_id":"a","title":"first"}"#,
            "\n",
            r#"{"type":"session","session_id":"b"}"#,
            "\n",
            r#"{"type":"session","session_id":"a","title":"second"}"#,
            "\n",
        );
        let parsed = parse_stream(stream.as_bytes()).expect("valid");
        assert_eq!(parsed.sessions.len(), 2);
        assert_eq!(parsed.sessions[0].session_id, "a");
        assert_eq!(parsed.sessions[0].title.as_deref(), Some("second"));
        assert_eq!(parsed.session_ids(), vec!["a", "b"]);
    }

    /// The rejection table. Each row is one leg of the whole-stream validator,
    /// and each message is the reference's including the `line N: ` prefix.
    #[test]
    fn every_stream_rejection_prints_the_reference_message() {
        let cases: [(&str, &str); 16] = [
            (
                "nope\n",
                "line 1: not valid JSON: Expecting value: line 1 column 1 (char 0)",
            ),
            ("[1, 2]\n", "line 1: each line must be a JSON object"),
            (
                r#"{"type":"nope"}"#,
                "line 1: unknown record type 'nope'; expected one of \
                 ['cursor', 'file_touch', 'message', 'session']",
            ),
            (
                "{}",
                "line 1: unknown record type None; expected one of \
                 ['cursor', 'file_touch', 'message', 'session']",
            ),
            (
                r#"{"type":"cursor"}"#,
                "line 1: 'cursor' record must carry a string 'cursor'",
            ),
            (
                r#"{"type":"cursor","cursor":7}"#,
                "line 1: 'cursor' record must carry a string 'cursor'",
            ),
            (
                r#"{"type":"session"}"#,
                "line 1: 'session_id' must be a non-empty string",
            ),
            (
                r#"{"type":"session","session_id":""}"#,
                "line 1: 'session_id' must be a non-empty string",
            ),
            (
                r#"{"type":"session","session_id":"s","project":7}"#,
                "line 1: 'project' must be a string when present",
            ),
            (
                r#"{"type":"message","session_id":"s","seq":0,"role":"root"}"#,
                "line 1: 'role' must be one of ['assistant', 'system', 'tool', 'user']; \
                 got 'root'",
            ),
            (
                r#"{"type":"message","session_id":"s","seq":0,"role":"user","tools":"Bash"}"#,
                "line 1: 'tools' must be a list of strings",
            ),
            (
                r#"{"type":"message","session_id":"s","seq":0,"role":"user","content":7}"#,
                "line 1: 'content' must be a string",
            ),
            (
                r#"{"type":"message","session_id":"s","seq":-1,"role":"user"}"#,
                "line 1: 'seq' must be a non-negative integer",
            ),
            (
                // `True` is an `int` subclass in Python and is refused anyway.
                r#"{"type":"message","session_id":"s","seq":true,"role":"user"}"#,
                "line 1: 'seq' must be a non-negative integer",
            ),
            (
                r#"{"type":"message","session_id":"s","seq":0,"role":"user","input_tokens":-4}"#,
                "line 1: 'input_tokens' must be a non-negative integer when present",
            ),
            (
                r#"{"type":"file_touch","session_id":"s","seq":0}"#,
                "line 1: 'path' must be a non-empty string",
            ),
        ];
        for (line, expected) in cases {
            let err = parse_stream(line.as_bytes()).expect_err("rejected");
            assert_eq!(err.to_string(), expected, "input {line}");
        }

        // A duplicate `(session_id, seq)` names both lines.
        let stream = concat!(
            r#"{"type":"message","session_id":"s","seq":3,"role":"user"}"#,
            "\n",
            r#"{"type":"file_touch","session_id":"s","seq":3,"path":"/a"}"#,
            "\n",
        );
        assert_eq!(
            parse_stream(stream.as_bytes())
                .expect_err("rejected")
                .to_string(),
            "line 2: duplicate seq 3 for session 's' (also on line 1); \
             seq must be unique within a session"
        );

        // The whole-stream UTF-8 failure carries NO line prefix.
        let err = parse_stream(b"{\"type\":\"cursor\",\"cursor\":\"\xff\"}").expect_err("rejected");
        assert_eq!(
            err.to_string(),
            "stream is not valid UTF-8: 'utf-8' codec can't decode byte 0xff in position 27: \
             invalid start byte"
        );
        assert!(matches!(
            err,
            HistorySourceError::StreamValidation { line_no: 0, .. }
        ));
    }

    #[test]
    fn a_late_bad_line_rejects_the_whole_stream() {
        // Fail-closed is the entire point: three good lines and one bad one is
        // an error, not three imported records.
        let stream = concat!(
            r#"{"type":"message","session_id":"s","seq":0,"role":"user"}"#,
            "\n",
            r#"{"type":"message","session_id":"s","seq":1,"role":"user"}"#,
            "\n",
            r#"{"type":"message","session_id":"s","seq":2,"role":"nope"}"#,
            "\n",
        );
        let err = parse_stream(stream.as_bytes()).expect_err("rejected");
        assert!(err.to_string().starts_with("line 3: 'role' must be one of"));
    }

    #[test]
    fn the_line_split_is_pythons_and_not_rusts() {
        // `str.splitlines()` breaks on `\x1e`; `str::lines()` does not. A
        // record-separator-framed stream is one line to Rust and three to the
        // reference, and the reference is what the store must agree with.
        let stream = concat!(
            r#"{"type":"message","session_id":"s","seq":0,"role":"user"}"#,
            "\u{1e}",
            r#"{"type":"message","session_id":"s","seq":1,"role":"user"}"#,
            "\u{2028}",
            r#"{"type":"message","session_id":"s","seq":2,"role":"user"}"#,
        );
        let parsed = parse_stream(stream.as_bytes()).expect("valid");
        assert_eq!(parsed.messages.len(), 3);
        // And `\r\n` is one boundary, not two blank lines.
        let crlf =
            "{\"type\":\"cursor\",\"cursor\":\"a\"}\r\n{\"type\":\"cursor\",\"cursor\":\"b\"}\r\n";
        assert_eq!(
            parse_stream(crlf.as_bytes()).expect("valid").next_cursor,
            Some("b".to_owned())
        );
        assert_eq!(py_splitlines("").count(), 0);
        assert_eq!(py_splitlines("a\n").collect::<Vec<_>>(), vec!["a"]);
        assert_eq!(py_splitlines("a\n\n").collect::<Vec<_>>(), vec!["a", ""]);
    }

    #[test]
    fn the_child_environment_is_cleared_and_allowlisted() {
        let mut data = manifest_value();
        data.as_object_mut()
            .expect("object")
            .insert("env_passthrough".into(), json!(["AMP_TOKEN", "PATH"]));
        let manifest = parse_manifest(&data, None).expect("valid");
        let parent: Vec<(String, String)> = [
            ("PATH", "/bin"),
            ("HOME", "/home/u"),
            ("TZ", "UTC"),
            ("AMP_TOKEN", "secret"),
            ("AWS_SECRET_ACCESS_KEY", "leak"),
            ("LANG", "C"),
        ]
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect();
        let env = build_child_env(&manifest, Some("page-2"), &parent);
        let keys: Vec<&str> = env.iter().map(|(k, _)| k.as_str()).collect();
        // Allowlist order first (only the keys the parent actually has), then
        // the passthrough keys; `PATH` keeps its FIRST position rather than
        // moving to the end, because Python's dict does.
        assert_eq!(
            keys,
            vec!["PATH", "HOME", "LANG", "TZ", "AMP_TOKEN", CURSOR_ENV_VAR]
        );
        // Nothing outside the two lists survives.
        assert!(!keys.contains(&"AWS_SECRET_ACCESS_KEY"));
        assert_eq!(
            env.iter()
                .find(|(k, _)| k == CURSOR_ENV_VAR)
                .map(|(_, v)| v.as_str()),
            Some("page-2")
        );
        // No cursor is the EMPTY STRING, not an absent variable — the export
        // command can tell "first run" from "not set" only because of that.
        let env = build_child_env(&manifest, None, &parent);
        assert_eq!(
            env.iter()
                .find(|(k, _)| k == CURSOR_ENV_VAR)
                .map(|(_, v)| v.as_str()),
            Some("")
        );
        // A manifest that passes the cursor variable through keeps its
        // position and loses its value to the assignment.
        let mut data = manifest_value();
        data.as_object_mut()
            .expect("object")
            .insert("env_passthrough".into(), json!([CURSOR_ENV_VAR]));
        let manifest = parse_manifest(&data, None).expect("valid");
        let parent = vec![(CURSOR_ENV_VAR.to_owned(), "stale".to_owned())];
        let env = build_child_env(&manifest, Some("fresh"), &parent);
        assert_eq!(env, vec![(CURSOR_ENV_VAR.to_owned(), "fresh".to_owned())]);
    }

    #[test]
    fn the_g_format_is_cs_and_not_rusts() {
        // `f"{x:g}"`, transcribed from CPython: the default timeout prints
        // `120`, not `120.0`, and the exponent form carries two digits.
        for (value, expected) in [
            (120.0, "120"),
            (3600.0, "3600"),
            (1.5, "1.5"),
            (0.5, "0.5"),
            (0.0001, "0.0001"),
            (0.00001, "1e-05"),
            (1_000_000.0, "1e+06"),
            (123_456.0, "123456"),
            (1_234_567.0, "1.23457e+06"),
            (2.5e-9, "2.5e-09"),
        ] {
            assert_eq!(py_format_g(value), expected, "value {value}");
        }
    }

    #[test]
    fn an_oserror_reads_like_pythons() {
        let err = std::io::Error::from_raw_os_error(2);
        assert_eq!(
            py_oserror(&err, Some("./nope.sh")),
            "[Errno 2] No such file or directory: './nope.sh'"
        );
        let err = std::io::Error::from_raw_os_error(13);
        assert_eq!(
            py_oserror(&err, Some("/x")),
            "[Errno 13] Permission denied: '/x'"
        );
    }

    // ── the runner, against real children ────────────────────────────────────

    struct Scratch(PathBuf);

    impl Scratch {
        fn new(tag: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "stax-custom-jsonl-{tag}-{}-{:?}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |delta| delta.subsec_nanos())
            ));
            std::fs::create_dir_all(&path).expect("scratch");
            Self(path)
        }

        /// A manifest whose command is `sh -c <script>`, which is how a test
        /// gets a deterministic child without shipping a fixture file. The
        /// runner still spawns with **no shell of its own** — `sh` here is the
        /// user's own program, exactly as a real manifest's would be.
        fn manifest(&self, script: &str) -> HistoryPluginManifest {
            parse_manifest(
                &json!({
                    "source_id": "amp",
                    "command": ["sh", "-c", script],
                    "timeout_seconds": 10,
                }),
                Some(&self.0.join(MANIFEST_FILENAME)),
            )
            .expect("valid")
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn parent_env() -> Vec<(String, String)> {
        std::env::vars()
            .filter(|(key, _)| ENV_ALLOWLIST.contains(&key.as_str()))
            .collect()
    }

    #[test]
    fn a_successful_run_returns_stdout_and_sees_the_cursor() {
        let scratch = Scratch::new("ok");
        let manifest = scratch.manifest("printf 'cursor=%s\\n' \"$STACKUNDERFLOW_HISTORY_CURSOR\"");
        let out =
            run_export(&manifest, Some("page-7"), Some(&scratch.0), &parent_env()).expect("ran");
        assert_eq!(String::from_utf8_lossy(&out), "cursor=page-7\n");
    }

    #[test]
    fn the_environment_the_child_sees_is_the_cleared_one() {
        let scratch = Scratch::new("env");
        let manifest = scratch.manifest("printf '%s\\n' \"${LEAKED:-absent}\"");
        let mut parent = parent_env();
        parent.push(("LEAKED".to_owned(), "yes".to_owned()));
        let out = run_export(&manifest, None, Some(&scratch.0), &parent).expect("ran");
        assert_eq!(String::from_utf8_lossy(&out), "absent\n");
    }

    #[test]
    fn a_nonzero_exit_carries_the_childs_stderr() {
        let scratch = Scratch::new("rc");
        let manifest = scratch.manifest("echo 'token expired' >&2; exit 3");
        let err = run_export(&manifest, None, Some(&scratch.0), &parent_env()).expect_err("failed");
        assert_eq!(
            err.to_string(),
            "export command 'sh' exited 3: token expired"
        );
        // An empty stderr prints no colon at all.
        let manifest = scratch.manifest("exit 4");
        assert_eq!(
            run_export(&manifest, None, Some(&scratch.0), &parent_env())
                .expect_err("failed")
                .to_string(),
            "export command 'sh' exited 4"
        );
    }

    #[test]
    fn an_over_cap_stream_is_refused_and_the_child_never_blocks() {
        let scratch = Scratch::new("cap");
        // 200 KB from a child whose cap is 1 KB: the reader must keep draining
        // past the cap or this test hangs instead of failing.
        let manifest = parse_manifest(
            &json!({
                "source_id": "amp",
                "command": ["sh", "-c", "i=0; while [ $i -lt 200 ]; do printf '%01024d' 0; i=$((i+1)); done"],
                "max_output_bytes": 1024,
                "timeout_seconds": 30,
            }),
            None,
        )
        .expect("valid");
        assert_eq!(
            run_export(&manifest, None, Some(&scratch.0), &parent_env())
                .expect_err("failed")
                .to_string(),
            "export command 'sh' produced more than 1024 bytes on stdout"
        );
        // Exactly at the cap is NOT truncated — the boundary a differ cannot
        // reach without a byte-exact child.
        let manifest = parse_manifest(
            &json!({
                "source_id": "amp",
                "command": ["sh", "-c", "printf '%01024d' 0"],
                "max_output_bytes": 1024,
            }),
            None,
        )
        .expect("valid");
        assert_eq!(
            run_export(&manifest, None, Some(&scratch.0), &parent_env())
                .expect("ran")
                .len(),
            1024
        );
    }

    #[test]
    fn a_runaway_child_times_out_and_is_killed() {
        let scratch = Scratch::new("timeout");
        // `sleep` directly, not `sh -c sleep`: a shell that FORKS rather than
        // execs leaves a grandchild holding the write end of the stdout pipe,
        // and the reader thread then blocks until the grandchild exits — on
        // BOTH implementations, since Python's `out_reader.join()` waits on the
        // same pipe. The runner's contract is over the process it spawned; a
        // process tree it did not create is the user's own.
        let manifest = parse_manifest(
            &json!({
                "source_id": "amp",
                "command": ["sleep", "30"],
                "timeout_seconds": 0.25,
            }),
            None,
        )
        .expect("valid");
        let started = std::time::Instant::now();
        let err =
            run_export(&manifest, None, Some(&scratch.0), &parent_env()).expect_err("timed out");
        assert_eq!(
            err.to_string(),
            "export command 'sleep' timed out after 0.25s"
        );
        // The kill is not a formality: the call returns promptly rather than
        // waiting out the child's own 30 seconds.
        assert!(started.elapsed() < std::time::Duration::from_secs(10));
    }

    #[test]
    fn a_command_that_does_not_exist_is_a_launch_failure() {
        let scratch = Scratch::new("spawn");
        let manifest = parse_manifest(
            &json!({"source_id": "amp", "command": ["./definitely-not-here"]}),
            None,
        )
        .expect("valid");
        let err = run_export(&manifest, None, Some(&scratch.0), &parent_env()).expect_err("failed");
        assert_eq!(
            err.to_string(),
            "could not launch export command './definitely-not-here': \
             [Errno 2] No such file or directory: './definitely-not-here'"
        );
    }

    #[test]
    fn the_manifest_loader_reads_a_file_or_its_directory() {
        let scratch = Scratch::new("load");
        let path = scratch.0.join(MANIFEST_FILENAME);
        std::fs::write(&path, manifest_value().to_string()).expect("write");
        // A directory resolves to the canonical filename inside it.
        let from_dir = load_manifest(&scratch.0).expect("loaded");
        let from_file = load_manifest(&path).expect("loaded");
        assert_eq!(from_dir, from_file);
        assert_eq!(from_file.path.as_deref(), Some(path.as_path()));

        // A missing file names the path it tried.
        let missing = scratch.0.join("nope.json");
        assert_eq!(
            load_manifest(&missing).expect_err("missing").to_string(),
            format!("manifest not found: {}", missing.display())
        );
        // Malformed JSON carries CPython's decoder message verbatim.
        std::fs::write(&path, "{oops").expect("write");
        assert_eq!(
            load_manifest(&path).expect_err("invalid").to_string(),
            format!(
                "manifest {} is not valid JSON: Expecting property name enclosed in \
                 double quotes: line 1 column 2 (char 1)",
                path.display()
            )
        );
        // And the manifest's own path appears in a validation message.
        std::fs::write(&path, "[]").expect("write");
        assert_eq!(
            load_manifest(&path).expect_err("invalid").to_string(),
            format!("manifest ({}) must be a JSON object", path.display())
        );
    }

    #[test]
    fn the_export_plan_is_what_the_spawn_will_use() {
        let scratch = Scratch::new("plan");
        let manifest = scratch.manifest("true");
        let parent = vec![("PATH".to_owned(), "/bin".to_owned())];
        let plan = ExportPlan::new(&manifest, Some("c1"), Some(&scratch.0), &parent);
        assert_eq!(plan.argv, vec!["sh", "-c", "true"]);
        assert_eq!(plan.cwd.as_deref(), Some(scratch.0.as_path()));
        assert_eq!(
            plan.env,
            vec![
                ("PATH".to_owned(), "/bin".to_owned()),
                (CURSOR_ENV_VAR.to_owned(), "c1".to_owned()),
            ]
        );
    }
}
