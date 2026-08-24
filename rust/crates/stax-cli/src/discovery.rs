//! The five top-level discovery commands — `cli.py`'s back-compat aliases.
//!
//! `find-failure-modes-for-file`, `find-sessions-in-path`,
//! `find-sessions-touching-file`, `find-sessions-where-action-worked` and
//! `search-past-decisions` are **top-level** commands, not `memory`
//! subcommands. They predate the `memory` namespace and Python keeps them as
//! thin aliases over the same `_run_*_query` helpers, so the engine is shared
//! with [`crate::memory`] — what differs, and what this module exists to get
//! right, is the surface:
//!
//! * **A different JSON shape.** These emit the original
//!   `{"sessions": [...]}` object (plus `_truncated` / `_more_available` /
//!   `_budget_used_tokens` / `_budget_max_tokens` when a budget applied), never
//!   the `staxtrace.memory/1` envelope. An agent that learned the alias
//!   output keeps working.
//! * **Flags the `memory` verbs do not have.** `--mode read|write|any`,
//!   `--provider`, `--file`, `--min-confidence`, `-v/--verbose`,
//!   `--use-embeddings`, `--embed-model`.
//! * **No cwd scoping and no intent gate.** `search-past-decisions` leaves
//!   `scope_to_cwd` off, so its `--project` default really is "every project" —
//!   the opposite of `memory decisions`. And an empty query is not an error
//!   here: it runs, matches nothing, and exits 0.
//!
//! One divergence is deliberate and shared with the rest of the port: Python's
//! discovery functions **write** to `discovery_telemetry` (`_record_loaded`
//! bumps `loaded_count` for every surfaced session) on every one of these five
//! commands. This binary opens the store `SQLITE_OPEN_READ_ONLY`, so the write
//! cannot happen. Nothing on stdout depends on the counter and no current
//! ranking term reads it; recorded as fixed-in-rust in `rust/TASKS-RS.md`,
//! matching the landed `memory` precedent (DIV-009 B-2).

use anyhow::Result;
use clap::{Args, ValueEnum};
use rusqlite::Connection;
use stax_core::queries::{
    self, ActionWorked, BudgetedResult, SessionMatch, ValueError, outcome, paths, pyint::PyInt,
    pyjson,
};
use stax_core::settings;
use stax_core::store::Store;

use crate::memory::{EmitFlags, Format, MemoryEnv, Output, emit_sessions_with, py_int};

// ── option types ─────────────────────────────────────────────────────────────

/// `--mode` — `click.Choice(("read", "write", "any"))`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Mode {
    /// Only `Read` tool args.
    Read,
    /// `Edit` / `Write` / `MultiEdit` / `NotebookEdit` args.
    Write,
    /// Any of those, or a free-form mention in the message text.
    Any,
}

impl Mode {
    /// The engine's spelling.
    fn engine(self) -> outcome::Mode {
        match self {
            Self::Read => outcome::Mode::Read,
            Self::Write => outcome::Mode::Write,
            Self::Any => outcome::Mode::Any,
        }
    }

    /// What `f"(mode={mode})"` prints — the Click choice string, not the enum.
    fn label(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
            Self::Any => "any",
        }
    }
}

