//! `services/yield_tracker.py` — did the money buy commits that stayed?
//!
//! | Item | Python | Rust |
//! |---|---|---|
//! | `compute_yield(conn, period, project_filter)` | 871 ln module | [`compute_yield`] |
//! | `yield_summary(entries)` | ↑ | [`yield_summary`] |
//! | `to_dicts(entries)` | ↑ | [`to_dicts`] |
//! | `YieldEntry` (dataclass) | ↑ | [`YieldEntry`] |
//!
//! Consumed by `routes/yield_route.rs` (`GET /api/yield`) and, in wave 8, by the
//! `stackunderflow yield` CLI verb — which is why the logic lives here and not
//! in the route module.
//!
//! # What the service actually does
//!
//! Every session in the store records the editor's working directory (`cwd`).
//! For a period's worth of sessions this asks, per session: *did a commit land
//! in this repo within 24 hours of the session starting, and is that commit
//! still there?* The four answers are `productive`, `reverted`, `abandoned` and
//! `no_repo`, and each carries the session's dollar cost so the route can roll
//! them into a spend breakdown.
//!
//! That means **this module reads the machine's git working trees.** It stats
//! `cwd`, and it shells out to `git rev-parse`, `git log` and `git rev-list`
//! (five-second ceiling each, batched to one set of calls per *distinct* `cwd`
//! rather than per session — the per-session version timed the route out past
//! 15 s on a 95-session project). Nothing is written: no verb here mutates a
//! repo, no file is created, no store row is touched. The full audit is
//! DIV-095, which is also why `GET /api/yield` is allowed parity case rows at
//! all under LAW 7.
//!
//! The subprocess surface is behind the [`Git`] trait so the classification
//! logic — which is where every interesting bug would live — is unit-testable
//! against a scripted repo instead of a real one. [`SystemGit`] is the
//! production implementation and the only one that spawns anything.
//!
//! # What is load-bearing
//!
//! * **There is no `sum()` in this module.** `yield_summary` seeds its cost
//!   fields with the literal `0.0` and steps them with `+=`, so an empty window
//!   renders `"total_cost":0.0` — a float — and the accumulation is *not*
//!   Neumaier-compensated. LAW 3 says match the operation; compensating here
//!   would be the divergence. DIV-096.
//! * **`week` is not a `reports/scope.py` period.** The route's allow-list
//!   accepts it and [`normalize_period`] rewrites it to `7days` before
//!   `parse_period` ever sees it.
//! * **Sessions come back in start-time order and stay in it.** The SQL sorts by
//!   `first_ts`, the per-project cap preserves the original order, and the
//!   entries are re-emitted by walking the original rows — not the `cwd`
//!   buckets. Callers depend on it; the route's cost sort is applied to a copy.
//! * **A malformed `started_at` classifies as `no_repo`; a *naive* one is a
//!   500.** `except ValueError` catches the first and not the second. DIV-098.
//! * **The mart is the fast path and it is not the same number.**
//!   `session_mart.cost_usd` is what the ETL normalizer stored; the empty-mart
//!   fallback re-prices `messages` through the injected [`PricingEngine`]. Both
//!   are ported; which one runs is a property of the store.

use std::collections::{HashMap, HashSet};
use std::process::{Command, Stdio};
use std::time::Duration;

use rusqlite::Connection;
use rusqlite::types::Value as SqlValue;
use serde_json::{Map, Value};
use stax_etl::pricing::RawTokens;
use stax_etl::pricing::costs::PricingEngine;
use stax_etl::stats::aggregator::jf;
use stax_etl::stats::pydatetime::{PyDateTime, parse_ts};

use super::scope::{Instant, Scope, parse_period};

/// `_GIT_TIMEOUT_SECONDS = 5`.
const GIT_TIMEOUT: Duration = Duration::from_secs(5);

/// `_FOLLOW_WINDOW_HOURS = 24` — the credit window after a session starts.
const FOLLOW_WINDOW_HOURS: i64 = 24;

/// `_DEFAULT_MAX_SESSIONS_PER_PROJECT = 200`.
const DEFAULT_MAX_SESSIONS_PER_PROJECT: usize = 200;

/// `_MAX_SESSIONS_ENV`.
pub const MAX_SESSIONS_ENV: &str = "STACKUNDERFLOW_YIELD_MAX_SESSIONS_PER_PROJECT";

/// `_GIT_LOG_MAX_COMMITS = 5000` — `--max-count`, so one enormous repo cannot
/// dominate the per-request workspace cache.
const GIT_LOG_MAX_COMMITS: i64 = 5000;

/// `_bulk_first_cwd_for_sessions`'s `chunk_size = 500` — stays under SQLite's
/// `SQLITE_MAX_VARIABLE_NUMBER` (commonly 999).
const CWD_CHUNK_SIZE: usize = 500;

// ── public types ─────────────────────────────────────────────────────────────

/// `Classification = Literal["productive", "reverted", "abandoned", "no_repo"]`.
///
/// The wire spelling is also a `yield_summary` **key prefix** (`f"{c}_cost"`),
/// so [`Classification::as_str`] is the one place the strings exist.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Classification {
    /// A commit landed in the window and is still reachable from `HEAD`.
    Productive,
    /// The credited commit was reverted, or is no longer reachable.
    Reverted,
    /// The repo is real, but nothing landed inside 24 hours.
    Abandoned,
    /// No `cwd`, an unresolvable `cwd`, or an unparseable `started_at`.
    NoRepo,
}

impl Classification {
    /// The literal Python writes into the payload.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Productive => "productive",
            Self::Reverted => "reverted",
            Self::Abandoned => "abandoned",
            Self::NoRepo => "no_repo",
        }
    }

    /// `f"{classification}_cost"` — the summary key this class accumulates into.
    const fn cost_key(self) -> &'static str {
        match self {
            Self::Productive => "productive_cost",
            Self::Reverted => "reverted_cost",
            Self::Abandoned => "abandoned_cost",
            Self::NoRepo => "no_repo_cost",
        }
    }
}

/// `@dataclass class YieldEntry` — one session's classification.
///
/// Field order is the payload's key order: `asdict()` walks the declaration.
#[derive(Debug, Clone)]
pub struct YieldEntry {
    /// `sessions.session_id` — the wire id, not the integer fk.
    pub session_id: String,
    /// `projects.slug`.
    pub project_slug: String,
    /// The editor's working directory, or `""` when none was recorded.
    pub cwd: String,
    /// ISO-8601 session start (`first_ts`), as stored.
    pub started_at: String,
    /// USD. Always rendered as a float, including `0.0`.
    pub cost_usd: f64,
    /// One of the four literals.
    pub classification: Classification,
    /// The credited commit's full SHA, when there is one.
    pub follow_commit_sha: Option<String>,
    /// The credited commit's subject line (`%s`). May legitimately be `""`.
    pub follow_commit_msg: Option<String>,
    /// Hours from session start to the credited commit.
    pub follow_commit_age_hours: Option<f64>,
}

impl YieldEntry {
    /// `YieldEntry.to_dict()` — `dataclasses.asdict`, so declaration order.
    #[must_use]
    pub fn to_dict(&self) -> Value {
        let mut out = Map::new();
        out.insert(
            "session_id".to_owned(),
            Value::String(self.session_id.clone()),
        );
        out.insert(
            "project_slug".to_owned(),
            Value::String(self.project_slug.clone()),
        );
        out.insert("cwd".to_owned(), Value::String(self.cwd.clone()));
        out.insert(
            "started_at".to_owned(),
            Value::String(self.started_at.clone()),
        );
        // `float(row["cost_usd"] or 0.0)` upstream, so this is a float even at
        // zero: `0` and `0.0` are different bytes (DIV-057's family).
        out.insert("cost_usd".to_owned(), jf(self.cost_usd));
        out.insert(
            "classification".to_owned(),
            Value::String(self.classification.as_str().to_owned()),
        );
        out.insert(
            "follow_commit_sha".to_owned(),
            opt_str(self.follow_commit_sha.as_ref()),
        );
        out.insert(
            "follow_commit_msg".to_owned(),
            opt_str(self.follow_commit_msg.as_ref()),
        );
        out.insert(
            "follow_commit_age_hours".to_owned(),
            self.follow_commit_age_hours.map_or(Value::Null, jf),
        );
        Value::Object(out)
    }
}

fn opt_str(value: Option<&String>) -> Value {
    value.map_or(Value::Null, |text| Value::String(text.clone()))
}

