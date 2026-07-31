//! `stax-rs memory {sessions,decisions,worked,file}` — the wave-1 read path.
//!
//! A port of `cli.py`'s `memory` group (`:2382`–`:2595`) and the helpers it
//! calls: `_memory_options`, `_memory_format`, `_memory_fail`,
//! `_resolve_context_budget`, `_emit_sessions`, `_memory_file_results`,
//! `_emit_memory_file_text`, `_detect_cwd_project_slug`. The queries themselves
//! live in [`stax_core::queries`]; this module is the surface: option parsing,
//! the two output formats, and the exit codes.
//!
//! Three properties are the point of the port, and each is load-bearing:
//!
//! * **Text output is byte-identical.** Every literal, every double space,
//!   every `…` (U+2026) is the reference's. `stax-rs memory sessions` diffs
//!   clean against `stackunderflow memory sessions` on the same store and cwd.
//! * **JSON goes through the shared contract.** The envelope is
//!   [`stax_memory`]'s `stackunderflow.memory/1` — same builder, same
//!   CPython-compatible writer, so `--json` output is byte-identical too and
//!   the goldens gate both crates.
//! * **The bugs come along.** `memory decisions` scopes to the current
//!   directory's project and returns nothing when no slug covers the cwd;
//!   phrases are matched as literal substrings so multi-word queries usually
//!   find nothing; tool-call turns store no `content_text` and are invisible to
//!   the content scan. All ported, all recorded in `rust/TASKS-RS.md`.
//!
//! What is deliberately *not* here: the FTS5/bm25 content half. The Python CLI
//! injects a `SearchService` into three of these queries, so on a machine whose
//! `search_index.db` is populated `decisions` / `worked` / `sessions <file>`
//! take a different path than this module does. That path is `stax-memory`'s
//! (RS-1-007) and its absence is the one *behavioral* divergence this file
//! carries; see the report in `rust/TASKS-RS.md`.

use std::path::{Path, PathBuf};

use anyhow::Result;
use clap::{Args, Subcommand, ValueEnum};
use serde_json::{Map, Value};
use stax_core::queries::{
    self, BudgetedResult, RiskSummary, SessionMatch, paths, pyjson, rank::Budget,
};
use stax_core::settings;
use stax_core::store::Store;
use stax_memory::{MemoryCommand, build_envelope, build_error_envelope, render};

// ── the command surface ──────────────────────────────────────────────────────

/// `--format` — `_VALID_FORMATS`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Format {
    /// Human-readable summary.
    Text,
    /// The stable agent-output envelope.
    Json,
}

/// The six options every `memory` subcommand shares (`cli._memory_options`).
#[derive(Debug, Clone, Args)]
pub struct MemoryOptions {
    /// Output format. 'json' emits the stable agent-output envelope.
    #[arg(long = "format", value_name = "FMT", default_value = "text")]
    pub format: Format,
    /// Shortcut for --format json.
    #[arg(long = "json")]
    pub as_json: bool,
    /// Project slug to scope to. Default: the current directory's project,
    /// when StackUnderflow recognises it.
    ///
    /// `allow_hyphen_values` because every slug starts with `-`
    /// (`-Users-you-dev-proj`): without it clap reads the value as a flag and
    /// exits 2 where Click happily scopes the query.
    #[arg(long, allow_hyphen_values = true)]
    pub project: Option<String>,
    /// Time lower bound: '7d', '1w', '1m', '24h', or an ISO date/datetime.
    #[arg(long)]
    pub since: Option<String>,
    /// Hard cap on the number of results.
    #[arg(long, default_value_t = 20)]
    pub limit: i64,
    /// Token budget for the output. Default:
    /// STACKUNDERFLOW_DISCOVERY_BUDGET_TOKENS or 2000. Pass 0 to disable.
    #[arg(long = "context-budget", value_name = "TOKENS")]
    pub context_budget: Option<i64>,
}

impl MemoryOptions {
    /// `cli._memory_format` — `--json` wins when both are given.
    #[must_use]
    pub fn json_mode(&self) -> bool {
        self.as_json || self.format == Format::Json
    }
}

/// `stax-rs memory` — ask the local store what past sessions already know.
#[derive(Debug, Args)]
pub struct MemoryArgs {
    /// Which question to ask.
    #[command(subcommand)]
    pub verb: MemoryVerb,
}

/// The four wave-1 verbs.
#[derive(Debug, Subcommand)]
pub enum MemoryVerb {
    /// Search past decisions — "did I decide something about this before?"
    Decisions {
        /// Text to substring-search across past message content.
        #[arg(allow_hyphen_values = true)]
        query: String,
        /// The six shared options.
        #[command(flatten)]
        options: MemoryOptions,
    },
    /// Everything known about a file — "what do I know about this file?"
    File {
        /// File (or directory) to report on.
        #[arg(allow_hyphen_values = true)]
        path: String,
        /// The six shared options.
        #[command(flatten)]
        options: MemoryOptions,
    },
    /// Find where an action worked — "what worked last time I tried this?"
    Worked {
        /// Action to match against tool calls and message text.
        #[arg(allow_hyphen_values = true)]
        action: String,
        /// The six shared options.
        #[command(flatten)]
        options: MemoryOptions,
    },
    /// List past sessions that touched here — "which sessions ran here?"
    Sessions {
        /// Directory or file. Default: the current directory.
        #[arg(allow_hyphen_values = true)]
        path: Option<String>,
        /// The six shared options.
        #[command(flatten)]
        options: MemoryOptions,
    },
}