/// `click.option(type=float)` — CPython's `float()`, not `str::parse`.
///
/// `float()` runs `PyUnicode_TransformDecimalAndSpaceToASCII` first, so any
/// Unicode decimal digit is a digit and any Unicode whitespace is a space; it
/// then strips, and accepts `_` between digits. `--min-confidence ' 0.5 '`,
/// `--min-confidence 0_5.0` and `--min-confidence nan` are all exit-0
/// invocations on the Python side; `str::parse` rejects the first two.
fn py_float(raw: &str) -> Result<f64, String> {
    // Transform: decimal digits → ASCII, whitespace → ' ', everything else as-is.
    let transformed: String = raw
        .chars()
        .map(|ch| {
            if queries::pyint::is_int_space(ch) {
                return ' ';
            }
            queries::pyint::decimal_value(ch)
                .and_then(|value| char::from_digit(value, 10))
                .unwrap_or(ch)
        })
        .collect();
    let text = transformed.trim_matches(' ');
    // `_` is legal only between two digits; strip the legal ones, reject the rest.
    let bytes: Vec<char> = text.chars().collect();
    let mut cleaned = String::with_capacity(bytes.len());
    for (index, ch) in bytes.iter().enumerate() {
        if *ch != '_' {
            cleaned.push(*ch);
            continue;
        }
        let before = index.checked_sub(1).and_then(|i| bytes.get(i));
        let after = bytes.get(index + 1);
        if !matches!((before, after), (Some(b), Some(a)) if b.is_ascii_digit() && a.is_ascii_digit())
        {
            return Err("is not a valid float".to_string());
        }
    }
    cleaned
        .parse::<f64>()
        .map_err(|_| "is not a valid float".to_string())
}

/// `max(0.0, min(1.0, x))` with Python's two-argument `min` / `max` semantics.
///
/// Not `f64::clamp`: `clamp` propagates a NaN and Python does not. `min(1.0,
/// nan)` returns `1.0` (the comparison `nan < 1.0` is false, so the first
/// argument stands), and `max(0.0, 1.0)` is `1.0` — so `--min-confidence nan`
/// is a threshold of 1.0, not a NaN that fails every comparison.
#[must_use]
fn py_clamp_unit(value: f64) -> f64 {
    let lowered = if value < 1.0 { value } else { 1.0 };
    if lowered > 0.0 { lowered } else { 0.0 }
}

// ── the five command surfaces ────────────────────────────────────────────────

/// `find-sessions-in-path PATH`.
#[derive(Debug, Clone, Args)]
pub struct InPathArgs {
    /// Directory or file whose project ancestry to search.
    #[arg(allow_hyphen_values = true)]
    pub path: String,
    /// Only sessions whose last activity is newer than this.
    #[arg(long, allow_hyphen_values = true, overrides_with = "since")]
    pub since: Option<String>,
    /// Max sessions to return (hard cap).
    #[arg(
        long,
        default_value_t = PyInt::from(20),
        value_parser = py_int,
        allow_hyphen_values = true,
        overrides_with = "limit"
    )]
    pub limit: PyInt,
    /// Token budget for the output. Pass 0 to disable.
    #[arg(
        long = "context-budget",
        value_name = "TOKENS",
        value_parser = py_int,
        allow_hyphen_values = true,
        overrides_with = "context_budget"
    )]
    pub context_budget: Option<PyInt>,
    /// Filter by provider slug (e.g. claude, codex, cursor).
    #[arg(long, allow_hyphen_values = true, overrides_with = "provider")]
    pub provider: Option<String>,
    /// Output format.
    #[arg(
        long = "format",
        value_name = "FMT",
        default_value = "text",
        overrides_with = "format"
    )]
    pub format: Format,
}

/// `find-sessions-touching-file FILE`.
#[derive(Debug, Clone, Args)]
pub struct TouchingFileArgs {
    /// File (or directory) to look for.
    #[arg(allow_hyphen_values = true)]
    pub file: String,
    /// Max sessions to return (hard cap).
    #[arg(
        long,
        default_value_t = PyInt::from(20),
        value_parser = py_int,
        allow_hyphen_values = true,
        overrides_with = "limit"
    )]
    pub limit: PyInt,
    /// Token budget for the output. Pass 0 to disable.
    #[arg(
        long = "context-budget",
        value_name = "TOKENS",
        value_parser = py_int,
        allow_hyphen_values = true,
        overrides_with = "context_budget"
    )]
    pub context_budget: Option<PyInt>,
    /// Match against Read tool args, Edit/Write tool args, or any mention.
    #[arg(
        long,
        value_name = "MODE",
        default_value = "any",
        overrides_with = "mode"
    )]
    pub mode: Mode,
    /// Output format.
    #[arg(
        long = "format",
        value_name = "FMT",
        default_value = "text",
        overrides_with = "format"
    )]
    pub format: Format,
}

