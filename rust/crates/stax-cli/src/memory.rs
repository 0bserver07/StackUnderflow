//! `stax memory {sessions,decisions,worked,file}` — the wave-1 read path.
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
//!   every `…` (U+2026) is the reference's. `stax memory sessions` diffs
//!   clean against `stax memory sessions` on the same store and cwd.
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
    self, BudgetedResult, RiskSummary, SessionMatch, ValueError, paths, pyint::PyInt, pyjson,
    rank::Budget,
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

/// `click.option(type=int)` — CPython's `int()`, not `str::parse`.
///
/// Click hands the raw token to `int()`, which strips surrounding whitespace,
/// takes a leading `+`, allows `_` between digits, reads decimal digits from any
/// script, and has no width bound. `--limit ' 5'`, `--limit 1_000`,
/// `--limit ٧` and `--limit 99999999999999999999` are all exit-0 invocations on
/// the Python side; each was an exit-2 parse rejection here.
///
/// The error text is clap's to render (D-2's precedent: parser-owned messages
/// differ, exit code and stdout do not) — Click says
/// `Invalid value for '--limit': '-x' is not a valid integer.`
pub fn py_int(raw: &str) -> Result<PyInt, String> {
    PyInt::parse(raw).ok_or_else(|| "is not a valid integer".to_string())
}

/// The six options every `memory` subcommand shares (`cli._memory_options`).
///
/// Every one carries a self-`overrides_with`: Click's parser keeps the **last**
/// occurrence of a repeated option (`--limit 3 --limit 5` is 5, `--format json
/// --format text` is text, and a repeated `--json` is simply true), where clap's
/// default is to reject the repeat with exit 2. Measured across all six against
/// Click 8.4.2, not assumed.
#[derive(Debug, Clone, Args)]
pub struct MemoryOptions {
    /// Output format. 'json' emits the stable agent-output envelope.
    #[arg(
        long = "format",
        value_name = "FMT",
        default_value = "text",
        overrides_with = "format"
    )]
    pub format: Format,
    /// Shortcut for --format json.
    #[arg(long = "json", overrides_with = "as_json")]
    pub as_json: bool,
    /// Project slug to scope to. Default: the current directory's project,
    /// when StackUnderflow recognises it.
    ///
    /// `allow_hyphen_values` because every slug starts with `-`
    /// (`-Users-you-dev-proj`): without it clap reads the value as a flag and
    /// exits 2 where Click happily scopes the query. Click's parser simply pops
    /// the next token whatever it looks like, so this is the faithful rule —
    /// `--project --json` really does scope to a project named `--json`.
    #[arg(long, allow_hyphen_values = true, overrides_with = "project")]
    pub project: Option<String>,
    /// Time lower bound: '7d', '1w', '1m', '24h', or an ISO date/datetime.
    #[arg(long, allow_hyphen_values = true, overrides_with = "since")]
    pub since: Option<String>,
    /// Hard cap on the number of results.
    ///
    /// `allow_hyphen_values` for the same reason `--project` needs it: Click
    /// accepts `--limit -1` (a negative cap means "no cap"), and without it
    /// clap reads the `-1` as an unknown flag and exits 2. The `--limit=-1`
    /// form already agreed; the space-separated one did not.
    #[arg(
        long,
        default_value_t = PyInt::from(20),
        value_parser = py_int,
        allow_hyphen_values = true,
        overrides_with = "limit"
    )]
    pub limit: PyInt,
    /// Token budget for the output. Default:
    /// STACKUNDERFLOW_DISCOVERY_BUDGET_TOKENS or 2000. Pass 0 to disable.
    ///
    /// `allow_hyphen_values` as on `--limit` — `--context-budget -5` is a
    /// disabled budget on the Python side, not a parse error.
    #[arg(
        long = "context-budget",
        value_name = "TOKENS",
        value_parser = py_int,
        allow_hyphen_values = true,
        overrides_with = "context_budget"
    )]
    pub context_budget: Option<PyInt>,
}

