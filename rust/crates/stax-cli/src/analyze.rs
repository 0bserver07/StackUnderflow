//! `stax analyze` — `cli.py`'s static-analysis group (Spec 21), the last
//! parked node family, translated 2026-08-06 on the maintainer's
//! translate-first order.
//!
//! `session` and `backfill` need Playback v2 and the analyzer runner, which
//! live in `stax-server` — a crate this one may not link (DIV-279/308). Same
//! answer as `ingest webhook serve`: spawn the sibling binary. The bin's
//! `--analyze <json>` runs one verb without binding and answers one JSON
//! object on stdout; this module renders click's text from it. `quality`
//! calls `stax_reports::grading` directly — that service was already shared.
//!
//! Recorded divergence: the reference's `--all` text mode prints one line per
//! session *as each grade lands*; the spawn shape prints them when the batch
//! returns. Same bytes, later. And `analyze session --language` repeats where
//! click uses `multiple=True` — clap's `action=Append` is the same surface.

use std::path::PathBuf;

use anyhow::Result;
use clap::{Args, Subcommand};
use serde_json::{Value, json};

use crate::click::{Output, UsageError};
use crate::compare::sort_keys;

/// `stax analyze [VERB]`.
#[derive(Debug, Args)]
pub struct AnalyzeArgs {
    /// Which analyze verb to run.
    #[command(subcommand)]
    pub verb: AnalyzeVerb,
}

/// The `analyze` subcommands.
#[derive(Debug, Subcommand)]
pub enum AnalyzeVerb {
    /// Run analyzers on every file SESSION_ID touched; persist findings.
    Session(SessionArgs),
    /// Analyze every recent session lacking static_analysis_findings rows.
    Backfill(BackfillArgs),
    /// Grade session quality using a local Ollama model.
    Quality(QualityArgs),
}

/// `analyze session`'s surface.
#[derive(Debug, Args)]
pub struct SessionArgs {
    /// The session UUID (matches sessions.session_id).
    pub session_id: String,
    /// Restrict to these languages (repeatable). Default: all supported.
    #[arg(long = "language", value_parser = ["python", "typescript", "go"])]
    pub languages: Vec<String>,
    /// Output format.
    #[arg(long = "format", default_value = "text", value_parser = ["text", "json"])]
    pub format: String,
}

/// `analyze backfill`'s surface.
#[derive(Debug, Args)]
pub struct BackfillArgs {
    /// Only sessions whose last activity is newer than this.
    /// '7d', '1w', '1m', '24h', or an ISO date.
    #[arg(long, default_value = "30d")]
    pub since: String,
    /// Cap on candidates analyzed (default: no cap).
    #[arg(short = 'N', long, value_parser = clap::value_parser!(i64).range(1..))]
    pub limit: Option<i64>,
    /// Worker count (default: min(4, cpu_count); analyzers fork shell processes).
    #[arg(long, value_parser = clap::value_parser!(u64).range(1..=16))]
    pub concurrency: Option<u64>,
    /// Output format.
    #[arg(long = "format", default_value = "text", value_parser = ["text", "json"])]
    pub format: String,
}

/// `analyze quality`'s surface.
#[derive(Debug, Args)]
pub struct QualityArgs {
    /// The session UUID. Optional when --all is given.
    pub session_id: Option<String>,
    /// Grade all sessions that have not been graded yet.
    #[arg(long = "all")]
    pub all_flag: bool,
    /// Force re-grading even if cached grade exists.
    #[arg(long)]
    pub force: bool,
    /// Output format.
    #[arg(long = "format", default_value = "text", value_parser = ["text", "json"])]
    pub format: String,
}

/// Run the requested `analyze` verb.
///
/// # Errors
/// Store failures from the `quality` path; spawn failures from the others.
pub fn run_analyze(args: &AnalyzeArgs) -> Result<Output> {
    match &args.verb {
        AnalyzeVerb::Session(session) => run_session(session),
        AnalyzeVerb::Backfill(backfill) => run_backfill(backfill),
        AnalyzeVerb::Quality(quality) => run_quality(quality),
    }
}

// ── the spawn plumbing ───────────────────────────────────────────────────────

/// The `stax-server` sitting next to this binary, or the bare name — the
/// `ingest webhook serve` resolution, verbatim.
fn server_binary() -> PathBuf {
    let sibling = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join("stax-server")));
    match sibling {
        Some(path) if path.is_file() => path,
        _ => PathBuf::from("stax-server"),
    }
}

/// Spawn `stax-server --analyze <request>` and parse its one-object answer.
fn spawn_analyze(request: &Value) -> Result<(Value, i32)> {
    let output = std::process::Command::new(server_binary())
        .arg("--analyze")
        .arg(stax_memory::pyjson::dumps_compact(request))
        .arg("--data-dir")
        .arg(stax_core::settings::app_dir())
        .output()?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let payload = serde_json::from_str::<Value>(stdout.trim()).unwrap_or_else(
        |_| json!({"error": {"kind": "spawn", "message": stdout.trim().to_owned()}}),
    );
    Ok((payload, output.status.code().unwrap_or(1)))
}

