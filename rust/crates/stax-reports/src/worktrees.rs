//! `services/worktrees.py` — detect, attribute and preview-prune git worktrees.
//!
//! | Item | Python | Rust |
//! |---|---|---|
//! | `list_worktrees(conn, project_root)` | 753 ln module | [`list_worktrees`] |
//! | `attribute_fragments(conn)` | ↑ | [`attribute_fragments`] |
//! | `is_worktree_slug(slug)` | ↑ | [`is_worktree_slug`] |
//! | `WorktreeInfo` (dataclass) | ↑ | [`WorktreeInfo`] |
//!
//! Consumed by `routes/worktrees.rs` (`GET /api/worktrees`,
//! `POST /api/worktrees/attribute`) and, in wave 8, by the `stackunderflow
//! worktrees` CLI verb — which is why the logic lives here and not in the route.
//!
//! # This module shells out to `git`, and that is the whole story
//!
//! `/api/worktrees` answers from **the machine's working trees**, not from the
//! store. Six distinct `git` invocations run per request (the full argv table is
//! in `parity/DIV-e-worktrees.md`), plus two filesystem probes and one clock
//! read. Everything is read-only — the chokepoint [`run_git`] refuses any
//! subcommand outside a five-entry allow-list and every call carries
//! `--no-optional-locks`, so not even `status` may refresh the index. Nothing
//! here writes a repo, a file, or a store row. The one writer in the module is
//! [`attribute_fragments`], which touches one additive column on `projects` and
//! never git.
//!
//! The batch-C precedent is `services/yield_tracker.rs`, which does the same
//! thing for `GET /api/yield` (DIV-095). Two of its findings apply directly and
//! one **does not**:
//!
//! * DIV-095 — a payload computed from live working trees cannot be pinned by a
//!   differ. Enumerated command by command in `parity/DIV-e-worktrees.md`.
//! * DIV-097 — `text=True` decodes STRICTLY, so non-UTF-8 git output raises
//!   `UnicodeDecodeError`. In `yield_tracker.py` that escapes a
//!   `except (TimeoutExpired, OSError)` guard and 500s the endpoint. **Here it
//!   does not**: `_run_git`'s guard is `except Exception`, so a non-UTF-8 byte
//!   degrades to `None` like every other failure. The port therefore refuses to
//!   decode lossily — see [`spawn_git`].
//! * DIV-098 — no two timestamps are compared in this module at all; the only
//!   clock arithmetic is `time.time() - st_mtime`, which cannot be
//!   naive-vs-aware.
//!
//! # What is load-bearing
//!
//! * **There is no `sum()` over floats here.** `_unique_commits` and
//!   `_dirty_count` are `sum(1 for …)` over a generator — integer counting, so
//!   LAW 3's Neumaier rule has nothing to compensate. The route's
//!   `attributed_cost_usd` *is* a float `+=` chain seeded with `0.0`; that lives
//!   in the route module.
//! * **`_table_exists` here is `type IN ('table','view')`** — LAW 7's
//!   distinction. `store/mart_queries.py`'s namesake is `type='table'` and would
//!   be the wrong guard. It is spelled out locally rather than imported from
//!   [`crate::mart_queries`], which owns the table-only spelling.
//! * **`json_extract` is evaluated ONCE**, in an inner CTE. Python's own commit
//!   `98e7f8b` made that change ("the cwd scan parsed every blob 3×", 1.56 s →
//!   1.19 s on a 3.9 GB store); the SQL SHAPE is reproduced, not just the
//!   result.
//! * **The verdict reads the UNROUNDED age.** `_verdict` is called with `age`
//!   and the payload carries `round(age, 2)`, so a worktree at 2.000_4 days is
//!   past the window while its `age_days` renders `2.0`.
//! * **`git cherry` runs in the REPO ROOT, `git status` runs in the WORKTREE.**
//!   Two different `cwd`s in the same function, and swapping them silently
//!   changes both answers.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use rusqlite::Connection;
use rusqlite::types::Value as SqlValue;
use serde_json::{Map, Value};
use stax_etl::stats::aggregator::{jf, round_py};

// ── tunables (the module's `# ── tunables` block) ────────────────────────────

/// `_GIT_TIMEOUT_SECONDS = 5`.
const GIT_TIMEOUT: Duration = Duration::from_secs(5);

/// `_ACTIVE_WINDOW_HOURS = 48.0` — mtime inside this window wins over
/// everything else.
const ACTIVE_WINDOW_HOURS: f64 = 48.0;

/// `_MAX_SESSIONS_SCANNED = 500`.
const MAX_SESSIONS_SCANNED: i64 = 500;

/// `_MAX_DISTINCT_CWDS = 50`.
const MAX_DISTINCT_CWDS: usize = 50;

/// `_bulk_first_cwd`'s `chunk_size = 500` — under SQLite's default variable cap.
const CWD_CHUNK_SIZE: usize = 500;

/// `VERDICT_ACTIVE`.
pub const VERDICT_ACTIVE: &str = "ACTIVE";

/// `VERDICT_MERGED_SAFE_TO_PRUNE`.
pub const VERDICT_MERGED_SAFE_TO_PRUNE: &str = "MERGED_SAFE_TO_PRUNE";

/// `VERDICT_HAS_UNIQUE_WORK`.
pub const VERDICT_HAS_UNIQUE_WORK: &str = "HAS_UNIQUE_WORK";

/// `_WORKTREE_SLUG_MARKERS` — order does not decide the winner; the LEFTMOST
/// hit across both markers does.
const WORKTREE_SLUG_MARKERS: [&str; 2] = ["--claude-worktrees-", "--worktrees-"];

/// `_ALLOWED_GIT_PREFIXES` — the complete set of subcommands [`run_git`] will
/// spawn. Every one is read-only; the allow-list is what makes that a property
/// of the chokepoint rather than of reviewer vigilance.
const ALLOWED_GIT_PREFIXES: [&[&str]; 5] = [
    &["worktree", "list"],
    &["status", "--porcelain"],
    &["cherry"],
    &["rev-parse"],
    &["symbolic-ref"],
];

// ── the host seam ────────────────────────────────────────────────────────────

/// Everything this module reaches outside its own process: `git`, two `stat`
/// calls and the wall clock.
///
/// Injected for the same reason `yield_tracker::Git` is — the interesting bugs
/// live in the parsing and the verdict table, and neither should need a repo on
/// disk to test. [`SystemHost`] is the production implementation and the only
/// one that spawns anything.
///
/// The trait is deliberately WIDER than `yield_tracker`'s `Git`: `_age_days`
/// reads `st_mtime` and `time.time()`, so a fake that only stubbed subprocesses
/// would still leave the verdict table pinned to the real clock.
pub trait Host {
    /// `_run_git`'s subprocess half — stdout on success, `None` on any failure.
    ///
    /// The allow-list check is NOT here: it lives in [`run_git`], the
    /// module-level chokepoint every caller goes through, exactly where Python's
    /// `_run_git` does it — before `subprocess.run`.
    fn run(&self, cwd: &str, args: &[&str]) -> Option<String>;

    /// `Path(p).is_dir()` — follows symlinks, false on any stat error.
    fn is_dir(&self, path: &str) -> bool;

    /// `Path(p).stat().st_mtime`, or `None` for the `OSError`.
    fn mtime_secs(&self, path: &str) -> Option<f64>;

    /// `time.time()`.
    fn now_secs(&self) -> f64;

    /// `Path(p).resolve()` — `None` for the `OSError` the caller catches.
    fn resolve(&self, path: &Path) -> Option<PathBuf>;
}

/// The real thing: `subprocess.run(["git", "--no-optional-locks", "-C", cwd,
/// *argv], capture_output=True, text=True, timeout=5)`.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemHost;

impl Host for SystemHost {
    fn run(&self, cwd: &str, args: &[&str]) -> Option<String> {
        spawn_git(cwd, args, GIT_TIMEOUT)
    }

    fn is_dir(&self, path: &str) -> bool {
        // NOTE: `Path("")` is `PosixPath(".")` in pathlib, so Python would stat
        // the SERVER's own working directory where `Path::new("").is_dir()` is
        // false. Unreachable — `list_worktrees` skips a falsy root and
        // `_candidate_roots_from_store` only emits non-empty cwds — so the two
        // cannot be told apart from outside. Recorded rather than emulated, for
        // the same reason DIV-c-yield §11 recorded it: emulating it would make
        // the port scan its own working directory.
        Path::new(path).is_dir()
    }

    fn mtime_secs(&self, path: &str) -> Option<f64> {
        let modified = std::fs::metadata(path).ok()?.modified().ok()?;
        Some(match modified.duration_since(std::time::UNIX_EPOCH) {
            Ok(delta) => delta.as_secs_f64(),
            Err(err) => -err.duration().as_secs_f64(),
        })
    }

    fn now_secs(&self) -> f64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0.0, |delta| delta.as_secs_f64())
    }

    fn resolve(&self, path: &Path) -> Option<PathBuf> {
        // `Path.resolve()` with the default `strict=False` normalises what it
        // can and does not require the path to exist; `canonicalize` requires
        // it. The caller's `except OSError` fallback covers the gap, and the one
        // live input is a git common dir, which exists by construction.
        std::fs::canonicalize(path).ok()
    }
}