/// Everything [`compute_yield`] can fail with.
#[derive(Debug)]
pub enum YieldError {
    /// A SQLite failure. Python lets `sqlite3.OperationalError` escape too.
    Sql(rusqlite::Error),
    /// `parse_period` raised `ValueError`. Unreachable from the route, which
    /// validates against its own allow-list first.
    Period(String),
    /// **DIV-098.** A naive `started_at` compared against an aware commit stamp
    /// is a `TypeError` in CPython, and neither `_GitWorkspace.classify` nor
    /// `_hours_between` catches it — `except ValueError` does not cover it. The
    /// port keeps it a hard failure rather than inventing a fallback.
    NaiveVsAware,
}

impl std::fmt::Display for YieldError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Sql(err) => write!(f, "{err}"),
            Self::Period(message) => write!(f, "{message}"),
            Self::NaiveVsAware => {
                write!(f, "can't compare offset-naive and offset-aware datetimes")
            }
        }
    }
}

impl std::error::Error for YieldError {}

impl From<rusqlite::Error> for YieldError {
    fn from(err: rusqlite::Error) -> Self {
        Self::Sql(err)
    }
}

// ── the git seam ─────────────────────────────────────────────────────────────

/// The subprocess surface, injected so the classifier can be tested without a
/// repo on disk.
///
/// Both methods swallow failure the way Python's do: `_is_git_repo` answers
/// `false` for "not a directory", "no git on `PATH`", a timeout or a non-zero
/// exit, and `_run_git` answers `None` for a timeout, an `OSError` or a
/// non-zero exit. A caller cannot tell those apart, and neither can Python's.
pub trait Git {
    /// `_is_git_repo(cwd)`.
    fn is_repo(&self, cwd: &str) -> bool;

    /// `_run_git(cwd, args)` — stdout on success, `None` on any failure.
    fn run(&self, cwd: &str, args: &[&str]) -> Option<String>;
}

/// The real thing: `subprocess.run(["git", "-C", cwd, *args], timeout=5)`.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemGit;

impl Git for SystemGit {
    fn is_repo(&self, cwd: &str) -> bool {
        // `p.exists() and p.is_dir()` — `is_dir()` follows symlinks and is false
        // on any stat error, which is both Python checks in one call.
        //
        // NOTE (DIV-c-yield §11): `Path("")` is `PosixPath(".")` in pathlib, so
        // Python would stat the SERVER's cwd here where Rust answers false.
        // Unreachable: `compute_yield` short-circuits an empty `cwd` before any
        // workspace is built.
        if !std::path::Path::new(cwd).is_dir() {
            return false;
        }
        // `if shutil.which("git") is None: return False` is not transcribed: a
        // missing git makes `Command::spawn` fail with `NotFound`, which
        // `run_git` already folds into the same `false`. The two are
        // behaviourally identical and one of them needs no `PATH` walker.
        self.run(cwd, &["rev-parse", "--git-dir"]).is_some()
    }

    fn run(&self, cwd: &str, args: &[&str]) -> Option<String> {
        run_git(cwd, args, GIT_TIMEOUT)
    }
}

/// Spawn `git -C <cwd> <args…>`, capture stdout, enforce a wall-clock ceiling.
///
/// The pipes are drained on their own threads before the exit status is waited
/// on: `git log` over a 5,000-commit window is far more than a pipe buffer, and
/// a single-threaded "wait then read" deadlocks on exactly the repos this
/// endpoint exists for.
fn run_git(cwd: &str, args: &[&str], timeout: Duration) -> Option<String> {
    let mut child = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        // Python inherits stdin; `null` is the safer spelling of the same
        // outcome for these four read-only verbs, none of which reads stdin.
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        // `except OSError` — git missing from PATH lands here.
        .ok()?;

    let mut stdout = child.stdout.take()?;
    let mut stderr = child.stderr.take()?;
    let reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = std::io::Read::read_to_end(&mut stdout, &mut buf);
        buf
    });
    let drainer = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = std::io::Read::read_to_end(&mut stderr, &mut buf);
    });

    let deadline = std::time::Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    // `except subprocess.TimeoutExpired` — run() kills the child
                    // and re-raises; the caller turns that into `None`.
                    let _ = child.kill();
                    let _ = child.wait();
                    break None;
                }
                std::thread::sleep(Duration::from_millis(2));
            }
            Err(_) => break None,
        }
    };

    let out = reader.join().unwrap_or_default();
    drop(drainer.join());
    let status = status?;
    if !status.success() {
        // `if result.returncode != 0: return None`.
        return None;
    }
    // DIV-097: `text=True` decodes STRICTLY, so a latin-1 commit subject makes
    // Python raise `UnicodeDecodeError` past its `(TimeoutExpired, OSError)`
    // guard and 500 the whole endpoint. Decoding lossily is a deliberate
    // deviation — see the ledger before "fixing" a divergence here.
    Some(String::from_utf8_lossy(&out).into_owned())
}

// ── public API ───────────────────────────────────────────────────────────────

/// `_normalize_period` — `{"week": "7days"}.get(period, period)`.
#[must_use]
pub fn normalize_period(period: &str) -> &str {
    if period == "week" { "7days" } else { period }
}

/// `_max_sessions_per_project()` — the per-project session cap.
///
/// `None` means *no cap*. The environment is injected rather than read here
/// (campaign finding 5); the route passes `&|key| std::env::var(key).ok()`.
///
/// Parse rules, ported exactly: absent → `Some(200)`; `""` / `unlimited` /
/// `none` after `strip().lower()` → `None`; unparseable → `Some(200)`;
/// `<= 0` → `None`.
#[must_use]
pub fn max_sessions_per_project(env: &dyn Fn(&str) -> Option<String>) -> Option<usize> {
    let Some(raw) = env(MAX_SESSIONS_ENV) else {
        return Some(DEFAULT_MAX_SESSIONS_PER_PROJECT);
    };
    let raw = raw.trim().to_lowercase();
    if matches!(raw.as_str(), "" | "unlimited" | "none") {
        return None;
    }
    // `int(raw)` — CPython accepts a leading sign and surrounding whitespace
    // (already stripped) and nothing else. `i64` first, so a value larger than
    // `usize` is a parse success then a clamp, not a fallback to the default.
    let Ok(parsed) = raw.parse::<i64>() else {
        return Some(DEFAULT_MAX_SESSIONS_PER_PROJECT);
    };
    if parsed <= 0 {
        return None;
    }
    Some(usize::try_from(parsed).unwrap_or(usize::MAX))
}

/// `compute_yield(conn, period, project_filter)`.
///
/// Returns one entry per session inside `period`, **in start-time order** — the
/// public contract, which the route's cost sort is applied to a copy of.
///
/// `cap` is [`max_sessions_per_project`]'s answer, `now` is the instant
/// `parse_period` builds its bounds around, `git` is the subprocess seam and
/// `engine` prices the empty-mart fallback path (LAW 2: never `default_engine`
/// in a server path — DIV-056 mispriced by 2% through exactly that seam).
///
/// # Errors
/// SQLite failure, an unknown `period`, or the naive-vs-aware `TypeError` of
/// DIV-098.
///
/// # Panics
/// Never: the one `expect` covers a workspace inserted for every distinct `cwd`
/// two loops above, keyed by the same string.
pub fn compute_yield(
    conn: &Connection,
    period: &str,
    project_filter: Option<&[String]>,
    cap: Option<usize>,
    now: Instant,
    git: &dyn Git,
    engine: &PricingEngine,
) -> Result<Vec<YieldEntry>, YieldError> {
    let scope = parse_period(normalize_period(period), now).map_err(YieldError::Period)?;
    let rows = query_sessions(conn, &scope, project_filter, engine)?;
    let rows = cap_sessions_per_project(rows, cap);

    // `by_cwd.setdefault(cwd, []).append(row)` — bucket by cwd so the git work
    // is per DISTINCT directory. `order` keeps the dict's insertion order; it
    // does not reach the payload, but it does fix the order the subprocesses run
    // in, which is worth being able to reason about.
    let mut order: Vec<String> = Vec::new();
    let mut by_cwd: HashMap<String, Vec<usize>> = HashMap::new();
    for (index, row) in rows.iter().enumerate() {
        let bucket = by_cwd.entry(row.cwd.clone()).or_insert_with(|| {
            order.push(row.cwd.clone());
            Vec::new()
        });
        bucket.push(index);
    }

    let mut workspaces: HashMap<&str, GitWorkspace> = HashMap::new();
    for cwd in &order {
        // `if not cwd:` — an empty cwd never triggers a subprocess.
        if cwd.is_empty() {
            workspaces.insert(cwd.as_str(), GitWorkspace::empty());
            continue;
        }
        let indexes = by_cwd.get(cwd).map_or(&[][..], Vec::as_slice);
        // `sorted(s["started_at"] for s in sessions if s["started_at"])` — a
        // STRING sort over ISO stamps, mixed `Z` / `+00:00` shapes and all.
        let mut starts: Vec<&str> = indexes
            .iter()
            .map(|index| rows[*index].started_at.as_str())
            .filter(|started| !started.is_empty())
            .collect();
        starts.sort_unstable();
        workspaces.insert(cwd.as_str(), build_workspace(cwd, &starts, git));
    }

    // Re-emit in the ORIGINAL row order, not the bucket order.
    let mut entries = Vec::with_capacity(rows.len());
    for row in &rows {
        let workspace = workspaces
            .get(row.cwd.as_str())
            .expect("every distinct cwd got a workspace above");
        let outcome = workspace.classify(&row.started_at)?;
        entries.push(YieldEntry {
            session_id: row.session_id.clone(),
            project_slug: row.project_slug.clone(),
            cwd: row.cwd.clone(),
            started_at: row.started_at.clone(),
            cost_usd: row.cost_usd,
            classification: outcome.classification,
            follow_commit_sha: outcome.commit_sha,
            follow_commit_msg: outcome.commit_msg,
            follow_commit_age_hours: outcome.commit_age_hours,
        });
    }
    Ok(entries)
}