/// `search-past-decisions QUERY`.
#[derive(Debug, Clone, Args)]
pub struct PastDecisionsArgs {
    /// Text to substring-search across past message content.
    #[arg(allow_hyphen_values = true)]
    pub query: String,
    /// Filter by project slug (e.g. -Users-yad-dev-foo).
    #[arg(long, allow_hyphen_values = true, overrides_with = "project")]
    pub project: Option<String>,
    /// Filter to messages newer than this.
    #[arg(long, allow_hyphen_values = true, overrides_with = "since")]
    pub since: Option<String>,
    /// Max sessions to return (hard cap).
    #[arg(
        long,
        default_value_t = PyInt::from(20),
        value_parser = py_int,
        allow_hyphen_values = true,
        overrides_with = "limit"
    )]
    pub limit: PyInt,
    /// Token budget for the output. Pass 0 to disable.
    #[arg(
        long = "context-budget",
        value_name = "TOKENS",
        value_parser = py_int,
        allow_hyphen_values = true,
        overrides_with = "context_budget"
    )]
    pub context_budget: Option<PyInt>,
    /// Re-rank substring matches by Ollama embeddings (cosine similarity).
    #[arg(long = "use-embeddings", overrides_with = "use_embeddings")]
    pub use_embeddings: bool,
    /// Override the Ollama embed model. Ignored without --use-embeddings.
    #[arg(
        long = "embed-model",
        value_name = "MODEL",
        allow_hyphen_values = true,
        overrides_with = "embed_model"
    )]
    pub embed_model: Option<String>,
    /// Output format.
    #[arg(
        long = "format",
        value_name = "FMT",
        default_value = "text",
        overrides_with = "format"
    )]
    pub format: Format,
}

/// `find-sessions-where-action-worked ACTION`.
#[derive(Debug, Clone, Args)]
pub struct ActionWorkedArgs {
    /// Action to match against tool calls and message text.
    #[arg(allow_hyphen_values = true)]
    pub action: String,
    /// Filter by project slug (e.g. -Users-yad-dev-foo).
    #[arg(long, allow_hyphen_values = true, overrides_with = "project")]
    pub project: Option<String>,
    /// Narrow to sessions that also touched this file.
    #[arg(
        long = "file",
        value_name = "FILE",
        allow_hyphen_values = true,
        overrides_with = "file_path"
    )]
    pub file_path: Option<String>,
    /// Only sessions whose matching activity is newer than this.
    #[arg(long, allow_hyphen_values = true, overrides_with = "since")]
    pub since: Option<String>,
    /// Max sessions to return.
    #[arg(
        long,
        default_value_t = PyInt::from(20),
        value_parser = py_int,
        allow_hyphen_values = true,
        overrides_with = "limit"
    )]
    pub limit: PyInt,
    /// Minimum outcome confidence in [0.0, 1.0]. Default 0.5.
    #[arg(
        long = "min-confidence",
        value_name = "FLOAT",
        value_parser = py_float,
        allow_hyphen_values = true,
        overrides_with = "min_confidence"
    )]
    pub min_confidence: Option<f64>,
    /// Append outcome_confidence to each row in text output.
    #[arg(short = 'v', long, overrides_with = "verbose")]
    pub verbose: bool,
    /// Output format.
    #[arg(
        long = "format",
        value_name = "FMT",
        default_value = "text",
        overrides_with = "format"
    )]
    pub format: Format,
}