/// Spawn `git --no-optional-locks -C <cwd> <args…>` with a wall-clock ceiling.
///
/// The pipes are drained on their own threads before the exit status is waited
/// on: `git worktree list --porcelain` on a repo with many worktrees, or
/// `git status --porcelain` on a dirty tree, is easily more than a pipe buffer,
/// and a single-threaded "wait then read" deadlocks on exactly those repos.
fn spawn_git(cwd: &str, args: &[&str], timeout: Duration) -> Option<String> {
    let mut child = Command::new("git")
        // `--no-optional-locks` is a GLOBAL option and comes before `-C`, which
        // is the order Python builds the argv in. git accepts either; the argv
        // is part of what this port promises to reproduce.
        .arg("--no-optional-locks")
        .arg("-C")
        .arg(cwd)
        .args(args)
        // Python inherits stdin; `null` is the safer spelling of the same
        // outcome for six read-only verbs, none of which reads stdin.
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        // `except Exception` — git missing from PATH lands here, as does a
        // `cwd` that is not a directory.
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
        buf
    });

    let deadline = std::time::Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    // `subprocess.TimeoutExpired` — run() kills the child and
                    // re-raises; `except Exception` turns that into `None`.
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
    let err = drainer.join().unwrap_or_default();
    let status = status?;
    // `text=True` decodes BOTH streams strictly, inside `run()` and therefore
    // inside the `try`. A non-UTF-8 byte on EITHER pipe is a
    // `UnicodeDecodeError` that `except Exception` swallows into `None` — so
    // stderr can fail a call whose stdout is perfectly clean. Contrast DIV-097,
    // where `yield_tracker`'s narrower `except (TimeoutExpired, OSError)` lets
    // the same error 500 the request and the port decodes lossily instead.
    let text = String::from_utf8(out).ok()?;
    String::from_utf8(err).ok()?;
    if !status.success() {
        // `if result.returncode != 0: return None`.
        return None;
    }
    Some(text)
}

/// `_run_git` — the allow-list chokepoint, then the spawn.
///
/// Answers `None` for any argv whose leading elements do not match one of
/// [`ALLOWED_GIT_PREFIXES`] exactly. Python logs a warning here; the port has no
/// logger, and the refusal itself is what the tests pin.
fn run_git(host: &dyn Host, cwd: &str, args: &[&str]) -> Option<String> {
    // `tuple(argv[:len(prefix)]) == prefix` — an argv SHORTER than the prefix
    // yields a shorter slice, which cannot compare equal.
    if !ALLOWED_GIT_PREFIXES
        .iter()
        .any(|prefix| args.len() >= prefix.len() && &args[..prefix.len()] == *prefix)
    {
        return None;
    }
    host.run(cwd, args)
}

// ── public dataclass ─────────────────────────────────────────────────────────

/// `@dataclass class WorktreeInfo` — one linked worktree, verdict included.
///
/// Field order is the payload's key order: `asdict()` walks the declaration.
#[derive(Debug, Clone)]
pub struct WorktreeInfo {
    /// The worktree's path, as `git worktree list` printed it.
    pub path: String,
    /// The short branch name (`refs/heads/` stripped), or `None` when detached.
    pub branch: Option<String>,
    /// The worktree's `HEAD` sha.
    pub head: Option<String>,
    /// The MAIN worktree's path, or `None` when the main entry is bare.
    pub parent_repo: Option<String>,
    /// [`path_to_slug`] of `parent_repo`.
    pub parent_slug: Option<String>,
    /// `git status --porcelain` line count; `0` also means "the probe failed".
    pub dirty_count: i64,
    /// `git cherry` `+` line count; `0` also means "the probe failed".
    pub unique_commits: i64,
    /// Days since the worktree directory's mtime, rounded to 2dp.
    pub age_days: Option<f64>,
    /// One of [`VERDICT_ACTIVE`] / [`VERDICT_MERGED_SAFE_TO_PRUNE`] /
    /// [`VERDICT_HAS_UNIQUE_WORK`].
    pub verdict: String,
    /// Sessions attributed through the fragment slug.
    pub sessions: i64,
    /// Attributed cost, rounded to 4dp. Always a float, including `0.0`.
    pub cost_usd: f64,
    /// PREVIEW strings. This module never executes them.
    pub prune_commands: Vec<String>,
    /// Why the verdict degraded, `"; "`-joined, or `None`.
    pub note: Option<String>,
}

impl WorktreeInfo {
    /// `WorktreeInfo.to_dict()` — `dataclasses.asdict`, so declaration order.
    #[must_use]
    pub fn to_dict(&self) -> Value {
        let mut out = Map::new();
        out.insert("path".to_owned(), Value::String(self.path.clone()));
        out.insert("branch".to_owned(), opt_str(self.branch.as_ref()));
        out.insert("head".to_owned(), opt_str(self.head.as_ref()));
        out.insert("parent_repo".to_owned(), opt_str(self.parent_repo.as_ref()));
        out.insert("parent_slug".to_owned(), opt_str(self.parent_slug.as_ref()));
        out.insert("dirty_count".to_owned(), Value::from(self.dirty_count));
        out.insert(
            "unique_commits".to_owned(),
            Value::from(self.unique_commits),
        );
        // `round(age, 2)` of a float is a float; `None` stays null.
        out.insert("age_days".to_owned(), self.age_days.map_or(Value::Null, jf));
        out.insert("verdict".to_owned(), Value::String(self.verdict.clone()));
        out.insert("sessions".to_owned(), Value::from(self.sessions));
        // `round(float(cost or 0.0), 4)` — a float even at zero, so `0.0` and
        // not `0` (DIV-057's family).
        out.insert("cost_usd".to_owned(), jf(self.cost_usd));
        out.insert(
            "prune_commands".to_owned(),
            Value::Array(
                self.prune_commands
                    .iter()
                    .map(|command| Value::String(command.clone()))
                    .collect(),
            ),
        );
        out.insert("note".to_owned(), opt_str(self.note.as_ref()));
        Value::Object(out)
    }
}

fn opt_str(value: Option<&String>) -> Value {
    value.map_or(Value::Null, |text| Value::String(text.clone()))
}

// ── pure slug logic (no I/O) ─────────────────────────────────────────────────

/// `is_worktree_slug(slug)` — the parent slug when `slug` is worktree-shaped.
///
/// The LEFTMOST marker wins across both spellings, so a worktree inside a
/// worktree attributes to the ROOT repo. `idx > 0` is strict: a slug that
/// *starts* with a marker has no parent and does not match, and the tail after
/// the marker must be non-empty.
#[must_use]
pub fn is_worktree_slug(slug: &str) -> Option<&str> {
    if slug.is_empty() {
        return None;
    }
    let mut best: Option<usize> = None;
    for marker in WORKTREE_SLUG_MARKERS {
        // `str.find` is a CHARACTER index in Python and a BYTE index here. Both
        // markers are ASCII and start with `-`, so every index this can produce
        // is a char boundary and `min` picks the same marker either way.
        if let Some(idx) = slug.find(marker)
            && idx > 0
            && !slug[idx + marker.len()..].is_empty()
        {
            best = Some(best.map_or(idx, |current: usize| current.min(idx)));
        }
    }
    best.map(|idx| &slug[..idx])
}

/// `_path_to_slug(path)` — every non-`[A-Za-z0-9]` character becomes `-`.
///
/// `rstrip("/\\")` first, so `/repo/` and `/repo` mangle identically. The class
/// is ASCII-literal, so a non-ASCII character is a single `-` (one per CHAR, not
/// per UTF-8 byte).
#[must_use]
pub fn path_to_slug(path: &str) -> String {
    path.trim_end_matches(['/', '\\'])
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '-' })
        .collect()
}

// ── store attribution (the one writer) ───────────────────────────────────────

/// `attribute_fragments(conn)` — stamp `projects.worktree_of`, return the count.
///
/// Idempotent by construction: a row whose `worktree_of` already equals the
/// computed parent is skipped, so a second run answers `0`. Degrades to `0` and
/// never raises — a store predating schema v027 has no column, and any SQL error
/// is swallowed.
///
/// Python ends with `conn.commit()` guarded by `if updated:`; rusqlite is in the
/// same autocommit mode `sqlite3` uses here, so every `UPDATE` has already
/// committed and there is nothing left for it to do. Recorded, not transcribed.
pub fn attribute_fragments(conn: &Connection) -> i64 {
    if !column_exists(conn, "projects", "worktree_of") {
        return 0;
    }
    let Ok(rows) = read_project_slugs(conn) else {
        return 0;
    };

    let mut updated = 0_i64;
    for (project_id, slug, current) in rows {
        let Some(parent) = is_worktree_slug(&slug) else {
            continue;
        };
        // `current == parent` — a NULL `worktree_of` is never equal to a string,
        // so it always updates.
        if current.as_deref() == Some(parent) {
            continue;
        }
        if conn
            .execute(
                "UPDATE projects SET worktree_of = ? WHERE id = ?",
                rusqlite::params![parent, project_id],
            )
            .is_ok()
        {
            updated += 1;
        }
    }
    updated
}