/// `yield_summary(entries)` — counts and cost totals, in the dict's key order.
///
/// **DIV-096.** The five cost fields are seeded with the float literal `0.0`
/// and stepped with `+=`. That is *not* `sum()`: there is no compensation, and
/// an empty list renders `0.0` rather than `sum()`'s int `0`. The four counts
/// and `total` are ints, seeded with `0` and stepped with `+= 1`.
///
/// Called by the route on the **unsorted** entries, while `entries` in the body
/// is the cost-sorted copy — so the addition order here is `compute_yield`'s
/// chronological one. That distinction is in the Python and it is preserved.
#[must_use]
pub fn yield_summary(entries: &[YieldEntry]) -> Value {
    let mut productive = 0_i64;
    let mut reverted = 0_i64;
    let mut abandoned = 0_i64;
    let mut no_repo = 0_i64;
    let mut total = 0_i64;
    let mut costs: HashMap<&str, f64> = HashMap::from([
        ("productive_cost", 0.0),
        ("reverted_cost", 0.0),
        ("abandoned_cost", 0.0),
        ("no_repo_cost", 0.0),
    ]);
    let mut total_cost = 0.0_f64;

    for entry in entries {
        match entry.classification {
            Classification::Productive => productive += 1,
            Classification::Reverted => reverted += 1,
            Classification::Abandoned => abandoned += 1,
            Classification::NoRepo => no_repo += 1,
        }
        total += 1;
        if let Some(bucket) = costs.get_mut(entry.classification.cost_key()) {
            *bucket += entry.cost_usd;
        }
        total_cost += entry.cost_usd;
    }

    let mut out = Map::new();
    out.insert("productive".to_owned(), Value::from(productive));
    out.insert("reverted".to_owned(), Value::from(reverted));
    out.insert("abandoned".to_owned(), Value::from(abandoned));
    out.insert("no_repo".to_owned(), Value::from(no_repo));
    out.insert("total".to_owned(), Value::from(total));
    for key in [
        "productive_cost",
        "reverted_cost",
        "abandoned_cost",
        "no_repo_cost",
    ] {
        out.insert(key.to_owned(), jf(costs.get(key).copied().unwrap_or(0.0)));
    }
    out.insert("total_cost".to_owned(), jf(total_cost));
    Value::Object(out)
}

/// `to_dicts(entries)`.
#[must_use]
pub fn to_dicts(entries: &[YieldEntry]) -> Vec<Value> {
    entries.iter().map(YieldEntry::to_dict).collect()
}

// ── session enumeration ──────────────────────────────────────────────────────

/// One row of `_query_sessions`' output.
#[derive(Debug, Clone)]
struct SessionRow {
    session_id: String,
    project_slug: String,
    cwd: String,
    started_at: String,
    cost_usd: f64,
}

/// `_query_sessions` — mart when it is materialised, `messages` otherwise.
fn query_sessions(
    conn: &Connection,
    scope: &Scope,
    project_filter: Option<&[String]>,
    engine: &PricingEngine,
) -> Result<Vec<SessionRow>, YieldError> {
    if mart_has_session_rows(conn)? {
        return query_sessions_from_mart(conn, scope, project_filter);
    }
    query_sessions_from_messages(conn, scope, project_filter, engine)
}

/// `_table_exists(conn, name)`.
fn table_exists(conn: &Connection, name: &str) -> rusqlite::Result<bool> {
    let mut stmt = conn.prepare("SELECT 1 FROM sqlite_master WHERE type='table' AND name=?")?;
    let mut rows = stmt.query([name])?;
    Ok(rows.next()?.is_some())
}

/// `mart_queries.mart_has_session_rows`.
///
/// DIV-099(a): duplicated here rather than imported — `services/mart_queries.rs`
/// belongs to another batch-C member and was an unported stub when this landed.
/// Collapse the two when it arrives.
fn mart_has_session_rows(conn: &Connection) -> rusqlite::Result<bool> {
    if !table_exists(conn, "session_mart")? {
        return Ok(false);
    }
    let mut stmt = conn.prepare("SELECT 1 FROM session_mart LIMIT 1")?;
    let mut rows = stmt.query([])?;
    Ok(rows.next()?.is_some())
}

/// One `session_mart_rows_for_yield` row, before the cwd join.
struct MartRow {
    session_id: String,
    project_slug: String,
    first_ts: String,
    cost_usd: f64,
    session_fk: Option<i64>,
}

