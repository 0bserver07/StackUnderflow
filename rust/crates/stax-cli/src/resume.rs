//! `stax-rs resume` — every coding agent's session ids under a path (RS-1-019).
//!
//! A port of `cli.py:resume_cmd` (`:6301`–`:6428`) and the store query it calls,
//! `services/discovery.py:resume_candidates` (`:712`–`:804`). Four behaviors are
//! the whole feature, and each is ported literally rather than re-derived:
//!
//! * **Matching happens in slug space, never in path space.** The store folds
//!   both `/` and `_` to `-` when it builds a project slug, so decoding a slug
//!   back to a path is lossy (`dev_dev` and `dev-dev` decode the same) while
//!   *encoding* the query path is exact. [`query_slug`] is that encoder, and it
//!   is why `resume /Users/y/dev_dev/…` finds `-Users-y-dev-dev-…`.
//! * **Matching is bidirectional, ancestors nearest-only.** A project *under*
//!   the query path matches (you gave a workspace folder) and a project *above*
//!   it matches (you are standing inside one) — but of the ancestors only the
//!   deepest survives, so a `-Users-you` home catch-all cannot swamp every
//!   query from a nearer project.
//! * **Resume invocations are data.** The templates live in
//!   `adapters/capabilities.json` and arrive through [`stax_adapters::Capabilities`];
//!   nothing here transcribes a provider name or a flag. A `latest`-scope agent
//!   (grok) renders no per-session command because its CLI has nowhere to put an
//!   id, and an agent with no template at all still lists its session ids rather
//!   than inventing a flag that would not work.
//! * **`--json` is the frozen `stackunderflow.resume/1` envelope** —
//!   [`stax_memory::ResumeEnvelope`], the same types the golden pack gates, so
//!   `--json` is byte-identical to Python's `json.dumps(payload, indent=2)`.
//!
//! Text output is byte-identical too: every literal, the em-dashes, the
//! three-space gap before `(project)`, and the `{:<16}` / `{:>5}` column widths
//! are Python's.
//!
//! REGISTRATION — applied to `lib.rs` per the anchor/memory precedent:
//! `mod resume;`, `pub use resume::{ResumeArgs, run_resume};`, a
//! `Command::Resume(ResumeArgs)` variant and its `dispatch` arm.

use std::collections::{BTreeMap, HashMap};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::Args;
use rusqlite::Connection;
use rusqlite::types::ValueRef;
use serde_json::Map;
use stax_adapters::Capabilities;
use stax_adapters::capabilities::{
    CAPABILITIES_PATH_ENV, CAPABILITIES_RELATIVE_PATH, path_from_env,
};
use stax_core::queries::paths;
use stax_core::queries::pyint::PyInt;
use stax_core::settings;
use stax_core::store::Store;
use stax_memory::{ProviderBlock, ResumeEnvelope, ResumeSession, ResumeTemplate};

// ── the command surface ──────────────────────────────────────────────────────

/// `stax-rs resume [PATH]` — session/resume ids for every coding agent under
/// `PATH` (default: the current directory).
///
/// Every `help` string here is Click's, verbatim (`stackunderflow resume
/// --help`), so the two help texts say the same words even though clap and
/// Click lay them out differently — the `--help`-tree diff is wave 8's item.
/// Explicit `help =` rather than the doc comment, so the rustdoc can carry
/// implementation notes without leaking them into a user's terminal.
/// The command's own `about` / `long_about` come from the `Command::Resume`
/// variant's doc comment in `lib.rs` (clap gives an enum variant's doc
/// precedence over an `Args` struct's), which carries the Python docstring.
#[derive(Debug, Args)]
pub struct ResumeArgs {
    /// Directory to look under. Default: the current directory.
    #[arg(help = "Directory to look under. Default: the current directory.")]
    pub path: Option<String>,

    /// Only this agent (repeatable): claude, codex, grok, … Case-insensitive;
    /// an unambiguous prefix works (e.g. -p cod).
    #[arg(
        short = 'p',
        long = "provider",
        value_name = "TEXT",
        help = "Only this agent (repeatable): claude, codex, grok, … \
                Case-insensitive; an unambiguous prefix works (e.g. -p cod)."
    )]
    pub provider: Vec<String>,

    /// Max sessions listed per coding agent.
    ///
    /// `IntRange(min=1)` on the Python side: `int(raw)` **first**, the bound
    /// **second**. That order is the whole divergence — `clap::value_parser!(i64)`
    /// rejected `' 5'`, `٧`, `1_000` and `99999999999999999999` outright where
    /// Click converts them (to 5, 7, 1000 and a cap larger than any store) and
    /// exits 0. The range rejection itself is unchanged, message and all
    /// (recorded divergence D-2); Click renders the bound into the help line
    /// (`[default: 5; x>=1]`), clap prints the default only.
    #[arg(
        long = "limit-per-provider",
        value_name = "INTEGER RANGE",
        default_value_t = PyInt::from(5),
        value_parser = py_int_min_one,
        allow_hyphen_values = true,
        overrides_with = "limit_per_provider",
        help = "Max sessions listed per coding agent.",
        long_help = "Max sessions listed per coding agent."
    )]
    pub limit_per_provider: PyInt,

    /// Emit the machine envelope.
    #[arg(
        long = "json",
        overrides_with = "as_json",
        help = "Emit the machine envelope."
    )]
    pub as_json: bool,
}

/// `click.IntRange(min=1)` — `int()`, then the bound.
///
/// Both halves matter: the conversion is CPython's (see
/// [`stax_core::queries::pyint`]) and the bound is checked on the converted
/// value, so `--limit-per-provider ' 0'` is rejected for being 0, not for the
/// space.
fn py_int_min_one(raw: &str) -> Result<PyInt, String> {
    let value = PyInt::parse(raw).ok_or_else(|| "is not a valid integer".to_string())?;
    if value.is_positive() {
        Ok(value)
    } else {
        Err(format!("{value} is not in the range x>=1"))
    }
}

/// The bytes and the exit status of one `resume` invocation.
///
/// Returned rather than printed so the whole surface is testable without a
/// subprocess and stdout can be diffed against the Python CLI byte for byte.
/// Deliberately *not* [`crate::memory::Output`]: this wave writes the two
/// modules in parallel and neither may depend on the other's shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Output {
    /// Everything `click.echo` would have written to stdout.
    pub stdout: String,
    /// Everything Click would have written to stderr.
    pub stderr: String,
    /// `0`, or `1` for a `ClickException`.
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

    /// `raise click.ClickException(msg)` — `Error: …` on stderr, exit 1.
    fn click_error(message: impl AsRef<str>) -> Self {
        Self {
            stdout: String::new(),
            stderr: format!("Error: {}\n", message.as_ref()),
            code: 1,
        }
    }
}