/// `find-failure-modes-for-file FILE`.
#[derive(Debug, Clone, Args)]
pub struct FailureModesArgs {
    /// File (or directory) whose edit history to inspect.
    #[arg(allow_hyphen_values = true)]
    pub file: String,
    /// Only sessions whose edit is newer than this.
    #[arg(long, allow_hyphen_values = true, overrides_with = "since")]
    pub since: Option<String>,
    /// Max sessions to return.
    #[arg(
        long,
        default_value_t = PyInt::from(20),
        value_parser = py_int,
        allow_hyphen_values = true,
        overrides_with = "limit"
    )]
    pub limit: PyInt,
    /// Minimum outcome confidence in [0.0, 1.0]. Default 0.5.
    #[arg(
        long = "min-confidence",
        value_name = "FLOAT",
        value_parser = py_float,
        allow_hyphen_values = true,
        overrides_with = "min_confidence"
    )]
    pub min_confidence: Option<f64>,
    /// Append outcome_confidence to each row in text output.
    #[arg(short = 'v', long, overrides_with = "verbose")]
    pub verbose: bool,
    /// Output format.
    #[arg(
        long = "format",
        value_name = "FMT",
        default_value = "text",
        overrides_with = "format"
    )]
    pub format: Format,
}

// ── entry points ─────────────────────────────────────────────────────────────

/// Run `find-sessions-in-path` against the default store.
///
/// # Errors
/// When the store cannot be opened, or a query fails for a reason Python's
/// `except ValueError` would not have caught.
pub fn run_in_path(args: &InPathArgs) -> Result<()> {
    with_store(|conn, env| in_path(conn, args, env))
}

/// Run `find-sessions-touching-file` against the default store.
///
/// # Errors
/// As [`run_in_path`].
pub fn run_touching_file(args: &TouchingFileArgs) -> Result<()> {
    with_store(|conn, env| touching_file(conn, args, env))
}

/// Run `search-past-decisions` against the default store.
///
/// # Errors
/// As [`run_in_path`].
pub fn run_past_decisions(args: &PastDecisionsArgs) -> Result<()> {
    with_store(|conn, env| past_decisions(conn, args, env))
}

/// Run `find-sessions-where-action-worked` against the default store.
///
/// # Errors
/// As [`run_in_path`].
pub fn run_action_worked(args: &ActionWorkedArgs) -> Result<()> {
    with_store(|conn, env| action_worked(conn, args, env))
}

/// Run `find-failure-modes-for-file` against the default store.
///
/// # Errors
/// As [`run_in_path`].
pub fn run_failure_modes(args: &FailureModesArgs) -> Result<()> {
    with_store(|conn, env| failure_modes(conn, args, env))
}

/// `_open_store()` + the resolved environment + Click's exit conventions.
fn with_store(run: impl FnOnce(&Connection, &MemoryEnv) -> Result<Output>) -> Result<()> {
    let env = MemoryEnv::from_process()?;
    let store = Store::open_read_only(&settings::store_path())?;
    let output = run(store.conn(), &env)?;
    print!("{}", output.stdout);
    if !output.stderr.is_empty() {
        eprint!("{}", output.stderr);
    }
    if output.code != 0 {
        std::process::exit(output.code);
    }
    Ok(())
}

// ── find-sessions-in-path ────────────────────────────────────────────────────

fn in_path(conn: &Connection, args: &InPathArgs, env: &MemoryEnv) -> Result<Output> {
    let budget = env.budget(args.context_budget.as_ref());
    let result = queries::find_sessions_in_path(
        conn,
        &args.path,
        args.since.as_deref(),
        &args.limit,
        // `if provider and …` — an empty `--provider` filters nothing.
        args.provider.as_deref().filter(|slug| !slug.is_empty()),
        &budget,
    );
    let result = match caught(result, "find-sessions-in-path", "[OPTIONS] PATH")? {
        Ok(result) => result,
        Err(failure) => return Ok(failure),
    };
    Ok(Output::ok(render_budgeted(
        &result,
        args.format,
        &format!("Sessions in path {}", args.path),
        EmitFlags::default(),
    )))
}

// ── find-sessions-touching-file ──────────────────────────────────────────────