/// `_query_sessions_from_mart` over `mart_queries.session_mart_rows_for_yield`.
///
/// DIV-099(a): see [`mart_has_session_rows`]. The SQL is transcribed verbatim,
/// `LEFT JOIN sessions` included — the mart has no `session_fk`, and the cwd
/// lookup needs the integer key. LAW 5: this is Python's join, not an
/// "improved" one.
fn query_sessions_from_mart(
    conn: &Connection,
    scope: &Scope,
    project_filter: Option<&[String]>,
) -> Result<Vec<SessionRow>, YieldError> {
    if !table_exists(conn, "session_mart")? {
        return Ok(Vec::new());
    }
    let mut sql = String::from(
        "SELECT m.session_id AS session_id, \
                p.slug AS project_slug, \
                p.provider AS provider, \
                m.project_id AS project_id, \
                m.first_ts AS first_ts, \
                m.primary_model AS primary_model, \
                m.cost_usd AS cost_usd, \
                sess.id AS session_fk \
         FROM session_mart m \
         JOIN projects p ON p.id = m.project_id \
         LEFT JOIN sessions sess \
                ON sess.project_id = m.project_id \
               AND sess.session_id = m.session_id \
         WHERE m.first_ts IS NOT NULL",
    );
    let mut params: Vec<String> = Vec::new();
    // `if since_iso:` / `if until_iso:` — truthiness, so an EMPTY bound is no
    // bound at all, not a bound of "".
    if let Some(since) = scope.since.as_ref().filter(|value| !value.is_empty()) {
        sql.push_str(" AND m.first_ts >= ?");
        params.push(since.clone());
    }
    if let Some(until) = scope.until.as_ref().filter(|value| !value.is_empty()) {
        sql.push_str(" AND m.first_ts <= ?");
        params.push(until.clone());
    }
    // `project_slugs=project_filter or None` at the call site, then
    // `if project_slugs:` here — an EMPTY list is no filter.
    if let Some(slugs) = project_filter.filter(|slugs| !slugs.is_empty()) {
        sql.push_str(" AND p.slug IN (");
        sql.push_str(&placeholders(slugs.len()));
        sql.push(')');
        params.extend(slugs.iter().cloned());
    }
    sql.push_str(" ORDER BY m.first_ts");

    let mut stmt = conn.prepare(&sql)?;
    let mart_rows = stmt
        .query_map(rusqlite::params_from_iter(params.iter()), |row| {
            Ok(MartRow {
                session_id: row
                    .get::<_, Option<String>>("session_id")?
                    .unwrap_or_default(),
                project_slug: row
                    .get::<_, Option<String>>("project_slug")?
                    .unwrap_or_default(),
                first_ts: row
                    .get::<_, Option<String>>("first_ts")?
                    .unwrap_or_default(),
                // `float(sess.get("cost_usd", 0.0) or 0.0)` — NULL and 0 alike.
                cost_usd: row.get::<_, Option<f64>>("cost_usd")?.unwrap_or(0.0),
                session_fk: row.get::<_, Option<i64>>("session_fk")?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let session_fks: Vec<i64> = mart_rows.iter().filter_map(|row| row.session_fk).collect();
    let cwd_by_fk = bulk_first_cwd_for_sessions(conn, &session_fks)?;

    Ok(mart_rows
        .into_iter()
        .map(|row| SessionRow {
            session_id: row.session_id,
            project_slug: row.project_slug,
            cwd: row
                .session_fk
                .and_then(|fk| cwd_by_fk.get(&fk).cloned())
                .unwrap_or_default(),
            started_at: row.first_ts,
            cost_usd: row.cost_usd,
        })
        .collect())
}

/// One `sessions` row on the empty-mart fallback path.
struct MessageSession {
    session_id: String,
    project_slug: String,
    started_at: String,
    session_fk: i64,
    provider: String,
}

/// `_query_sessions_from_messages` — the empty-mart fallback.
///
/// Prices each session through [`estimate_session_cost`], which is the only
/// place in this module that touches the [`PricingEngine`].
fn query_sessions_from_messages(
    conn: &Connection,
    scope: &Scope,
    project_filter: Option<&[String]>,
    engine: &PricingEngine,
) -> Result<Vec<SessionRow>, YieldError> {
    let mut sql = String::from(
        "SELECT s.session_id AS session_id, \
                p.slug AS project_slug, \
                p.provider AS provider, \
                s.first_ts AS started_at, \
                s.id AS session_fk \
         FROM sessions s \
         JOIN projects p ON p.id = s.project_id \
         WHERE s.first_ts IS NOT NULL ",
    );
    let mut params: Vec<String> = Vec::new();
    // `if scope.since is not None:` — note this leg tests for None, while the
    // mart leg tests truthiness. Both are transcribed as written.
    if let Some(since) = scope.since.as_ref() {
        sql.push_str("AND s.first_ts >= ? ");
        params.push(since.clone());
    }
    if let Some(until) = scope.until.as_ref() {
        sql.push_str("AND s.first_ts <= ? ");
        params.push(until.clone());
    }
    if let Some(slugs) = project_filter.filter(|slugs| !slugs.is_empty()) {
        sql.push_str("AND p.slug IN (");
        sql.push_str(&placeholders(slugs.len()));
        sql.push_str(") ");
        params.extend(slugs.iter().cloned());
    }
    sql.push_str("ORDER BY s.first_ts");

    let mut stmt = conn.prepare(&sql)?;
    let sessions = stmt
        .query_map(rusqlite::params_from_iter(params.iter()), |row| {
            Ok(MessageSession {
                session_id: row
                    .get::<_, Option<String>>("session_id")?
                    .unwrap_or_default(),
                project_slug: row
                    .get::<_, Option<String>>("project_slug")?
                    .unwrap_or_default(),
                started_at: row
                    .get::<_, Option<String>>("started_at")?
                    .unwrap_or_default(),
                session_fk: row.get::<_, Option<i64>>("session_fk")?.unwrap_or_default(),
                // `sess["provider"] or "anthropic"` — NULL *and* "" both fall
                // back, which is Python truthiness and not `is None`.
                provider: row
                    .get::<_, Option<String>>("provider")?
                    .filter(|value| !value.is_empty())
                    .unwrap_or_else(|| "anthropic".to_owned()),
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let session_fks: Vec<i64> = sessions.iter().map(|row| row.session_fk).collect();
    let cwd_by_fk = bulk_first_cwd_for_sessions(conn, &session_fks)?;

    let mut out = Vec::with_capacity(sessions.len());
    for session in sessions {
        let cost_usd = estimate_session_cost(conn, session.session_fk, &session.provider, engine)?;
        out.push(SessionRow {
            session_id: session.session_id,
            project_slug: session.project_slug,
            cwd: cwd_by_fk
                .get(&session.session_fk)
                .cloned()
                .unwrap_or_default(),
            started_at: session.started_at,
            cost_usd,
        });
    }
    Ok(out)
}

/// `",".join("?" for _ in seq)`.
fn placeholders(count: usize) -> String {
    let mut out = String::with_capacity(count * 2);
    for index in 0..count {
        if index > 0 {
            out.push(',');
        }
        out.push('?');
    }
    out
}

/// `_bulk_first_cwd_for_sessions` — `{session_fk: first_non_empty_cwd}`.
///
/// LAW 5: the filter is a bound `session_fk IN (…)` list, not a join against
/// `sessions`. `messages` is a partitioned VIEW, and a join makes the planner
/// materialise the whole thing — that is the July hang. `ROW_NUMBER() OVER
/// (PARTITION BY session_fk ORDER BY seq)` is what keeps this one round trip
/// per 500 sessions instead of one per session.
fn bulk_first_cwd_for_sessions(
    conn: &Connection,
    session_fks: &[i64],
) -> rusqlite::Result<HashMap<i64, String>> {
    if session_fks.is_empty() {
        return Ok(HashMap::new());
    }
    let mut out = HashMap::new();
    for chunk in session_fks.chunks(CWD_CHUNK_SIZE) {
        let sql = format!(
            "WITH ranked AS (SELECT session_fk, json_extract(raw_json, '$.cwd') AS cwd, \
             ROW_NUMBER() OVER (PARTITION BY session_fk ORDER BY seq) AS rn FROM messages \
             WHERE session_fk IN ({}) AND json_extract(raw_json, '$.cwd') IS NOT NULL \
             AND json_extract(raw_json, '$.cwd') != '') SELECT session_fk, cwd FROM ranked \
             WHERE rn = 1",
            placeholders(chunk.len())
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(chunk.iter()), |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, SqlValue>(1)?))
        })?;
        for row in rows {
            let (fk, cwd) = row?;
            out.insert(fk, py_str_or_empty(&cwd));
        }
    }
    Ok(out)
}

/// `str(row["cwd"] or "")` — Python truthiness first, then `str()`.
///
/// `json_extract` on a JSON string yields TEXT, which is the only shape this
/// ever sees in practice. The numeric legs exist because the SQL's `!= ''`
/// comparison does *not* exclude a numeric `cwd` (SQLite orders every number
/// before every string), so a `{"cwd": 0}` message would reach here.
fn py_str_or_empty(value: &SqlValue) -> String {
    match value {
        SqlValue::Null => String::new(),
        SqlValue::Text(text) => text.clone(),
        SqlValue::Integer(0) => String::new(),
        SqlValue::Integer(number) => number.to_string(),
        // `repr(float)`; `0.0` is falsy and becomes "".
        SqlValue::Real(number) if *number == 0.0 => String::new(),
        SqlValue::Real(number) => stax_memory::pyjson::dumps_http(&jf(*number)),
        // `str(b"…")` is `"b'…'"` in Python. Unreachable through `json_extract`;
        // the empty string is the honest stand-in rather than a fake `b'…'`.
        SqlValue::Blob(_) => String::new(),
    }
}

/// `_estimate_session_cost` — sum cost across `(model, speed)` groups.
///
/// The `GROUP BY model, speed` dimension is load-bearing: without `speed` an
/// Anthropic priority-tier row prices at 1× instead of 6× and the session's
/// spend is silently understated. `total += …` is a plain accumulation (LAW 3);
/// it is not `sum()` and must not be compensated.
fn estimate_session_cost(
    conn: &Connection,
    session_fk: i64,
    provider: &str,
    engine: &PricingEngine,
) -> rusqlite::Result<f64> {
    let mut stmt = conn.prepare(
        "SELECT COALESCE(model, '') AS model, \
                COALESCE(speed, 'standard') AS speed, \
                SUM(input_tokens) AS inp, \
                SUM(output_tokens) AS out, \
                SUM(cache_create_tokens) AS cc, \
                SUM(cache_read_tokens) AS cr \
         FROM messages WHERE session_fk = ? \
         GROUP BY model, speed",
    )?;
    let rows = stmt
        .query_map([session_fk], |row| {
            Ok((
                row.get::<_, String>("model")?,
                row.get::<_, Option<String>>("speed")?,
                row.get::<_, Option<i64>>("inp")?.unwrap_or(0),
                row.get::<_, Option<i64>>("out")?.unwrap_or(0),
                row.get::<_, Option<i64>>("cc")?.unwrap_or(0),
                row.get::<_, Option<i64>>("cr")?.unwrap_or(0),
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let mut total = 0.0_f64;
    for (model, speed, inp, out, cc, cr) in rows {
        // `if not model: continue` — the COALESCE'd empty string is skipped.
        if model.is_empty() {
            continue;
        }
        // `speed = r["speed"] or "standard"` — a second truthiness guard on top
        // of the COALESCE, because an empty-string `speed` survives the SQL.
        let speed = speed
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| "standard".to_owned());
        let tokens = RawTokens::canonical(inp, out, cc, cr);
        // `except Exception: logger.debug(...)` — a pricing failure must not
        // stall the report. The Rust engine returns a breakdown rather than
        // raising, so the guard has nothing to catch and no branch is lost.
        total += engine
            .compute_cost(&tokens, &model, provider, &speed, None)
            .total_cost;
    }
    Ok(total)
}

/// `_cap_sessions_per_project` — keep each project's most recent `cap` rows.
///
/// Bug-for-bug (ledger §6): `keep_ids` is a FLAT set of `session_id` across
/// every project, so a `session_id` that appears under two projects survives the
/// cap in the project where it was trimmed. Not fixed here.
fn cap_sessions_per_project(rows: Vec<SessionRow>, cap: Option<usize>) -> Vec<SessionRow> {
    let Some(cap) = cap else {
        return rows;
    };
    if rows.len() <= cap {
        return rows;
    }
    let mut by_project: HashMap<&str, Vec<&SessionRow>> = HashMap::new();
    for row in &rows {
        by_project
            .entry(row.project_slug.as_str())
            .or_default()
            .push(row);
    }
    let mut keep_ids: HashSet<String> = HashSet::new();
    for sessions in by_project.values() {
        // `sessions[-cap:]` — the chronological tail, because the SQL sorted by
        // `first_ts` and the grouping preserved it.
        let tail = if sessions.len() <= cap {
            &sessions[..]
        } else {
            &sessions[sessions.len() - cap..]
        };
        keep_ids.extend(tail.iter().map(|row| row.session_id.clone()));
    }
    rows.into_iter()
        .filter(|row| keep_ids.contains(&row.session_id))
        .collect()
}

// ── git introspection ────────────────────────────────────────────────────────

/// `@dataclass class _Commit`.
#[derive(Debug, Clone)]
struct Commit {
    sha: String,
    subject: String,
    /// The raw `%cI` string, which carries the *committer's* offset.
    committed_at: String,
    /// The same instant, pre-parsed. `None` is impossible by construction — a
    /// commit whose stamp will not parse is dropped rather than kept — but the
    /// `if ts is None: continue` guard is in the Python and is kept here.
    committed_at_utc: Option<PyDateTime>,
}

/// `@dataclass class _GitOutcome`.
#[derive(Debug, Clone)]
struct GitOutcome {
    classification: Classification,
    commit_sha: Option<String>,
    commit_msg: Option<String>,
    commit_age_hours: Option<f64>,
}

impl GitOutcome {
    fn bare(classification: Classification) -> Self {
        Self {
            classification,
            commit_sha: None,
            commit_msg: None,
            commit_age_hours: None,
        }
    }
}

/// `@dataclass class _GitWorkspace` — one repo's state, computed once per call.
#[derive(Debug, Default)]
struct GitWorkspace {
    is_repo: bool,
    /// Commits in the union of every session window, ascending by instant.
    commits: Vec<Commit>,
    /// SHAs reachable from `HEAD`.
    reachable_from_head: HashSet<String>,
    /// 7-char short SHAs mentioned in any revert subject, lowercased.
    revert_short_shas: HashSet<String>,
    /// Did `rev-list HEAD` succeed? An empty set means "reverted by
    /// unreachability" only when this is true — a brand-new or broken-HEAD repo
    /// must not mark every commit reverted.
    head_known: bool,
}

impl GitWorkspace {
    /// `_GitWorkspace.empty(cwd)`.
    fn empty() -> Self {
        Self::default()
    }

    /// `_GitWorkspace.classify(started_at)`.
    fn classify(&self, started_at: &str) -> Result<GitOutcome, YieldError> {
        if !self.is_repo {
            return Ok(GitOutcome::bare(Classification::NoRepo));
        }
        // `except ValueError: return _GitOutcome("no_repo")` — an unparseable
        // start is a non-repo, not an error. Contrast DIV-098's naive stamp,
        // which IS an error. Same field, two fates.
        let Some(start_dt) = parse_ts(started_at) else {
            return Ok(GitOutcome::bare(Classification::NoRepo));
        };
        let window_end_dt = start_dt.plus_minutes(FOLLOW_WINDOW_HOURS * 60);

        // The first commit chronologically inside [start, start+24h]. Compared
        // on the PARSED instant, never on the raw string — `%cI` carries the
        // committer's local offset, and string-comparing `…Z` against `…-04:00`
        // silently dropped valid commits in an earlier version of this code.
        let mut first: Option<&Commit> = None;
        for commit in &self.commits {
            let Some(ts) = commit.committed_at_utc else {
                continue;
            };
            // `if ts < start_dt or ts > window_end_dt: continue`.
            if cmp(ts, start_dt)?.is_lt() || cmp(ts, window_end_dt)?.is_gt() {
                continue;
            }
            let replace = match first {
                None => true,
                Some(current) => match current.committed_at_utc {
                    Some(current_ts) => cmp(ts, current_ts)?.is_lt(),
                    // Unreachable: `first` is only ever set from a commit whose
                    // stamp parsed. Python would `TypeError` on `ts < None`.
                    None => false,
                },
            };
            if replace {
                first = Some(commit);
            }
        }
        let Some(first) = first else {
            return Ok(GitOutcome::bare(Classification::Abandoned));
        };

        // Re-parses both stamps from their strings, exactly as Python does.
        let age = hours_between(started_at, &first.committed_at)?;
        let classification = if self.is_reverted(first) {
            Classification::Reverted
        } else {
            Classification::Productive
        };
        Ok(GitOutcome {
            classification,
            commit_sha: Some(first.sha.clone()),
            commit_msg: Some(first.subject.clone()),
            commit_age_hours: age,
        })
    }

    /// `_GitWorkspace._is_reverted` — the two-signal check, in memory.
    fn is_reverted(&self, commit: &Commit) -> bool {
        // `c.sha[:7]` — a character slice, and `%H` is ASCII hex, so the two
        // spellings cannot disagree.
        let short: String = commit.sha.chars().take(7).collect();
        if self.revert_short_shas.contains(&short) {
            return true;
        }
        // `if self.head_known and c.sha not in self.reachable_from_head` — the
        // `head_known` guard is why a fresh repo is not one big revert.
        self.head_known && !self.reachable_from_head.contains(&commit.sha)
    }
}

/// `_build_workspace(cwd, session_starts=…)`.
fn build_workspace(cwd: &str, session_starts: &[&str], git: &dyn Git) -> GitWorkspace {
    let mut workspace = GitWorkspace::empty();
    if !git.is_repo(cwd) {
        return workspace;
    }
    workspace.is_repo = true;

    // `if not session_starts: return ws` — is_repo stays true, so every session
    // in this cwd falls through to `abandoned` on an empty commit list.
    let (Some(earliest), Some(latest)) = (session_starts.first(), session_starts.last()) else {
        return workspace;
    };
    // `except ValueError: return ws` — an unparseable last start means no
    // window, which is again `abandoned` rather than an error.
    let Some(window_end) = add_hours_iso(latest, FOLLOW_WINDOW_HOURS) else {
        return workspace;
    };

    workspace.commits = bulk_git_log_window(cwd, earliest, &window_end, git);
    let (reachable, head_known) = bulk_reachable_from_head(cwd, git);
    workspace.reachable_from_head = reachable;
    workspace.head_known = head_known;
    workspace.revert_short_shas = bulk_revert_short_shas(cwd, git);
    workspace
}

/// `_bulk_git_log_window` — one `git log` over the union of every session
/// window, ascending by parsed instant (`git log` itself is newest-first).
fn bulk_git_log_window(cwd: &str, since: &str, until: &str, git: &dyn Git) -> Vec<Commit> {
    let since_arg = format!("--since={since}");
    let until_arg = format!("--until={until}");
    let max_arg = format!("--max-count={GIT_LOG_MAX_COMMITS}");
    let Some(out) = git.run(
        cwd,
        &[
            "log",
            "--all",
            &since_arg,
            &until_arg,
            &max_arg,
            "--format=%H|%cI|%s",
        ],
    ) else {
        return Vec::new();
    };

    let mut commits: Vec<Commit> = Vec::new();
    for line in split_lines(&out) {
        if line.trim().is_empty() {
            continue;
        }
        // `line.partition("|")` — no separator yields `(line, "", "")`.
        let (sha, rest) = partition(line, '|');
        let (committed_at, subject) = partition(rest, '|');
        if sha.is_empty() {
            continue;
        }
        // A stamp that will not parse drops the whole commit; keeping it with a
        // null instant would force a string-compare branch downstream.
        let Some(ts) = parse_ts(committed_at) else {
            continue;
        };
        commits.push(Commit {
            sha: sha.to_owned(),
            subject: subject.to_owned(),
            committed_at: committed_at.to_owned(),
            committed_at_utc: Some(ts),
        });
    }
    // `commits.sort(key=lambda c: c.committed_at_utc)` — stable, ascending.
    // Every stamp here parsed and `git log` output is single-frame, so the
    // mixed-awareness case cannot arise inside this sort.
    commits.sort_by(
        |left, right| match (left.committed_at_utc, right.committed_at_utc) {
            (Some(a), Some(b)) => a.cmp_instant(b).unwrap_or(std::cmp::Ordering::Equal),
            _ => std::cmp::Ordering::Equal,
        },
    );
    commits
}

/// `_bulk_reachable_from_head` — `(shas, head_known)`.
fn bulk_reachable_from_head(cwd: &str, git: &dyn Git) -> (HashSet<String>, bool) {
    let Some(out) = git.run(cwd, &["rev-list", "HEAD"]) else {
        return (HashSet::new(), false);
    };
    // An EMPTY stdout still means "we read HEAD successfully" — the `None` guard
    // above is the only thing that clears `head_known`.
    let shas = split_lines(&out)
        .into_iter()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect();
    (shas, true)
}

/// `_bulk_revert_short_shas` — every 7-hex run in a revert subject, lowercased.
fn bulk_revert_short_shas(cwd: &str, git: &dyn Git) -> HashSet<String> {
    let Some(out) = git.run(cwd, &["log", "--all", "--format=%s", "-i", "--grep=revert"]) else {
        return HashSet::new();
    };
    if out.trim().is_empty() {
        return HashSet::new();
    }
    let mut short_shas = HashSet::new();
    for line in split_lines(&out) {
        for word in word_runs(line) {
            // `re.compile(r"\b([0-9a-fA-F]{7})\b")`: the `\b` boundaries mean
            // the seven hex characters must be a WHOLE word-character run — so
            // `deadbee` matches and `deadbeef` (8) does not.
            if word.chars().count() == 7 && word.chars().all(|ch| ch.is_ascii_hexdigit()) {
                short_shas.insert(word.to_lowercase());
            }
        }
    }
    short_shas
}

/// Maximal runs of `\w` (`[A-Za-z0-9_]` plus Unicode alphanumerics, which is
/// what Python's `re` treats as word characters for `str` patterns).
fn word_runs(line: &str) -> Vec<&str> {
    let mut runs = Vec::new();
    let mut start: Option<usize> = None;
    for (index, ch) in line.char_indices() {
        let is_word = ch.is_alphanumeric() || ch == '_';
        match (is_word, start) {
            (true, None) => start = Some(index),
            (false, Some(begin)) => {
                runs.push(&line[begin..index]);
                start = None;
            }
            _ => {}
        }
    }
    if let Some(begin) = start {
        runs.push(&line[begin..]);
    }
    runs
}

/// `str.partition(sep)` reduced to the two halves the callers use.
fn partition(text: &str, sep: char) -> (&str, &str) {
    text.split_once(sep).unwrap_or((text, ""))
}

/// `str.splitlines()` — the FULL CPython separator set.
///
/// `str::lines()` would break on `\n` / `\r\n` only. A commit subject carrying a
/// vertical tab, a form feed, `\x1c`–`\x1e`, `\x85`, `U+2028` or `U+2029` splits
/// into two "commits" in Python, and reproducing that is cheaper than explaining
/// a one-repo divergence later.
fn split_lines(text: &str) -> Vec<&str> {
    fn is_sep(ch: char) -> bool {
        matches!(
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
        )
    }
    let mut out = Vec::new();
    let mut start = 0_usize;
    let mut chars = text.char_indices().peekable();
    while let Some((index, ch)) = chars.next() {
        if !is_sep(ch) {
            continue;
        }
        out.push(&text[start..index]);
        let mut next = index + ch.len_utf8();
        // `\r\n` is ONE separator.
        if ch == '\r' && chars.peek().map(|(_, next_ch)| *next_ch) == Some('\n') {
            chars.next();
            next += 1;
        }
        start = next;
    }
    if start < text.len() {
        out.push(&text[start..]);
    }
    out
}

// ── time helpers ─────────────────────────────────────────────────────────────

/// `a < b` / `a > b` between two `datetime`s, with CPython's `TypeError` for the
/// mixed naive/aware case surfaced instead of guessed. DIV-098.
fn cmp(left: PyDateTime, right: PyDateTime) -> Result<std::cmp::Ordering, YieldError> {
    left.cmp_instant(right).ok_or(YieldError::NaiveVsAware)
}

/// `_add_hours_iso(iso_ts, hours)` — `(dt + timedelta(hours=…)).isoformat()`.
///
/// `None` is the `ValueError` the caller catches. The offset is preserved, so a
/// `-04:00` start yields a `-04:00` window end; `git log --until` reads it fine.
fn add_hours_iso(iso_ts: &str, hours: i64) -> Option<String> {
    Some(isoformat(parse_ts(iso_ts)?.plus_minutes(hours * 60)))
}

/// `_hours_between(start_iso, end_iso)`.
///
/// `Ok(None)` is Python's `except ValueError: return None`; the mixed
/// naive/aware case is the uncaught `TypeError` of DIV-098.
fn hours_between(start_iso: &str, end_iso: &str) -> Result<Option<f64>, YieldError> {
    let (Some(end), Some(start)) = (parse_ts(end_iso), parse_ts(start_iso)) else {
        return Ok(None);
    };
    let seconds = end
        .sub_total_seconds(start)
        .ok_or(YieldError::NaiveVsAware)?;
    Ok(Some(seconds / 3600.0))
}

/// `datetime.isoformat()` for a value parsed by
/// [`stax_etl::stats::pydatetime::parse_ts`].
///
/// The fraction appears only when the microsecond is non-zero, and the offset is
/// `±HH:MM` — widening to `±HH:MM:SS` only for the sub-minute offsets CPython
/// spells that way (no real zone has one; the branch exists so a synthetic stamp
/// round-trips rather than truncating).
fn isoformat(value: PyDateTime) -> String {
    use std::fmt::Write as _;

    let seconds = value.wall_us.div_euclid(1_000_000);
    let micro = value.wall_us.rem_euclid(1_000_000);
    let days = seconds.div_euclid(86_400);
    let secs_of_day = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let (hour, minute, second) = (
        secs_of_day / 3600,
        (secs_of_day % 3600) / 60,
        secs_of_day % 60,
    );

    let mut out = format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}");
    if micro != 0 {
        let _ = write!(out, ".{micro:06}");
    }
    if let Some(offset) = value.offset_s {
        let sign = if offset < 0 { '-' } else { '+' };
        let magnitude = offset.abs();
        let (oh, om, os) = (magnitude / 3600, (magnitude % 3600) / 60, magnitude % 60);
        let _ = write!(out, "{sign}{oh:02}:{om:02}");
        if os != 0 {
            let _ = write!(out, ":{os:02}");
        }
    }
    out
}

/// Howard Hinnant's `civil_from_days`, the algorithm CPython's `datetime` uses.
/// (`services/scope.rs` has its own copy; it is private there.)
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };
    (year, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scripted repo: every `git` invocation answers from a table.
    #[derive(Default)]
    struct FakeGit {
        repos: HashSet<String>,
        log: HashMap<String, String>,
        rev_list: HashMap<String, String>,
        greps: HashMap<String, String>,
    }

    impl Git for FakeGit {
        fn is_repo(&self, cwd: &str) -> bool {
            self.repos.contains(cwd)
        }

        fn run(&self, cwd: &str, args: &[&str]) -> Option<String> {
            match args.first().copied() {
                Some("rev-list") => self.rev_list.get(cwd).cloned(),
                Some("log") if args.contains(&"--grep=revert") => self.greps.get(cwd).cloned(),
                Some("log") => self.log.get(cwd).cloned(),
                _ => None,
            }
        }
    }

    fn entry(cost: f64, classification: Classification, session: &str) -> YieldEntry {
        YieldEntry {
            session_id: session.to_owned(),
            project_slug: "p".to_owned(),
            cwd: "/repo".to_owned(),
            started_at: "2026-07-01T00:00:00+00:00".to_owned(),
            cost_usd: cost,
            classification,
            follow_commit_sha: None,
            follow_commit_msg: None,
            follow_commit_age_hours: None,
        }
    }

    #[test]
    fn an_empty_entry_list_summarises_to_int_counts_and_float_zero_costs() {
        // DIV-096: the cost seeds are the float literal `0.0`, NOT `sum()`'s
        // int `0`. `"total_cost":0` instead of `0.0` is a one-byte divergence
        // on every empty window, which is most days for `period=today`.
        assert_eq!(
            stax_memory::pyjson::dumps_http(&yield_summary(&[])),
            r#"{"productive":0,"reverted":0,"abandoned":0,"no_repo":0,"total":0,"productive_cost":0.0,"reverted_cost":0.0,"abandoned_cost":0.0,"no_repo_cost":0.0,"total_cost":0.0}"#
        );
    }

    #[test]
    fn the_summary_buckets_by_classification_and_totals_everything() {
        let entries = vec![
            entry(1.5, Classification::Productive, "a"),
            entry(0.25, Classification::Reverted, "b"),
            entry(2.0, Classification::Productive, "c"),
            entry(0.0, Classification::NoRepo, "d"),
        ];
        assert_eq!(
            stax_memory::pyjson::dumps_http(&yield_summary(&entries)),
            r#"{"productive":2,"reverted":1,"abandoned":0,"no_repo":1,"total":4,"productive_cost":3.5,"reverted_cost":0.25,"abandoned_cost":0.0,"no_repo_cost":0.0,"total_cost":3.75}"#
        );
    }

    #[test]
    fn an_entry_renders_its_declaration_order_with_a_float_zero_cost() {
        let mut value = entry(0.0, Classification::Abandoned, "s1");
        value.follow_commit_msg = Some(String::new());
        assert_eq!(
            stax_memory::pyjson::dumps_http(&value.to_dict()),
            r#"{"session_id":"s1","project_slug":"p","cwd":"/repo","started_at":"2026-07-01T00:00:00+00:00","cost_usd":0.0,"classification":"abandoned","follow_commit_sha":null,"follow_commit_msg":"","follow_commit_age_hours":null}"#
        );
    }

    #[test]
    fn the_period_alias_is_rewritten_before_parse_period_sees_it() {
        assert_eq!(normalize_period("week"), "7days");
        assert_eq!(normalize_period("7days"), "7days");
        assert_eq!(normalize_period("month"), "month");
        // `parse_period` itself has never heard of `week` — proving the rewrite
        // is the only thing keeping the route's allow-list honest.
        let now = Instant::from_parts(2026, 7, 31, 12, 0, 0, 0);
        assert!(parse_period("week", now).is_err());
        assert!(parse_period(normalize_period("week"), now).is_ok());
    }

    #[test]
    fn the_session_cap_reads_pythons_whole_vocabulary() {
        let none = |_: &str| None;
        assert_eq!(max_sessions_per_project(&none), Some(200));
        let set = |value: &'static str| move |_: &str| Some(value.to_owned());
        assert_eq!(max_sessions_per_project(&set("50")), Some(50));
        assert_eq!(max_sessions_per_project(&set("  50 ")), Some(50));
        // Every disabling spelling.
        assert_eq!(max_sessions_per_project(&set("")), None);
        assert_eq!(max_sessions_per_project(&set("UNLIMITED")), None);
        assert_eq!(max_sessions_per_project(&set("none")), None);
        assert_eq!(max_sessions_per_project(&set("0")), None);
        assert_eq!(max_sessions_per_project(&set("-3")), None);
        // Unparseable falls back to the DEFAULT, not to "no cap" — the
        // conservative direction, and the one a careless port inverts.
        assert_eq!(max_sessions_per_project(&set("many")), Some(200));
    }

    fn session_row(slug: &str, id: &str) -> SessionRow {
        SessionRow {
            session_id: id.to_owned(),
            project_slug: slug.to_owned(),
            cwd: String::new(),
            started_at: String::new(),
            cost_usd: 0.0,
        }
    }

    #[test]
    fn the_cap_keeps_the_chronological_tail_per_project_and_the_row_order() {
        let rows = vec![
            session_row("a", "a1"),
            session_row("b", "b1"),
            session_row("a", "a2"),
            session_row("b", "b2"),
            session_row("a", "a3"),
        ];
        let capped = cap_sessions_per_project(rows, Some(2));
        // `a1` is dropped (project `a` has three); the survivors keep the
        // ORIGINAL interleaved order, not a per-project regrouping.
        assert_eq!(
            capped
                .iter()
                .map(|row| row.session_id.as_str())
                .collect::<Vec<_>>(),
            vec!["b1", "a2", "b2", "a3"]
        );
    }

    #[test]
    fn a_cap_at_or_above_the_row_count_short_circuits_before_grouping() {
        let rows = vec![session_row("a", "only")];
        assert_eq!(cap_sessions_per_project(rows.clone(), Some(1)).len(), 1);
        assert_eq!(cap_sessions_per_project(rows, None).len(), 1);
    }

    fn workspace_for(log: &str, rev_list: &str, grep: &str) -> GitWorkspace {
        let mut git = FakeGit::default();
        git.repos.insert("/repo".to_owned());
        git.log.insert("/repo".to_owned(), log.to_owned());
        git.rev_list.insert("/repo".to_owned(), rev_list.to_owned());
        git.greps.insert("/repo".to_owned(), grep.to_owned());
        build_workspace("/repo", &["2026-07-01T00:00:00+00:00"], &git)
    }

    #[test]
    fn a_commit_inside_the_window_is_productive_and_carries_its_age() {
        let ws = workspace_for(
            "aaaaaaabbbbbbbcccccccddddddd1111111222|2026-07-01T06:30:00+00:00|feat: land it\n",
            "aaaaaaabbbbbbbcccccccddddddd1111111222\n",
            "",
        );
        let outcome = ws.classify("2026-07-01T00:00:00+00:00").expect("aware");
        assert_eq!(outcome.classification, Classification::Productive);
        assert_eq!(outcome.commit_msg.as_deref(), Some("feat: land it"));
        assert_eq!(outcome.commit_age_hours, Some(6.5));
    }

    #[test]
    fn the_earliest_commit_in_the_window_wins_not_the_first_line_git_printed() {
        // `git log` is newest-first; the classifier must pick the EARLIEST
        // in-window commit, so a port that took `commits[0]` off the raw output
        // would credit the wrong sha here.
        let ws = workspace_for(
            "bbbbbbb0000000000000000000000000000000|2026-07-01T20:00:00+00:00|late\n\
             aaaaaaa0000000000000000000000000000000|2026-07-01T02:00:00+00:00|early\n",
            "aaaaaaa0000000000000000000000000000000\nbbbbbbb0000000000000000000000000000000\n",
            "",
        );
        let outcome = ws.classify("2026-07-01T00:00:00+00:00").expect("aware");
        assert_eq!(outcome.commit_msg.as_deref(), Some("early"));
    }

    #[test]
    fn a_commit_past_the_twenty_four_hour_window_leaves_the_session_abandoned() {
        let ws = workspace_for(
            "aaaaaaa0000000000000000000000000000000|2026-07-02T00:00:01+00:00|too late\n",
            "aaaaaaa0000000000000000000000000000000\n",
            "",
        );
        let outcome = ws.classify("2026-07-01T00:00:00+00:00").expect("aware");
        assert_eq!(outcome.classification, Classification::Abandoned);
        assert_eq!(outcome.commit_sha, None);
        // Exactly 24h is still INSIDE — the bound is `ts > window_end`.
        let ws = workspace_for(
            "aaaaaaa0000000000000000000000000000000|2026-07-02T00:00:00+00:00|on the edge\n",
            "aaaaaaa0000000000000000000000000000000\n",
            "",
        );
        assert_eq!(
            ws.classify("2026-07-01T00:00:00+00:00")
                .expect("aware")
                .classification,
            Classification::Productive
        );
    }

    #[test]
    fn a_short_sha_named_in_a_revert_subject_flips_the_classification() {
        let ws = workspace_for(
            "deadbee0000000000000000000000000000000|2026-07-01T06:00:00+00:00|feat: oops\n",
            "deadbee0000000000000000000000000000000\n",
            "Revert \"feat: oops\" (deadbee)\n",
        );
        assert_eq!(
            ws.classify("2026-07-01T00:00:00+00:00")
                .expect("aware")
                .classification,
            Classification::Reverted
        );
    }

    #[test]
    fn an_eight_character_hex_word_is_not_a_short_sha() {
        // `\b([0-9a-fA-F]{7})\b` needs the run to be exactly seven long, so an
        // 8-hex word must NOT register. A port that scanned for any 7-char
        // substring would flag it and revert a healthy commit.
        let ws = workspace_for(
            "deadbee0000000000000000000000000000000|2026-07-01T06:00:00+00:00|feat: fine\n",
            "deadbee0000000000000000000000000000000\n",
            "Revert something deadbeef here\n",
        );
        assert_eq!(
            ws.classify("2026-07-01T00:00:00+00:00")
                .expect("aware")
                .classification,
            Classification::Productive
        );
    }

    #[test]
    fn an_unreachable_commit_is_reverted_but_only_when_head_was_readable() {
        let ws = workspace_for(
            "aaaaaaa0000000000000000000000000000000|2026-07-01T06:00:00+00:00|rebased away\n",
            "bbbbbbb0000000000000000000000000000000\n",
            "",
        );
        assert_eq!(
            ws.classify("2026-07-01T00:00:00+00:00")
                .expect("aware")
                .classification,
            Classification::Reverted
        );

        // `rev-list` failing must NOT turn every commit into a revert — that is
        // what `head_known` is for, and it is the whole reason the helper
        // returns a tuple instead of a set.
        let mut git = FakeGit::default();
        git.repos.insert("/repo".to_owned());
        git.log.insert(
            "/repo".to_owned(),
            "aaaaaaa0000000000000000000000000000000|2026-07-01T06:00:00+00:00|new repo\n"
                .to_owned(),
        );
        let ws = build_workspace("/repo", &["2026-07-01T00:00:00+00:00"], &git);
        assert!(!ws.head_known);
        assert_eq!(
            ws.classify("2026-07-01T00:00:00+00:00")
                .expect("aware")
                .classification,
            Classification::Productive
        );
    }

    #[test]
    fn a_non_repo_cwd_classifies_without_ever_calling_git() {
        let git = FakeGit::default();
        let ws = build_workspace("/not-a-repo", &["2026-07-01T00:00:00+00:00"], &git);
        assert_eq!(
            ws.classify("2026-07-01T00:00:00+00:00")
                .expect("aware")
                .classification,
            Classification::NoRepo
        );
        // And an empty workspace (the empty-cwd short circuit) is the same.
        assert_eq!(
            GitWorkspace::empty()
                .classify("2026-07-01T00:00:00+00:00")
                .expect("aware")
                .classification,
            Classification::NoRepo
        );
    }

    #[test]
    fn an_unparseable_start_is_no_repo_but_a_naive_one_is_an_error() {
        let ws = workspace_for(
            "aaaaaaa0000000000000000000000000000000|2026-07-01T06:00:00+00:00|x\n",
            "aaaaaaa0000000000000000000000000000000\n",
            "",
        );
        // `except ValueError` — caught, and it becomes `no_repo`.
        assert_eq!(
            ws.classify("not a timestamp")
                .expect("caught")
                .classification,
            Classification::NoRepo
        );
        // DIV-098: naive vs aware is a `TypeError`, which nothing catches.
        assert!(matches!(
            ws.classify("2026-07-01T00:00:00"),
            Err(YieldError::NaiveVsAware)
        ));
    }

    #[test]
    fn a_commit_line_without_a_parseable_timestamp_is_dropped_not_kept_as_null() {
        let ws = workspace_for(
            "aaaaaaa0000000000000000000000000000000\n\
             bbbbbbb0000000000000000000000000000000|garbage|subject\n\
             \n\
             ccccccc0000000000000000000000000000000|2026-07-01T01:00:00+00:00|good\n",
            "ccccccc0000000000000000000000000000000\n",
            "",
        );
        assert_eq!(ws.commits.len(), 1);
        assert_eq!(ws.commits[0].subject, "good");
    }

    #[test]
    fn splitlines_breaks_where_cpython_breaks_and_str_lines_does_not() {
        assert_eq!(split_lines("a\nb\r\nc\rd"), vec!["a", "b", "c", "d"]);
        // The separators `str::lines()` would have missed.
        assert_eq!(
            split_lines("a\u{0b}b\u{0c}c\u{85}d"),
            vec!["a", "b", "c", "d"]
        );
        assert_eq!(split_lines("a\u{2028}b"), vec!["a", "b"]);
        assert_eq!(split_lines(""), Vec::<&str>::new());
        assert_eq!(split_lines("trailing\n"), vec!["trailing"]);
    }

    #[test]
    fn add_hours_keeps_the_stamps_own_offset_and_its_microseconds() {
        assert_eq!(
            add_hours_iso("2026-07-01T00:00:00+00:00", 24).as_deref(),
            Some("2026-07-02T00:00:00+00:00")
        );
        // A `Z` stamp normalises to `+00:00`, exactly as `fromisoformat` +
        // `isoformat()` round-trip it in Python.
        assert_eq!(
            add_hours_iso("2026-07-01T12:00:00Z", 24).as_deref(),
            Some("2026-07-02T12:00:00+00:00")
        );
        // A local offset survives, and the fraction only prints when non-zero.
        assert_eq!(
            add_hours_iso("2026-07-01T23:30:00.500000-04:00", 24).as_deref(),
            Some("2026-07-02T23:30:00.500000-04:00")
        );
        assert_eq!(add_hours_iso("nonsense", 24), None);
    }

    #[test]
    fn hours_between_is_a_float_and_reads_both_stamps_offsets() {
        assert_eq!(
            hours_between("2026-07-01T00:00:00+00:00", "2026-07-01T01:30:00+00:00").expect("aware"),
            Some(1.5)
        );
        // Both aware, different offsets — a real instant difference, not a
        // string one: `04:00-04:00` is `08:00+00:00`.
        assert_eq!(
            hours_between("2026-07-01T00:00:00+00:00", "2026-07-01T04:00:00-04:00").expect("aware"),
            Some(8.0)
        );
        assert_eq!(
            hours_between("junk", "2026-07-01T00:00:00Z").expect("caught"),
            None
        );
        assert!(matches!(
            hours_between("2026-07-01T00:00:00", "2026-07-01T01:00:00+00:00"),
            Err(YieldError::NaiveVsAware)
        ));
    }

    #[test]
    fn a_falsy_sqlite_cwd_becomes_the_empty_string_not_its_repr() {
        assert_eq!(py_str_or_empty(&SqlValue::Null), "");
        assert_eq!(py_str_or_empty(&SqlValue::Integer(0)), "");
        assert_eq!(py_str_or_empty(&SqlValue::Real(0.0)), "");
        assert_eq!(
            py_str_or_empty(&SqlValue::Text("/repo".to_owned())),
            "/repo"
        );
        assert_eq!(py_str_or_empty(&SqlValue::Integer(7)), "7");
    }

    #[test]
    fn the_real_git_runner_answers_none_for_a_path_that_is_not_a_repo() {
        // The one test that spawns: proves the timeout/pipe plumbing does not
        // hang or panic on the ordinary failure path. `/` is a directory and is
        // not a git repo on any machine this runs on.
        assert!(!SystemGit.is_repo("/nonexistent-directory-for-yield-parity"));
        assert!(SystemGit.run("/", &["rev-parse", "--git-dir"]).is_none());
    }
}
