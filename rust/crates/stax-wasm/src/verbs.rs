//! The four memory verbs and `store`, assembled for a caller that has no OS.
//!
//! ## What this module is, and what it deliberately is not
//!
//! It is **not** a second implementation of the queries. Every row on this path
//! comes out of [`stax_core::queries`] — the same functions, the same SQL
//! shapes §6b calls load-bearing — and every byte of JSON comes out of
//! [`stax_memory`]'s `staxtrace.memory/1` writer. What is re-expressed
//! here is the ~200 lines of *assembly* that `stax-cli`'s `memory` module
//! performs between the two: the `query` echo, the intent gate, the cwd-slug
//! fallback, the file verb's merge/dedup/cap/pack, and the error envelope.
//!
//! It is re-expressed because `stax-cli` cannot be depended on from wasm32: it
//! links `stax-server` (axum + tokio), `stax-sync` (age) and `stax-hooks`, none
//! of which build for `wasm32-unknown-unknown`. The clean fix is a `stax-verbs`
//! crate that both the CLI and this crate depend on — the same shape as the
//! `stax-reports` extraction `stax-cli`'s own manifest files rather than does.
//! **DIV-333** files it; `rust/wasm-differ.sh` is the anti-drift gate in the
//! meantime, comparing these bytes against the CLI's on the same store.
//!
//! Only the `--json` half is ported. The text renderer (`_emit_sessions`,
//! `_emit_memory_file_text`) is ~300 lines of column arithmetic for a terminal,
//! and a browser renders from the envelope. Recorded as **DIV-334**, not
//! forgotten.
//!
//! ## What the browser cannot know
//!
//! Three inputs the CLI reads from the OS are *parameters* here, because wasm32
//! has no filesystem, no cwd, and no clock:
//!
//! * `now_epoch` — the recency term of the ranker (`Budget::at`). JS passes
//!   `Date.now() / 1000`.
//! * `cwd` — `memory decisions` / `worked` scope to the current directory's
//!   project when `--project` is absent. A page has no cwd; the caller supplies
//!   one (the demo leaves it empty, which means "every project").
//! * `is_file` — `memory sessions` branches on `Path::is_file()`. **DIV-335**:
//!   the browser cannot stat, so the caller declares the scope.

use anyhow::Result;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use stax_core::queries::{
    self, BudgetedResult, RiskSummary, SessionMatch, ValueError, paths, pyjson, rank::Budget,
};
use stax_memory::{MemoryCommand, build_envelope, build_error_envelope, render};

/// `cli._require_search_intent`'s message, character for character.
const NO_SEARCH_INTENT: &str =
    "query has no searchable terms — provide at least one word to search for";

/// Everything the caller supplies in place of an operating system.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Options {
    /// `--project`. `Some("")` is not `None`: the reference tests `is None`, so
    /// an empty slug means "every project" rather than "fall back to the cwd".
    pub project: Option<String>,
    /// `--since`, echoed verbatim and parsed by `pytime::parse_since`.
    pub since: Option<String>,
    /// `--limit`. The CLI's `PyInt` surface (unbounded ints, `٥`, `1_000`) is a
    /// *argv-parsing* concern; a JS caller passes a number.
    pub limit: i64,
    /// `--context-budget`, else [`Options::budget_default`].
    pub context_budget: Option<i64>,
    /// `Settings().discovery_budget_tokens`.
    pub budget_default: i64,
    /// `Settings().discovery_rank_weights` — recency, cost, relevance.
    pub weights: (f64, f64, f64),
    /// `datetime.now(UTC)` as epoch seconds.
    pub now_epoch: f64,
    /// `Path.cwd()`, for the project-slug fallback.
    pub cwd: String,
    /// `Path.home()`, for `~` expansion in `memory sessions`.
    pub home: Option<String>,
    /// `Path(target).is_file()` — see DIV-335.
    pub is_file: bool,
    /// The path the `store` verb prints on its first line.
    pub store_label: String,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            project: None,
            since: None,
            limit: 20,
            context_budget: None,
            budget_default: 2000,
            weights: queries::rank::parse_rank_weights(None),
            now_epoch: 0.0,
            cwd: String::new(),
            home: None,
            is_file: false,
            store_label: "store.db".to_string(),
        }
    }
}