fn touching_file(conn: &Connection, args: &TouchingFileArgs, env: &MemoryEnv) -> Result<Output> {
    let budget = env.budget(args.context_budget.as_ref());
    // No `try:` around this one in the reference — it takes no `--since`, so
    // there is no `ValueError` to catch and a store failure exits 1.
    let result = queries::find_sessions_touching_file_budgeted_indexed_mode(
        conn,
        env.index.as_ref(),
        &args.file,
        &args.limit,
        args.mode.engine(),
        &budget,
    )?;
    Ok(Output::ok(render_budgeted(
        &result,
        args.format,
        &format!(
            "Sessions touching {}  (mode={})",
            args.file,
            args.mode.label()
        ),
        EmitFlags::default(),
    )))
}

// ── search-past-decisions ────────────────────────────────────────────────────

fn past_decisions(conn: &Connection, args: &PastDecisionsArgs, env: &MemoryEnv) -> Result<Output> {
    let budget = env.budget(args.context_budget.as_ref());
    // `scope_to_cwd=False`: the alias's `--project` default really is "every
    // project", the opposite of `memory decisions`. Left as found.
    let project = args.project.as_deref();
    let result = if args.use_embeddings {
        // The embeddings path keeps its own substring+cosine pipeline, so the
        // lexical index is deliberately *not* injected — `--use-embeddings` on
        // a populated-FTS machine still takes the LIKE branch.
        let endpoints = crate::embeddings::endpoints_from_process();
        let model = crate::embeddings::model_from_process(args.embed_model.as_deref());
        let scorer = |conn: &Connection, needle: &str, pairs: &[(i64, i64)]| {
            crate::embeddings::scores(conn, needle, pairs, &model, &endpoints)
        };
        queries::search_past_decisions_embeddings(
            conn,
            &args.query,
            project,
            args.since.as_deref(),
            args.limit.saturating_i64(),
            &budget,
            &scorer,
        )
    } else {
        queries::search_past_decisions_indexed(
            conn,
            env.index.as_ref(),
            &args.query,
            project,
            args.since.as_deref(),
            args.limit.saturating_i64(),
            &budget,
        )
    };
    let result = match caught(result, "search-past-decisions", "[OPTIONS] QUERY")? {
        Ok(result) => result,
        Err(failure) => return Ok(failure),
    };
    Ok(Output::ok(render_budgeted(
        &result,
        args.format,
        &format!("Past decisions matching {}", paths::py_repr(&args.query)),
        EmitFlags {
            show_snippet: true,
            show_embedding_score: args.use_embeddings,
            show_outcome_confidence: false,
        },
    )))
}

// ── find-sessions-where-action-worked ────────────────────────────────────────

fn action_worked(conn: &Connection, args: &ActionWorkedArgs, env: &MemoryEnv) -> Result<Output> {
    let threshold = args
        .min_confidence
        .map_or(outcome::DEFAULT_MIN_OUTCOME_CONFIDENCE, py_clamp_unit);
    let matches = queries::find_sessions_where_action_worked_indexed(
        conn,
        env.index.as_ref(),
        &args.action,
        &ActionWorked {
            project: args.project.as_deref(),
            file_path: args.file_path.as_deref(),
            since: args.since.as_deref(),
            limit: args.limit.saturating_i64(),
            min_confidence: threshold,
        },
    );
    let matches = match caught(
        matches,
        "find-sessions-where-action-worked",
        "[OPTIONS] ACTION",
    )? {
        Ok(matches) => matches,
        Err(failure) => return Ok(failure),
    };
    // `bool(verbose) or (min_confidence is not None)` — an explicit threshold
    // opts you into the score even without `-v`.
    let flags = EmitFlags {
        show_outcome_confidence: args.verbose || args.min_confidence.is_some(),
        ..EmitFlags::default()
    };
    Ok(Output::ok(render_list(
        &matches,
        args.format,
        &format!("Sessions where {} worked", paths::py_repr(&args.action)),
        flags,
    )))
}