/// One `SELECT id, slug, worktree_of FROM projects` row — no ORDER BY, as
/// written.
type ProjectSlugRow = (i64, String, Option<String>);

fn read_project_slugs(conn: &Connection) -> rusqlite::Result<Vec<ProjectSlugRow>> {
    let mut stmt = conn.prepare("SELECT id, slug, worktree_of FROM projects")?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            // `str(row[1] or "")`.
            row.get::<_, Option<String>>(1)?.unwrap_or_default(),
            row.get::<_, Option<String>>(2)?,
        ))
    })?;
    rows.collect()
}

/// `_fragment_rollup(conn, worktree_path)` → `(sessions, cost_usd)`.
///
/// Attribution is by FRAGMENT SLUG, never by a per-cwd `json_extract` scan over
/// the partitioned `messages` view — the unbounded pattern the yield-route fix
/// removed. Both lookups are single indexed queries.
///
/// Note the fallback order: `project_mart.total_cost_usd` first, and
/// `usage_events.cost_usd` only when the mart's `SUM` came back NULL. `SUM` over
/// zero rows *is* NULL, so an unmaterialised project falls through; a
/// materialised one with genuinely zero spend does not (its `SUM` is `0.0`).
fn fragment_rollup(conn: &Connection, worktree_path: &str) -> (i64, f64) {
    let slug = path_to_slug(worktree_path);
    rollup_inner(conn, &slug).unwrap_or((0, 0.0))
}

fn rollup_inner(conn: &Connection, slug: &str) -> rusqlite::Result<(i64, f64)> {
    let ids: Vec<i64> = {
        let mut stmt = conn.prepare("SELECT id FROM projects WHERE slug = ?")?;
        let rows = stmt.query_map([slug], |row| row.get::<_, i64>(0))?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    if ids.is_empty() {
        return Ok((0, 0.0));
    }
    let holes = placeholders(ids.len());

    let sessions: i64 = conn
        .query_row(
            &format!("SELECT COUNT(*) FROM sessions WHERE project_id IN ({holes})"),
            rusqlite::params_from_iter(ids.iter()),
            |row| row.get::<_, Option<i64>>(0),
        )?
        // `int(sessions or 0)`.
        .unwrap_or(0);

    let mut cost: Option<f64> = None;
    if table_or_view_exists(conn, "project_mart") {
        cost = conn.query_row(
            &format!("SELECT SUM(total_cost_usd) FROM project_mart WHERE project_id IN ({holes})"),
            rusqlite::params_from_iter(ids.iter()),
            |row| row.get::<_, Option<f64>>(0),
        )?;
    }
    if cost.is_none() && table_or_view_exists(conn, "usage_events") {
        cost = conn.query_row(
            &format!("SELECT SUM(cost_usd) FROM usage_events WHERE project_id IN ({holes})"),
            rusqlite::params_from_iter(ids.iter()),
            |row| row.get::<_, Option<f64>>(0),
        )?;
    }
    // `round(float(cost or 0.0), 4)` — Python's `round`, which is ties-to-even
    // on the correctly-rounded decimal expansion, not `f64::round`.
    Ok((sessions, round_py(cost.unwrap_or(0.0), 4)))
}

// ── repo-level scan ──────────────────────────────────────────────────────────

/// `list_worktrees(conn, project_root=…)`.
///
/// `project_root` is Python's truthiness: `None` **and** `""` both mean
/// whole-store, so the caller may normalise an empty string away.
///
/// Roots are deduplicated by `git rev-parse --git-common-dir`, so two worktrees
/// (or two subdirectories) of one repo produce ONE `git worktree list`. The main
/// worktree and bare entries are skipped — only linked worktrees are reported.
///
/// Never raises: a missing git binary, a non-repo root or a timeout skips that
/// root, and a per-worktree probe failure degrades to
/// [`VERDICT_HAS_UNIQUE_WORK`] with a note.
pub fn list_worktrees(
    conn: &Connection,
    project_root: Option<&str>,
    host: &dyn Host,
) -> Vec<WorktreeInfo> {
    let roots: Vec<String> = match project_root.filter(|root| !root.is_empty()) {
        Some(root) => vec![root.to_owned()],
        None => candidate_roots_from_store(conn),
    };

    let mut out: Vec<WorktreeInfo> = Vec::new();
    let mut seen_repos: HashSet<String> = HashSet::new();
    let mut seen_worktrees: HashSet<String> = HashSet::new();
    for root in &roots {
        // `if not root: continue`.
        if root.is_empty() {
            continue;
        }
        let Some(common) = git_common_dir(host, root) else {
            continue;
        };
        if !seen_repos.insert(common) {
            continue;
        }

        let Some(listing) = run_git(host, root, &["worktree", "list", "--porcelain"]) else {
            continue;
        };
        let entries = parse_worktree_porcelain(&listing);
        // `if not entries: continue`, then `main = entries[0]`.
        let Some(main) = entries.first() else {
            continue;
        };
        let parent_repo = if main.bare {
            None
        } else {
            Some(main.path.clone())
        };
        let main_path = main.path.clone();
        // One default-branch resolution per repo, batched like the listing —
        // and resolved in ROOT, which may itself be a linked worktree rather
        // than the main one. Same refs either way; the cwd is what Python
        // passes, so it is what is passed here.
        let default = default_branch(host, root);

        for entry in entries.iter().skip(1) {
            if entry.bare || entry.path == main_path || seen_worktrees.contains(&entry.path) {
                continue;
            }
            seen_worktrees.insert(entry.path.clone());
            out.push(inspect_worktree(
                conn,
                host,
                root,
                entry,
                parent_repo.as_deref(),
                default.as_deref(),
            ));
        }
    }
    // `out.sort(key=lambda w: (w.parent_repo or "", w.path))` — STABLE, and a
    // `None` parent sorts as the empty string rather than raising.
    out.sort_by(|left, right| {
        let lp = left.parent_repo.as_deref().unwrap_or("");
        let rp = right.parent_repo.as_deref().unwrap_or("");
        lp.cmp(rp).then_with(|| left.path.cmp(&right.path))
    });
    out
}

/// `_inspect_worktree` — every probe failure degrades, never raises.
fn inspect_worktree(
    conn: &Connection,
    host: &dyn Host,
    root: &str,
    entry: &PorcelainEntry,
    parent_repo: Option<&str>,
    default: Option<&str>,
) -> WorktreeInfo {
    let mut notes: Vec<String> = Vec::new();

    // `target = entry.branch or entry.head` — truthiness, and the parser has
    // already turned an empty value into `None` on both fields.
    let target = entry.branch.as_ref().or(entry.head.as_ref());
    let unique: Option<i64> = match (default, target) {
        (None, _) => {
            notes.push(
                "could not resolve the repo's default branch; treated as unique work (conservative)"
                    .to_owned(),
            );
            None
        }
        (Some(_), None) => {
            notes.push(
                "worktree has neither a branch nor a readable HEAD; treated as unique work (conservative)"
                    .to_owned(),
            );
            None
        }
        (Some(default), Some(target)) => {
            // `git cherry` runs in ROOT, not in the worktree.
            let unique = unique_commits(host, root, default, target);
            if unique.is_none() {
                notes.push(format!(
                    "git cherry against {default} failed; treated as unique work (conservative)"
                ));
            }
            unique
        }
    };

    // `git status` runs in the WORKTREE.
    let dirty = dirty_count(host, &entry.path);
    if dirty.is_none() {
        notes.push("git status failed; treated as unique work (conservative)".to_owned());
    }

    let age = age_days(host, &entry.path);
    if let Some(prunable) = &entry.prunable {
        notes.push(format!("git reports the worktree prunable ({prunable})"));
    }
    if let Some(locked) = &entry.locked {
        notes.push(format!("worktree is locked ({locked})"));
    }

    // The UNROUNDED age decides the verdict; the rounded one reaches the wire.
    let verdict = verdict(age, unique, dirty);
    let (sessions, cost_usd) = fragment_rollup(conn, &entry.path);

    WorktreeInfo {
        path: entry.path.clone(),
        branch: entry.branch.clone(),
        head: entry.head.clone(),
        parent_repo: parent_repo.map(str::to_owned),
        parent_slug: parent_repo.map(path_to_slug),
        // `int(dirty or 0)` — `None` AND `0` both land on 0.
        dirty_count: dirty.unwrap_or(0),
        unique_commits: unique.unwrap_or(0),
        age_days: age.map(|age| round_py(age, 2)),
        verdict,
        sessions,
        cost_usd,
        prune_commands: prune_commands(&entry.path, entry.branch.as_deref(), default),
        // `"; ".join(notes) if notes else None`.
        note: (!notes.is_empty()).then(|| notes.join("; ")),
    }
}

/// `_verdict` — conservative-first. `None` means "a git probe failed".
///
/// Activity wins over everything; a failed probe is never
/// [`VERDICT_MERGED_SAFE_TO_PRUNE`]. The comparison is `age_days * 24.0 <= 48.0`
/// on the RAW age.
fn verdict(age_days: Option<f64>, unique_commits: Option<i64>, dirty_count: Option<i64>) -> String {
    if let Some(age) = age_days
        && age * 24.0 <= ACTIVE_WINDOW_HOURS
    {
        return VERDICT_ACTIVE.to_owned();
    }
    let (Some(unique), Some(dirty)) = (unique_commits, dirty_count) else {
        return VERDICT_HAS_UNIQUE_WORK.to_owned();
    };
    if unique > 0 || dirty > 0 {
        return VERDICT_HAS_UNIQUE_WORK.to_owned();
    }
    VERDICT_MERGED_SAFE_TO_PRUNE.to_owned()
}

/// `_prune_commands` — PREVIEW strings, never executed.
///
/// `git branch -D` is added only when the worktree has a branch AND that branch
/// is not the repo's default short name: advising `git branch -D main` is never
/// sensible, even as a preview.
fn prune_commands(path: &str, branch: Option<&str>, default_branch: Option<&str>) -> Vec<String> {
    let mut commands = vec![format!("git worktree remove {}", shlex_quote(path))];
    // `"origin/main".rsplit("/", 1)[-1]` → `"main"`; a bare local name passes
    // through unchanged.
    let default_short =
        default_branch.map(|name| name.rsplit_once('/').map_or(name, |(_, tail)| tail));
    // `if branch and branch != default_short` — truthiness, so an empty branch
    // adds nothing; and `default_short` is `None` when the default is unknown,
    // which no real branch name can equal.
    if let Some(branch) = branch.filter(|branch| !branch.is_empty())
        && Some(branch) != default_short
    {
        commands.push(format!("git branch -D {}", shlex_quote(branch)));
    }
    commands
}

/// `shlex.quote(s)`.
///
/// `_find_unsafe = re.compile(r'[^\w@%+=:,./-]', re.ASCII)`, so the safe set is
/// exactly `[A-Za-z0-9_@%+=:,./-]`. An empty string is `''`, and an unsafe one
/// is single-quoted with every embedded `'` rewritten as `'"'"'`.
fn shlex_quote(text: &str) -> String {
    if text.is_empty() {
        return "''".to_owned();
    }
    let safe = |ch: char| {
        ch.is_ascii_alphanumeric()
            || matches!(
                ch,
                '_' | '@' | '%' | '+' | '=' | ':' | ',' | '.' | '/' | '-'
            )
    };
    if text.chars().all(safe) {
        return text.to_owned();
    }
    format!("'{}'", text.replace('\'', "'\"'\"'"))
}

// ── candidate repo discovery (store-driven) ──────────────────────────────────

/// `_candidate_roots_from_store` — distinct session cwds, most recent first.
///
/// Bounded twice: the 500 most recent sessions, then the first 50 distinct
/// cwds. Repo-level dedup happens later via `--git-common-dir`, so several cwds
/// inside one repo still cost one listing.
fn candidate_roots_from_store(conn: &Connection) -> Vec<String> {
    let Ok(session_fks) = recent_session_fks(conn) else {
        return Vec::new();
    };
    let cwd_by_fk = bulk_first_cwd(conn, &session_fks);

    let mut ordered: Vec<String> = Vec::new();
    let mut seen: HashSet<&str> = HashSet::new();
    // Walks the fk list, which preserves the SQL's recency order.
    for fk in &session_fks {
        let cwd = cwd_by_fk.get(fk).map_or("", String::as_str);
        if !cwd.is_empty() && seen.insert(cwd) {
            ordered.push(cwd.to_owned());
            if ordered.len() >= MAX_DISTINCT_CWDS {
                break;
            }
        }
    }
    ordered
}

fn recent_session_fks(conn: &Connection) -> rusqlite::Result<Vec<i64>> {
    let mut stmt = conn.prepare(
        "SELECT id FROM sessions ORDER BY COALESCE(last_ts, first_ts) DESC, id DESC LIMIT ?",
    )?;
    let rows = stmt.query_map([MAX_SESSIONS_SCANNED], |row| row.get::<_, i64>(0))?;
    rows.collect()
}

/// `_bulk_first_cwd` — `{session_fk: first non-empty cwd}` in chunks of 500.
///
/// **The `json_extract` CSE shape is the point** (`98e7f8b`, `RS-5-036`): the
/// extract is evaluated ONCE in an inner `extracted` CTE and read as a plain
/// column by both the ranking and the NULL/`''` filter. SQLite does no
/// common-subexpression elimination, so the older three-spelling form parsed
/// every message's `raw_json` blob three times (1.56 s vs 1.19 s on a 3.9 GB
/// store, same rows). Reproducing the RESULT without the SHAPE would give back
/// a 30% regression the Python fix already paid for.
///
/// A SQL error `break`s — it does not `continue` — so whatever was resolved
/// before it is kept and the remaining chunks are abandoned.
fn bulk_first_cwd(conn: &Connection, session_fks: &[i64]) -> HashMap<i64, String> {
    let mut out: HashMap<i64, String> = HashMap::new();
    if session_fks.is_empty() {
        return out;
    }
    for chunk in session_fks.chunks(CWD_CHUNK_SIZE) {
        let sql = format!(
            "WITH extracted AS (SELECT session_fk, seq, json_extract(raw_json, '$.cwd') AS cwd \
             FROM messages WHERE session_fk IN ({})), ranked AS (SELECT session_fk, cwd, \
             ROW_NUMBER() OVER (PARTITION BY session_fk ORDER BY seq) AS rn FROM extracted \
             WHERE cwd IS NOT NULL AND cwd != '') SELECT session_fk, cwd FROM ranked WHERE rn = 1",
            placeholders(chunk.len())
        );
        let resolved = (|| -> rusqlite::Result<Vec<(i64, SqlValue)>> {
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt.query_map(rusqlite::params_from_iter(chunk.iter()), |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, SqlValue>(1)?))
            })?;
            rows.collect()
        })();
        let Ok(rows) = resolved else {
            break;
        };
        for (fk, cwd) in rows {
            out.insert(fk, py_str_or_empty(&cwd));
        }
    }
    out
}