impl MemoryOptions {
    /// `cli._memory_format` — `--json` wins when both are given.
    #[must_use]
    pub fn json_mode(&self) -> bool {
        self.as_json || self.format == Format::Json
    }

    /// `--limit` as the cap the Python-side comparisons use.
    ///
    /// Saturating is exact here: no result set has 2⁶³ rows. The one place the
    /// full-width value still matters is a `LIMIT ?` bind, which takes
    /// [`queries::Limit`] instead and reproduces `sqlite3`'s `OverflowError`.
    #[must_use]
    pub fn limit_i64(&self) -> i64 {
        self.limit.saturating_i64()
    }
}

/// `stax memory` — ask the local store what past sessions already know.
#[derive(Debug, Args)]
pub struct MemoryArgs {
    /// Run this query on a registered remote's dataset instead of the local
    /// store (agent-remotes Phase 1; see `stax remote ls`). Global so it can
    /// sit after the subverb, as in `stax memory sessions --at tmos-hq`.
    #[arg(long = "at", value_name = "REMOTE", global = true)]
    pub at: Option<String>,

    /// Which question to ask.
    #[command(subcommand)]
    pub verb: MemoryVerb,
}

/// The four wave-1 verbs.
#[derive(Debug, Subcommand)]
pub enum MemoryVerb {
    /// Search past decisions — "did I decide something about this before?"
    ///
    /// Substring-searches QUERY across past message content and returns the
    /// matching sessions, newest first, each with a short snippet. Wraps
    /// ``services/discovery.py``'s ``search_past_decisions``.
    Decisions {
        /// Text to substring-search across past message content.
        #[arg(allow_hyphen_values = true)]
        query: String,
        /// The six shared options.
        #[command(flatten)]
        options: MemoryOptions,
    },
    /// Everything known about a file — "what do I know about this file?"
    ///
    /// Merges three file-scoped discovery calls into one report: known
    /// failure modes, every session that touched the file, and a risk
    /// summary (revert / fail / work counts). PATH is resolved against the
    /// current directory, so ``memory file src/foo.py`` works inside a repo.
    File {
        /// File (or directory) to report on.
        #[arg(allow_hyphen_values = true)]
        path: String,
        /// The six shared options.
        #[command(flatten)]
        options: MemoryOptions,
    },
    /// Find where an action worked — "what worked last time I tried this?"
    ///
    /// ACTION is matched as a substring against tool calls and message text.
    /// Returns sessions where ACTION was performed and the next user turn
    /// confirmed success. Wraps ``services/discovery.py``'s
    /// ``find_sessions_where_action_worked``.
    Worked {
        /// Action to match against tool calls and message text.
        #[arg(allow_hyphen_values = true)]
        action: String,
        /// The six shared options.
        #[command(flatten)]
        options: MemoryOptions,
    },
    /// List past sessions that touched here — "which sessions ran here?"
    ///
    /// With no PATH, lists sessions for the current directory's project. Give
    /// a directory to scope to that project tree, or a file to list only the
    /// sessions that touched that file. An explicit ``--project SLUG``
    /// overrides PATH. Wraps ``services/discovery.py``'s
    /// ``find_sessions_in_path`` / ``find_sessions_touching_file``; note the
    /// file form has no time bound, so ``--since`` applies to the path form
    /// only.
    Sessions {
        /// Directory or file. Default: the current directory.
        #[arg(allow_hyphen_values = true)]
        path: Option<String>,
        /// The six shared options.
        #[command(flatten)]
        options: MemoryOptions,
    },
    /// Backfill vector embeddings for your existing indexed messages.
    ///
    /// ``memory ask`` embeds NEW messages as they're ingested; this one-time
    /// backfill embeds everything already in the search index so semantic recall
    /// works over your whole history. Needs a reachable Ollama — cloud
    /// (``STACKUNDERFLOW_OLLAMA_URL`` + ``STACKUNDERFLOW_OLLAMA_API_KEY``) or
    /// local; with neither it explains how to enable one and exits.
    // Appended at the tail of this enum rather than filed next to its siblings:
    // three agents edit `stax-cli` concurrently and the add-only rule applies
    // here for the same reason it applies to `lib.rs`. This note is a `//`
    // comment and not a doc comment ON PURPOSE — clap prints a variant's doc
    // comment as its `--help` body, so a sentence explaining the PORT would
    // become text the reference does not have. `help-tree.sh` reported exactly
    // that on this node's first run; the `Ingest` variant learned it one leg
    // earlier, and the lesson only transferred because the differ ran.
    Embed(crate::memory_embed::EmbedArgs),
    /// Ask a natural-language question of the local store.
    ///
    /// ``ask`` runs a **hybrid** retrieval: a keyword search over past
    /// decisions fused (reciprocal-rank fusion) with a local semantic vector
    /// search. The vector half uses a small local embedding model served by
    /// Ollama; when Ollama is not running it is silently skipped and ``ask``
    /// degrades to the keyword search alone — so the command always works,
    /// and gets sharper (finds sessions you didn't have the exact words for)
    /// when a local Ollama is available. Every result carries its provenance:
    /// session id, date (``last_ts``) and cost (``cost_usd``).
    Ask {
        /// The question. Hybrid retrieval: keyword search fused with a local
        /// semantic vector search, which is skipped when Ollama is not running.
        #[arg(allow_hyphen_values = true)]
        question: String,
        /// The six shared options.
        #[command(flatten)]
        options: MemoryOptions,
    },
}