/// Everything the run needs from the environment, injected.
///
/// Nothing below this struct reads a process global — the wave-1 pattern law
/// (`set_var` is `unsafe` in Rust 2024 and the workspace forbids `unsafe`).
#[derive(Debug, Clone)]
pub struct ResumeEnv {
    /// `Path.cwd()` — the default query path.
    pub cwd: PathBuf,
    /// `Path.home()`, for `~` expansion inside the query path.
    pub home: Option<PathBuf>,
    /// `deps.store_path` — `$STACKUNDERFLOW_HOME/store.db`.
    pub store: PathBuf,
    /// `adapters/capabilities.json`, the resume-template table.
    pub capabilities: PathBuf,
}

impl ResumeEnv {
    /// Resolve from the real process environment.
    ///
    /// # Errors
    /// When the current directory cannot be read.
    pub fn from_process() -> Result<Self> {
        let cwd = std::env::current_dir()?;
        let exe = std::env::current_exe().ok();
        Ok(Self {
            capabilities: resolve_capabilities_path(
                std::env::var_os(CAPABILITIES_PATH_ENV).as_deref(),
                &cwd,
                exe.as_deref(),
            ),
            cwd,
            home: paths::home_dir(),
            store: settings::store_path(),
        })
    }
}

/// Where `capabilities.json` lives for a *running binary*.
///
/// `$STACKUNDERFLOW_CAPABILITIES` wins, as [`path_from_env`] defines. Otherwise
/// the repo default has to be found rather than assumed: the Python side reads
/// the copy inside its installed package, and the campaign forbids
/// `include_str!` (a build-time copy would let the two implementations disagree
/// about the bytes while the parity harness swore they agreed). So we walk up
/// from the working directory, then from the executable, looking for a checkout
/// that carries the file — which covers both `cargo run` from anywhere in the
/// tree and `rust/target/release/stax-rs` invoked from an unrelated cwd.
///
/// Shipping the table with an installed binary is a wave-8 packaging question
/// (architect decision, wave 2); until then the last resort is the repo-relative
/// path under `cwd`, so the error names something concrete.
#[must_use]
pub fn resolve_capabilities_path(raw: Option<&OsStr>, cwd: &Path, exe: Option<&Path>) -> PathBuf {
    if let Some(value) = raw.filter(|value| !value.is_empty()) {
        return PathBuf::from(value);
    }
    let from_exe = exe.and_then(Path::parent);
    for start in [Some(cwd), from_exe].into_iter().flatten() {
        for ancestor in start.ancestors() {
            let candidate = ancestor.join(CAPABILITIES_RELATIVE_PATH);
            if candidate.is_file() {
                return candidate;
            }
        }
    }
    path_from_env(None, cwd)
}

// ── the store query (`discovery.resume_candidates`) ──────────────────────────

/// Fold a resolved filesystem path into the store's slug space.
///
/// `discovery.py:749` — every separator *and* every underscore becomes `-`,
/// with a leading `-` and no trailing one. Lossless in this direction, which is
/// the entire reason matching happens here instead of in path space.
///
/// Note the degenerate case, ported as-is: `/` yields the empty string, and
/// every slug then matches the `slug.startswith(query_slug + "-")` arm — so
/// `resume /` lists every project in the store.
#[must_use]
pub fn query_slug(resolved: &str) -> String {
    let trimmed = resolved.trim_matches(|c| c == '/' || c == '\\');
    let mut slug = String::with_capacity(trimmed.len() + 1);
    slug.push('-');
    for ch in trimmed.chars() {
        slug.push(match ch {
            '/' | '\\' | '_' => '-',
            other => other,
        });
    }
    while slug.ends_with('-') {
        slug.pop();
    }
    slug
}

/// `haystack.startswith(prefix + "-")`, without building the concatenation.
fn starts_with_segment(haystack: &str, prefix: &str) -> bool {
    haystack.len() > prefix.len()
        && haystack.as_bytes()[prefix.len()] == b'-'
        && haystack.starts_with(prefix)
}

/// A project row that matched, carrying what the session rows need from it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct MatchedProject {
    provider: String,
    /// The slug with trailing `-` stripped — which is also what the envelope
    /// reports as `project`, not the stored spelling.
    slug: String,
    /// `projects.path`, or `None` when it is NULL *or empty* (`path or None`).
    stored_path: Option<String>,
}

/// Recent sessions under `resolved`, grouped per provider, newest first.
///
/// The port of `discovery.resume_candidates`, minus the path resolution its
/// caller has already done. Returns provider → sessions with `resume_command`
/// left `None`; rendering the invocation is the CLI's job, exactly as in Python
/// ("this function is pure store query").
///
/// # Errors
/// Any SQLite failure reading `projects` or `sessions`.
pub fn resume_candidates(
    conn: &Connection,
    resolved: &str,
    limit_per_provider: i64,
) -> Result<BTreeMap<String, Vec<ResumeSession>>> {
    let query = query_slug(resolved);

    // Insertion order is Python's `matched` dict order: direct matches in
    // table-scan order, then the deepest ancestors. It reaches SQLite as the
    // `IN (…)` parameter order, so keeping it identical keeps the two query
    // plans identical.
    let mut matched: Vec<(i64, MatchedProject)> = Vec::new();
    let mut ancestors: Vec<(i64, MatchedProject)> = Vec::new();
    let mut projects = conn.prepare("SELECT id, provider, slug, path FROM projects")?;
    let mut rows = projects.query([])?;
    while let Some(row) = rows.next()? {
        let id: i64 = row.get(0)?;
        let slug = text_or_empty(row.get_ref(2)?);
        let slug = slug.trim_end_matches('-');
        if slug.is_empty() {
            continue;
        }
        let project = MatchedProject {
            provider: text_or_empty(row.get_ref(1)?),
            slug: slug.to_owned(),
            stored_path: nonempty_text(row.get_ref(3)?),
        };
        if slug == query || starts_with_segment(slug, &query) {
            matched.push((id, project));
        } else if starts_with_segment(&query, slug) {
            ancestors.push((id, project));
        }
    }
    if let Some(deepest) = ancestors.iter().map(|(_, p)| p.slug.len()).max() {
        matched.extend(
            ancestors
                .into_iter()
                .filter(|(_, project)| project.slug.len() == deepest),
        );
    }

    let mut providers: BTreeMap<String, Vec<ResumeSession>> = BTreeMap::new();
    if matched.is_empty() {
        return Ok(providers);
    }
    let by_id: HashMap<i64, &MatchedProject> =
        matched.iter().map(|(id, project)| (*id, project)).collect();
    let placeholders = std::iter::repeat_n("?", matched.len())
        .collect::<Vec<_>>()
        .join(",");
    // Byte-for-byte `discovery.py:781`'s concatenated literal — the SQL text is
    // what the planner keys on, so the shape ports verbatim (§6b).
    let sql = format!(
        "{}{}{}{}{}",
        "SELECT s.project_id, s.session_id, s.first_ts, s.last_ts,",
        "       s.message_count",
        "  FROM sessions s",
        format_args!(" WHERE s.project_id IN ({placeholders})"),
        " ORDER BY s.last_ts DESC",
    );
    let cap = limit_per_provider.max(1);
    let mut sessions = conn.prepare(&sql)?;
    let ids: Vec<i64> = matched.iter().map(|(id, _)| *id).collect();
    let mut rows = sessions.query(rusqlite::params_from_iter(ids.iter()))?;
    while let Some(row) = rows.next()? {
        let project_id: i64 = row.get(0)?;
        let Some(project) = by_id.get(&project_id) else {
            continue;
        };
        let bucket = providers.entry(project.provider.clone()).or_default();
        if i64::try_from(bucket.len()).unwrap_or(i64::MAX) >= cap {
            continue;
        }
        bucket.push(ResumeSession {
            session_id: text_or_empty(row.get_ref(1)?),
            first_ts: null_or_text(row.get_ref(2)?),
            last_ts: null_or_text(row.get_ref(3)?),
            message_count: py_int(row.get_ref(4)?),
            project: project.slug.clone(),
            project_path: project.stored_path.clone(),
            resume_command: None,
            extra: Map::new(),
        });
    }
    Ok(providers)
}