/// Map the bin's error envelope onto click's rendering.
fn render_bin_error(payload: &Value, command_path: &str, arg_spec: &str) -> Output {
    let error = payload.get("error");
    let message = error
        .and_then(|e| e.get("message"))
        .and_then(Value::as_str)
        .unwrap_or("analyze failed");
    let kind = error
        .and_then(|e| e.get("kind"))
        .and_then(Value::as_str)
        .unwrap_or("");
    if kind == "bad_parameter" {
        let error = UsageError::bad_parameter(command_path, arg_spec, "SESSION_ID", message);
        return Output::usage(&error, crate::click::PROGRAM);
    }
    Output {
        stdout: String::new(),
        stderr: format!("Error: {message}\n"),
        code: 1,
    }
}

// ── session ──────────────────────────────────────────────────────────────────

fn run_session(args: &SessionArgs) -> Result<Output> {
    let request = json!({
        "verb": "session",
        "session_id": args.session_id,
        "languages": args.languages,
    });
    let (payload, code) = spawn_analyze(&request)?;
    if code != 0 || payload.get("error").is_some() {
        return Ok(render_bin_error(
            &payload,
            "analyze session",
            "[OPTIONS] SESSION_ID",
        ));
    }

    if args.format == "json" {
        // `json.dumps(..., indent=2, sort_keys=True)`.
        return Ok(Output::ok(format!(
            "{}\n",
            stax_memory::pyjson::dumps_pretty(&sort_keys(&payload))
        )));
    }

    let text = |key: &str| payload.get(key).cloned().unwrap_or(Value::Null);
    let list = |key: &str| -> Vec<String> {
        payload
            .get(key)
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .map(|v| v.as_str().unwrap_or_default().to_owned())
                    .collect()
            })
            .unwrap_or_default()
    };
    let mut out = String::new();
    out.push_str(&format!(
        "Session: {}\n",
        text("session_id").as_str().unwrap_or_default()
    ));
    out.push_str(&format!(
        "  files analyzed: {}\n",
        text("files_analyzed").as_i64().unwrap_or_default()
    ));
    out.push_str(&format!(
        "  rows written:   {}\n",
        text("rows_written").as_i64().unwrap_or_default()
    ));
    let languages = list("languages");
    out.push_str(&format!(
        "  languages:      {}\n",
        if languages.is_empty() {
            "(none)".to_owned()
        } else {
            languages.join(", ")
        }
    ));
    let skipped = list("skipped_files");
    if !skipped.is_empty() {
        out.push_str("  skipped:\n");
        for entry in &skipped {
            out.push_str(&format!("    - {entry}\n"));
        }
    }
    let warnings = list("warnings");
    if !warnings.is_empty() {
        out.push_str("  warnings:\n");
        for entry in warnings.iter().take(10) {
            out.push_str(&format!("    - {entry}\n"));
        }
        if warnings.len() > 10 {
            out.push_str(&format!("    ...and {} more\n", warnings.len() - 10));
        }
    }
    Ok(Output::ok(out))
}

// ── backfill ─────────────────────────────────────────────────────────────────

fn run_backfill(args: &BackfillArgs) -> Result<Output> {
    // `_parse_analyze_since_arg` — empty/whitespace ⇒ no bound; a bad value is
    // `click.BadParameter(..., param_hint="--since")`.
    let since_iso = if args.since.trim().is_empty() {
        None
    } else {
        match stax_core::queries::pytime::parse_since(Some(&args.since)) {
            Ok(parsed) => parsed,
            Err(error) => {
                let usage = UsageError::bad_parameter(
                    "analyze backfill",
                    "[OPTIONS]",
                    "'--since'",
                    error.to_string(),
                );
                return Ok(Output::usage(&usage, crate::click::PROGRAM));
            }
        }
    };

    let request = json!({
        "verb": "backfill",
        "since": since_iso,
        "limit": args.limit,
        "concurrency": args.concurrency,
    });
    let (payload, code) = spawn_analyze(&request)?;
    if code != 0 || payload.get("error").is_some() {
        return Ok(render_bin_error(&payload, "analyze backfill", "[OPTIONS]"));
    }

    if args.format == "json" {
        return Ok(Output::ok(format!(
            "{}\n",
            stax_memory::pyjson::dumps_pretty(&sort_keys(&payload))
        )));
    }
    let get = |key: &str| payload.get(key).and_then(Value::as_i64).unwrap_or_default();
    Ok(Output::ok(format!(
        "Backfill complete:\n  candidates:    {}\n  analyzed:      {}\n  rows written:  {}\n  warnings:      {}\n",
        get("candidates"),
        get("analyzed"),
        get("rows_written"),
        get("warnings_count"),
    )))
}