impl MemoryVerb {
    /// The shared options, whichever verb this is — `None` for `embed`.
    ///
    /// `memory embed` is the one leaf `cli._memory_options` is not applied to:
    /// its only parameter is `--batch`. So the accessor is an `Option` rather
    /// than a lie, and the sole caller (a parser test on `memory decisions`)
    /// unwraps it.
    #[must_use]
    pub fn options(&self) -> Option<&MemoryOptions> {
        match self {
            Self::Decisions { options, .. }
            | Self::File { options, .. }
            | Self::Worked { options, .. }
            | Self::Sessions { options, .. }
            | Self::Ask { options, .. } => Some(options),
            Self::Embed(_) => None,
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
    pub(crate) fn ok(stdout: String) -> Self {
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
    /// `cli._lexical_search_service()` — the FTS5 sidecar beside the store.
    ///
    /// `Some` on every real invocation, because Python's is: `SearchService`
    /// is constructed unconditionally (it even *creates* `search_index.db`),
    /// and the "is there an index?" question is answered later, per query, by
    /// whether the `messages` table has rows. Four of the five verbs consult
    /// it; `memory ask` has its own hybrid retriever ([`crate::ask`]).
    pub index: Option<stax_core::lexical::LexicalIndex>,
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
            index: stax_core::lexical::LexicalIndex::beside_store(&settings::store_path()),
            budget_default: resolve_budget_default(
                stax_core::settings::env_var("DISCOVERY_BUDGET_TOKENS")
                    .ok_or(())
                    .ok()
                    .as_deref(),
                config.as_ref(),
            ),
            weights: resolve_weights(
                stax_core::settings::env_var("DISCOVERY_RANK_WEIGHTS")
                    .ok_or(())
                    .ok()
                    .as_deref(),
                config.as_ref(),
            ),
            now_epoch: stax_core::queries::rank::now_epoch(),
        })
    }