impl MemoryVerb {
    /// The shared options, whichever verb this is.
    #[must_use]
    pub fn options(&self) -> &MemoryOptions {
        match self {
            Self::Decisions { options, .. }
            | Self::File { options, .. }
            | Self::Worked { options, .. }
            | Self::Sessions { options, .. } => options,
        }
    }
}

// ── what a run produces ──────────────────────────────────────────────────────

/// The bytes and the exit status of one `memory` invocation.
///
/// Returned rather than printed so the whole surface is testable without a
/// subprocess, and so `stdout` can be compared with the Python CLI's byte for
/// byte in the parity harness.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Output {
    /// Everything `click.echo` would have written to stdout.
    pub stdout: String,
    /// Everything Click would have written to stderr (the text-mode error).
    pub stderr: String,
    /// `0`, `1` (JSON error envelope), or `2` (Click's `BadParameter`).
    pub code: i32,
}

impl Output {
    fn ok(stdout: String) -> Self {
        Self {
            stdout,
            stderr: String::new(),
            code: 0,
        }
    }
}

/// Everything the run needs from the environment, injected.
///
/// Nothing inside reads a process global, which is what makes the renderers
/// testable and keeps the crate clear of `std::env::set_var` (finding 5).
#[derive(Debug, Clone)]
pub struct MemoryEnv {
    /// `Path.cwd()`.
    pub cwd: PathBuf,
    /// `Path.home()`, for `~` expansion.
    pub home: Option<PathBuf>,
    /// `Settings().discovery_budget_tokens`.
    pub budget_default: i64,
    /// `Settings().discovery_rank_weights`, parsed.
    pub weights: (f64, f64, f64),
    /// `datetime.now(UTC)` as epoch seconds, for the recency term.
    pub now_epoch: f64,
}

impl MemoryEnv {
    /// Resolve from the real process environment and `config.json`.
    ///
    /// # Errors
    /// When the current directory cannot be read.
    pub fn from_process() -> Result<Self> {
        let config = read_config(&settings::app_dir());
        Ok(Self {
            cwd: std::env::current_dir()?,
            home: paths::home_dir(),
            budget_default: resolve_budget_default(
                std::env::var("STACKUNDERFLOW_DISCOVERY_BUDGET_TOKENS")
                    .ok()
                    .as_deref(),
                config.as_ref(),
            ),
            weights: resolve_weights(
                std::env::var("STACKUNDERFLOW_DISCOVERY_RANK_WEIGHTS")
                    .ok()
                    .as_deref(),
                config.as_ref(),
            ),
            now_epoch: stax_core::queries::rank::now_epoch(),
        })
    }

    /// `cli._resolve_context_budget` — the flag, else the setting.
    #[must_use]
    pub fn budget(&self, flag: Option<i64>) -> Budget {
        Budget::at(
            flag.unwrap_or(self.budget_default),
            self.weights,
            self.now_epoch,
        )
    }
}

/// `settings._load()` — `config.json`, or nothing when it is absent or corrupt.
fn read_config(app_dir: &Path) -> Option<pyjson::Value> {
    let raw = std::fs::read_to_string(app_dir.join("config.json")).ok()?;
    pyjson::loads(&raw)
}

/// `Settings().discovery_budget_tokens` — env, then file, then `2000`.
///
/// The env leg goes through `int(raw)`, which falls back to the default on a
/// non-integer; the file leg is returned as stored and cast by
/// `_resolve_context_budget`'s `int(...)`.
#[must_use]
pub fn resolve_budget_default(env: Option<&str>, config: Option<&pyjson::Value>) -> i64 {
    const DEFAULT: i64 = 2000;
    if let Some(raw) = env {
        return raw.trim().parse::<i64>().unwrap_or(DEFAULT);
    }
    match config.and_then(|config| config.get("discovery_budget_tokens")) {
        Some(pyjson::Value::Int(value)) => *value,
        Some(pyjson::Value::Float(value)) => *value as i64,
        Some(pyjson::Value::Str(value)) => value.trim().parse::<i64>().unwrap_or(DEFAULT),
        _ => DEFAULT,
    }
}

/// `Settings().discovery_rank_weights` — env, then file, then `0.5,0.2,0.3`.
#[must_use]
pub fn resolve_weights(env: Option<&str>, config: Option<&pyjson::Value>) -> (f64, f64, f64) {
    if let Some(raw) = env {
        return stax_core::queries::rank::parse_rank_weights(Some(raw));
    }
    match config.and_then(|config| config.get("discovery_rank_weights")) {
        Some(pyjson::Value::Str(value)) => {
            stax_core::queries::rank::parse_rank_weights(Some(value))
        }
        _ => stax_core::queries::rank::parse_rank_weights(None),
    }
}

// ── entry point ──────────────────────────────────────────────────────────────