impl Options {
    /// `cli._resolve_context_budget` — the flag, else the setting.
    fn budget(&self) -> Budget {
        Budget::at(
            self.context_budget.unwrap_or(self.budget_default),
            self.weights,
            self.now_epoch,
        )
    }
}

/// One question, in the shape the JS side posts it.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "verb", rename_all = "lowercase", deny_unknown_fields)]
pub enum Request {
    /// `stax memory decisions QUERY --json`.
    Decisions {
        /// The search text.
        query: String,
        /// The shared options.
        #[serde(default)]
        options: Options,
    },
    /// `stax memory file PATH --json`.
    File {
        /// The file to report on.
        path: String,
        /// The shared options.
        #[serde(default)]
        options: Options,
    },
    /// `stax memory worked ACTION --json`.
    Worked {
        /// The action to look for.
        action: String,
        /// The shared options.
        #[serde(default)]
        options: Options,
    },
    /// `stax memory sessions [PATH] --json`.
    Sessions {
        /// The path to scope to; absent means the cwd.
        #[serde(default)]
        path: Option<String>,
        /// The shared options.
        #[serde(default)]
        options: Options,
    },
    /// `stax store` — schema version plus per-object row counts.
    Store {
        /// Only `store_label` is read.
        #[serde(default)]
        options: Options,
    },
}

impl Request {
    /// The shared options, whichever verb this is.
    #[must_use]
    pub fn options(&self) -> &Options {
        match self {
            Self::Decisions { options, .. }
            | Self::File { options, .. }
            | Self::Worked { options, .. }
            | Self::Sessions { options, .. }
            | Self::Store { options } => options,
        }
    }
}

/// What one request produced: the exact bytes the CLI would have printed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Outcome {
    /// stdout, trailing newline included — what the differ compares.
    pub stdout: String,
    /// `0`, or `1` for the JSON error envelope.
    pub code: i32,
}

impl Outcome {
    fn ok(stdout: String) -> Self {
        Self { stdout, code: 0 }
    }
}

/// Run one request against an open, read-only connection.
///
/// # Errors
/// When a query fails for a reason the reference would not have caught — a
/// corrupt store, a `LIKE` pattern SQLite refuses. A malformed `--since` is a
/// `ValueError` and comes back as an error *envelope*, not an `Err`.
pub fn run(conn: &Connection, request: &Request) -> Result<Outcome> {
    // ONE clock in, both consumers fed. Natively `--since`'s window and the
    // ranker's recency term read the same wall clock microseconds apart
    // (`pytime::now_micros` and `rank::now_epoch`, which is that same function
    // divided by a million); on wasm32 the first of those has no clock to read,
    // so `now_epoch` is pushed into it here rather than becoming a second knob
    // a caller could set inconsistently. See `queries::pytime::set_now_micros`.
    #[cfg(target_arch = "wasm32")]
    stax_core::queries::pytime::set_now_micros((request.options().now_epoch * 1_000_000.0) as i64);
    match request {
        Request::Decisions { query, options } => decisions(conn, query, options),
        Request::File { path, options } => file(conn, path, options),
        Request::Worked { action, options } => worked(conn, action, options),
        Request::Sessions { path, options } => sessions(conn, path.as_deref(), options),
        Request::Store { options } => store(conn, options),
    }
}

// ── memory decisions ─────────────────────────────────────────────────────────