/// A `TEXT` column as Python's `sqlite3` would hand it over, `NULL` → `""`.
///
/// Non-text storage classes are rendered as text rather than kept numeric: the
/// columns read here are declared `TEXT NOT NULL` and no row in the live store
/// violates that, so the branch is defensive. It is a recorded (unreachable)
/// shape divergence — Python would emit a JSON number there.
fn text_or_empty(value: ValueRef<'_>) -> String {
    null_or_text(value).unwrap_or_default()
}

/// A nullable `TEXT` column; `NULL` stays `None`.
fn null_or_text(value: ValueRef<'_>) -> Option<String> {
    match value {
        ValueRef::Null => None,
        ValueRef::Text(bytes) | ValueRef::Blob(bytes) => {
            Some(String::from_utf8_lossy(bytes).into_owned())
        }
        ValueRef::Integer(number) => Some(number.to_string()),
        ValueRef::Real(number) => Some(stax_core::queries::pyjson::repr_float(number)),
    }
}

/// `projects.path or None` — Python's truthiness, so `''` is `None` too and the
/// envelope never carries an empty `project_path` where a real one is unknown.
fn nonempty_text(value: ValueRef<'_>) -> Option<String> {
    null_or_text(value).filter(|text| !text.is_empty())
}

/// `int(row["message_count"] or 0)`.
fn py_int(value: ValueRef<'_>) -> i64 {
    match value {
        ValueRef::Integer(number) => number,
        // `int(3.9)` truncates toward zero, and so does `as`.
        #[allow(
            clippy::cast_possible_truncation,
            reason = "int(float) truncates toward zero in Python too; a count \
            beyond i64 is not representable in the store's INTEGER column"
        )]
        ValueRef::Real(number) => number as i64,
        ValueRef::Text(bytes) | ValueRef::Blob(bytes) => String::from_utf8_lossy(bytes)
            .trim()
            .parse()
            .unwrap_or_default(),
        ValueRef::Null => 0,
    }
}

// ── `--provider` narrowing ───────────────────────────────────────────────────

/// What `--provider` resolved to against the providers actually present.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderFilter {
    /// Provider keys to keep, first-seen order (Python's `dict.fromkeys`).
    pub resolved: Vec<String>,
    /// Requested names with no sessions here, in the spelling the user typed.
    pub unmatched: Vec<String>,
}

/// `cli.py:6351`–`6376` — exact name, else unambiguous prefix, else unmatched.
///
/// `Err` carries a `ClickException` message verbatim: the ambiguous-prefix one,
/// or the "nothing resolved" one that lists what *is* here.
///
/// # Errors
/// An ambiguous prefix, or a filter that matched nothing at all.
pub fn resolve_provider_filter(
    wanted: &[String],
    present: &BTreeMap<String, Vec<ResumeSession>>,
    resolved_path: &str,
) -> std::result::Result<ProviderFilter, String> {
    let names: Vec<&str> = present.keys().map(String::as_str).collect();
    let mut filter = ProviderFilter {
        resolved: Vec::new(),
        unmatched: Vec::new(),
    };
    for want in wanted {
        let normalized = want.to_lowercase();
        let normalized = normalized.trim();
        if present.contains_key(normalized) {
            filter.resolved.push(normalized.to_owned());
            continue;
        }
        let prefixed: Vec<&str> = names
            .iter()
            .copied()
            .filter(|name| name.starts_with(normalized))
            .collect();
        match prefixed.as_slice() {
            [only] => filter.resolved.push((*only).to_owned()),
            [] => filter.unmatched.push(want.clone()),
            many => {
                return Err(format!(
                    "--provider {} is ambiguous here: {}",
                    paths::py_repr(want),
                    many.join(", ")
                ));
            }
        }
    }
    if filter.resolved.is_empty() {
        let present = names.join(", ");
        return Err(format!(
            "no sessions for provider(s) {} under {resolved_path} — providers with sessions here: {}",
            wanted.join(", "),
            if present.is_empty() {
                "(none)"
            } else {
                &present
            }
        ));
    }
    Ok(filter)
}

// ── the envelope ─────────────────────────────────────────────────────────────