    /// `cli._resolve_context_budget` — the flag, else the setting.
    #[must_use]
    pub fn budget(&self, flag: Option<&PyInt>) -> Budget {
        Budget::at(
            flag.map_or(self.budget_default, PyInt::saturating_i64),
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
///
/// `int(raw)` — not `raw.parse::<i64>()`. That difference was silent and it
/// changed answers: `STACKUNDERFLOW_DISCOVERY_BUDGET_TOKENS=99999999999999999999`
/// is an effectively unbounded budget in Python (20 sessions surfaced on the
/// live store) while an `i64` parse failed and fell back to 2000 (13 sessions),
/// with exit 0 and no warning on either side. Saturating an out-of-range value
/// is exact for a budget, which is only ever compared against a token estimate.
#[must_use]
pub fn resolve_budget_default(env: Option<&str>, config: Option<&pyjson::Value>) -> i64 {
    const DEFAULT: i64 = 2000;
    if let Some(raw) = env {
        return PyInt::parse(raw).map_or(DEFAULT, |value| value.saturating_i64());
    }
    match config.and_then(|config| config.get("discovery_budget_tokens")) {
        Some(pyjson::Value::Int(value)) => *value,
        Some(pyjson::Value::Float(value)) => *value as i64,
        Some(pyjson::Value::Str(value)) => {
            PyInt::parse(value).map_or(DEFAULT, |value| value.saturating_i64())
        }
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
    // `memory embed` is the one verb in this group that never opens `store.db`
    // — `cli.py`'s body reads `search_index.db` and writes `embeddings.db` and
    // imports neither `db` nor `schema`. Routing it through the shared
    // `Store::open_read_only` below would make it fail on a machine that has an
    // index and no store, which the reference happily serves.
    if let MemoryVerb::Embed(embed) = &args.verb {
        let output = crate::memory_embed::run_memory_embed(embed)?;
        print!("{}", output.stdout);
        if !output.stderr.is_empty() {
            eprint!("{}", output.stderr);
        }
        if output.code != 0 {
            std::process::exit(output.code);
        }
        return Ok(());
    }
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
        MemoryVerb::Ask { question, options } => crate::ask::run_ask(conn, question, options, env),
        // Intercepted in `run_memory` before any store handle exists; reached
        // only by a direct caller of `run_verb`, which the tests are. The two
        // `Output` types are the same three fields under two names (this one is
        // the `memory` group's, `click::Output` the wave-8 verbs').
        MemoryVerb::Embed(args) => {
            let out = crate::memory_embed::run_memory_embed(args)?;
            Ok(Output {
                stdout: out.stdout,
                stderr: out.stderr,
                code: out.code,
            })
        }
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
    let budget = env.budget(options.context_budget.as_ref());
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
    // `slug = project; if slug is None and scope_to_cwd: …` — the test is
    // `is None`, so `--project ''` stays the empty string and does NOT fall
    // back to the cwd. The queries then read `''` as "every project"
    // (`queries::project_filter`), which is what makes the two agree.
    let slug = match &options.project {
        Some(slug) => Some(slug.clone()),
        None => queries::detect_cwd_project_slug(conn, &paths::path_to_string(&env.cwd)),
    };
    let result = match queries::search_past_decisions_indexed(
        conn,
        env.index.as_ref(),
        query,
        slug.as_deref(),
        options.since.as_deref(),
        options.limit_i64(),
        &budget,
    ) {
        Ok(result) => result,
        Err(error) => {
            return caught(
                error,
                "decisions",
                &echo,
                json_mode,
                "memory decisions [OPTIONS] QUERY",
            );
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
    let budget = env.budget(options.context_budget.as_ref());
    let mut echo = query_echo(&[("path", Value::String(path.to_string()))], options);

    let report = file_report(conn, path, options, env);
    let (failure_modes, touching, risk) = match report {
        Ok(report) => report,
        Err(error) => {
            return caught(
                error,
                "file",
                &echo,
                json_mode,
                "memory file [OPTIONS] PATH",
            );
        }
    };
    // `risk['path']` is the absolute path discovery actually matched.
    if let Some(slot) = echo.get_mut("path") {
        *slot = Value::String(risk.path.clone());
    }
    let (results, truncated) =
        file_results(&failure_modes, &touching, options.limit_i64(), &budget);

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
///
/// Only the middle one is index-aware, and that asymmetry is the contract:
/// `file_risk_summary` counts touching sessions with its own `LIKE` scan while
/// the session *list* comes from bm25, so on a populated index Python prints
/// "sessions touching the file: 0" directly above a list of twenty of them.
/// Ported bug-for-bug — the report contradicting itself is what the agent
/// reading it sees today.
fn file_report(
    conn: &rusqlite::Connection,
    path: &str,
    options: &MemoryOptions,
    env: &MemoryEnv,
) -> Result<(Vec<SessionMatch>, Vec<SessionMatch>, RiskSummary)> {
    let failure_modes = queries::find_failure_modes_for_file(
        conn,
        path,
        options.since.as_deref(),
        options.limit_i64(),
        stax_core::queries::outcome::DEFAULT_MIN_OUTCOME_CONFIDENCE,
    )?;
    let touching = queries::find_sessions_touching_file_indexed(
        conn,
        env.index.as_ref(),
        path,
        &options.limit,
    )?;
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
    let budget = env.budget(options.context_budget.as_ref());
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
    let matches = match queries::find_sessions_where_action_worked_indexed(
        conn,
        env.index.as_ref(),
        action,
        &queries::ActionWorked::new(
            slug.as_deref(),
            options.since.as_deref(),
            options.limit_i64(),
            stax_core::queries::outcome::DEFAULT_MIN_OUTCOME_CONFIDENCE,
        ),
    ) {
        Ok(matches) => matches,
        Err(error) => {
            return caught(
                error,
                "worked",
                &echo,
                json_mode,
                "memory worked [OPTIONS] ACTION",
            );
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
    let budget = env.budget(options.context_budget.as_ref());

    // An explicit --project decodes to a path and overrides PATH; else the PATH
    // argument, else the cwd. `if project:` here, so `--project ''` falls
    // through to PATH — unlike the `is None` test the other verbs use.
    let target = match (&options.project, path) {
        (Some(project), _) if !project.is_empty() => {
            let decoded = paths::decode_slug_to_path(project);
            if decoded.is_empty() {
                paths::path_to_string(&env.cwd)
            } else {
                decoded
            }
        }
        (_, Some(path)) => path.to_string(),
        (_, None) => paths::path_to_string(&env.cwd),
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

    // `&PyInt` rather than the saturated `i64`: these two are the only wave-1
    // queries that bind `--limit` into SQL, so they are the only two where a
    // limit past 2⁶³ has to raise instead of silently becoming "no cap".
    let result: Result<BudgetedResult> = if as_file {
        queries::find_sessions_touching_file_budgeted_indexed(
            conn,
            env.index.as_ref(),
            &target_path,
            &options.limit,
            &budget,
        )
    } else {
        queries::find_sessions_in_path(
            conn,
            &target_path,
            options.since.as_deref(),
            &options.limit,
            None,
            &budget,
        )
    };
    let result = match result {
        Ok(result) => result,
        Err(error) => {
            return caught(
                error,
                "sessions",
                &echo,
                json_mode,
                "memory sessions [OPTIONS] [PATH]",
            );
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
pub(crate) const NO_SEARCH_INTENT: &str =
    "query has no searchable terms — provide at least one word to search for";

/// `search_service.search_has_intent` — any `\w` character.
#[must_use]
pub fn search_has_intent(query: &str) -> bool {
    query.chars().any(|ch| ch.is_alphanumeric() || ch == '_')
}

/// `except ValueError` — and nothing wider.
///
/// Every `memory` verb's body is wrapped in `try: … except ValueError`, so only
/// a malformed `--since` (or the intent gate) becomes the `--since` parameter
/// error / JSON error envelope. A `sqlite3.DatabaseError` — a corrupt store, a
/// `LIKE` pattern SQLite refuses as too complex — is not caught: Python exits 1
/// with a traceback and an empty stdout. This port used to funnel *every*
/// failure into `Invalid value for --since`, so a corrupt store exited 2
/// blaming an option the caller had never passed. Now the marker decides, and
/// anything else propagates to `main`, which is exit 1 with the message on
/// stderr — Python's exit code and Python's (empty) stdout.
pub(crate) fn caught(
    error: anyhow::Error,
    command: &str,
    query: &Map<String, Value>,
    json_mode: bool,
    usage: &str,
) -> Result<Output> {
    match ValueError::of(&error) {
        Some(message) => Ok(memory_fail(command, query, message, json_mode, usage)),
        None => Err(error),
    }
}

/// `--limit` as the envelope echoes it back.
///
/// Python prints the *normalised* `int`, so `' 5'`, `+5` and `٥` all render as
/// `5`. Exact across `[i64::MIN, u64::MAX]`; past that the echo clamps, because
/// `serde_json::Value` cannot hold a wider integer without the
/// `arbitrary_precision` feature and turning that on would change float
/// rendering for the whole workspace (the golden pack gates those bytes). The
/// clamp is echo-only — the *behaviour* of such a limit is reproduced exactly,
/// including the `OverflowError` at a `LIMIT ?` bind.
fn limit_json(value: &PyInt) -> Value {
    if let Some(exact) = value.fits_i64() {
        return Value::from(exact);
    }
    value
        .fits_u64()
        .map_or_else(|| Value::from(value.saturating_i64()), Value::from)
}

/// The `q` dict each command echoes back, in the reference's key order.
pub(crate) fn query_echo(leading: &[(&str, Value)], options: &MemoryOptions) -> Map<String, Value> {
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
    echo.insert("limit".to_string(), limit_json(&options.limit));
    echo
}

/// `q["project"] = slug` — the resolved scope replaces the raw flag.
pub(crate) fn set_project(echo: &mut Map<String, Value>, slug: Option<&str>) {
    let value = slug.map_or(Value::Null, |slug| Value::String(slug.to_string()));
    if let Some(slot) = echo.get_mut("project") {
        *slot = value;
    }
}

/// `[m.to_dict() for m in …]`.
pub(crate) fn rows(sessions: &[SessionMatch]) -> Vec<Value> {
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
pub(crate) fn to_serde(value: &pyjson::Value) -> Value {
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
pub(crate) fn envelope_line(
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
pub(crate) fn memory_fail(
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
            "Usage: stax {usage}\nTry 'stax {} --help' for help.\n\nError: Invalid value for --since: {error}\n",
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

/// `cli._emit_sessions`'s three text-only display switches.
///
/// The `memory` namespace only ever sets `show_snippet`; the back-compat
/// top-level aliases (`search-past-decisions --use-embeddings`,
/// `find-sessions-where-action-worked -v`) are what the other two exist for.
#[derive(Debug, Clone, Copy, Default)]
pub struct EmitFlags {
    /// Append the matched content excerpt under each row.
    pub show_snippet: bool,
    /// Render the outcome label as `worked (confidence 0.80)`.
    pub show_outcome_confidence: bool,
    /// Append `cos=X.XX` to the headline when the row carries a score.
    pub show_embedding_score: bool,
}

/// `cli._emit_sessions` in text mode.
#[must_use]
pub fn emit_sessions(
    sessions: &[SessionMatch],
    truncated: bool,
    more_available: usize,
    title: &str,
    show_snippet: bool,
) -> String {
    emit_sessions_with(
        sessions,
        truncated,
        more_available,
        title,
        EmitFlags {
            show_snippet,
            ..EmitFlags::default()
        },
    )
}

/// `cli._emit_sessions` in text mode, with every display switch exposed.
#[must_use]
pub fn emit_sessions_with(
    sessions: &[SessionMatch],
    truncated: bool,
    more_available: usize,
    title: &str,
    flags: EmitFlags,
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
        let mut head = format!(
            "  [{}] {}…  {}  msgs={}  ${:.4}",
            row.provider,
            clip(&row.session_id, 12),
            if row.last_ts.is_empty() {
                "(no ts)".to_string()
            } else {
                clip(&row.last_ts, 19)
            },
            row.message_count,
            row.cost_usd,
        );
        // `if score is not None` — a `--use-embeddings` run with no daemon has
        // no scores at all, so the headline is unchanged there.
        if flags.show_embedding_score
            && let Some(score) = row.embedding_score
        {
            head.push_str(&format!("  cos={score:.2}"));
        }
        out.push_str(&head);
        out.push('\n');
        out.push_str(&format!(
            "      {}  {}\n",
            row.project_slug, row.project_path
        ));
        if let Some(fields) = &row.outcome
            && !fields.outcome.is_empty()
        {
            let label = if flags.show_outcome_confidence {
                format!(
                    "{} (confidence {:.2})",
                    fields.outcome, fields.outcome_confidence
                )
            } else {
                fields.outcome.clone()
            };
            out.push_str(&format!(
                "      → {}: {}\n",
                label,
                ellipsize(&fields.outcome_evidence, 200)
            ));
        }
        if flags.show_snippet
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
pub(crate) fn truncation_footer(more_available: usize) -> String {
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
pub(crate) fn clip(text: &str, width: usize) -> String {
    text.chars().take(width).collect()
}

/// `text[:limit - 3] + "…"` when longer than `limit` — the reference's shape.
pub(crate) fn ellipsize(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let kept: String = text.chars().take(limit - 3).collect();
    format!("{kept}…")
}

#[cfg(test)]
mod tests {
    use clap::Parser as _;
    use stax_core::queries::OutcomeFields;

    use super::*;

    fn options(json: bool) -> MemoryOptions {
        MemoryOptions {
            format: Format::Text,
            as_json: json,
            project: None,
            since: None,
            limit: PyInt::from(20),
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

    // ── the argument surface (Click's parser, not clap's defaults) ───────────

    /// Parse a `memory decisions` line and hand back its shared options.
    fn parse(extra: &[&str]) -> Result<MemoryOptions, clap::error::ErrorKind> {
        let mut argv = vec!["stax", "memory", "decisions", "cache"];
        argv.extend_from_slice(extra);
        match crate::Cli::try_parse_from(argv) {
            Ok(cli) => {
                let crate::Command::Memory(args) = cli.command else {
                    panic!("expected memory");
                };
                Ok(args
                    .verb
                    .options()
                    .expect("decisions carries the shared options")
                    .clone())
            }
            Err(error) => Err(error.kind()),
        }
    }

    /// Click hands `--limit` straight to `int()`. Each of these exited 0 on the
    /// Python side and 2 here, on a store the two otherwise agreed about.
    #[test]
    fn limit_accepts_everything_click_accepts() {
        for (argv, expected) in [
            (vec!["--limit", "-1"], "-1"),
            (vec!["--limit=-1"], "-1"),
            (vec!["--limit", " 5"], "5"),
            (vec!["--limit", "  5  "], "5"),
            (vec!["--limit", "+5"], "5"),
            (vec!["--limit", "1_000"], "1000"),
            (vec!["--limit", "\u{667}"], "7"),
            (
                vec!["--limit", "9223372036854775808"],
                "9223372036854775808",
            ),
            (
                vec!["--limit", "99999999999999999999"],
                "99999999999999999999",
            ),
            // Last occurrence wins, as Click's parser does for every option.
            (vec!["--limit", "3", "--limit", "5"], "5"),
        ] {
            let options = parse(&argv).unwrap_or_else(|kind| panic!("{argv:?} → {kind:?}"));
            assert_eq!(options.limit.to_string(), expected, "{argv:?}");
        }
        // …and rejects exactly what `int()` rejects, with Click's exit 2.
        for argv in [
            vec!["--limit", "0x10"],
            vec!["--limit", "5.0"],
            vec!["--limit", "-x"],
            vec!["--limit", "--json"],
            vec!["--limit", ""],
        ] {
            assert!(parse(&argv).is_err(), "{argv:?} must be a parameter error");
        }
    }

    #[test]
    fn every_shared_option_is_last_wins_like_clicks_parser() {
        assert_eq!(
            parse(&["--context-budget", "10", "--context-budget", "3000"])
                .expect("parses")
                .context_budget
                .expect("a budget")
                .to_string(),
            "3000"
        );
        assert_eq!(
            parse(&["--since", "7d", "--since", "30d"])
                .expect("parses")
                .since
                .as_deref(),
            Some("30d")
        );
        assert_eq!(
            parse(&["--project", "a", "--project", "b"])
                .expect("parses")
                .project
                .as_deref(),
            Some("b")
        );
        // `--format json --format text` is text; a repeated `--json` is true.
        assert!(
            !parse(&["--format", "json", "--format", "text"])
                .expect("parses")
                .json_mode()
        );
        assert!(parse(&["--json", "--json"]).expect("parses").json_mode());
        // `--project` still takes a leading-dash slug, because Click's parser
        // pops the next token whatever it looks like.
        assert_eq!(
            parse(&["--project", "-Users-you-dev-proj"])
                .expect("parses")
                .project
                .as_deref(),
            Some("-Users-you-dev-proj")
        );
    }

    /// The echo is Python's *normalised* `int`, and it is exact through
    /// `u64::MAX`. Past that it clamps — `serde_json::Value` has no wider
    /// integer without `arbitrary_precision`, which would change float
    /// rendering for the whole workspace.
    #[test]
    fn the_limit_echo_is_the_normalised_integer() {
        let echo = |raw: &str| {
            let options = parse(&["--limit", raw]).expect("parses");
            limit_json(&options.limit).to_string()
        };
        assert_eq!(echo(" 5"), "5");
        assert_eq!(echo("+5"), "5");
        assert_eq!(echo("1_000"), "1000");
        assert_eq!(echo("\u{667}"), "7");
        assert_eq!(echo("-1"), "-1");
        assert_eq!(echo("9223372036854775807"), "9223372036854775807");
        assert_eq!(echo("9223372036854775808"), "9223372036854775808");
        assert_eq!(
            echo("99999999999999999999"),
            "9223372036854775807",
            "RECORDED CLAMP: the value is unbounded in Python's echo"
        );
    }

    /// `STACKUNDERFLOW_DISCOVERY_BUDGET_TOKENS` goes through `int(raw)`, and an
    /// `i64` parse silently fell back to 2000 for anything wider — 13 sessions
    /// where Python surfaced 20, exit 0 on both sides.
    #[test]
    fn the_budget_env_var_is_int_not_str_parse() {
        assert_eq!(
            resolve_budget_default(Some("99999999999999999999"), None),
            i64::MAX,
            "an unbounded budget, not the 2000 default"
        );
        assert_eq!(resolve_budget_default(Some(" 500 "), None), 500);
        assert_eq!(resolve_budget_default(Some("1_000"), None), 1000);
        assert_eq!(resolve_budget_default(Some("\u{667}00"), None), 700);
        assert_eq!(resolve_budget_default(Some("nope"), None), 2000);
        assert_eq!(resolve_budget_default(Some("5.0"), None), 2000);
    }

    /// `except ValueError` and nothing wider. A corrupt store used to exit 2
    /// with `Invalid value for --since` on a command that never saw `--since`.
    #[test]
    fn only_a_value_error_becomes_the_since_parameter_error() {
        let echo = query_echo(&[("text", Value::String("cache".into()))], &options(false));
        let caught_one = caught(
            anyhow::Error::new(ValueError("Invalid since value 'x'".into())),
            "decisions",
            &echo,
            false,
            "memory decisions [OPTIONS] QUERY",
        )
        .expect("a ValueError is handled, not returned");
        assert_eq!(caught_one.code, 2);
        assert!(caught_one.stderr.contains("Invalid value for --since"));

        let propagated = caught(
            anyhow::anyhow!("database disk image is malformed"),
            "decisions",
            &echo,
            false,
            "memory decisions [OPTIONS] QUERY",
        )
        .expect_err("a store failure propagates to main → exit 1, empty stdout");
        assert_eq!(propagated.to_string(), "database disk image is malformed");

        // The JSON side too: a DatabaseError is not an error *envelope*.
        assert!(
            caught(
                anyhow::anyhow!("LIKE or GLOB pattern too complex"),
                "decisions",
                &echo,
                true,
                "memory decisions [OPTIONS] QUERY",
            )
            .is_err()
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