/// `str(row[1] or "")` — Python truthiness first, then `str()`.
///
/// `json_extract` on a JSON string yields TEXT, which is the only shape this
/// sees in practice. The numeric legs exist because SQLite's `cwd != ''`
/// comparison does not exclude a numeric `cwd` (every number sorts before every
/// string), so a `{"cwd": 0}` message would reach here.
fn py_str_or_empty(value: &SqlValue) -> String {
    match value {
        SqlValue::Null => String::new(),
        SqlValue::Text(text) => text.clone(),
        SqlValue::Integer(0) => String::new(),
        SqlValue::Integer(number) => number.to_string(),
        SqlValue::Real(number) if *number == 0.0 => String::new(),
        SqlValue::Real(number) => stax_memory::pyjson::dumps_http(&jf(*number)),
        // `str(b"…")` is `"b'…'"` in Python. Unreachable through `json_extract`;
        // the empty string is the honest stand-in rather than a fake `b'…'`.
        SqlValue::Blob(_) => String::new(),
    }
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

// ── git plumbing (read-only, allowlisted, degrade-on-error) ──────────────────

/// `@dataclass class _PorcelainEntry` — one block of `git worktree list
/// --porcelain`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct PorcelainEntry {
    path: String,
    head: Option<String>,
    /// Short name, `refs/heads/` stripped.
    branch: Option<String>,
    detached: bool,
    bare: bool,
    locked: Option<String>,
    prunable: Option<String>,
}

/// `_parse_worktree_porcelain`.
///
/// Blocks are separated by blank lines. Unknown keys are ignored so a newer git
/// may add attributes without breaking the parser, and a missing blank separator
/// is tolerated (a second `worktree` line flushes the current entry).
fn parse_worktree_porcelain(out: &str) -> Vec<PorcelainEntry> {
    let mut entries: Vec<PorcelainEntry> = Vec::new();
    let mut cur: Option<PorcelainEntry> = None;
    for line in split_lines(out) {
        if line.trim().is_empty() {
            if let Some(entry) = cur.take() {
                entries.push(entry);
            }
            continue;
        }
        // `line.partition(" ")` — no space yields `(line, "", "")`.
        let (key, value) = line.split_once(' ').unwrap_or((line, ""));
        if key == "worktree" {
            if let Some(entry) = cur.take() {
                entries.push(entry);
            }
            cur = Some(PorcelainEntry {
                path: value.to_owned(),
                ..PorcelainEntry::default()
            });
            continue;
        }
        // `elif cur is None: continue` — an attribute before any `worktree`
        // line is dropped.
        let Some(entry) = cur.as_mut() else {
            continue;
        };
        match key {
            // `value or None` — an attribute line with an empty value is None.
            "HEAD" => entry.head = non_empty(value),
            "branch" => {
                entry.branch = non_empty(value.strip_prefix("refs/heads/").unwrap_or(value));
            }
            "detached" => entry.detached = true,
            "bare" => entry.bare = true,
            // `value or "locked"` — the flag with no reason still records one.
            "locked" => {
                entry.locked = Some(non_empty(value).unwrap_or_else(|| "locked".to_owned()))
            }
            "prunable" => {
                entry.prunable = Some(non_empty(value).unwrap_or_else(|| "prunable".to_owned()));
            }
            _ => {}
        }
    }
    if let Some(entry) = cur {
        entries.push(entry);
    }
    entries
}