/// Run one `memory` subcommand against the default store and exit accordingly.
///
/// # Errors
/// When the store cannot be opened, or a query fails for a reason the reference
/// would not have caught (`_memory_fail` only catches `ValueError`).
pub fn run_memory(args: &MemoryArgs) -> Result<()> {
    let env = MemoryEnv::from_process()?;
    let store = Store::open_read_only(&settings::store_path())?;
    let output = run_verb(store.conn(), &args.verb, &env)?;
    print!("{}", output.stdout);
    if !output.stderr.is_empty() {
        eprint!("{}", output.stderr);
    }
    if output.code != 0 {
        std::process::exit(output.code);
    }
    Ok(())
}

/// Run one verb against an already-open connection.
///
/// # Errors
/// When a query fails for a non-`ValueError` reason.
pub fn run_verb(conn: &rusqlite::Connection, verb: &MemoryVerb, env: &MemoryEnv) -> Result<Output> {
    match verb {
        MemoryVerb::Decisions { query, options } => run_decisions(conn, query, options, env),
        MemoryVerb::File { path, options } => run_file(conn, path, options, env),
        MemoryVerb::Worked { action, options } => run_worked(conn, action, options, env),
        MemoryVerb::Sessions { path, options } => run_sessions(conn, path.as_deref(), options, env),
    }
}

// ── memory decisions ─────────────────────────────────────────────────────────

fn run_decisions(
    conn: &rusqlite::Connection,
    query: &str,
    options: &MemoryOptions,
    env: &MemoryEnv,
) -> Result<Output> {
    let json_mode = options.json_mode();
    let budget = env.budget(options.context_budget);
    let mut echo = query_echo(&[("text", Value::String(query.to_string()))], options);

    if !search_has_intent(query) {
        return Ok(memory_fail(
            "decisions",
            &echo,
            NO_SEARCH_INTENT,
            json_mode,
            "memory decisions [OPTIONS] QUERY",
        ));
    }
    let slug = match &options.project {
        Some(slug) => Some(slug.clone()),
        None => queries::detect_cwd_project_slug(conn, &paths::path_to_string(&env.cwd)),
    };
    let result = match queries::search_past_decisions(
        conn,
        query,
        slug.as_deref(),
        options.since.as_deref(),
        options.limit,
        &budget,
    ) {
        Ok(result) => result,
        Err(error) => {
            return Ok(memory_fail(
                "decisions",
                &echo,
                &error.to_string(),
                json_mode,
                "memory decisions [OPTIONS] QUERY",
            ));
        }
    };
    set_project(&mut echo, slug.as_deref());

    if json_mode {
        return Ok(Output::ok(envelope_line(
            "decisions",
            echo,
            rows(&result.sessions),
            budget.tokens,
            result.truncated,
            Map::new(),
        )));
    }
    Ok(Output::ok(emit_sessions(
        &result.sessions,
        result.truncated,
        result.more_available,
        &format!("Past decisions matching {}", paths::py_repr(query)),
        true,
    )))
}

// ── memory file ──────────────────────────────────────────────────────────────

fn run_file(
    conn: &rusqlite::Connection,
    path: &str,
    options: &MemoryOptions,
    env: &MemoryEnv,
) -> Result<Output> {
    let json_mode = options.json_mode();
    let budget = env.budget(options.context_budget);
    let mut echo = query_echo(&[("path", Value::String(path.to_string()))], options);

    let report = file_report(conn, path, options);
    let (failure_modes, touching, risk) = match report {
        Ok(report) => report,
        Err(error) => {
            return Ok(memory_fail(
                "file",
                &echo,
                &error.to_string(),
                json_mode,
                "memory file [OPTIONS] PATH",
            ));
        }
    };
    // `risk['path']` is the absolute path discovery actually matched.
    if let Some(slot) = echo.get_mut("path") {
        *slot = Value::String(risk.path.clone());
    }
    let (results, truncated) = file_results(&failure_modes, &touching, options.limit, &budget);

    if json_mode {
        let mut extra = Map::new();
        extra.insert("risk".to_string(), to_serde(&risk.to_dict()));
        return Ok(Output::ok(envelope_line(
            "file",
            echo,
            results,
            budget.tokens,
            truncated,
            extra,
        )));
    }
    Ok(Output::ok(emit_memory_file_text(
        &risk.path, &risk, &results,
    )))
}

/// `cli._run_file_report` — the three file-scoped calls on one connection.
fn file_report(
    conn: &rusqlite::Connection,
    path: &str,
    options: &MemoryOptions,
) -> Result<(Vec<SessionMatch>, Vec<SessionMatch>, RiskSummary)> {
    let failure_modes = queries::find_failure_modes_for_file(
        conn,
        path,
        options.since.as_deref(),
        options.limit,
        stax_core::queries::outcome::DEFAULT_MIN_OUTCOME_CONFIDENCE,
    )?;
    let touching = queries::find_sessions_touching_file(conn, path, options.limit)?;
    let risk = queries::file_risk_summary(conn, path, options.since.as_deref(), 5)?;
    Ok((failure_modes, touching, risk))
}