// ── find-failure-modes-for-file ──────────────────────────────────────────────

/// The one alias with no environment dependency at all: `_run_failure_modes_query`
/// passes no `search_service` and takes no budget, so neither the FTS sidecar nor
/// `Settings().discovery_budget_tokens` is consulted.
fn failure_modes(conn: &Connection, args: &FailureModesArgs, _env: &MemoryEnv) -> Result<Output> {
    let threshold = args
        .min_confidence
        .map_or(outcome::DEFAULT_MIN_OUTCOME_CONFIDENCE, py_clamp_unit);
    let matches = queries::find_failure_modes_for_file(
        conn,
        &args.file,
        args.since.as_deref(),
        args.limit.saturating_i64(),
        threshold,
    );
    let matches = match caught(matches, "find-failure-modes-for-file", "[OPTIONS] FILE")? {
        Ok(matches) => matches,
        Err(failure) => return Ok(failure),
    };
    let flags = EmitFlags {
        show_outcome_confidence: args.verbose || args.min_confidence.is_some(),
        ..EmitFlags::default()
    };
    Ok(Output::ok(render_list(
        &matches,
        args.format,
        &format!("Failure modes for {}", args.file),
        flags,
    )))
}

// ── shared plumbing ──────────────────────────────────────────────────────────

/// `except ValueError as exc: raise click.BadParameter(str(exc), "--since")`.
///
/// Only a `ValueError` becomes the parameter error; anything else (a corrupt
/// store, an overflowing `LIMIT ?` bind) propagates to `main`, which is exit 1
/// with an empty stdout — Python's behaviour, where the traceback is uncaught.
fn caught<T>(
    result: Result<T>,
    command: &str,
    usage_args: &str,
) -> Result<Result<T, Output>, anyhow::Error> {
    match result {
        Ok(value) => Ok(Ok(value)),
        Err(error) => match ValueError::of(&error) {
            Some(message) => Ok(Err(bad_since(command, usage_args, message))),
            None => Err(error),
        },
    }
}

/// Click's `BadParameter(param_hint="--since")` rendering, on stderr, exit 2.
fn bad_since(command: &str, usage_args: &str, message: &str) -> Output {
    Output {
        stdout: String::new(),
        stderr: format!(
            "Usage: stax {command} {usage_args}\n\
             Try 'stax {command} --help' for help.\n\
             \n\
             Error: Invalid value for --since: {message}\n"
        ),
        code: 2,
    }
}

/// `_emit_sessions(result: BudgetedResult, …)`.
fn render_budgeted(
    result: &BudgetedResult,
    format: Format,
    title: &str,
    flags: EmitFlags,
) -> String {
    if format == Format::Json {
        let mut payload: Vec<(String, pyjson::Value)> = vec![(
            "sessions".to_string(),
            pyjson::Value::Array(result.sessions.iter().map(SessionMatch::to_dict).collect()),
        )];
        if result.truncated {
            payload.push(("_truncated".to_string(), pyjson::Value::Bool(true)));
            payload.push((
                "_more_available".to_string(),
                pyjson::Value::Int(i64::try_from(result.more_available).unwrap_or(i64::MAX)),
            ));
        }
        // A `BudgetedResult` always carries both, so both keys always appear.
        payload.push((
            "_budget_used_tokens".to_string(),
            pyjson::Value::Int(result.budget_used_tokens),
        ));
        payload.push((
            "_budget_max_tokens".to_string(),
            pyjson::Value::Int(result.budget_max_tokens),
        ));
        return format!(
            "{}\n",
            pyjson::dumps_indent2(&pyjson::Value::Object(payload))
        );
    }
    emit_sessions_with(
        &result.sessions,
        result.truncated,
        result.more_available,
        title,
        flags,
    )
}