fn non_empty(value: &str) -> Option<String> {
    (!value.is_empty()).then(|| value.to_owned())
}

/// `_git_common_dir(path)` — the repo identity used as the dedup key.
///
/// Every worktree of a repo shares one common dir, which is what makes scanning
/// two worktrees of one repo produce one listing. A relative answer (`.git` at
/// the repo root) is resolved against `path`; any git failure is `None`.
fn git_common_dir(host: &dyn Host, path: &str) -> Option<String> {
    // `Path(path)`; `str(p)` then normalises (trailing separators dropped,
    // repeated separators collapsed, bare `.` components removed).
    let normalised = py_path_str(path);
    if !host.is_dir(&normalised) {
        return None;
    }
    let out = run_git(host, &normalised, &["rev-parse", "--git-common-dir"])?;
    // `if out is None or not out.strip(): return None`.
    if out.trim().is_empty() {
        return None;
    }
    let first = split_lines(out.trim()).first().copied().unwrap_or("");
    let common = Path::new(first);
    let common = if common.is_absolute() {
        common.to_path_buf()
    } else {
        Path::new(&normalised).join(common)
    };
    // `except OSError: return str(common)` — an unresolvable path still yields
    // a dedup key, it just may not be canonical.
    Some(
        host.resolve(&common)
            .unwrap_or(common)
            .to_string_lossy()
            .into_owned(),
    )
}

/// `str(Path(p))` — pathlib's normalisation, which is purely lexical.
///
/// Drops trailing separators, collapses runs of `/`, and removes bare `.`
/// components. `..` is NOT removed (pathlib refuses to guess past a symlink),
/// and a path beginning with exactly two slashes keeps both (POSIX reserves it).
fn py_path_str(path: &str) -> String {
    if path.is_empty() {
        // `str(Path(""))` is `"."`. Reachable only through a `""` cwd, which
        // `list_worktrees` already skipped; kept so the helper is total.
        return ".".to_owned();
    }
    let leading = if path.starts_with("//") && !path.starts_with("///") {
        "//"
    } else if path.starts_with('/') {
        "/"
    } else {
        ""
    };
    let parts: Vec<&str> = path
        .split('/')
        .filter(|part| !part.is_empty() && *part != ".")
        .collect();
    if parts.is_empty() {
        return if leading.is_empty() {
            ".".to_owned()
        } else {
            leading.to_owned()
        };
    }
    format!("{leading}{}", parts.join("/"))
}

/// `_default_branch(root)` — `origin/HEAD` first, then a verifiable
/// `main` / `master`.
///
/// `symbolic-ref` answers `refs/remotes/origin/main`, of which only
/// `refs/remotes/` is stripped — so the return value is `origin/main`, a
/// REMOTE-tracking name, which is the safest "did it land" comparison base.
/// `None` means every candidate failed and the caller degrades.
fn default_branch(host: &dyn Host, root: &str) -> Option<String> {
    if let Some(out) = run_git(
        host,
        root,
        &["symbolic-ref", "--quiet", "refs/remotes/origin/HEAD"],
    ) && !out.trim().is_empty()
    {
        let first = split_lines(out.trim()).first().copied().unwrap_or("");
        let name = first.strip_prefix("refs/remotes/").unwrap_or(first);
        if !name.is_empty() {
            return Some(name.to_owned());
        }
    }
    for candidate in ["main", "master"] {
        // `--verify --quiet` makes a missing ref a non-zero exit, which
        // `_run_git` turns into `None`. Success is the whole test; the sha it
        // prints is discarded, and the name returned is the BARE candidate.
        if run_git(
            host,
            root,
            &[
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("refs/heads/{candidate}"),
            ],
        )
        .is_some()
        {
            return Some(candidate.to_owned());
        }
    }
    None
}

/// `_unique_commits(root, default_branch, target)` — `git cherry` `+` lines.
///
/// `git cherry` matches by PATCH ID, so a commit that landed squashed or
/// cherry-picked under a different sha still reads as landed (`-`); only
/// genuinely unlanded work is `+`. `None` on any git failure.
fn unique_commits(host: &dyn Host, root: &str, default_branch: &str, target: &str) -> Option<i64> {
    let out = run_git(host, root, &["cherry", default_branch, target])?;
    Some(
        split_lines(&out)
            .into_iter()
            .filter(|line| line.starts_with('+'))
            .count()
            .try_into()
            .unwrap_or(i64::MAX),
    )
}

/// `_dirty_count(worktree_path)` — changed + untracked paths.
///
/// Untracked files count: `git worktree remove` would lose them just as it would
/// lose modifications, so they matter for prune safety. `None` on any git
/// failure, including a worktree directory that no longer exists.
fn dirty_count(host: &dyn Host, worktree_path: &str) -> Option<i64> {
    let out = run_git(host, worktree_path, &["status", "--porcelain"])?;
    Some(
        split_lines(&out)
            .into_iter()
            .filter(|line| !line.trim().is_empty())
            .count()
            .try_into()
            .unwrap_or(i64::MAX),
    )
}

/// `_age_days(worktree_path)` — days since the directory's mtime.
///
/// `max(0.0, …)`, so a clock that ran backwards (or an mtime in the future)
/// reads as zero rather than negative — which also makes it `ACTIVE`.
fn age_days(host: &dyn Host, worktree_path: &str) -> Option<f64> {
    let mtime = host.mtime_secs(worktree_path)?;
    Some(f64::max(0.0, (host.now_secs() - mtime) / 86400.0))
}

/// `str.splitlines()` — the FULL CPython separator set.
///
/// DEDUP NOTE for the integrator: byte-identical to the private `split_lines` in
/// `services/yield_tracker.rs`, which belongs to another member and could not be
/// widened from here (the batch fence). Collapse the two when a pass is allowed
/// to touch both — same disposition as DIV-099(a).
///
/// `str::lines()` would break on `\n` / `\r\n` only. Git output reaches this
/// through `worktree list` paths, `status` filenames and `cherry` subjects, any
/// of which may legitimately carry a vertical tab, a form feed, `\x1c`–`\x1e`,
/// `\x85`, `U+2028` or `U+2029` — and in CPython each of those splits the line.
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

// ── small SQL probes ─────────────────────────────────────────────────────────

/// `_table_exists` — **`type IN ('table','view')`**, LAW 7's wider guard.
///
/// Not [`crate::mart_queries::table_exists`], which is `type='table'`. The two
/// are different on purpose, and this module's Python says `view` too.
fn table_or_view_exists(conn: &Connection, name: &str) -> bool {
    conn.query_row(
        "SELECT 1 FROM sqlite_master WHERE type IN ('table', 'view') AND name = ? LIMIT 1",
        [name],
        |_| Ok(()),
    )
    .is_ok()
}