/// `cli._memory_file_results` — merge, dedup, cap, pack; tag each row's `kind`.
fn file_results(
    failure_modes: &[SessionMatch],
    touching: &[SessionMatch],
    limit: i64,
    budget: &Budget,
) -> (Vec<Value>, bool) {
    let mut union: Vec<(SessionMatch, &'static str)> = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    for (rows, kind) in [(failure_modes, "failure_mode"), (touching, "touched")] {
        for row in rows {
            if seen.contains(&row.session_id) {
                continue;
            }
            seen.push(row.session_id.clone());
            union.push((row.clone(), kind));
        }
    }
    if limit > 0 {
        union.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
    }
    let kinds: Vec<&'static str> = union.iter().map(|(_, kind)| *kind).collect();
    let matches: Vec<SessionMatch> = union.into_iter().map(|(row, _)| row).collect();
    // `rank_fn=None`: the outcome queries do not expose the discovery ranker, so
    // the recency order the SQL produced is kept and only the budget applies.
    let (kept, dropped, _used) =
        stax_core::queries::rank::pack_within_budget(matches, budget.tokens, None);
    let mut results: Vec<Value> = Vec::with_capacity(kept.len());
    for row in &kept {
        let mut value = to_serde(&row.to_dict());
        let kind = seen
            .iter()
            .position(|session_id| *session_id == row.session_id)
            .and_then(|index| kinds.get(index).copied())
            .unwrap_or("touched");
        if let Value::Object(map) = &mut value {
            map.insert("kind".to_string(), Value::String(kind.to_string()));
        }
        results.push(value);
    }
    (results, dropped > 0)
}

// ── memory worked ────────────────────────────────────────────────────────────

fn run_worked(
    conn: &rusqlite::Connection,
    action: &str,
    options: &MemoryOptions,
    env: &MemoryEnv,
) -> Result<Output> {
    let json_mode = options.json_mode();
    let budget = env.budget(options.context_budget);
    let mut echo = query_echo(&[("action", Value::String(action.to_string()))], options);

    if !search_has_intent(action) {
        return Ok(memory_fail(
            "worked",
            &echo,
            NO_SEARCH_INTENT,
            json_mode,
            "memory worked [OPTIONS] ACTION",
        ));
    }
    let slug = match &options.project {
        Some(slug) => Some(slug.clone()),
        None => queries::detect_cwd_project_slug(conn, &paths::path_to_string(&env.cwd)),
    };
    let matches = match queries::find_sessions_where_action_worked(
        conn,
        action,
        slug.as_deref(),
        options.since.as_deref(),
        options.limit,
        stax_core::queries::outcome::DEFAULT_MIN_OUTCOME_CONFIDENCE,
    ) {
        Ok(matches) => matches,
        Err(error) => {
            return Ok(memory_fail(
                "worked",
                &echo,
                &error.to_string(),
                json_mode,
                "memory worked [OPTIONS] ACTION",
            ));
        }
    };
    set_project(&mut echo, slug.as_deref());

    // No native budget path on this query — pack the recency-ordered matches
    // here so `--context-budget` still applies (`rank_fn=None`).
    let (kept, dropped, _used) =
        stax_core::queries::rank::pack_within_budget(matches, budget.tokens, None);

    if json_mode {
        return Ok(Output::ok(envelope_line(
            "worked",
            echo,
            rows(&kept),
            budget.tokens,
            dropped > 0,
            Map::new(),
        )));
    }
    // The text branch is handed a bare list, so it never prints the truncation
    // footer even when the budget dropped rows — the reference's wart, kept.
    Ok(Output::ok(emit_sessions(
        &kept,
        false,
        0,
        &format!("Sessions where {} worked", paths::py_repr(action)),
        false,
    )))
}

// ── memory sessions ──────────────────────────────────────────────────────────

fn run_sessions(
    conn: &rusqlite::Connection,
    path: Option<&str>,
    options: &MemoryOptions,
    env: &MemoryEnv,
) -> Result<Output> {
    let json_mode = options.json_mode();
    let budget = env.budget(options.context_budget);

    // An explicit --project decodes to a path and overrides PATH; else the PATH
    // argument, else the cwd.
    let target = match (&options.project, path) {
        (Some(project), _) => {
            let decoded = paths::decode_slug_to_path(project);
            if decoded.is_empty() {
                paths::path_to_string(&env.cwd)
            } else {
                decoded
            }
        }
        (None, Some(path)) => path.to_string(),
        (None, None) => paths::path_to_string(&env.cwd),
    };
    let target_path = paths::purepath_str(&paths::expanduser(
        &paths::purepath_str(&target),
        env.home.as_deref(),
    ));
    let as_file = Path::new(&target_path).is_file();

    let mut echo = query_echo(&[("path", Value::String(target_path.clone()))], options);
    echo.insert(
        "scope".to_string(),
        Value::String(if as_file { "file" } else { "path" }.to_string()),
    );

    let result: Result<BudgetedResult> = if as_file {
        queries::find_sessions_touching_file_budgeted(conn, &target_path, options.limit, &budget)
    } else {
        queries::find_sessions_in_path(
            conn,
            &target_path,
            options.since.as_deref(),
            options.limit,
            None,
            &budget,
        )
    };
    let result = match result {
        Ok(result) => result,
        Err(error) => {
            return Ok(memory_fail(
                "sessions",
                &echo,
                &error.to_string(),
                json_mode,
                "memory sessions [OPTIONS] [PATH]",
            ));
        }
    };

    if json_mode {
        return Ok(Output::ok(envelope_line(
            "sessions",
            echo,
            rows(&result.sessions),
            budget.tokens,
            result.truncated,
            Map::new(),
        )));
    }
    let title = if as_file {
        format!("Sessions touching {target_path}")
    } else {
        format!("Sessions in path {target_path}")
    };
    Ok(Output::ok(emit_sessions(
        &result.sessions,
        result.truncated,
        result.more_available,
        &title,
        false,
    )))
}