fn decisions(conn: &Connection, query: &str, options: &Options) -> Result<Outcome> {
    let budget = options.budget();
    let mut echo = query_echo(&[("text", Value::String(query.to_string()))], options);

    if !search_has_intent(query) {
        return Ok(memory_fail("decisions", &echo, NO_SEARCH_INTENT));
    }
    let slug = resolve_slug(conn, options);
    let result = match queries::search_past_decisions_indexed(
        conn,
        None,
        query,
        slug.as_deref(),
        options.since.as_deref(),
        options.limit,
        &budget,
    ) {
        Ok(result) => result,
        Err(error) => return caught(error, "decisions", &echo),
    };
    set_project(&mut echo, slug.as_deref());
    Ok(Outcome::ok(envelope_line(
        "decisions",
        echo,
        rows(&result.sessions),
        budget.tokens,
        result.truncated,
        Map::new(),
    )))
}

// ── memory file ──────────────────────────────────────────────────────────────

fn file(conn: &Connection, path: &str, options: &Options) -> Result<Outcome> {
    let budget = options.budget();
    let mut echo = query_echo(&[("path", Value::String(path.to_string()))], options);

    let resolved = host_resolve(path, options);
    let (failure_modes, touching, risk) = match file_report(conn, &resolved, options) {
        Ok(report) => report,
        Err(error) => return caught(error, "file", &echo),
    };
    // `risk['path']` is the absolute path discovery actually matched.
    if let Some(slot) = echo.get_mut("path") {
        *slot = Value::String(risk.path.clone());
    }
    let (results, truncated) = file_results(&failure_modes, &touching, options.limit, &budget);

    let mut extra = Map::new();
    extra.insert("risk".to_string(), to_serde(&risk.to_dict()));
    Ok(Outcome::ok(envelope_line(
        "file",
        echo,
        results,
        budget.tokens,
        truncated,
        extra,
    )))
}