/// `_column_exists` — the row-factory-agnostic `PRAGMA table_info` probe.
///
/// The table name is interpolated, exactly as Python does it; the one call site
/// passes a literal.
fn column_exists(conn: &Connection, table: &str, column: &str) -> bool {
    let probe = (|| -> rusqlite::Result<bool> {
        let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(1))?;
        for name in rows {
            if name? == column {
                return Ok(true);
            }
        }
        Ok(false)
    })();
    probe.unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── the pure slug logic ──────────────────────────────────────────────────

    #[test]
    fn a_single_dash_before_worktrees_is_a_real_directory_and_does_not_match() {
        // The double dash comes from the `/.` in `<repo>/.worktrees/<name>`; a
        // directory genuinely named `worktrees` mangles to ONE dash and must not
        // be folded into a phantom parent.
        assert_eq!(is_worktree_slug("-Users-x-worktrees-app"), None);
        assert_eq!(
            is_worktree_slug("-Users-x--worktrees-app"),
            Some("-Users-x")
        );
        assert_eq!(
            is_worktree_slug("-Users-x--claude-worktrees-app"),
            Some("-Users-x")
        );
    }

    #[test]
    fn the_leftmost_marker_wins_so_a_nested_worktree_attributes_to_the_root() {
        assert_eq!(
            is_worktree_slug("-repo--worktrees-a--claude-worktrees-b"),
            Some("-repo")
        );
        assert_eq!(
            is_worktree_slug("-repo--claude-worktrees-a--worktrees-b"),
            Some("-repo")
        );
    }

    #[test]
    fn an_empty_parent_or_an_empty_tail_is_not_a_worktree_slug() {
        // `idx > 0` is strict, so a slug that STARTS with the marker has no
        // parent to attribute to …
        assert_eq!(is_worktree_slug("--worktrees-a"), None);
        // … and the tail after the marker must be non-empty.
        assert_eq!(is_worktree_slug("-repo--worktrees-"), None);
        assert_eq!(is_worktree_slug(""), None);
    }

    #[test]
    fn path_to_slug_is_the_ascii_mangle_with_trailing_separators_stripped() {
        assert_eq!(
            path_to_slug("/Users/x/dev_dev/proj"),
            "-Users-x-dev-dev-proj"
        );
        assert_eq!(path_to_slug("/repo/"), path_to_slug("/repo"));
        assert_eq!(path_to_slug("/repo\\\\"), "-repo");
        assert_eq!(
            path_to_slug("/r/.claude/worktrees/a"),
            "-r--claude-worktrees-a"
        );
        // The class is ASCII-literal, so a non-ASCII letter is one dash — per
        // CHARACTER, not per UTF-8 byte.
        assert_eq!(path_to_slug("/café"), "-caf-");
    }

    // ── the porcelain parser ─────────────────────────────────────────────────

    #[test]
    fn the_porcelain_parser_reads_the_shape_real_git_emits() {
        let out = "worktree /repo\nHEAD abc\nbranch refs/heads/feat/x\n\n\
                   worktree /repo/.worktrees/w\nHEAD def\ndetached\n\n";
        let entries = parse_worktree_porcelain(out);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].path, "/repo");
        // Only `refs/heads/` is stripped — the slashes INSIDE the name stay.
        assert_eq!(entries[0].branch.as_deref(), Some("feat/x"));
        assert_eq!(entries[1].head.as_deref(), Some("def"));
        assert!(entries[1].detached);
        assert_eq!(entries[1].branch, None);
    }

    #[test]
    fn a_missing_blank_separator_still_flushes_the_previous_entry() {
        // Tolerated on purpose: a `worktree` line always starts a new block.
        let entries = parse_worktree_porcelain("worktree /a\nworktree /b\nbare\n");
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].path, "/a");
        assert!(entries[1].bare);
    }

    #[test]
    fn a_valueless_lock_or_prunable_flag_gets_its_default_reason() {
        let entries = parse_worktree_porcelain("worktree /a\nlocked\nprunable\n");
        assert_eq!(entries[0].locked.as_deref(), Some("locked"));
        assert_eq!(entries[0].prunable.as_deref(), Some("prunable"));
        let entries = parse_worktree_porcelain("worktree /a\nlocked in use\n");
        assert_eq!(entries[0].locked.as_deref(), Some("in use"));
    }

    #[test]
    fn an_attribute_line_before_any_worktree_line_is_dropped_not_crashed() {
        assert_eq!(parse_worktree_porcelain("HEAD abc\nbranch x\n"), vec![]);
    }

    #[test]
    fn unknown_attributes_are_ignored_so_a_newer_git_cannot_break_the_parse() {
        let entries = parse_worktree_porcelain("worktree /a\nsomethingnew yes\nHEAD abc\n");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].head.as_deref(), Some("abc"));
    }

    // ── the verdict table ────────────────────────────────────────────────────

    #[test]
    fn activity_wins_over_unique_work_and_dirt() {
        // 24h old, 9 unique commits, 5 dirty files — still ACTIVE.
        assert_eq!(verdict(Some(1.0), Some(9), Some(5)), VERDICT_ACTIVE);
    }

    #[test]
    fn safe_to_prune_needs_both_probes_to_have_succeeded_and_both_to_be_zero() {
        assert_eq!(
            verdict(Some(30.0), Some(0), Some(0)),
            VERDICT_MERGED_SAFE_TO_PRUNE
        );
        // A failed probe is NEVER safe.
        assert_eq!(verdict(Some(30.0), None, Some(0)), VERDICT_HAS_UNIQUE_WORK);
        assert_eq!(verdict(Some(30.0), Some(0), None), VERDICT_HAS_UNIQUE_WORK);
        assert_eq!(verdict(None, None, None), VERDICT_HAS_UNIQUE_WORK);
        // An unreadable mtime is not activity, so it falls through to the rest.
        assert_eq!(
            verdict(None, Some(0), Some(0)),
            VERDICT_MERGED_SAFE_TO_PRUNE
        );
    }

    #[test]
    fn the_active_window_boundary_is_inclusive_at_exactly_forty_eight_hours() {
        assert_eq!(verdict(Some(2.0), Some(0), Some(0)), VERDICT_ACTIVE);
        assert_eq!(
            verdict(Some(2.000_001), Some(0), Some(0)),
            VERDICT_MERGED_SAFE_TO_PRUNE
        );
    }

    #[test]
    fn the_verdict_reads_the_unrounded_age_while_the_payload_rounds_it() {
        // 2.0004 days is 48.0096 hours — past the window — yet `round(x, 2)`
        // renders `2.0`, which a reader would call "exactly 48h, so ACTIVE".
        let age = 2.000_4_f64;
        assert_eq!(
            verdict(Some(age), Some(0), Some(0)),
            VERDICT_MERGED_SAFE_TO_PRUNE
        );
        assert!((round_py(age, 2) - 2.0).abs() < f64::EPSILON);
    }

    // ── prune previews ───────────────────────────────────────────────────────

    #[test]
    fn a_branch_that_is_the_default_gets_no_delete_preview() {
        assert_eq!(
            prune_commands("/w", Some("main"), Some("origin/main")),
            vec!["git worktree remove /w".to_owned()]
        );
        assert_eq!(
            prune_commands("/w", Some("feat"), Some("origin/main")),
            vec![
                "git worktree remove /w".to_owned(),
                "git branch -D feat".to_owned()
            ]
        );
    }

    #[test]
    fn a_detached_worktree_gets_only_the_remove_preview() {
        assert_eq!(
            prune_commands("/w", None, Some("origin/main")),
            vec!["git worktree remove /w".to_owned()]
        );
    }

    #[test]
    fn previews_are_shell_quoted_so_a_path_with_spaces_is_copy_pasteable() {
        assert_eq!(
            prune_commands("/a b/w", Some("has space"), None),
            vec![
                "git worktree remove '/a b/w'".to_owned(),
                "git branch -D 'has space'".to_owned()
            ]
        );
    }

    #[test]
    fn shlex_quote_matches_pythons_safe_set_and_its_single_quote_escape() {
        assert_eq!(shlex_quote(""), "''");
        assert_eq!(
            shlex_quote("/a/b-c_d.e:f,g+h=i%j@k"),
            "/a/b-c_d.e:f,g+h=i%j@k"
        );
        assert_eq!(shlex_quote("a b"), "'a b'");
        assert_eq!(shlex_quote("it's"), "'it'\"'\"'s'");
        // `~` and `$` are NOT in `[\w@%+=:,./-]`, so they force quoting.
        assert_eq!(shlex_quote("~/x"), "'~/x'");
        assert_eq!(shlex_quote("$HOME"), "'$HOME'");
    }

    // ── the git chokepoint ───────────────────────────────────────────────────

    /// A [`Host`] that records every argv and answers from a script. Nothing is
    /// spawned, so the allow-list can be probed without a repo.
    #[derive(Default)]
    struct FakeHost {
        calls: std::cell::RefCell<Vec<Vec<String>>>,
        replies: HashMap<String, String>,
        dirs: HashSet<String>,
        mtime: Option<f64>,
        now: f64,
    }

    impl Host for FakeHost {
        fn run(&self, cwd: &str, args: &[&str]) -> Option<String> {
            let mut call = vec![cwd.to_owned()];
            call.extend(args.iter().map(|arg| (*arg).to_owned()));
            self.calls.borrow_mut().push(call);
            self.replies.get(&args.join(" ")).cloned()
        }
        fn is_dir(&self, path: &str) -> bool {
            self.dirs.contains(path)
        }
        fn mtime_secs(&self, _path: &str) -> Option<f64> {
            self.mtime
        }
        fn now_secs(&self) -> f64 {
            self.now
        }
        fn resolve(&self, path: &Path) -> Option<PathBuf> {
            Some(path.to_path_buf())
        }
    }

    #[test]
    fn the_allowlist_refuses_a_mutating_verb_without_ever_spawning_it() {
        let host = FakeHost::default();
        for argv in [
            vec!["fetch"],
            vec!["gc"],
            vec!["checkout", "main"],
            vec!["worktree", "prune"],
            vec!["worktree", "remove", "/w"],
            vec!["config", "--global", "user.name", "x"],
            vec!["status"], // the allow-list entry is `status --porcelain`
        ] {
            assert_eq!(run_git(&host, "/r", &argv), None, "{argv:?}");
        }
        // Not merely refused — never handed to the host at all.
        assert!(host.calls.borrow().is_empty());
    }

    #[test]
    fn every_verb_this_module_issues_is_on_the_allowlist() {
        let host = FakeHost::default();
        for argv in [
            vec!["worktree", "list", "--porcelain"],
            vec!["status", "--porcelain"],
            vec!["cherry", "origin/main", "feat"],
            vec!["rev-parse", "--git-common-dir"],
            vec!["rev-parse", "--verify", "--quiet", "refs/heads/main"],
            vec!["symbolic-ref", "--quiet", "refs/remotes/origin/HEAD"],
        ] {
            // The fake has no scripted reply, so `None` here would be the reply
            // and not the refusal — the call reaching `calls` is the assertion.
            let _ = run_git(&host, "/r", &argv);
        }
        assert_eq!(host.calls.borrow().len(), 6);
    }

    #[test]
    fn an_argv_shorter_than_a_prefix_cannot_match_it() {
        let host = FakeHost::default();
        assert_eq!(run_git(&host, "/r", &["worktree"]), None);
        assert!(host.calls.borrow().is_empty());
    }

    #[test]
    fn default_branch_prefers_origin_head_and_strips_only_the_remote_prefix() {
        let mut host = FakeHost::default();
        host.replies.insert(
            "symbolic-ref --quiet refs/remotes/origin/HEAD".to_owned(),
            "refs/remotes/origin/trunk\n".to_owned(),
        );
        assert_eq!(default_branch(&host, "/r").as_deref(), Some("origin/trunk"));
        // One call only: the `main` / `master` probes never ran.
        assert_eq!(host.calls.borrow().len(), 1);
    }

    #[test]
    fn default_branch_falls_back_to_a_verifiable_local_name_then_gives_up() {
        let mut host = FakeHost::default();
        host.replies.insert(
            "rev-parse --verify --quiet refs/heads/master".to_owned(),
            "deadbeef\n".to_owned(),
        );
        // `main` is probed first and fails, `master` verifies — and the name
        // returned is the BARE candidate, not the ref path that verified it.
        assert_eq!(default_branch(&host, "/r").as_deref(), Some("master"));
        assert_eq!(default_branch(&FakeHost::default(), "/r"), None);
    }

    #[test]
    fn unique_commits_counts_only_the_plus_lines() {
        let mut host = FakeHost::default();
        host.replies.insert(
            "cherry origin/main feat".to_owned(),
            "+ aaa\n- bbb\n+ ccc\n".to_owned(),
        );
        assert_eq!(unique_commits(&host, "/r", "origin/main", "feat"), Some(2));
        // A git failure is `None`, which is NOT the same as zero — it is what
        // keeps the verdict off MERGED_SAFE_TO_PRUNE.
        assert_eq!(unique_commits(&host, "/r", "origin/main", "other"), None);
    }

    #[test]
    fn dirty_count_counts_untracked_files_and_ignores_blank_lines() {
        let mut host = FakeHost::default();
        host.replies.insert(
            "status --porcelain".to_owned(),
            " M a\n?? b\n\n?? c\n".to_owned(),
        );
        assert_eq!(dirty_count(&host, "/w"), Some(3));
        assert_eq!(dirty_count(&FakeHost::default(), "/w"), None);
    }

    #[test]
    fn git_common_dir_resolves_a_relative_answer_against_the_root() {
        let mut host = FakeHost::default();
        host.dirs.insert("/repo".to_owned());
        host.replies
            .insert("rev-parse --git-common-dir".to_owned(), ".git\n".to_owned());
        assert_eq!(
            git_common_dir(&host, "/repo").as_deref(),
            Some("/repo/.git")
        );
        // A trailing separator must not produce a different dedup key.
        assert_eq!(
            git_common_dir(&host, "/repo/").as_deref(),
            Some("/repo/.git")
        );
    }

    #[test]
    fn git_common_dir_is_none_for_a_non_directory_and_for_empty_output() {
        let host = FakeHost::default();
        assert_eq!(git_common_dir(&host, "/repo"), None);
        let mut host = FakeHost::default();
        host.dirs.insert("/repo".to_owned());
        host.replies
            .insert("rev-parse --git-common-dir".to_owned(), "   \n".to_owned());
        assert_eq!(git_common_dir(&host, "/repo"), None);
    }

    #[test]
    fn py_path_str_matches_pathlibs_lexical_normalisation() {
        assert_eq!(py_path_str("/a/b/"), "/a/b");
        assert_eq!(py_path_str("/a//b"), "/a/b");
        assert_eq!(py_path_str("/a/./b"), "/a/b");
        assert_eq!(py_path_str("//a/b"), "//a/b");
        assert_eq!(py_path_str("/"), "/");
        assert_eq!(py_path_str("a/b"), "a/b");
        // `..` is kept: pathlib refuses to collapse it, because a symlink makes
        // the collapse wrong.
        assert_eq!(py_path_str("/a/../b"), "/a/../b");
    }

    #[test]
    fn age_days_is_clamped_at_zero_for_a_future_mtime() {
        let host = FakeHost {
            mtime: Some(1_000.0),
            now: 900.0,
            ..FakeHost::default()
        };
        assert_eq!(age_days(&host, "/w"), Some(0.0));
        let host = FakeHost {
            mtime: Some(0.0),
            now: 86_400.0 * 3.0,
            ..FakeHost::default()
        };
        assert_eq!(age_days(&host, "/w"), Some(3.0));
        assert_eq!(age_days(&FakeHost::default(), "/w"), None);
    }

    #[test]
    fn split_lines_breaks_on_the_full_cpython_separator_set() {
        // A `status --porcelain` filename containing a form feed splits into two
        // "dirty files" in Python. `str::lines()` would keep it as one.
        assert_eq!(split_lines("a\u{0c}b"), vec!["a", "b"]);
        assert_eq!(split_lines("a\r\nb"), vec!["a", "b"]);
        assert_eq!(split_lines("a\u{2028}b"), vec!["a", "b"]);
        assert_eq!(split_lines("a\nb\n"), vec!["a", "b"]);
    }

    // ── the store legs ───────────────────────────────────────────────────────

    fn seeded() -> Connection {
        let conn = Connection::open_in_memory().expect("open");
        conn.execute_batch(
            r#"CREATE TABLE projects (id INTEGER PRIMARY KEY, slug TEXT, worktree_of TEXT);
              CREATE TABLE sessions (id INTEGER PRIMARY KEY, project_id INTEGER,
                                     first_ts TEXT, last_ts TEXT);
              CREATE TABLE messages (id INTEGER PRIMARY KEY, session_fk INTEGER,
                                     seq INTEGER, raw_json TEXT);
              INSERT INTO projects (id, slug, worktree_of) VALUES
                  (1, '-repo', NULL),
                  (2, '-repo--worktrees-w', NULL),
                  (3, '-repo--claude-worktrees-x', '-repo');
              INSERT INTO sessions (id, project_id, first_ts, last_ts) VALUES
                  (10, 1, '2026-01-01T00:00:00Z', '2026-01-05T00:00:00Z'),
                  (11, 2, '2026-01-02T00:00:00Z', NULL),
                  (12, 2, '2026-01-03T00:00:00Z', '2026-01-09T00:00:00Z');
              INSERT INTO messages (session_fk, seq, raw_json) VALUES
                  (10, 1, '{"cwd": ""}'),
                  (10, 2, '{"cwd": "/repo"}'),
                  (11, 1, '{"nope": 1}'),
                  (12, 1, '{"cwd": "/repo/.worktrees/w"}'),
                  (12, 2, '{"cwd": "/elsewhere"}');"#,
        )
        .expect("seed");
        conn
    }

    #[test]
    fn attribute_fragments_is_idempotent_and_skips_rows_already_stamped() {
        let conn = seeded();
        // Row 3 is already correct, so only row 2 moves.
        assert_eq!(attribute_fragments(&conn), 1);
        assert_eq!(attribute_fragments(&conn), 0);
        let parent: Option<String> = conn
            .query_row("SELECT worktree_of FROM projects WHERE id = 2", [], |row| {
                row.get(0)
            })
            .expect("row");
        assert_eq!(parent.as_deref(), Some("-repo"));
    }

    #[test]
    fn a_store_without_the_v027_column_degrades_to_zero_rather_than_raising() {
        let conn = Connection::open_in_memory().expect("open");
        conn.execute_batch("CREATE TABLE projects (id INTEGER PRIMARY KEY, slug TEXT);")
            .expect("seed");
        assert_eq!(attribute_fragments(&conn), 0);
        // And so does a store with no `projects` table at all.
        let empty = Connection::open_in_memory().expect("open");
        assert_eq!(attribute_fragments(&empty), 0);
    }

    #[test]
    fn the_first_non_empty_cwd_per_session_is_what_ranks() {
        let conn = seeded();
        let cwds = bulk_first_cwd(&conn, &[10, 11, 12]);
        // Session 10's seq-1 message has an EMPTY cwd, which the CTE's
        // `cwd != ''` filter drops before the ranking — so seq 2 is "first".
        assert_eq!(cwds.get(&10).map(String::as_str), Some("/repo"));
        // Session 11 has no `$.cwd` key at all.
        assert_eq!(cwds.get(&11), None);
        assert_eq!(
            cwds.get(&12).map(String::as_str),
            Some("/repo/.worktrees/w")
        );
    }

    #[test]
    fn candidate_roots_are_distinct_and_in_recency_order() {
        let conn = seeded();
        // `ORDER BY COALESCE(last_ts, first_ts) DESC, id DESC`: session 12
        // (2026-01-09), then 10 (2026-01-05), then 11 (falls back to first_ts,
        // 2026-01-02). Session 11 contributes no cwd.
        assert_eq!(
            candidate_roots_from_store(&conn),
            vec!["/repo/.worktrees/w".to_owned(), "/repo".to_owned()]
        );
    }

    #[test]
    fn the_fragment_rollup_finds_sessions_by_the_mangled_worktree_path() {
        let conn = seeded();
        // `/repo/.worktrees/w` mangles to `-repo--worktrees-w`, which is project
        // 2, which owns sessions 11 and 12. No mart and no usage_events, so the
        // cost is the float zero.
        assert_eq!(fragment_rollup(&conn, "/repo/.worktrees/w"), (2, 0.0));
        // A worktree with no fragment project at all.
        assert_eq!(fragment_rollup(&conn, "/nowhere"), (0, 0.0));
    }

    #[test]
    fn the_mart_sum_wins_and_usage_events_only_answers_when_it_is_null() {
        let conn = seeded();
        conn.execute_batch(
            "CREATE TABLE project_mart (project_id INTEGER, total_cost_usd REAL);
             CREATE TABLE usage_events (project_id INTEGER, cost_usd REAL);
             INSERT INTO usage_events VALUES (2, 9.0);",
        )
        .expect("marts");
        // `project_mart` exists but has no row for project 2, so its SUM is NULL
        // and the fallback answers.
        assert_eq!(fragment_rollup(&conn, "/repo/.worktrees/w"), (2, 9.0));
        conn.execute_batch("INSERT INTO project_mart VALUES (2, 1.23456);")
            .expect("mart row");
        // Now the mart answers, and `round(x, 4)` trims it.
        assert_eq!(fragment_rollup(&conn, "/repo/.worktrees/w"), (2, 1.2346));
    }

    #[test]
    fn a_materialised_project_with_zero_spend_does_not_fall_through_to_usage_events() {
        let conn = seeded();
        conn.execute_batch(
            "CREATE TABLE project_mart (project_id INTEGER, total_cost_usd REAL);
             CREATE TABLE usage_events (project_id INTEGER, cost_usd REAL);
             INSERT INTO project_mart VALUES (2, 0.0);
             INSERT INTO usage_events VALUES (2, 9.0);",
        )
        .expect("marts");
        // SUM over one row of 0.0 is 0.0, not NULL — so the `is None` gate holds
        // and the 9.0 never surfaces.
        assert_eq!(fragment_rollup(&conn, "/repo/.worktrees/w"), (2, 0.0));
    }

    #[test]
    fn table_or_view_exists_accepts_a_view_where_the_mart_queries_guard_would_not() {
        let conn = Connection::open_in_memory().expect("open");
        conn.execute_batch("CREATE TABLE t (x INTEGER); CREATE VIEW v AS SELECT x FROM t;")
            .expect("seed");
        assert!(table_or_view_exists(&conn, "t"));
        assert!(table_or_view_exists(&conn, "v"));
        assert!(!table_or_view_exists(&conn, "nope"));
        // LAW 7: the narrower `mart_queries` spelling answers false for the view.
        assert!(!crate::mart_queries::table_exists(&conn, "v").expect("probe"));
    }

    // ── the whole scan, against a scripted repo ──────────────────────────────

    #[test]
    fn the_scan_dedups_by_common_dir_and_reports_only_linked_worktrees() {
        let conn = seeded();
        let mut host = FakeHost {
            mtime: Some(0.0),
            now: 86_400.0 * 30.0,
            ..FakeHost::default()
        };
        host.dirs.insert("/repo".to_owned());
        host.dirs.insert("/repo/.worktrees/w".to_owned());
        host.replies.insert(
            "rev-parse --git-common-dir".to_owned(),
            "/repo/.git".to_owned(),
        );
        host.replies.insert(
            "worktree list --porcelain".to_owned(),
            "worktree /repo\nHEAD aaa\nbranch refs/heads/main\n\n\
             worktree /repo/.worktrees/w\nHEAD bbb\nbranch refs/heads/feat\n\n\
             worktree /repo/bare\nbare\n\n"
                .to_owned(),
        );
        host.replies.insert(
            "symbolic-ref --quiet refs/remotes/origin/HEAD".to_owned(),
            "refs/remotes/origin/main\n".to_owned(),
        );
        host.replies
            .insert("cherry origin/main feat".to_owned(), "+ a\n".to_owned());
        host.replies
            .insert("status --porcelain".to_owned(), String::new());

        // Both candidate roots resolve to the SAME common dir, so the listing
        // runs once and `/repo/.worktrees/w` is not reported twice.
        let found = list_worktrees(&conn, None, &host);
        assert_eq!(found.len(), 1);
        let info = &found[0];
        assert_eq!(info.path, "/repo/.worktrees/w");
        assert_eq!(info.parent_repo.as_deref(), Some("/repo"));
        assert_eq!(info.parent_slug.as_deref(), Some("-repo"));
        assert_eq!(info.unique_commits, 1);
        assert_eq!(info.dirty_count, 0);
        assert_eq!(info.verdict, VERDICT_HAS_UNIQUE_WORK);
        assert_eq!(info.sessions, 2);
        assert_eq!(info.note, None);
        assert_eq!(
            info.prune_commands,
            vec![
                "git worktree remove /repo/.worktrees/w".to_owned(),
                "git branch -D feat".to_owned()
            ]
        );
        // One `worktree list` for two roots.
        let listings = host
            .calls
            .borrow()
            .iter()
            .filter(|call| call.get(1).map(String::as_str) == Some("worktree"))
            .count();
        assert_eq!(listings, 1);
    }

    #[test]
    fn a_failed_probe_degrades_with_a_note_and_never_reads_safe() {
        let conn = seeded();
        let mut host = FakeHost {
            mtime: Some(0.0),
            now: 86_400.0 * 30.0,
            ..FakeHost::default()
        };
        host.dirs.insert("/repo".to_owned());
        host.replies.insert(
            "rev-parse --git-common-dir".to_owned(),
            "/repo/.git".to_owned(),
        );
        host.replies.insert(
            "worktree list --porcelain".to_owned(),
            "worktree /repo\nHEAD aaa\nbranch refs/heads/main\n\n\
             worktree /w\nHEAD bbb\nbranch refs/heads/feat\n\
             prunable gitdir file points to non-existent location\n\n"
                .to_owned(),
        );
        // No `symbolic-ref`, no `main`/`master`, no `cherry`, no `status`.
        let found = list_worktrees(&conn, Some("/repo"), &host);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].verdict, VERDICT_HAS_UNIQUE_WORK);
        assert_eq!(
            found[0].note.as_deref(),
            Some(
                "could not resolve the repo's default branch; treated as unique work (conservative); \
                 git status failed; treated as unique work (conservative); \
                 git reports the worktree prunable (gitdir file points to non-existent location)"
            )
        );
        // Zeros on the wire, `None` in the verdict — the two are not the same
        // thing, and only the verdict can tell them apart.
        assert_eq!(found[0].unique_commits, 0);
        assert_eq!(found[0].dirty_count, 0);
    }

    #[test]
    fn the_scan_sorts_by_parent_repo_then_path() {
        let conn = Connection::open_in_memory().expect("open");
        conn.execute_batch(
            "CREATE TABLE projects (id INTEGER PRIMARY KEY, slug TEXT, worktree_of TEXT);
             CREATE TABLE sessions (id INTEGER PRIMARY KEY, project_id INTEGER,
                                    first_ts TEXT, last_ts TEXT);
             CREATE TABLE messages (id INTEGER PRIMARY KEY, session_fk INTEGER,
                                    seq INTEGER, raw_json TEXT);",
        )
        .expect("seed");
        let mut host = FakeHost {
            mtime: Some(0.0),
            now: 86_400.0 * 30.0,
            ..FakeHost::default()
        };
        host.dirs.insert("/repo".to_owned());
        host.replies.insert(
            "rev-parse --git-common-dir".to_owned(),
            "/repo/.git".to_owned(),
        );
        host.replies.insert(
            "worktree list --porcelain".to_owned(),
            "worktree /repo\nHEAD aaa\n\nworktree /z\nHEAD b\n\nworktree /a\nHEAD c\n\n".to_owned(),
        );
        let found = list_worktrees(&conn, Some("/repo"), &host);
        let paths: Vec<&str> = found.iter().map(|info| info.path.as_str()).collect();
        assert_eq!(paths, vec!["/a", "/z"]);
    }

    #[test]
    fn an_empty_project_root_means_whole_store_not_a_root_named_empty() {
        let conn = seeded();
        let host = FakeHost::default();
        // Neither candidate root is a directory in the fake, so the answer is
        // empty either way — the assertion is that `Some("")` took the SAME
        // branch as `None` and probed the store's cwds rather than statting `""`.
        assert!(list_worktrees(&conn, Some(""), &host).is_empty());
        assert_eq!(
            host.calls.borrow().len(),
            0,
            "a non-directory root never reaches git"
        );
    }

    // ── the payload shape ────────────────────────────────────────────────────

    #[test]
    fn to_dict_is_the_dataclass_declaration_order_with_float_zeros() {
        let info = WorktreeInfo {
            path: "/w".to_owned(),
            branch: None,
            head: Some("abc".to_owned()),
            parent_repo: Some("/repo".to_owned()),
            parent_slug: Some("-repo".to_owned()),
            dirty_count: 0,
            unique_commits: 0,
            age_days: Some(0.0),
            verdict: VERDICT_ACTIVE.to_owned(),
            sessions: 0,
            cost_usd: 0.0,
            prune_commands: vec!["git worktree remove /w".to_owned()],
            note: None,
        };
        assert_eq!(
            stax_memory::pyjson::dumps_http(&info.to_dict()),
            r#"{"path":"/w","branch":null,"head":"abc","parent_repo":"/repo","parent_slug":"-repo","dirty_count":0,"unique_commits":0,"age_days":0.0,"verdict":"ACTIVE","sessions":0,"cost_usd":0.0,"prune_commands":["git worktree remove /w"],"note":null}"#
        );
    }
}