// ── shared plumbing ──────────────────────────────────────────────────────────

/// The message `_require_search_intent` raises.
const NO_SEARCH_INTENT: &str =
    "query has no searchable terms — provide at least one word to search for";

/// `search_service.search_has_intent` — any `\w` character.
#[must_use]
pub fn search_has_intent(query: &str) -> bool {
    query.chars().any(|ch| ch.is_alphanumeric() || ch == '_')
}

/// The `q` dict each command echoes back, in the reference's key order.
fn query_echo(leading: &[(&str, Value)], options: &MemoryOptions) -> Map<String, Value> {
    let mut echo = Map::new();
    for (key, value) in leading {
        echo.insert((*key).to_string(), value.clone());
    }
    echo.insert(
        "project".to_string(),
        options
            .project
            .as_ref()
            .map_or(Value::Null, |slug| Value::String(slug.clone())),
    );
    echo.insert(
        "since".to_string(),
        options
            .since
            .as_ref()
            .map_or(Value::Null, |since| Value::String(since.clone())),
    );
    echo.insert("limit".to_string(), Value::from(options.limit));
    echo
}

/// `q["project"] = slug` — the resolved scope replaces the raw flag.
fn set_project(echo: &mut Map<String, Value>, slug: Option<&str>) {
    let value = slug.map_or(Value::Null, |slug| Value::String(slug.to_string()));
    if let Some(slot) = echo.get_mut("project") {
        *slot = value;
    }
}

/// `[m.to_dict() for m in …]`.
fn rows(sessions: &[SessionMatch]) -> Vec<Value> {
    sessions
        .iter()
        .map(|row| to_serde(&row.to_dict()))
        .collect()
}

/// Bridge the store-side value model to the envelope crate's.
///
/// `stax-core` cannot depend on `serde_json` (it is the bedrock crate and the
/// contract lives one layer up), so the two models meet here. `preserve_order`
/// is on, so object key order survives.
fn to_serde(value: &pyjson::Value) -> Value {
    match value {
        pyjson::Value::Null => Value::Null,
        pyjson::Value::Bool(flag) => Value::Bool(*flag),
        pyjson::Value::Int(number) => Value::from(*number),
        pyjson::Value::Float(number) => {
            serde_json::Number::from_f64(*number).map_or(Value::Null, Value::Number)
        }
        pyjson::Value::Str(text) => Value::String(text.clone()),
        pyjson::Value::Array(items) => Value::Array(items.iter().map(to_serde).collect()),
        pyjson::Value::Object(entries) => Value::Object(
            entries
                .iter()
                .map(|(key, item)| (key.clone(), to_serde(item)))
                .collect(),
        ),
    }
}

/// Build + render the success envelope, with the newline `click.echo` adds.
fn envelope_line(
    command: &str,
    query: Map<String, Value>,
    results: Vec<Value>,
    budget: i64,
    truncated: bool,
    extra: Map<String, Value>,
) -> String {
    let envelope = build_envelope(
        MemoryCommand::from(command),
        query,
        results,
        budget,
        truncated,
        extra,
    );
    format!("{}\n", render(&envelope))
}

/// `cli._memory_fail` — the error envelope, or Click's parameter error.
fn memory_fail(
    command: &str,
    query: &Map<String, Value>,
    error: &str,
    json_mode: bool,
    usage: &str,
) -> Output {
    if json_mode {
        let envelope = build_error_envelope(MemoryCommand::from(command), query.clone(), error);
        return Output {
            stdout: format!("{}\n", render(&envelope)),
            stderr: String::new(),
            code: 1,
        };
    }
    // Click's `BadParameter(param_hint="--since")`, with our program name in
    // the usage line — the one text-mode byte difference, and unavoidable:
    // the binary is not called `stackunderflow`.
    Output {
        stdout: String::new(),
        stderr: format!(
            "Usage: stax-rs {usage}\nTry 'stax-rs {} --help' for help.\n\nError: Invalid value for --since: {error}\n",
            usage
                .split_whitespace()
                .take(2)
                .collect::<Vec<_>>()
                .join(" ")
        ),
        code: 2,
    }
}

// ── text rendering ───────────────────────────────────────────────────────────