/// `cli._run_file_report` — the three file-scoped calls on one connection.
///
/// Only the middle one is index-aware, and the asymmetry is inherited: the
/// count and the list can contradict each other on a populated index. In the
/// browser there is no index at all (there is no `search_index.db` to drop),
/// so all three take the store path.
fn file_report(
    conn: &Connection,
    path: &str,
    options: &Options,
) -> Result<(Vec<SessionMatch>, Vec<SessionMatch>, RiskSummary)> {
    let failure_modes = queries::find_failure_modes_for_file(
        conn,
        path,
        options.since.as_deref(),
        options.limit,
        queries::outcome::DEFAULT_MIN_OUTCOME_CONFIDENCE,
    )?;
    let touching = queries::find_sessions_touching_file_indexed(conn, None, path, options.limit)?;
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
    for (matches, kind) in [(failure_modes, "failure_mode"), (touching, "touched")] {
        for row in matches {
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
    let (kept, dropped, _used) = queries::rank::pack_within_budget(matches, budget.tokens, None);
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

fn worked(conn: &Connection, action: &str, options: &Options) -> Result<Outcome> {
    let budget = options.budget();
    let mut echo = query_echo(&[("action", Value::String(action.to_string()))], options);

    if !search_has_intent(action) {
        return Ok(memory_fail("worked", &echo, NO_SEARCH_INTENT));
    }
    let slug = resolve_slug(conn, options);
    let matches = match queries::find_sessions_where_action_worked_indexed(
        conn,
        None,
        action,
        &queries::ActionWorked::new(
            slug.as_deref(),
            options.since.as_deref(),
            options.limit,
            queries::outcome::DEFAULT_MIN_OUTCOME_CONFIDENCE,
        ),
    ) {
        Ok(matches) => matches,
        Err(error) => return caught(error, "worked", &echo),
    };
    set_project(&mut echo, slug.as_deref());

    // No native budget path on this query — pack the recency-ordered matches
    // here so `--context-budget` still applies (`rank_fn=None`).
    let (kept, dropped, _used) = queries::rank::pack_within_budget(matches, budget.tokens, None);
    Ok(Outcome::ok(envelope_line(
        "worked",
        echo,
        rows(&kept),
        budget.tokens,
        dropped > 0,
        Map::new(),
    )))
}

// ── memory sessions ──────────────────────────────────────────────────────────

fn sessions(conn: &Connection, path: Option<&str>, options: &Options) -> Result<Outcome> {
    let budget = options.budget();

    // An explicit --project decodes to a path and overrides PATH; else the PATH
    // argument, else the cwd. `if project:` here, so `--project ''` falls
    // through to PATH — unlike the `is None` test the other verbs use.
    let target = match (&options.project, path) {
        (Some(project), _) if !project.is_empty() => {
            let decoded = paths::decode_slug_to_path(project);
            if decoded.is_empty() {
                options.cwd.clone()
            } else {
                decoded
            }
        }
        (_, Some(path)) => path.to_string(),
        (_, None) => options.cwd.clone(),
    };
    let home = home_path(options);
    let target_path = paths::purepath_str(&paths::expanduser(
        &paths::purepath_str(&target),
        home.as_deref(),
    ));
    // The echo keeps `target_path` exactly as the reference prints it; only the
    // value handed to the query is pre-resolved (see `host_resolve`).
    let query_path = host_resolve(&target_path, options);
    // DIV-335: `Path::is_file()` on the caller's word, because wasm32 cannot stat.
    let as_file = options.is_file;

    let mut echo = query_echo(&[("path", Value::String(target_path.clone()))], options);
    echo.insert(
        "scope".to_string(),
        Value::String(if as_file { "file" } else { "path" }.to_string()),
    );

    let result: Result<BudgetedResult> = if as_file {
        queries::find_sessions_touching_file_budgeted_indexed(
            conn,
            None,
            &query_path,
            options.limit,
            &budget,
        )
    } else {
        queries::find_sessions_in_path(
            conn,
            &query_path,
            options.since.as_deref(),
            options.limit,
            None,
            &budget,
        )
    };
    let result = match result {
        Ok(result) => result,
        Err(error) => return caught(error, "sessions", &echo),
    };
    Ok(Outcome::ok(envelope_line(
        "sessions",
        echo,
        rows(&result.sessions),
        budget.tokens,
        result.truncated,
        Map::new(),
    )))
}

// ── store ────────────────────────────────────────────────────────────────────

/// One `sqlite_master` object and its `COUNT(*)`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ObjectCount {
    /// The object name, as stored in `sqlite_master`.
    pub name: String,
    /// `"table"` or `"view"` — the view's rows are also counted under the
    /// partitions it selects from, which is why the kind is printed.
    pub kind: String,
    /// Live `COUNT(*)`, not an estimate.
    pub rows: i64,
}

/// `PRAGMA user_version`.
///
/// # Errors
/// When the pragma cannot be read.
pub fn schema_version(conn: &Connection) -> Result<i64> {
    Ok(conn.pragma_query_value(None, "user_version", |row| row.get(0))?)
}

/// Every table and view in `sqlite_master` with its `COUNT(*)`, sorted by name.
///
/// `ORDER BY name` happens in SQL (BINARY collation) so the order is the
/// engine's, not a locale's — `stax_core::store::Store::object_counts` sorts
/// the same way, and this is the same statement against a connection that has
/// no `Store` wrapper (a `Store` demands a path that `exists()`, and the
/// browser's database is a VFS name, not a file).
///
/// # Errors
/// When `sqlite_master` cannot be read, or a `COUNT(*)` fails.
pub fn object_counts(conn: &Connection) -> Result<Vec<ObjectCount>> {
    let mut stmt = conn.prepare(
        "SELECT name, type FROM sqlite_master \
         WHERE type IN ('table', 'view') \
         ORDER BY name",
    )?;
    let objects = stmt
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    objects
        .into_iter()
        .map(|(name, kind)| {
            let quoted = name.replace('"', "\"\"");
            let rows =
                conn.query_row(&format!("SELECT COUNT(*) FROM \"{quoted}\""), [], |row| {
                    row.get(0)
                })?;
            Ok(ObjectCount { name, kind, rows })
        })
        .collect()
}

/// Column widths — `stax-cli`'s `render_store`, so a diff of the two is empty.
const NAME_WIDTH: usize = 40;
const KIND_WIDTH: usize = 6;
const ROWS_WIDTH: usize = 12;

fn store(conn: &Connection, options: &Options) -> Result<Outcome> {
    use std::fmt::Write as _;

    let version = schema_version(conn)?;
    let objects = object_counts(conn)?;
    let mut out = String::with_capacity(96 + objects.len() * (NAME_WIDTH + 24));
    let _ = writeln!(out, "store: {}", options.store_label);
    let _ = writeln!(out, "schema: v{version:03}");
    let _ = writeln!(out, "objects: {}", objects.len());
    out.push('\n');
    let _ = writeln!(
        out,
        "{:<NAME_WIDTH$} {:<KIND_WIDTH$} {:>ROWS_WIDTH$}",
        "NAME", "KIND", "ROWS"
    );
    for object in &objects {
        let _ = writeln!(
            out,
            "{:<NAME_WIDTH$} {:<KIND_WIDTH$} {:>ROWS_WIDTH$}",
            object.name, object.kind, object.rows
        );
    }
    Ok(Outcome::ok(out))
}

// ── shared plumbing ──────────────────────────────────────────────────────────

/// The caller's home directory, as a path.
fn home_path(options: &Options) -> Option<std::path::PathBuf> {
    options.home.as_ref().map(std::path::PathBuf::from)
}

/// Resolve a user-supplied path the way the *process* would have.
///
/// Every file-scoped query calls `paths::resolve_input_path` on its argument,
/// and that function asks the OS for the current directory. On wasm32 there is
/// no current directory, `std::env::current_dir()` fails, and the fallback is
/// `/` — so `memory file python-legacy: cli.py` looked at
/// `/python-legacy: cli.py` and found nothing while the CLI looked under its
/// cwd and found five sessions. That was five of the first differ run's 32
/// cases, and it is the class of divergence worth catching: not a crash, a
/// *changed answer*.
///
/// The fix is to resolve here, against the cwd the caller declared, and hand
/// the queries an absolute path — `resolve_input_path` is idempotent on one, so
/// the native path through this crate is unchanged. What a browser still cannot
/// do is follow a symlink (`canonicalize` needs a filesystem): a caller who
/// names a symlinked path gets the lexical answer. DIV-336.
fn host_resolve(path: &str, options: &Options) -> String {
    paths::resolve_input_path_with(
        path,
        home_path(options).as_deref(),
        std::path::Path::new(&options.cwd),
    )
}

/// `search_service.search_has_intent` — any `\w` character.
#[must_use]
pub fn search_has_intent(query: &str) -> bool {
    query.chars().any(|ch| ch.is_alphanumeric() || ch == '_')
}

/// `slug = project; if slug is None and scope_to_cwd: …` — the test is
/// `is None`, so `--project ''` stays the empty string.
fn resolve_slug(conn: &Connection, options: &Options) -> Option<String> {
    match &options.project {
        Some(slug) => Some(slug.clone()),
        None => queries::detect_cwd_project_slug(conn, &options.cwd),
    }
}

/// The `q` dict each command echoes back, in the reference's key order.
fn query_echo(leading: &[(&str, Value)], options: &Options) -> Map<String, Value> {
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

/// Bridge the store-side value model to the envelope crate's — `stax-cli`'s
/// `to_serde`, kept identical because `preserve_order` makes key order the
/// contract.
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

/// `cli._memory_fail` in `--json` mode — the error envelope and exit 1.
fn memory_fail(command: &str, query: &Map<String, Value>, error: &str) -> Outcome {
    let envelope = build_error_envelope(MemoryCommand::from(command), query.clone(), error);
    Outcome {
        stdout: format!("{}\n", render(&envelope)),
        code: 1,
    }
}

/// `except ValueError` — and nothing wider. A corrupt store propagates.
fn caught(error: anyhow::Error, command: &str, query: &Map<String, Value>) -> Result<Outcome> {
    match ValueError::of(&error) {
        Some(message) => Ok(memory_fail(command, query, message)),
        None => Err(error),
    }
}