// ── quality ──────────────────────────────────────────────────────────────────

fn run_quality(args: &QualityArgs) -> Result<Output> {
    use stax_reports::grading::{DEFAULT_OLLAMA_URL, grade_session};

    if args.session_id.is_none() && !args.all_flag {
        // `click.UsageError("Must specify either SESSION_ID or --all.")`.
        let usage = UsageError::bad_parameter(
            "analyze quality",
            "[OPTIONS] [SESSION_ID]",
            "SESSION_ID",
            "Must specify either SESSION_ID or --all.",
        );
        return Ok(Output::usage(&usage, crate::click::PROGRAM));
    }

    let conn = crate::reports::open_store()?;

    if args.all_flag {
        let mut statement = conn.prepare(
            "SELECT session_id FROM sessions \
             WHERE first_ts IS NOT NULL \
               AND session_id NOT IN (SELECT session_id FROM session_quality_metrics)",
        )?;
        let sessions: Vec<String> = statement
            .query_map([], |row| row.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        drop(statement);

        if sessions.is_empty() {
            return Ok(Output::ok(if args.format == "text" {
                "No ungraded sessions found.\n".to_owned()
            } else {
                format!(
                    "{}\n",
                    stax_memory::pyjson::dumps_compact(&json!({"results": []}))
                )
            }));
        }

        let mut out = String::new();
        let mut results = Vec::new();
        for sid in &sessions {
            let grade = grade_session(&conn, sid, args.force, DEFAULT_OLLAMA_URL)
                .map_err(|err| anyhow::anyhow!("{err:?}"))?;
            if args.format == "text" {
                let score = grade.get("overall_score").cloned().unwrap_or(Value::Null);
                out.push_str(&format!(
                    "Graded session {sid}: score={}\n",
                    stax_etl::stats::pytext::py_str(&score)
                ));
            }
            results.push(grade);
        }
        if args.format == "json" {
            out = format!(
                "{}\n",
                stax_memory::pyjson::dumps_pretty(&Value::Array(results))
            );
        }
        return Ok(Output::ok(out));
    }

    let session_id = args.session_id.as_deref().unwrap_or_default();
    let known: Option<i64> = conn
        .query_row(
            "SELECT id FROM sessions WHERE session_id = ?",
            [session_id],
            |row| row.get(0),
        )
        .ok();
    if known.is_none() {
        let usage = UsageError::bad_parameter(
            "analyze quality",
            "[OPTIONS] [SESSION_ID]",
            "SESSION_ID",
            format!("Session '{session_id}' not found in database."),
        );
        return Ok(Output::usage(&usage, crate::click::PROGRAM));
    }

    let grade = grade_session(&conn, session_id, args.force, DEFAULT_OLLAMA_URL)
        .map_err(|err| anyhow::anyhow!("{err:?}"))?;
    if args.format == "json" {
        return Ok(Output::ok(format!(
            "{}\n",
            stax_memory::pyjson::dumps_pretty(&grade)
        )));
    }

    let get_str = |value: &Value, key: &str| -> String {
        value
            .get(key)
            .map(|v| match v {
                Value::String(text) => text.clone(),
                other => stax_etl::stats::pytext::py_str(other),
            })
            .unwrap_or_default()
    };
    let grades = grade.get("grades").cloned().unwrap_or_else(|| json!({}));
    let sub = |key: &str| -> String {
        grades
            .get(key)
            .map_or("None".to_owned(), stax_etl::stats::pytext::py_str)
    };
    let mut out = String::new();
    out.push_str(&format!("Session: {}\n", get_str(&grade, "session_id")));
    out.push_str(&format!(
        "  Overall Score: {}/10.0\n",
        stax_etl::stats::pytext::py_str(grade.get("overall_score").unwrap_or(&Value::Null))
    ));
    out.push_str("  Sub-grades:\n");
    out.push_str(&format!(
        "    - Goal Clarity:         {}/10.0\n",
        sub("goal_clarity")
    ));
    out.push_str(&format!(
        "    - Execution Efficiency: {}/10.0\n",
        sub("execution_efficiency")
    ));
    out.push_str(&format!(
        "    - Success:              {}/10.0\n",
        sub("success")
    ));
    out.push_str(&format!("  Rationale: {}\n", get_str(&grade, "rationale")));
    out.push_str("  Suggestions:\n");
    if let Some(suggestions) = grade.get("suggestions").and_then(Value::as_array) {
        for suggestion in suggestions {
            out.push_str(&format!(
                "    - {}\n",
                suggestion.as_str().map_or_else(
                    || stax_etl::stats::pytext::py_str(suggestion),
                    str::to_owned
                )
            ));
        }
    }
    Ok(Output::ok(out))
}