/// `cli._emit_sessions` in text mode.
#[must_use]
pub fn emit_sessions(
    sessions: &[SessionMatch],
    truncated: bool,
    more_available: usize,
    title: &str,
    show_snippet: bool,
) -> String {
    let mut out = String::new();
    if sessions.is_empty() {
        out.push_str(&format!("{title}: no matching sessions.\n"));
        if truncated {
            out.push_str(&truncation_footer(more_available));
        }
        return out;
    }
    out.push_str(&format!("{title}  ({} session(s))\n\n", sessions.len()));
    for row in sessions {
        out.push_str(&format!(
            "  [{}] {}…  {}  msgs={}  ${:.4}\n",
            row.provider,
            clip(&row.session_id, 12),
            if row.last_ts.is_empty() {
                "(no ts)".to_string()
            } else {
                clip(&row.last_ts, 19)
            },
            row.message_count,
            row.cost_usd,
        ));
        out.push_str(&format!(
            "      {}  {}\n",
            row.project_slug, row.project_path
        ));
        if let Some(fields) = &row.outcome
            && !fields.outcome.is_empty()
        {
            out.push_str(&format!(
                "      → {}: {}\n",
                fields.outcome,
                ellipsize(&fields.outcome_evidence, 200)
            ));
        }
        if show_snippet
            && let Some(snippet) = &row.snippet
            && !snippet.is_empty()
        {
            out.push_str(&format!("      … {}\n", ellipsize(snippet, 200)));
        }
        out.push('\n');
    }
    if truncated {
        out.push_str(&truncation_footer(more_available));
    }
    out
}

/// `cli._truncation_footer`.
fn truncation_footer(more_available: usize) -> String {
    let noun = if more_available == 1 {
        "session"
    } else {
        "sessions"
    };
    format!(
        "... ({more_available} more {noun} matched but truncated to fit context budget; \
         raise --limit or --context-budget to see more)\n"
    )
}

/// `cli._emit_memory_file_text`.
#[must_use]
pub fn emit_memory_file_text(path: &str, risk: &RiskSummary, results: &[Value]) -> String {
    let mut out = String::new();
    out.push_str(&format!("What I know about {path}\n\n"));
    out.push_str("  risk:\n");
    out.push_str(&format!(
        "    sessions touching the file: {}\n",
        risk.total_sessions
    ));
    out.push_str(&format!(
        "    reverted: {}   failed: {}   worked: {}\n\n",
        risk.reverted, risk.failed, risk.worked
    ));
    if results.is_empty() {
        out.push_str("  no failure modes or touching sessions on record.\n");
        return out;
    }
    let is_kind = |row: &&Value, kind: &str| row.get("kind").and_then(Value::as_str) == Some(kind);
    let fails: Vec<&Value> = results
        .iter()
        .filter(|row| is_kind(row, "failure_mode"))
        .collect();
    let touched: Vec<&Value> = results
        .iter()
        .filter(|row| is_kind(row, "touched"))
        .collect();
    if !fails.is_empty() {
        out.push_str(&format!("  failure modes ({}):\n", fails.len()));
        for row in &fails {
            out.push_str(&format!(
                "    [{}] {}…  {}\n",
                text_of(row, "provider"),
                clip(&text_of(row, "session_id"), 12),
                clip(&text_of(row, "last_ts"), 19),
            ));
            out.push_str(&format!(
                "      → {}: {}\n",
                row.get("outcome").and_then(Value::as_str).unwrap_or("?"),
                ellipsize(&text_of(row, "outcome_evidence"), 160),
            ));
        }
        out.push('\n');
    }
    if !touched.is_empty() {
        out.push_str(&format!(
            "  other sessions touching the file ({}):\n",
            touched.len()
        ));
        for row in &touched {
            out.push_str(&format!(
                "    [{}] {}…  {}  msgs={}\n",
                text_of(row, "provider"),
                clip(&text_of(row, "session_id"), 12),
                clip(&text_of(row, "last_ts"), 19),
                row.get("message_count")
                    .and_then(Value::as_i64)
                    .unwrap_or(0),
            ));
        }
    }
    out
}