/// `_emit_sessions(result: list[SessionMatch], …)` — the un-budgeted queries.
///
/// A bare list has no `truncated` / `budget_*` attributes, so `getattr` returns
/// the defaults and the JSON object is `{"sessions": [...]}` and nothing else.
fn render_list(sessions: &[SessionMatch], format: Format, title: &str, flags: EmitFlags) -> String {
    if format == Format::Json {
        let payload = pyjson::Value::Object(vec![(
            "sessions".to_string(),
            pyjson::Value::Array(sessions.iter().map(SessionMatch::to_dict).collect()),
        )]);
        return format!("{}\n", pyjson::dumps_indent2(&payload));
    }
    emit_sessions_with(sessions, false, 0, title, flags)
}

#[cfg(test)]
mod tests {
    use clap::Parser as _;

    use super::*;

    #[test]
    fn float_parsing_is_cpythons_float_not_str_parse() {
        assert_eq!(py_float("0.5"), Ok(0.5));
        assert_eq!(py_float(" 0.5 "), Ok(0.5));
        assert_eq!(py_float("+.5"), Ok(0.5));
        assert_eq!(py_float("1_0.5"), Ok(10.5));
        assert_eq!(py_float("\u{667}"), Ok(7.0), "an Arabic-Indic digit");
        assert!(py_float("nan").expect("nan parses").is_nan());
        assert_eq!(py_float("infinity"), Ok(f64::INFINITY));
        assert!(py_float("_1").is_err());
        assert!(py_float("1_").is_err());
        assert!(py_float("1__0").is_err());
        assert!(py_float("0x1").is_err());
        assert!(py_float("").is_err());
    }

    /// `max(0.0, min(1.0, x))` — and NaN comes out 1.0, where `clamp` keeps NaN.
    #[test]
    fn the_confidence_clamp_follows_pythons_min_max() {
        assert_eq!(py_clamp_unit(0.5), 0.5);
        assert_eq!(py_clamp_unit(-3.0), 0.0);
        assert_eq!(py_clamp_unit(7.0), 1.0);
        assert_eq!(py_clamp_unit(f64::NEG_INFINITY), 0.0);
        assert_eq!(py_clamp_unit(f64::INFINITY), 1.0);
        assert_eq!(
            py_clamp_unit(f64::NAN),
            1.0,
            "min(1.0, nan) is 1.0 in CPython — clamp would keep the NaN"
        );
    }

    fn parse(argv: &[&str]) -> Result<crate::Command, clap::error::ErrorKind> {
        let mut all = vec!["stax"];
        all.extend_from_slice(argv);
        crate::Cli::try_parse_from(all)
            .map(|cli| cli.command)
            .map_err(|error| error.kind())
    }

    #[test]
    fn every_alias_keeps_its_python_name() {
        for argv in [
            vec!["find-sessions-in-path", "/tmp"],
            vec!["find-sessions-touching-file", "/tmp/x.py"],
            vec!["search-past-decisions", "cache"],
            vec!["find-sessions-where-action-worked", "Edit"],
            vec!["find-failure-modes-for-file", "/tmp/x.py"],
        ] {
            assert!(parse(&argv).is_ok(), "{argv:?} must parse");
        }
    }