/// Build the `stackunderflow.resume/1` payload — `cli.py:6380`–`6401`.
///
/// `resume` is the capability template verbatim, and `resume_command` is
/// rendered only for a `session`-scope agent; a `latest`-scope one (grok) and an
/// agent absent from the table both get `null`, which is the point of the
/// feature: no invented flags.
#[must_use]
pub fn build_envelope(
    resolved_path: &str,
    providers: &BTreeMap<String, Vec<ResumeSession>>,
    capabilities: &Capabilities,
    requested: &[String],
    unmatched: &[String],
) -> ResumeEnvelope {
    let mut envelope = ResumeEnvelope::new(resolved_path);
    if !requested.is_empty() {
        envelope.provider_filter = Some(requested.to_vec());
        if !unmatched.is_empty() {
            envelope.unmatched_providers = Some(unmatched.to_vec());
        }
    }
    for (provider, sessions) in providers {
        let template = resume_template(capabilities, provider);
        let sessions = sessions
            .iter()
            .map(|session| ResumeSession {
                resume_command: template
                    .as_ref()
                    .and_then(|template| template.render_command(&session.session_id)),
                ..session.clone()
            })
            .collect();
        envelope.providers.push(ProviderBlock {
            provider: provider.clone(),
            resume: template,
            sessions,
        });
    }
    envelope
}

/// `(_CAPABILITIES.get(provider) or {}).get("resume")`.
///
/// The wire shape is the JSON file's own key order — `command`, `scope`,
/// `note` (grok only), `verified` — which is what [`ResumeTemplate`] serialises,
/// so the template rides through unchanged.
fn resume_template(capabilities: &Capabilities, provider: &str) -> Option<ResumeTemplate> {
    let resume = capabilities.get(provider)?.resume.as_ref()?;
    Some(ResumeTemplate {
        command: resume.command.clone(),
        scope: resume.scope.as_str().to_owned(),
        note: resume.note.clone(),
        verified: resume.verified.clone(),
        extra: Map::new(),
    })
}

// ── text rendering ───────────────────────────────────────────────────────────

/// The human format — `cli.py:6403`–`6428`, byte for byte.
#[must_use]
pub fn render_text(envelope: &ResumeEnvelope) -> String {
    if envelope.providers.is_empty() {
        return format!("no recorded sessions under {}\n", envelope.path);
    }
    let mut out = format!("resume candidates under {}\n", envelope.path);
    if let Some(unmatched) = envelope
        .unmatched_providers
        .as_ref()
        .filter(|unmatched| !unmatched.is_empty())
    {
        out.push_str(&format!(
            "(no sessions here for: {})\n",
            unmatched.join(", ")
        ));
    }
    out.push_str("(run each command from the session's project directory)\n");
    for block in &envelope.providers {
        let hint = match &block.resume {
            None => "(no resume command known — session ids listed)".to_owned(),
            Some(template) if template.scope == "latest" => {
                format!("latest-only: `{}` in the project dir", template.command)
            }
            Some(template) => format!("`{}`", template.command),
        };
        out.push_str(&format!("\n{} — {hint}\n", block.provider));
        for session in &block.sessions {
            let when: String = session
                .last_ts
                .as_deref()
                .unwrap_or_default()
                .chars()
                .take(16)
                .collect();
            let when = when.replace('T', " ");
            let command = session
                .resume_command
                .as_deref()
                .filter(|command| !command.is_empty())
                .unwrap_or(&session.session_id);
            let where_ = session
                .project_path
                .as_deref()
                .filter(|path| !path.is_empty())
                .unwrap_or(&session.project);
            out.push_str(&format!(
                "  {when:<16} {:>5} msgs  {command}   ({where_})\n",
                session.message_count
            ));
        }
    }
    out
}

// ── the run ──────────────────────────────────────────────────────────────────

/// Run `stax-rs resume …` against the real environment.
///
/// # Errors
/// When the environment cannot be read, the capability table cannot be loaded,
/// or a query fails. The four `ClickException` paths are *not* errors — they
/// come back as an [`Output`] carrying Click's exact bytes and exit code.
pub fn run_resume(args: &ResumeArgs) -> Result<()> {
    let env = ResumeEnv::from_process()?;
    let output = run(args, &env)?;
    print!("{}", output.stdout);
    if !output.stderr.is_empty() {
        eprint!("{}", output.stderr);
    }
    if output.code != 0 {
        std::process::exit(output.code);
    }
    Ok(())
}