/// `d.get(key) or ""` for a string field.
fn text_of(row: &Value, key: &str) -> String {
    row.get(key)
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

/// `text[:width]` in Python's character units.
fn clip(text: &str, width: usize) -> String {
    text.chars().take(width).collect()
}

/// `text[:limit - 3] + "…"` when longer than `limit` — the reference's shape.
fn ellipsize(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let kept: String = text.chars().take(limit - 3).collect();
    format!("{kept}…")
}

#[cfg(test)]
mod tests {
    use stax_core::queries::OutcomeFields;

    use super::*;

    fn options(json: bool) -> MemoryOptions {
        MemoryOptions {
            format: Format::Text,
            as_json: json,
            project: None,
            since: None,
            limit: 20,
            context_budget: None,
        }
    }

    fn row(session_id: &str, cost: f64) -> SessionMatch {
        SessionMatch {
            session_id: session_id.into(),
            project_slug: "-home-dev-alpha".into(),
            project_path: "/home/dev/alpha".into(),
            provider: "claude".into(),
            first_ts: "2026-01-02T09:00:00+00:00".into(),
            last_ts: "2026-01-02T10:00:00+00:00".into(),
            message_count: 6,
            cost_usd: cost,
            snippet: None,
            embedding_score: None,
            more_matches_in_session: None,
            outcome: None,
        }
    }

    #[test]
    fn the_session_list_is_rendered_byte_for_byte() {
        let mut first = row("aaaaaaaa-1111-4111-8111-111111111111", 1.25);
        first.snippet = Some("we should cache the watermark lookup".into());
        let rendered = emit_sessions(
            &[first],
            false,
            0,
            "Past decisions matching 'watermark'",
            true,
        );
        assert_eq!(
            rendered,
            "Past decisions matching 'watermark'  (1 session(s))\n\
             \n\
             \u{20}\u{20}[claude] aaaaaaaa-111…  2026-01-02T10:00:00  msgs=6  $1.2500\n\
             \u{20}     -home-dev-alpha  /home/dev/alpha\n\
             \u{20}     … we should cache the watermark lookup\n\
             \n"
        );
    }

    #[test]
    fn an_outcome_row_carries_its_evidence_line() {
        let mut only = row("bbbbbbbb-2222-4222-8222-222222222222", 0.0);
        only.outcome = Some(OutcomeFields {
            outcome: "worked".into(),
            outcome_evidence: "user wrote: 'that worked, thanks'".into(),
            outcome_msg_id: 7,
            outcome_confidence: 0.8,
        });
        let rendered = emit_sessions(&[only], false, 0, "Sessions where 'cache' worked", false);
        assert!(rendered.contains("      → worked: user wrote: 'that worked, thanks'\n"));
        assert!(rendered.contains("  $0.0000\n"));
    }

    #[test]
    fn an_empty_result_says_so_on_one_line() {
        assert_eq!(
            emit_sessions(&[], false, 0, "Sessions in path /tmp", false),
            "Sessions in path /tmp: no matching sessions.\n"
        );
        assert_eq!(
            emit_sessions(&[], true, 1, "Sessions in path /tmp", false),
            "Sessions in path /tmp: no matching sessions.\n... (1 more session matched but \
             truncated to fit context budget; raise --limit or --context-budget to see more)\n"
        );
        assert!(
            emit_sessions(&[], true, 4, "T", false).contains("(4 more sessions matched"),
            "the noun agrees with the count"
        );
    }

    #[test]
    fn missing_timestamps_print_the_no_ts_marker() {
        let mut undated = row("cccccccc-3333-4333-8333-333333333333", 0.0);
        undated.last_ts = String::new();
        assert!(emit_sessions(&[undated], false, 0, "T", false).contains("  (no ts)  "));
    }

    #[test]
    fn long_evidence_and_snippets_are_clipped_the_way_python_clips_them() {
        let long = "x".repeat(300);
        assert_eq!(ellipsize(&long, 200).chars().count(), 198);
        assert!(ellipsize(&long, 200).ends_with('…'));
        assert_eq!(ellipsize("short", 200), "short");
    }

    #[test]
    fn json_mode_is_the_shared_envelope_contract() {
        let envelope = envelope_line(
            "sessions",
            query_echo(&[("path", Value::String("/tmp".into()))], &options(true)),
            vec![],
            2000,
            false,
            Map::new(),
        );
        assert!(envelope.ends_with('\n'));
        assert_eq!(
            envelope,
            "{\n  \"schema\": \"stackunderflow.memory/1\",\n  \"command\": \"sessions\",\n  \
             \"query\": {\n    \"path\": \"/tmp\",\n    \"project\": null,\n    \
             \"since\": null,\n    \"limit\": 20\n  },\n  \"results\": [],\n  \
             \"result_count\": 0,\n  \"token_estimate\": 1,\n  \"budget\": 2000,\n  \
             \"truncated\": false\n}\n"
        );
    }

    #[test]
    fn an_intentless_query_is_an_error_envelope_and_exit_one() {
        let failure = memory_fail(
            "decisions",
            &query_echo(&[("text", Value::String(String::new()))], &options(true)),
            NO_SEARCH_INTENT,
            true,
            "memory decisions [OPTIONS] QUERY",
        );
        assert_eq!(failure.code, 1);
        assert!(failure.stderr.is_empty());
        // `ensure_ascii=True`, so the em dash in the message is escaped on the
        // wire exactly as CPython escapes it.
        assert_eq!(
            failure.stdout,
            "{\n  \"schema\": \"stackunderflow.memory/1\",\n  \"command\": \"decisions\",\n  \
             \"query\": {\n    \"text\": \"\",\n    \"project\": null,\n    \"since\": null,\n    \
             \"limit\": 20\n  },\n  \"error\": \"query has no searchable terms \\u2014 provide at \
             least one word to search for\"\n}\n"
        );
    }

    #[test]
    fn a_text_mode_failure_is_a_parameter_error_on_stderr_with_exit_two() {
        let failure = memory_fail(
            "sessions",
            &Map::new(),
            "Invalid since value 'x'",
            false,
            "memory sessions [OPTIONS] [PATH]",
        );
        assert_eq!(failure.code, 2);
        assert!(failure.stdout.is_empty());
        assert!(
            failure
                .stderr
                .contains("Error: Invalid value for --since: Invalid since value 'x'")
        );
    }

    #[test]
    fn search_intent_matches_the_python_gate() {
        assert!(search_has_intent("cache"));
        assert!(search_has_intent("  a  "));
        assert!(search_has_intent("_"));
        assert!(search_has_intent("café"));
        assert!(!search_has_intent(""));
        assert!(!search_has_intent("   "));
        assert!(!search_has_intent("!!!"));
        assert!(!search_has_intent("***"));
    }

    #[test]
    fn the_json_flag_is_a_shortcut_for_format_json() {
        assert!(options(true).json_mode());
        let mut explicit = options(false);
        assert!(!explicit.json_mode());
        explicit.format = Format::Json;
        assert!(explicit.json_mode());
    }

    #[test]
    fn the_budget_default_walks_env_then_file_then_two_thousand() {
        let config = pyjson::loads(r#"{"discovery_budget_tokens": 750}"#);
        assert_eq!(resolve_budget_default(Some("512"), config.as_ref()), 512);
        assert_eq!(resolve_budget_default(None, config.as_ref()), 750);
        assert_eq!(resolve_budget_default(None, None), 2000);
        // `int(raw)` raising falls back to the default, as `_Opt._cast` does.
        assert_eq!(resolve_budget_default(Some("nope"), None), 2000);
    }

    #[test]
    fn rank_weights_walk_the_same_chain() {
        let config = pyjson::loads(r#"{"discovery_rank_weights": "0.1,0.2,0.7"}"#);
        assert_eq!(resolve_weights(Some("0.6,0.2,0.2"), None), (0.6, 0.2, 0.2));
        assert_eq!(resolve_weights(None, config.as_ref()), (0.1, 0.2, 0.7));
        assert_eq!(resolve_weights(None, None), (0.5, 0.2, 0.3));
    }

    #[test]
    fn the_file_report_text_matches_the_reference_layout() {
        let risk = RiskSummary {
            path: "/home/dev/alpha/main.py".into(),
            since: None,
            total_sessions: 3,
            reverted: 1,
            failed: 2,
            worked: 0,
            recent_session_ids: vec!["aaaa".into()],
        };
        let mut failed = row("aaaaaaaa-1111-4111-8111-111111111111", 0.0);
        failed.outcome = Some(OutcomeFields {
            outcome: "failed".into(),
            outcome_evidence: "user wrote: 'that broke the build'".into(),
            outcome_msg_id: 3,
            outcome_confidence: 0.8,
        });
        let touched = row("bbbbbbbb-2222-4222-8222-222222222222", 0.0);
        let (results, truncated) = file_results(
            std::slice::from_ref(&failed),
            std::slice::from_ref(&touched),
            20,
            &Budget::at(0, (0.5, 0.2, 0.3), 1_785_456_000.0),
        );
        assert!(!truncated);
        assert_eq!(results.len(), 2);
        assert_eq!(
            emit_memory_file_text("/home/dev/alpha/main.py", &risk, &results),
            "What I know about /home/dev/alpha/main.py\n\
             \n\
             \u{20} risk:\n\
             \u{20}   sessions touching the file: 3\n\
             \u{20}   reverted: 1   failed: 2   worked: 0\n\
             \n\
             \u{20} failure modes (1):\n\
             \u{20}   [claude] aaaaaaaa-111…  2026-01-02T10:00:00\n\
             \u{20}     → failed: user wrote: 'that broke the build'\n\
             \n\
             \u{20} other sessions touching the file (1):\n\
             \u{20}   [claude] bbbbbbbb-222…  2026-01-02T10:00:00  msgs=6\n"
        );
    }

    #[test]
    fn a_file_report_with_no_history_says_so() {
        let risk = RiskSummary {
            path: "/tmp/x.py".into(),
            since: None,
            total_sessions: 0,
            reverted: 0,
            failed: 0,
            worked: 0,
            recent_session_ids: vec![],
        };
        assert!(
            emit_memory_file_text("/tmp/x.py", &risk, &[])
                .ends_with("  no failure modes or touching sessions on record.\n")
        );
    }

    #[test]
    fn failure_mode_rows_lead_and_duplicates_collapse() {
        let mut failed = row("shared", 0.0);
        failed.outcome = Some(OutcomeFields {
            outcome: "failed".into(),
            outcome_evidence: "user wrote: 'no'".into(),
            outcome_msg_id: 1,
            outcome_confidence: 0.8,
        });
        let duplicate = row("shared", 0.0);
        let (results, _) = file_results(
            std::slice::from_ref(&failed),
            &[duplicate, row("other", 0.0)],
            20,
            &Budget::at(0, (0.5, 0.2, 0.3), 1_785_456_000.0),
        );
        assert_eq!(results.len(), 2, "the duplicate session id collapses");
        assert_eq!(
            results[0].get("kind").and_then(Value::as_str),
            Some("failure_mode")
        );
        assert_eq!(
            results[1].get("kind").and_then(Value::as_str),
            Some("touched")
        );
        // `kind` is appended last, after the outcome fields.
        let keys: Vec<&str> = results[0]
            .as_object()
            .expect("an object")
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(keys.last(), Some(&"kind"));
    }
}