    #[test]
    fn the_alias_flags_are_the_ones_click_declares() {
        let crate::Command::FindSessionsTouchingFile(args) = parse(&[
            "find-sessions-touching-file",
            "/tmp/x.py",
            "--mode",
            "write",
            "--limit",
            " 3",
            "--context-budget",
            "0",
            "--format",
            "json",
        ])
        .expect("parses") else {
            panic!("expected find-sessions-touching-file");
        };
        assert_eq!(args.mode, Mode::Write);
        assert_eq!(args.limit.to_string(), "3");
        assert_eq!(
            args.context_budget.as_ref().map(ToString::to_string),
            Some("0".to_string())
        );
        assert_eq!(args.format, Format::Json);

        let crate::Command::FindSessionsWhereActionWorked(args) = parse(&[
            "find-sessions-where-action-worked",
            "Edit",
            "--file",
            "/tmp/x.py",
            "--min-confidence",
            "0.0",
            "-v",
            "--project",
            "-Users-you-dev-proj",
        ])
        .expect("parses") else {
            panic!("expected find-sessions-where-action-worked");
        };
        assert_eq!(args.file_path.as_deref(), Some("/tmp/x.py"));
        assert_eq!(args.min_confidence, Some(0.0));
        assert!(args.verbose);
        assert_eq!(args.project.as_deref(), Some("-Users-you-dev-proj"));

        let crate::Command::SearchPastDecisions(args) = parse(&[
            "search-past-decisions",
            "cache",
            "--use-embeddings",
            "--embed-model",
            "mxbai-embed-large",
        ])
        .expect("parses") else {
            panic!("expected search-past-decisions");
        };
        assert!(args.use_embeddings);
        assert_eq!(args.embed_model.as_deref(), Some("mxbai-embed-large"));
    }

    /// Click keeps the **last** occurrence of a repeated option; clap's default
    /// is exit 2. Every alias option carries `overrides_with` for that reason.
    #[test]
    fn repeated_options_are_last_wins_on_the_aliases_too() {
        let crate::Command::FindSessionsInPath(args) = parse(&[
            "find-sessions-in-path",
            "/tmp",
            "--limit",
            "3",
            "--limit",
            "5",
            "--provider",
            "codex",
            "--provider",
            "claude",
        ])
        .expect("parses") else {
            panic!("expected find-sessions-in-path");
        };
        assert_eq!(args.limit.to_string(), "5");
        assert_eq!(args.provider.as_deref(), Some("claude"));
    }

    #[test]
    fn the_bad_since_error_is_clicks_parameter_error() {
        let failure = bad_since(
            "find-sessions-in-path",
            "[OPTIONS] PATH",
            "Invalid since value 'x'",
        );
        assert_eq!(failure.code, 2);
        assert!(failure.stdout.is_empty());
        assert_eq!(
            failure.stderr,
            "Usage: stax find-sessions-in-path [OPTIONS] PATH\n\
             Try 'stax find-sessions-in-path --help' for help.\n\
             \n\
             Error: Invalid value for --since: Invalid since value 'x'\n"
        );
    }

    #[test]
    fn the_alias_json_is_the_sessions_object_not_the_envelope() {
        let empty = BudgetedResult {
            sessions: vec![],
            truncated: false,
            more_available: 0,
            budget_used_tokens: 0,
            budget_max_tokens: 2000,
        };
        assert_eq!(
            render_budgeted(&empty, Format::Json, "T", EmitFlags::default()),
            "{\n  \"sessions\": [],\n  \"_budget_used_tokens\": 0,\n  \
             \"_budget_max_tokens\": 2000\n}\n"
        );
        let truncated = BudgetedResult {
            more_available: 4,
            truncated: true,
            ..empty
        };
        assert_eq!(
            render_budgeted(&truncated, Format::Json, "T", EmitFlags::default()),
            "{\n  \"sessions\": [],\n  \"_truncated\": true,\n  \"_more_available\": 4,\n  \
             \"_budget_used_tokens\": 0,\n  \"_budget_max_tokens\": 2000\n}\n"
        );
        // The un-budgeted queries carry neither key.
        assert_eq!(
            render_list(&[], Format::Json, "T", EmitFlags::default()),
            "{\n  \"sessions\": []\n}\n"
        );
    }

    #[test]
    fn the_text_titles_are_the_reference_strings() {
        assert_eq!(
            render_list(
                &[],
                Format::Text,
                "Failure modes for /tmp/x.py",
                EmitFlags::default()
            ),
            "Failure modes for /tmp/x.py: no matching sessions.\n"
        );
        assert_eq!(Mode::Any.label(), "any");
        assert_eq!(Mode::Read.label(), "read");
        assert_eq!(Mode::Write.label(), "write");
    }
}