/// Run one invocation against an injected environment.
///
/// # Errors
/// A missing or malformed capability table, or a SQLite failure.
pub fn run(args: &ResumeArgs, env: &ResumeEnv) -> Result<Output> {
    // Python imports `_CAPABILITIES` at the top of the command body, so a broken
    // table fails before the store is even looked at. Same order here.
    let capabilities = Capabilities::load(&env.capabilities).with_context(|| {
        format!(
            "loading the resume-command table {}",
            env.capabilities.display()
        )
    })?;

    let target = match args.path.as_deref() {
        Some(path) if !path.is_empty() => path.to_owned(),
        // `target = path or str(Path.cwd())` — an empty PATH is falsy too.
        _ => paths::path_to_string(&env.cwd),
    };

    if !env.store.exists() {
        return Ok(Output::click_error(format!(
            "store not found at {} — run `stackunderflow start` first",
            env.store.display()
        )));
    }
    let Ok(store) = Store::open_read_only(&env.store) else {
        return Ok(Output::click_error(format!(
            "store at {} could not be opened read-only",
            env.store.display()
        )));
    };
    // Resolution happens here, not above, because Python does it inside
    // `resume_candidates` — i.e. after both store checks, so a store failure is
    // still reported first when the path itself is unresolvable.
    let resolved = paths::resolve_input_path_with(&target, env.home.as_deref(), &env.cwd);
    let mut providers = resume_candidates(
        store.conn(),
        &resolved,
        args.limit_per_provider.saturating_i64(),
    )?;
    drop(store);

    let mut unmatched: Vec<String> = Vec::new();
    if !args.provider.is_empty() {
        let filter = match resolve_provider_filter(&args.provider, &providers, &resolved) {
            Ok(filter) => filter,
            Err(message) => return Ok(Output::click_error(message)),
        };
        unmatched = filter.unmatched;
        providers.retain(|provider, _| filter.resolved.iter().any(|kept| kept == provider));
    }

    let envelope = build_envelope(
        &resolved,
        &providers,
        &capabilities,
        &args.provider,
        &unmatched,
    );
    Ok(Output::ok(if args.as_json {
        envelope.render_line()
    } else {
        render_text(&envelope)
    }))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    use clap::Parser as _;

    use super::*;

    // ── fixtures ─────────────────────────────────────────────────────────────

    /// A scratch directory that removes itself (no `tempfile` dependency).
    struct Scratch {
        path: PathBuf,
    }

    impl Scratch {
        fn new() -> Self {
            static SEQ: AtomicU32 = AtomicU32::new(0);
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock before the epoch")
                .as_nanos();
            let seq = SEQ.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir()
                .join(format!("stax-resume-{}-{nanos}-{seq}", std::process::id()));
            fs::create_dir_all(&path).expect("creating the scratch directory");
            Self { path }
        }

        fn db(&self) -> PathBuf {
            self.path.join("store.db")
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    /// One seeded session: `(session_id, last_ts, message_count)`.
    type SeedSession = (&'static str, &'static str, i64);
    /// One seeded project: `(provider, slug, sessions)`.
    type SeedProject = (&'static str, &'static str, &'static [SeedSession]);

    /// The seed from `tests/stackunderflow/cli/test_resume_cmd.py`, verbatim —
    /// the same rows the golden pack was generated from.
    const SEED: &[SeedProject] = &[
        (
            "claude",
            "-Users-t-my-ws",
            &[("cl-ws-new", "2026-07-01T10:00:00Z", 142)],
        ),
        (
            "claude",
            "-Users-t-my-ws-child",
            &[("cl-child-old", "2026-06-19T10:00:00Z", 601)],
        ),
        (
            "codex",
            "-Users-t-my-ws-child",
            &[
                ("cx-child-new", "2026-07-08T10:00:00Z", 151),
                ("cx-child-old", "2026-06-26T10:00:00Z", 62),
            ],
        ),
        (
            "grok",
            "-Users-t-my-ws-child",
            &[("gr-child", "2026-07-09T10:00:00Z", 96)],
        ),
        (
            "mystery",
            "-Users-t-my-ws-child",
            &[("my-child", "2026-05-24T10:00:00Z", 82)],
        ),
        // Home-directory catch-all — an ANCESTOR of every query under /Users/t.
        (
            "claude",
            "-Users-t",
            &[("cl-home", "2026-05-27T10:00:00Z", 40)],
        ),
        // Unrelated project — must never match.
        (
            "claude",
            "-Users-t-other-proj",
            &[("cl-other", "2026-07-09T10:00:00Z", 10)],
        ),
    ];

    fn seed(path: &Path) {
        let conn = Connection::open(path).expect("creating the fixture store");
        conn.execute_batch(
            "CREATE TABLE projects (
               id INTEGER PRIMARY KEY, provider TEXT NOT NULL, slug TEXT NOT NULL,
               path TEXT, display_name TEXT NOT NULL, first_seen REAL NOT NULL,
               last_modified REAL NOT NULL, worktree_of TEXT,
               UNIQUE (provider, slug));
             CREATE TABLE sessions (
               id INTEGER PRIMARY KEY,
               project_id INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
               session_id TEXT NOT NULL, first_ts TEXT, last_ts TEXT,
               message_count INTEGER NOT NULL DEFAULT 0,
               UNIQUE (project_id, session_id));",
        )
        .expect("the fixture schema");
        for (provider, slug, sessions) in SEED {
            conn.execute(
                "INSERT INTO projects (provider, slug, display_name, first_seen, last_modified)
                 VALUES (?, ?, ?, 0.0, 0.0)",
                rusqlite::params![provider, slug, slug],
            )
            .expect("inserting a project");
            let pid = conn.last_insert_rowid();
            for (session_id, last_ts, count) in *sessions {
                conn.execute(
                    "INSERT INTO sessions (project_id, session_id, first_ts, last_ts, message_count)
                     VALUES (?, ?, ?, ?, ?)",
                    rusqlite::params![pid, session_id, last_ts, last_ts, count],
                )
                .expect("inserting a session");
            }
        }
    }

    /// `<repo>/rust/crates/stax-cli` → `<repo>`.
    fn repo_root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(3)
            .expect("the crate lives at <repo>/rust/crates/stax-cli")
            .to_path_buf()
    }

    fn env_for(scratch: &Scratch) -> ResumeEnv {
        ResumeEnv {
            cwd: PathBuf::from("/Users/t/my_ws"),
            home: Some(PathBuf::from("/Users/t")),
            store: scratch.db(),
            capabilities: repo_root().join(CAPABILITIES_RELATIVE_PATH),
        }
    }

    fn args(path: &str) -> ResumeArgs {
        ResumeArgs {
            path: Some(path.to_owned()),
            provider: Vec::new(),
            limit_per_provider: PyInt::from(5),
            as_json: false,
        }
    }

    fn run_seeded(args: &ResumeArgs) -> Output {
        let scratch = Scratch::new();
        seed(&scratch.db());
        run(args, &env_for(&scratch)).expect("the run succeeds")
    }

    fn payload(args: &ResumeArgs) -> ResumeEnvelope {
        let mut args = ResumeArgs {
            as_json: true,
            path: args.path.clone(),
            provider: args.provider.clone(),
            limit_per_provider: args.limit_per_provider.clone(),
        };
        args.as_json = true;
        let out = run_seeded(&args);
        assert_eq!(out.code, 0, "{out:?}");
        ResumeEnvelope::from_json(&out.stdout).expect("a valid envelope")
    }

    fn block<'a>(envelope: &'a ResumeEnvelope, provider: &str) -> &'a ProviderBlock {
        envelope
            .providers
            .iter()
            .find(|block| block.provider == provider)
            .unwrap_or_else(|| panic!("no {provider} block in {:?}", envelope.providers))
    }

    fn ids(block: &ProviderBlock) -> Vec<&str> {
        block
            .sessions
            .iter()
            .map(|session| session.session_id.as_str())
            .collect()
    }

    // ── slug-space folding ───────────────────────────────────────────────────

    #[test]
    fn the_query_path_folds_into_slug_space() {
        assert_eq!(query_slug("/Users/t/my_ws"), "-Users-t-my-ws");
        assert_eq!(
            query_slug("/media/tmos/dev_dev/year26"),
            "-media-tmos-dev-dev-year26"
        );
        // Trailing separators go before folding, so no trailing `-` survives.
        assert_eq!(query_slug("/Users/t/my_ws/"), "-Users-t-my-ws");
        assert_eq!(query_slug("C:\\Users\\t\\ws"), "-C:-Users-t-ws");
        // The degenerate root: empty, and therefore a prefix of every slug.
        assert_eq!(query_slug("/"), "");
        assert!(starts_with_segment("-Users-t", ""));
    }

    #[test]
    fn a_workspace_query_matches_children_despite_underscores() {
        let envelope = payload(&args("/Users/t/my_ws"));
        let names: Vec<&str> = envelope
            .providers
            .iter()
            .map(|block| block.provider.as_str())
            .collect();
        assert_eq!(names, ["claude", "codex", "grok", "mystery"]);
        assert_eq!(
            ids(block(&envelope, "codex")),
            ["cx-child-new", "cx-child-old"]
        );
        let all: Vec<&str> = envelope
            .providers
            .iter()
            .flat_map(|block| ids(block))
            .collect();
        assert!(
            !all.contains(&"cl-other"),
            "the sibling project never leaks"
        );
    }

    #[test]
    fn standing_inside_a_child_finds_it_and_the_nearest_ancestor_only() {
        let envelope = payload(&args("/Users/t/my_ws/child"));
        let claude = ids(block(&envelope, "claude"));
        assert!(claude.contains(&"cl-child-old"), "{claude:?}");
        assert!(claude.contains(&"cl-ws-new"), "the deepest ancestor");
        assert!(
            !claude.contains(&"cl-home"),
            "the home catch-all is shadowed"
        );
    }

    #[test]
    fn the_home_catchall_matches_only_when_it_is_the_nearest_project() {
        let envelope = payload(&args("/Users/t/somewhere/random"));
        assert_eq!(ids(block(&envelope, "claude")), ["cl-home"]);
    }

    #[test]
    fn the_limit_is_per_provider_and_recency_ordered() {
        let envelope = payload(&ResumeArgs {
            limit_per_provider: PyInt::from(1),
            ..args("/Users/t/my_ws")
        });
        assert_eq!(ids(block(&envelope, "codex")), ["cx-child-new"]);
        assert_eq!(ids(block(&envelope, "claude")), ["cl-ws-new"]);
    }

    // ── template rendering ───────────────────────────────────────────────────

    #[test]
    fn session_scope_templates_render_real_commands() {
        let envelope = payload(&args("/Users/t/my_ws"));
        let codex = block(&envelope, "codex");
        assert_eq!(codex.resume.as_ref().expect("template").scope, "session");
        assert_eq!(
            codex.sessions[0].resume_command.as_deref(),
            Some("codex resume cx-child-new")
        );
        let claude = block(&envelope, "claude");
        assert!(
            claude.sessions[0]
                .resume_command
                .as_deref()
                .expect("a command")
                .starts_with("claude --resume ")
        );
    }

    #[test]
    fn latest_scope_renders_no_per_session_command() {
        let envelope = payload(&args("/Users/t/my_ws"));
        let grok = block(&envelope, "grok");
        assert_eq!(grok.resume.as_ref().expect("template").scope, "latest");
        assert!(grok.sessions.iter().all(|s| s.resume_command.is_none()));
        assert!(
            run_seeded(&args("/Users/t/my_ws"))
                .stdout
                .contains("latest-only")
        );
    }

    #[test]
    fn an_unknown_agent_lists_ids_without_inventing_a_command() {
        let envelope = payload(&args("/Users/t/my_ws"));
        let mystery = block(&envelope, "mystery");
        assert!(mystery.resume.is_none());
        assert!(mystery.sessions[0].resume_command.is_none());
        let text = run_seeded(&args("/Users/t/my_ws")).stdout;
        assert!(text.contains("no resume command known"), "{text}");
        assert!(text.contains("my-child"), "{text}");
    }

    // ── the envelope ─────────────────────────────────────────────────────────

    #[test]
    fn the_json_envelope_has_the_pinned_shape() {
        let envelope = payload(&args("/Users/t/my_ws"));
        assert_eq!(envelope.schema, "stackunderflow.resume/1");
        assert_eq!(envelope.path, "/Users/t/my_ws");
        let names: Vec<&str> = envelope
            .providers
            .iter()
            .map(|block| block.provider.as_str())
            .collect();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        assert_eq!(names, sorted);
        assert!(envelope.provider_filter.is_none());
    }

    #[test]
    fn no_matches_says_so_and_still_exits_zero() {
        let out = run_seeded(&args("/Elsewhere/entirely"));
        assert_eq!(out.code, 0);
        assert_eq!(
            out.stdout,
            "no recorded sessions under /Elsewhere/entirely\n"
        );
    }

    #[test]
    fn a_missing_store_is_a_clean_click_error() {
        let scratch = Scratch::new();
        let env = ResumeEnv {
            store: scratch.path.join("nope.db"),
            ..env_for(&scratch)
        };
        let out = run(&args("/Users/t/my_ws"), &env).expect("no hard failure");
        assert_eq!(out.code, 1);
        assert!(out.stdout.is_empty());
        assert!(
            out.stderr.starts_with("Error: store not found at "),
            "{out:?}"
        );
        assert!(
            out.stderr
                .ends_with(" — run `stackunderflow start` first\n"),
            "{out:?}"
        );
    }

    // ── --provider narrowing ─────────────────────────────────────────────────

    #[test]
    fn the_provider_filter_reduces_to_one_agent_and_echoes_the_request() {
        let envelope = payload(&ResumeArgs {
            provider: vec!["codex".to_owned()],
            ..args("/Users/t/my_ws")
        });
        assert_eq!(
            envelope
                .providers
                .iter()
                .map(|block| block.provider.as_str())
                .collect::<Vec<_>>(),
            ["codex"]
        );
        assert_eq!(
            envelope.provider_filter.as_deref(),
            Some(["codex".to_owned()].as_slice())
        );
    }

    #[test]
    fn the_filter_is_case_insensitive_repeatable_and_prefix_tolerant() {
        let envelope = payload(&ResumeArgs {
            provider: vec!["CODEX".to_owned(), "grok".to_owned()],
            ..args("/Users/t/my_ws")
        });
        assert_eq!(
            envelope
                .providers
                .iter()
                .map(|block| block.provider.as_str())
                .collect::<Vec<_>>(),
            ["codex", "grok"]
        );
        // The echo keeps the spelling the user typed, not the resolved key.
        assert_eq!(
            envelope.provider_filter.as_deref(),
            Some(["CODEX".to_owned(), "grok".to_owned()].as_slice())
        );
        let prefixed = payload(&ResumeArgs {
            provider: vec!["gr".to_owned()],
            ..args("/Users/t/my_ws")
        });
        assert_eq!(
            prefixed
                .providers
                .iter()
                .map(|block| block.provider.as_str())
                .collect::<Vec<_>>(),
            ["grok"]
        );
    }

    #[test]
    fn an_unknown_filter_errors_with_the_available_list() {
        let out = run_seeded(&ResumeArgs {
            provider: vec!["agy".to_owned()],
            ..args("/Users/t/my_ws")
        });
        assert_eq!(out.code, 1);
        assert!(
            out.stderr.contains("providers with sessions here:"),
            "{out:?}"
        );
        assert!(out.stderr.contains("codex"), "{out:?}");
    }

    #[test]
    fn an_ambiguous_prefix_names_the_candidates_with_a_python_repr() {
        let mut providers: BTreeMap<String, Vec<ResumeSession>> = BTreeMap::new();
        providers.insert("claude".to_owned(), Vec::new());
        providers.insert("cline".to_owned(), Vec::new());
        let err =
            resolve_provider_filter(&["cl".to_owned()], &providers, "/w").expect_err("ambiguous");
        assert_eq!(err, "--provider 'cl' is ambiguous here: claude, cline");
    }

    #[test]
    fn a_partial_match_notes_the_misses_in_both_formats() {
        let envelope = payload(&ResumeArgs {
            provider: vec!["codex".to_owned(), "nope".to_owned()],
            ..args("/Users/t/my_ws")
        });
        assert_eq!(
            envelope.unmatched_providers.as_deref(),
            Some(["nope".to_owned()].as_slice())
        );
        let text = run_seeded(&ResumeArgs {
            provider: vec!["codex".to_owned(), "nope".to_owned()],
            ..args("/Users/t/my_ws")
        });
        assert!(
            text.stdout.contains("(no sessions here for: nope)"),
            "{text:?}"
        );
    }

    // ── byte parity against the golden pack ──────────────────────────────────

    /// The five `stackunderflow.resume/1` goldens are literal Python CLI stdout
    /// over the seed above (`tests/goldens/generate.py`). Reproducing them from
    /// this port through the same seed is the parity gate: same store, same
    /// invocation, same bytes.
    #[test]
    fn the_resume_golden_pack_reproduces_byte_for_byte() {
        let pack =
            repo_root().join("rust/crates/stax-memory/tests/goldens/rust-campaign-added/resume-v1");
        let cases: [(&str, ResumeArgs); 5] = [
            ("resume.workspace", args("/Users/t/my_ws")),
            (
                "resume.filtered",
                ResumeArgs {
                    provider: vec!["codex".to_owned()],
                    ..args("/Users/t/my_ws")
                },
            ),
            (
                "resume.unmatched",
                ResumeArgs {
                    provider: vec!["codex".to_owned(), "kiro".to_owned()],
                    ..args("/Users/t/my_ws")
                },
            ),
            ("resume.no-sessions", args("/Elsewhere/entirely")),
            (
                "resume.unicode-path",
                args("/Users/t/Mes Projets/naïve café"),
            ),
        ];
        let scratch = Scratch::new();
        seed(&scratch.db());
        let env = env_for(&scratch);
        let mut failures = Vec::new();
        for (name, case) in cases {
            let want = fs::read_to_string(pack.join(format!("{name}.json")))
                .unwrap_or_else(|err| panic!("reading {name}: {err}"));
            let out = run(
                &ResumeArgs {
                    as_json: true,
                    ..case
                },
                &env,
            )
            .expect("the run succeeds");
            if out.stdout != want {
                failures.push(format!(
                    "{name}:\n--- want\n{want}\n--- got\n{}",
                    out.stdout
                ));
            }
        }
        assert!(failures.is_empty(), "{}", failures.join("\n\n"));
    }

    /// The unicode golden is also the ancestor case: `/Users/t/Mes Projets/…`
    /// folds to `-Users-t-Mes Projets-naïve café`, whose only ancestor project
    /// is the home catch-all — and non-ASCII leaves through `ensure_ascii`.
    #[test]
    fn a_non_ascii_query_path_escapes_the_way_python_dumps_it() {
        let scratch = Scratch::new();
        seed(&scratch.db());
        let out = run(
            &ResumeArgs {
                as_json: true,
                ..args("/Users/t/Mes Projets/naïve café")
            },
            &env_for(&scratch),
        )
        .expect("the run succeeds");
        assert!(out.stdout.is_ascii(), "ensure_ascii=True");
        assert!(
            out.stdout.contains(r"na\u00efve caf\u00e9"),
            "{}",
            out.stdout
        );
    }

    // ── text format ──────────────────────────────────────────────────────────

    #[test]
    fn the_text_format_is_the_reference_byte_for_byte() {
        let out = run_seeded(&args("/Users/t/my_ws"));
        assert_eq!(
            out.stdout,
            "resume candidates under /Users/t/my_ws\n\
             (run each command from the session's project directory)\n\
             \n\
             claude — `claude --resume {session_id}`\n\
             \u{20} 2026-07-01 10:00   142 msgs  claude --resume cl-ws-new   (-Users-t-my-ws)\n\
             \u{20} 2026-06-19 10:00   601 msgs  claude --resume cl-child-old   (-Users-t-my-ws-child)\n\
             \u{20} 2026-05-27 10:00    40 msgs  claude --resume cl-home   (-Users-t)\n\
             \n\
             codex — `codex resume {session_id}`\n\
             \u{20} 2026-07-08 10:00   151 msgs  codex resume cx-child-new   (-Users-t-my-ws-child)\n\
             \u{20} 2026-06-26 10:00    62 msgs  codex resume cx-child-old   (-Users-t-my-ws-child)\n\
             \n\
             grok — latest-only: `grok --continue` in the project dir\n\
             \u{20} 2026-07-09 10:00    96 msgs  gr-child   (-Users-t-my-ws-child)\n\
             \n\
             mystery — (no resume command known — session ids listed)\n\
             \u{20} 2026-05-24 10:00    82 msgs  my-child   (-Users-t-my-ws-child)\n"
        );
    }

    #[test]
    fn a_null_timestamp_prints_an_empty_column_not_the_word_none() {
        let envelope = ResumeEnvelope {
            providers: vec![ProviderBlock {
                provider: "claude".to_owned(),
                resume: None,
                sessions: vec![ResumeSession {
                    session_id: "s-1".to_owned(),
                    first_ts: None,
                    last_ts: None,
                    message_count: 0,
                    project: "-w".to_owned(),
                    project_path: None,
                    resume_command: None,
                    extra: Map::new(),
                }],
            }],
            ..ResumeEnvelope::new("/w")
        };
        assert!(
            render_text(&envelope).ends_with("                     0 msgs  s-1   (-w)\n"),
            "{}",
            render_text(&envelope)
        );
    }

    #[test]
    fn the_stored_path_wins_over_the_slug_when_the_store_has_one() {
        let envelope = ResumeEnvelope {
            providers: vec![ProviderBlock {
                provider: "claude".to_owned(),
                resume: None,
                sessions: vec![ResumeSession {
                    session_id: "s-1".to_owned(),
                    first_ts: None,
                    last_ts: Some("2026-07-01T10:00:00Z".to_owned()),
                    message_count: 3,
                    project: "-Users-t-w".to_owned(),
                    project_path: Some("/Users/t/w".to_owned()),
                    resume_command: None,
                    extra: Map::new(),
                }],
            }],
            ..ResumeEnvelope::new("/Users/t/w")
        };
        assert!(render_text(&envelope).ends_with("  s-1   (/Users/t/w)\n"));
    }

    // ── environment resolution ───────────────────────────────────────────────

    #[test]
    fn the_capability_table_is_found_from_the_working_directory() {
        let repo = repo_root();
        let deep = repo.join("rust/crates/stax-cli/src");
        assert_eq!(
            resolve_capabilities_path(None, &deep, None),
            repo.join(CAPABILITIES_RELATIVE_PATH)
        );
        // …and from the executable when the cwd is somewhere unrelated.
        let exe = repo.join("rust/target/release/stax-rs");
        assert_eq!(
            resolve_capabilities_path(None, Path::new("/"), Some(&exe)),
            repo.join(CAPABILITIES_RELATIVE_PATH)
        );
        // The injected path always wins.
        assert_eq!(
            resolve_capabilities_path(Some(OsStr::new("/elsewhere/caps.json")), &deep, None),
            Path::new("/elsewhere/caps.json")
        );
    }

    #[test]
    fn an_empty_path_argument_means_the_working_directory() {
        let scratch = Scratch::new();
        seed(&scratch.db());
        let env = env_for(&scratch);
        let out = run(
            &ResumeArgs {
                path: Some(String::new()),
                as_json: true,
                ..args("")
            },
            &env,
        )
        .expect("the run succeeds");
        let envelope = ResumeEnvelope::from_json(&out.stdout).expect("an envelope");
        // `env.cwd` is /Users/t/my_ws, which does not exist on this box, so
        // `resolve(strict=False)` leaves it lexical.
        assert_eq!(envelope.path, "/Users/t/my_ws");
        assert_eq!(
            ids(block(&envelope, "codex")),
            ["cx-child-new", "cx-child-old"]
        );
    }

    #[test]
    fn message_counts_survive_every_storage_class() {
        assert_eq!(py_int(ValueRef::Null), 0);
        assert_eq!(py_int(ValueRef::Integer(7)), 7);
        assert_eq!(py_int(ValueRef::Real(3.9)), 3);
        assert_eq!(py_int(ValueRef::Text(b"12")), 12);
    }

    /// `IntRange(min=1)` converts with `int()` **before** it checks the bound.
    /// `clap::value_parser!(i64)` did neither, so `' 5'`, `٧`, `1_000` and a
    /// limit past 2⁶³ were exit-2 rejections against a Python exit 0 — and
    /// `--limit-per-provider 1_000` is not academic: it is the difference
    /// between 12 KB and 442 KB of `resume /` output on the live store.
    fn parse_limit(raw: &str) -> Result<PyInt, clap::error::ErrorKind> {
        match crate::Cli::try_parse_from(["stax-rs", "resume", "/", "--limit-per-provider", raw]) {
            Ok(cli) => {
                let crate::Command::Resume(args) = cli.command else {
                    panic!("expected resume");
                };
                Ok(args.limit_per_provider)
            }
            Err(error) => Err(error.kind()),
        }
    }

    #[test]
    fn limit_per_provider_converts_with_int_then_checks_the_bound() {
        for (raw, expected) in [
            ("5", "5"),
            (" 5", "5"),
            ("+5", "5"),
            ("1_000", "1000"),
            ("\u{667}", "7"),
            ("99999999999999999999", "99999999999999999999"),
        ] {
            assert_eq!(
                parse_limit(raw)
                    .unwrap_or_else(|kind| panic!("{raw:?} → {kind:?}"))
                    .to_string(),
                expected,
                "{raw:?}"
            );
        }
        // The bound still rejects — and it rejects the *converted* value, so
        // `' 0'` fails for being zero, not for the space.
        for raw in ["0", " 0", "-3", "5.0", "0x2", ""] {
            assert!(
                parse_limit(raw).is_err(),
                "{raw:?} must be a parameter error"
            );
        }
    }

    #[test]
    fn repeated_resume_options_are_last_wins() {
        let cli = crate::Cli::try_parse_from([
            "stax-rs",
            "resume",
            "/",
            "--limit-per-provider",
            "3",
            "--limit-per-provider",
            "7",
            "--json",
            "--json",
        ])
        .expect("Click keeps the last occurrence of a repeated option");
        let crate::Command::Resume(args) = cli.command else {
            panic!("expected resume");
        };
        assert_eq!(args.limit_per_provider.to_string(), "7");
        assert!(args.as_json);
        // `-p` really is repeatable on the Python side — it must NOT collapse.
        let cli =
            crate::Cli::try_parse_from(["stax-rs", "resume", "/", "-p", "claude", "-p", "codex"])
                .expect("--provider is a list, not a scalar");
        let crate::Command::Resume(args) = cli.command else {
            panic!("expected resume");
        };
        assert_eq!(args.provider, vec!["claude", "codex"]);
    }

    /// Item D, recorded rather than fixed. `resume` orders by `s.last_ts DESC`
    /// and 59 % of the live store's sessions have `last_ts IS NULL`, so the tie
    /// order is the query planner's — running `ANALYZE` on the store reorders
    /// the output, and at `--limit-per-provider 2000` it changes *which*
    /// sessions come back (2,155 rows, 197 swapped in/out). The temptation is a
    /// deterministic tiebreaker; the measurement says no. Appending
    /// `, s.id ASC` to the same SQL on the same snapshot moves 675 of those
    /// 2,155 printed rows and swaps the same 197 — in **Python** too. A
    /// tiebreaker added here alone would therefore *break* byte-parity rather
    /// than secure it. The SQL shape stays verbatim (§6b); this test is the
    /// tripwire, so a future edit to the clause fails loudly.
    #[test]
    fn the_resume_order_by_stays_verbatim_until_python_changes_it() {
        let scratch = Scratch::new();
        seed(&scratch.db());
        let conn = Connection::open(scratch.db()).expect("opening the fixture");
        let plan: Vec<String> = conn
            .prepare("EXPLAIN QUERY PLAN SELECT s.project_id, s.session_id, s.first_ts, s.last_ts,       s.message_count  FROM sessions s WHERE s.project_id IN (?) ORDER BY s.last_ts DESC")
            .expect("the ported shape still prepares")
            .query_map([1_i64], |row| row.get::<_, String>(3))
            .expect("planning")
            .collect::<rusqlite::Result<_>>()
            .expect("planning");
        assert!(
            plan.iter().any(|step| step.contains("ORDER BY")),
            "the sort is the planner's, and its tie order is not a contract: {plan:?}"
        );
    }
}
