//! `stax backup` — the whole group: `list` / `verify` (tranche 1) and
//! `create` / `restore` / `auto` (tranche 2), `cli.py:906`–`:1577`.
//!
//! # Tranche 2: what a writer's proof actually is
//!
//! `create` / `restore` / `auto` shell out to `rsync`, `launchctl` and
//! `crontab`, so the contract has three parts and each gets its own proof:
//!
//! 1. **The argv.** [`create_rsync_argv`] / [`restore_rsync_argv`] are pure
//!    functions returning the exact `Vec<String>` the reference assembles, and
//!    `rust/backup-differ.sh` proves them by putting a **fake `rsync` first on
//!    `PATH`** that logs `"$@"` and exits with an injected code. Both
//!    implementations are intercepted by the same shim, so the comparison is of
//!    two real spawns rather than of a Rust value against a Python value some
//!    probe re-derived. The shim's exit code is how the rsync-23 / rsync-24
//!    tolerances get crossed on both sides (law: a constant needs a corpus row
//!    that crosses it).
//! 2. **The bytes on disk.** The same differ then runs *without* the shim
//!    against a scratch `~/.claude` and diffs the two resulting backup trees
//!    recursively.
//! 3. **The generated file.** `backup auto` on Darwin writes a launchd plist.
//!    That branch cannot execute on this host, so [`darwin_plist`] is a pure
//!    renderer and `tests/plist_golden.rs` diffs it byte-for-byte against a
//!    plist produced by the *real* `cli.py` under a faked `platform.system()`.
//!    Nothing is ever installed: no `launchctl`, no `crontab -e`, no
//!    `~/Library/LaunchAgents`.
//!
//! # Two truthiness boundaries, both live
//!
//! `backup verify --name ''` verifies the LATEST backup (`if name:`), and
//! `backup create --label '///'` produces an *unsuffixed* name because the
//! sanitiser runs first and `if label:` then sees `''`. `--label '0'` keeps its
//! suffix — a non-empty string is truthy in Python whatever it spells. All
//! three are parity rows; the class is the one wave 1 found on `--project ''`.
//!
//! Two reference behaviours are reproduced deliberately rather than fixed:
//!
//! * **`list` counts entries and prints directories.** `len(backups)` is
//!   `len(sorted(_BACKUP_DIR.iterdir()))` — every entry, including stray files
//!   — while the loop `continue`s on anything that is not a directory. A backup
//!   directory holding one stray file therefore announces "2 backup(s)" and
//!   lists one. Ported as-is.
//! * **`rglob` sees dotfiles.** `pathlib`'s globbing has no "leading dot"
//!   exclusion (that is the `glob` module's rule), so `.hidden/a.jsonl` counts
//!   toward the file total and a `store.db` under a hidden directory satisfies
//!   `verify`. Measured against CPython 3.12.13 rather than assumed.
//!
//! `verify`'s failure line goes to **stderr** through `logging` — with no
//! handler configured, CPython's `lastResort` handler writes the bare message
//! at `WARNING` and above, so the stderr bytes are exactly
//! `backup verify: <name> missing <a, b>`.

use std::path::{Component, Path, PathBuf};

use anyhow::Result;
use clap::{Args, Subcommand};
use stax_core::settings::app_dir;

use crate::click::Output;
use crate::pyclock;

/// `_CRITICAL_ARTIFACTS` — order matters, it is the print order.
pub const CRITICAL_ARTIFACTS: [&str; 4] =
    ["store.db", "search_index.db", "qa_pairs.db", "tags.json"];

/// `backup create`'s exclude list — order matters twice over: it is the argv
/// order rsync receives *and* the first four are the ones the banner names.
pub const EXCLUDES: [&str; 10] = [
    "debug/",
    "plugins/",
    "cache/",
    "statsig/",
    "telemetry/",
    "paste-cache/",
    "ccnotify/",
    "session-env/",
    "downloads/",
    "backups/",
];

/// `stax backup` — back up and restore session data.
#[derive(Debug, Args)]
pub struct BackupArgs {
    /// The subcommand.
    #[command(subcommand)]
    pub verb: BackupVerb,
}

/// The ported `backup` verbs.
#[derive(Debug, Subcommand)]
pub enum BackupVerb {
    /// Set up or remove daily automatic backups via launchd (macOS) or cron.
    Auto {
        /// Enable or disable daily backups
        #[arg(long, overrides_with = "disable")]
        enable: bool,
        /// Enable or disable daily backups
        #[arg(long, overrides_with = "enable")]
        disable: bool,
    },
    /// Create an incremental backup of every agent's session data.
    ///
    /// ``~/.claude`` (sessions, file history, plans, tasks, todos, settings,
    /// shell snapshots, prompt history) mirrors at the backup root, exactly as
    /// before, so existing restores keep working. Every OTHER registered
    /// adapter's source roots — self-declared by each adapter via
    /// ``source_roots()`` / ``watch_paths()``, never listed here — copy under
    /// ``sources/<adapter>/`` with a ``sources/manifest.json`` mapping each
    /// subdir back to its original absolute path. Excludes debug logs and
    /// plugin binaries to save space.
    ///
    /// Uses hard links for efficiency — unchanged files cost zero disk. Files
    /// that vanish or partly copy because an agent is writing to the tree right
    /// now (rsync 24 / 23) are reported, not fatal — a live machine must still be
    /// able to finish a backup.
    Create {
        /// Optional label for the backup
        #[arg(long, value_name = "LABEL")]
        label: Option<String>,
        /// Max backups to retain (oldest pruned)
        #[arg(long, default_value_t = 10, value_parser = clap::value_parser!(u64).range(1..))]
        keep: u64,
        /// Also replicate the finished backup to ssh://[user@]host[:port]/abs/path.
        /// One-way whole-artifact copy — for peer sync of aggregates use
        /// `stax sync` instead.
        #[arg(long = "to", value_name = "TO")]
        to_url: Option<String>,
    },
    /// List existing backups.
    List,
    /// Restore ~/.claude/ from a backup.
    Restore {
        /// The backup directory name.
        name: String,
        /// Show what would be restored without doing it
        #[arg(long = "dry-run")]
        dry_run: bool,
    },
    /// Verify a backup contains all critical artifacts.
    ///
    /// Checks that the backup holds every file needed for a full restore —
    /// store.db plus the search / Q&A / tags sidecars. The SQLite store alone is
    /// not the complete source of truth, so a store-only backup silently loses
    /// search, Q&A, and tags. Exits non-zero if the backup is missing or
    /// incomplete, so wrapper scripts can detect it.
    Verify {
        /// Backup to verify (default: latest)
        #[arg(long, value_name = "NAME")]
        name: Option<String>,
    },
}

/// `_BACKUP_DIR` — `$STACKUNDERFLOW_HOME/backups`.
#[must_use]
pub fn backup_dir() -> PathBuf {
    app_dir().join("backups")
}

/// Run a `backup` verb.
///
/// # Errors
/// A filesystem failure the reference would have raised too.
pub fn run_backup(args: &BackupArgs) -> Result<Output> {
    let root = backup_dir();
    match &args.verb {
        BackupVerb::Auto { disable, .. } => Ok(auto(!disable, &Env::from_process())),
        BackupVerb::Create {
            label,
            keep,
            to_url,
        } => Ok(create(
            label.as_deref(),
            *keep,
            to_url.as_deref(),
            &Env::from_process(),
            &SystemRunner,
        )),
        BackupVerb::List => Ok(list(&root)),
        BackupVerb::Restore { name, dry_run } => Ok(restore(
            name,
            *dry_run,
            &Env::from_process(),
            &SystemRunner,
            &mut StdinConfirm,
        )),
        BackupVerb::Verify { name } => Ok(verify(&root, name.as_deref())),
    }
}

// ── the injected environment ─────────────────────────────────────────────────

/// Everything `backup` reads from outside itself.
///
/// Injected rather than read at each site for the campaign's standing reason
/// (finding 5: `std::env::set_var` is `unsafe` in Rust 2024 and the workspace
/// forbids `unsafe`, so a test cannot move the process env). It is also what
/// makes `create` runnable against a scratch `~/.claude` in the differ without
/// the differ having to own a second copy of the path logic.
#[derive(Debug, Clone)]
pub struct Env {
    /// `_STATE_DIR` — `$STACKUNDERFLOW_HOME`, already resolved.
    pub state_dir: PathBuf,
    /// `_claude_dir()` — `claude_home()`, `CLAUDE_CONFIG_DIR` honoured.
    pub claude_dir: PathBuf,
    /// `Path.home()`.
    pub home: PathBuf,
    /// `platform.system()` — `"Darwin"`, `"Linux"`, `"Windows"`.
    pub system: String,
    /// `time.time()` at the start of the command.
    pub now_epoch_secs: i64,
    /// `shutil.which("stackunderflow")`, resolved from `$PATH`.
    pub stackunderflow_bin: Option<PathBuf>,
}

impl Env {
    /// Read the real environment, once, at the top of the command.
    #[must_use]
    pub fn from_process() -> Self {
        Self {
            state_dir: app_dir(),
            claude_dir: stax_adapters::claude::claude_home(),
            home: home_dir(),
            system: platform_system().to_owned(),
            now_epoch_secs: pyclock::now_epoch_secs(),
            stackunderflow_bin: which("stackunderflow"),
        }
    }

    fn backup_root(&self) -> PathBuf {
        self.state_dir.join("backups")
    }
}

/// `platform.system()` for the platforms this port builds on.
fn platform_system() -> &'static str {
    match std::env::consts::OS {
        "macos" => "Darwin",
        "windows" => "Windows",
        "linux" => "Linux",
        other => other,
    }
}

fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default()
}

/// `shutil.which(name)` — first executable match on `$PATH`, in `$PATH` order.
///
/// `shutil.which` also accepts a name containing a separator as a direct path;
/// the only caller passes the literal `"stackunderflow"`, so that branch is not
/// reached and is not reproduced.
#[must_use]
pub fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).find_map(|dir| {
        let candidate = dir.join(name);
        is_executable(&candidate).then_some(candidate)
    })
}

fn is_executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::metadata(path)
            .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    {
        path.is_file()
    }
}

// ── the subprocess seam ──────────────────────────────────────────────────────

/// What `subprocess.run(cmd, capture_output=True, text=True, timeout=…)` returns,
/// including the two exceptions every caller catches by name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Spawn {
    /// The process ran to completion.
    Completed {
        /// Its exit status (`returncode`); a signal death is `-signal`, as
        /// CPython reports it.
        code: i32,
        /// Everything it wrote to stderr, decoded as text.
        stderr: String,
    },
    /// `FileNotFoundError` — the executable is not on `$PATH`.
    NotFound,
    /// `subprocess.TimeoutExpired`.
    TimedOut,
}

/// The seam a test or the differ replaces to observe the argv without spawning.
pub trait Runner {
    /// Run `argv`, giving up after `timeout_secs`.
    ///
    /// [`NO_TIMEOUT`] means "wait indefinitely", which is what
    /// `subprocess.run(...)` with no `timeout=` argument does.
    fn run(&self, argv: &[String], timeout_secs: u64) -> Spawn;
}

/// "No timeout" — the reference calls `subprocess.run` with no `timeout=` at
/// the two `launchctl` sites, and this is how the port says the same thing.
///
/// It is a named constant rather than a bare `u64::MAX` because the bare value
/// was a **crash**: `Instant::now() + Duration::from_secs(u64::MAX)` overflows,
/// and `impl Add<Duration> for Instant` panics on overflow rather than
/// saturating (`overflow when adding duration to instant`, exit **101**).
/// `backup auto --enable` / `--disable` on Darwin took that path on every run.
/// Linux CI never executes the Darwin branch — and per this tranche's brief it
/// never may — so no gate here could have caught it; the fix is that
/// [`SystemRunner`] now computes the deadline with `checked_add` and treats
/// `None` as "no deadline" instead of trusting the arithmetic.
pub const NO_TIMEOUT: u64 = u64::MAX;

/// The real one: spawn, capture, and enforce the timeout the reference passes.
#[derive(Debug, Clone, Copy)]
pub struct SystemRunner;

impl Runner for SystemRunner {
    fn run(&self, argv: &[String], timeout_secs: u64) -> Spawn {
        let Some((program, rest)) = argv.split_first() else {
            return Spawn::NotFound;
        };
        let child = std::process::Command::new(program)
            .args(rest)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn();
        let mut child = match child {
            Ok(child) => child,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Spawn::NotFound,
            Err(_) => return Spawn::NotFound,
        };
        // `subprocess.run(timeout=…)` kills the child and raises; std has no
        // timed wait, so poll. The granularity only has to be small against a
        // 600 s budget.
        //
        // `checked_add`, not `+`: `Instant + Duration` PANICS on overflow, and
        // `NO_TIMEOUT` overflows it by construction. `None` therefore means
        // "no deadline", which is exactly the semantics the untimed
        // `subprocess.run` at the `launchctl` sites needs.
        let deadline =
            std::time::Instant::now().checked_add(std::time::Duration::from_secs(timeout_secs));
        loop {
            match child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) => {
                    if deadline.is_some_and(|limit| std::time::Instant::now() >= limit) {
                        let _ = child.kill();
                        let _ = child.wait();
                        return Spawn::TimedOut;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                Err(_) => return Spawn::NotFound,
            }
        }
        let Ok(output) = child.wait_with_output() else {
            return Spawn::NotFound;
        };
        Spawn::Completed {
            code: exit_code(&output.status),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        }
    }
}

fn exit_code(status: &std::process::ExitStatus) -> i32 {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt as _;
        if let Some(signal) = status.signal() {
            return -signal;
        }
    }
    status.code().unwrap_or(1)
}

// ── click.confirm ────────────────────────────────────────────────────────────

/// `click.confirm(text)` — the one interactive prompt in the group.
pub trait Confirm {
    /// Ask, and answer. `None` is Click's `Abort` (EOF / interrupt).
    fn confirm(&mut self, prompt: &str, out: &mut String) -> Option<bool>;
}

/// The real prompt: write to stdout, read a line from stdin.
#[derive(Debug, Clone, Copy)]
pub struct StdinConfirm;

impl Confirm for StdinConfirm {
    fn confirm(&mut self, prompt: &str, out: &mut String) -> Option<bool> {
        use std::io::{BufRead as _, Write as _};
        // Click writes the prompt through `echo(..., nl=False)`, i.e. to the
        // same stdout the rest of the command uses. Everything buffered so far
        // has to land first, or the prompt appears above lines that preceded it.
        print!("{out}{prompt}");
        out.clear();
        let _ = std::io::stdout().flush();
        let stdin = std::io::stdin();
        loop {
            let mut line = String::new();
            match stdin.lock().read_line(&mut line) {
                // EOF → `EOFError` → `click.Abort`.
                Ok(0) | Err(_) => return None,
                Ok(_) => {}
            }
            match line.trim().to_lowercase().as_str() {
                "y" | "yes" => return Some(true),
                "n" | "no" => return Some(false),
                // `default=False` is not None, so the empty answer takes it.
                "" => return Some(false),
                _ => {
                    // `echo(_("Error: invalid input"), err=err)` with `err=False`.
                    print!("Error: invalid input\n{prompt}");
                    let _ = std::io::stdout().flush();
                }
            }
        }
    }
}

// ── backup list ──────────────────────────────────────────────────────────────

const NO_BACKUPS: &str = "  No backups yet. Run: stax backup create\n";

/// `backup_list`.
#[must_use]
pub fn list(root: &Path) -> Output {
    if !root.exists() {
        return Output::ok(NO_BACKUPS);
    }
    let entries = sorted_entries(root);
    if entries.is_empty() {
        return Output::ok(NO_BACKUPS);
    }
    // `click.echo(f"…\n")` — the f-string's own newline plus echo's.
    let mut out = format!("  {} backup(s) in {}\n\n", entries.len(), root.display());
    for entry in &entries {
        if !entry.is_dir() {
            continue;
        }
        // `backup list` prints the *jsonl* count under the label "files" —
        // `file_count = sum(1 for _ in b.rglob("*.jsonl"))`. Ported as found.
        let (_, jsonl, bytes) = measure(entry);
        let megabytes = bytes as f64 / f64::from(1_u32 << 20);
        out.push_str(&format!(
            "  {}  ({jsonl} files, {megabytes:.1} MB)\n",
            file_name(entry),
        ));
    }
    Output::ok(out)
}

/// `sorted(root.iterdir())` — `PurePath.__lt__` compares the case-normalised
/// parts, which for siblings is the entry name.
fn sorted_entries(root: &Path) -> Vec<PathBuf> {
    let Ok(reader) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut entries: Vec<PathBuf> = reader
        .filter_map(|entry| Some(entry.ok()?.path()))
        .collect();
    entries.sort();
    entries
}

/// The three `rglob` walks `list` / `create` / `restore` run, in one pass:
/// `(total_files, jsonl_entries, total_bytes)`.
///
/// * `total_files` — `sum(1 for _ in b.rglob("*") if _.is_file())`. `is_file()`
///   **follows** symlinks, so a symlink to a regular file counts.
/// * `jsonl_entries` — `sum(1 for _ in b.rglob("*.jsonl"))`, over **every**
///   matched entry, files and directories alike: `rglob` does not filter by
///   kind and the reference does not either.
/// * `total_bytes` — `sum(f.stat().st_size …)`, also following symlinks.
///
/// Recursion, on the other hand, uses `symlink_metadata`: `rglob` in CPython
/// 3.12 does not descend into a symlinked directory.
fn measure(root: &Path) -> (u64, u64, u64) {
    let mut total_files = 0_u64;
    let mut jsonl = 0_u64;
    let mut bytes = 0_u64;
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(reader) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in reader.flatten() {
            let path = entry.path();
            // `pathlib` matches dotfiles; there is no hidden-name exclusion.
            if file_name(&path).ends_with(".jsonl") {
                jsonl += 1;
            }
            let Ok(link_meta) = std::fs::symlink_metadata(&path) else {
                continue;
            };
            if link_meta.is_dir() {
                stack.push(path);
                continue;
            }
            if let Ok(meta) = std::fs::metadata(&path)
                && meta.is_file()
            {
                total_files += 1;
                bytes += meta.len();
            }
        }
    }
    (total_files, jsonl, bytes)
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

// ── backup verify ────────────────────────────────────────────────────────────

/// `backup_verify`.
///
/// `if name:` is **truthiness**, not `is not None` — `--name ''` therefore
/// falls through to `_latest_backup()` and verifies the newest backup rather
/// than rejecting the empty string. Same shape as wave 1's `--project ''`
/// (DIV-013 / fix-2 item B2), found the same way: by writing the row.
#[must_use]
pub fn verify(root: &Path, name: Option<&str>) -> Output {
    let target = match name.filter(|value| !value.is_empty()) {
        Some(name) => {
            let target = py_resolve(&root.join(name));
            let mut prefix = py_resolve(root).into_os_string();
            prefix.push(std::path::MAIN_SEPARATOR_STR);
            if !target
                .as_os_str()
                .to_string_lossy()
                .starts_with(&*prefix.to_string_lossy())
            {
                return Output::exit1("  Invalid backup name.\n");
            }
            if !target.is_dir() {
                return Output::exit1(format!(
                    "  Backup '{name}' not found. Run: stax backup list\n"
                ));
            }
            target
        }
        None => match latest_backup(root) {
            Some(target) => target,
            None => {
                return Output::exit1("  No backups to verify. Run: stax backup create\n");
            }
        },
    };

    let mut out = format!("  Verifying {}\n", file_name(&target));
    let mut missing: Vec<&str> = Vec::new();
    for artifact in CRITICAL_ARTIFACTS {
        let present = contains_file_named(&target, artifact);
        out.push_str(&format!(
            "    {} {}\n",
            crate::cfg::pad(artifact, 16),
            if present { "ok" } else { "MISSING" }
        ));
        if !present {
            missing.push(artifact);
        }
    }

    if missing.is_empty() {
        out.push_str(&format!(
            "  OK: all {} critical artifacts present.\n",
            CRITICAL_ARTIFACTS.len()
        ));
        return Output::ok(out);
    }
    out.push_str(&format!(
        "  Incomplete: missing {} of {} artifact(s): {}\n",
        missing.len(),
        CRITICAL_ARTIFACTS.len(),
        missing.join(", ")
    ));
    Output {
        stdout: out,
        // `_log.error` through CPython's `lastResort` handler: the bare
        // message, on stderr, no level prefix and no logger name.
        stderr: format!(
            "backup verify: {} missing {}\n",
            file_name(&target),
            missing.join(", ")
        ),
        code: 1,
    }
}

/// `_latest_backup()` — the last directory by name, or `None`.
#[must_use]
pub fn latest_backup(root: &Path) -> Option<PathBuf> {
    if !root.exists() {
        return None;
    }
    let mut dirs: Vec<PathBuf> = sorted_entries(root)
        .into_iter()
        .filter(|path| path.is_dir())
        .collect();
    // `key=lambda d: d.name` — the same order `sorted_entries` produced for
    // siblings, but re-stated because the reference re-sorts explicitly.
    dirs.sort_by_key(|path| file_name(path));
    dirs.pop()
}

/// `any(p.is_file() for p in target.rglob(name))`.
fn contains_file_named(root: &Path, name: &str) -> bool {
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(reader) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in reader.flatten() {
            let path = entry.path();
            let Ok(meta) = std::fs::symlink_metadata(&path) else {
                continue;
            };
            if meta.is_dir() {
                stack.push(path);
            } else if file_name(&path) == name && path.is_file() {
                return true;
            }
        }
    }
    false
}

/// `Path.resolve()` with `strict=False`.
///
/// Canonicalise the deepest existing ancestor (which resolves symlinks the way
/// CPython's `os.path.realpath` does) and re-attach the components that do not
/// exist yet, folding `.` and `..` lexically. Enough for the containment test
/// `verify` performs, and it keeps `--name ../..` outside the backup root,
/// which is the check's entire purpose.
fn py_resolve(path: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("/"))
            .join(path)
    };
    let mut normalised = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalised.pop();
            }
            other => normalised.push(other.as_os_str()),
        }
    }
    // Walk back to the deepest existing prefix, canonicalise it, re-attach.
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    let mut probe = normalised.clone();
    loop {
        if let Ok(real) = std::fs::canonicalize(&probe) {
            let mut out = real;
            for part in tail.iter().rev() {
                out.push(part);
            }
            return out;
        }
        match probe.file_name() {
            Some(name) => {
                tail.push(name.to_os_string());
                if !probe.pop() {
                    return normalised;
                }
            }
            None => return normalised,
        }
    }
}

// ── backup create ────────────────────────────────────────────────────────────

/// `cmd` as `backup_create` assembles it, before the spawn.
///
/// Pure, and public, because this argv **is** the contract: the exclude order,
/// the `--link-dest` that only appears when a previous generation exists, and
/// the trailing slashes that make rsync copy the *contents* of `~/.claude`
/// rather than the directory itself. `rust/backup-differ.sh` proves it by
/// intercepting the real spawn on both sides.
#[must_use]
pub fn create_rsync_argv(claude_dir: &Path, dest: &Path, previous: Option<&Path>) -> Vec<String> {
    let mut argv = vec!["rsync".to_owned(), "-a".to_owned()];
    for exclude in EXCLUDES {
        argv.push("--exclude".to_owned());
        argv.push((*exclude).to_owned());
    }
    if let Some(previous) = previous {
        argv.push("--link-dest".to_owned());
        argv.push(display(previous));
    }
    argv.push(format!("{}/", display(claude_dir)));
    argv.push(format!("{}/", display(dest)));
    argv
}

/// `re.sub(r'[^a-zA-Z0-9_-]', '', label)` — the label sanitiser.
///
/// It runs *before* the `if label:` test, which is why `--label '///'` produces
/// an unsuffixed backup name while `--label '0'` keeps its suffix.
#[must_use]
pub fn sanitise_label(label: &str) -> String {
    label
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
        .collect()
}

/// `backup_create`.
pub fn create<R: Runner + ?Sized>(
    label: Option<&str>,
    keep: u64,
    to_url: Option<&str>,
    env: &Env,
    runner: &R,
) -> Output {
    let mut out = Output::default();

    // The `--to` target is parsed before any work: a typo must not cost a full
    // local backup first. `parse_ssh_url` lives in `stax-sync` — the same
    // function `sync` uses, ported by the sync agent, not a second copy.
    if let Some(url) = to_url.filter(|value| !value.is_empty())
        && let Err(message) = stax_sync::ssh_store::parse_ssh_url(url)
    {
        out.stdout
            .push_str(&format!("  Invalid --to destination: {message}\n"));
        out.code = 1;
        return out;
    }

    let claude_dir = &env.claude_dir;
    if !claude_dir.exists() {
        out.stdout.push_str(&format!(
            "  No {}/ found — nothing to back up.\n",
            display(claude_dir)
        ));
        return out;
    }

    let stamp = pyclock::local_stamp(env.now_epoch_secs);
    let label = label.map(sanitise_label).filter(|value| !value.is_empty());
    let name = match &label {
        Some(label) => format!("{stamp}-{label}"),
        None => stamp,
    };
    let root = env.backup_root();
    let dest = py_resolve(&root.join(&name));

    let _ = std::fs::create_dir_all(&root);

    let mut prefix = py_resolve(&root).into_os_string();
    prefix.push(std::path::MAIN_SEPARATOR_STR);
    if !display(&dest).starts_with(&*prefix.to_string_lossy()) {
        out.stdout.push_str("  Invalid backup label.\n");
        return out;
    }

    let previous = latest_backup(&root);
    let argv = create_rsync_argv(claude_dir, &dest, previous.as_deref());

    out.stdout
        .push_str(&format!("  Backing up ~/.claude → {}\n", display(&dest)));
    out.stdout.push_str(&format!(
        "  (excluding: {}...)\n",
        EXCLUDES
            .iter()
            .take(4)
            .map(|exclude| exclude.trim_end_matches('/'))
            .collect::<Vec<_>>()
            .join(", ")
    ));

    match runner.run(&argv, 600) {
        Spawn::Completed { code, stderr } => {
            let (ok, message) =
                stax_sync::replicate::rsync_outcome(code, &stderr, &display(claude_dir));
            if !ok {
                out.stdout.push_str(&format!("  rsync error: {message}\n"));
                out.stderr.push_str(&format!(
                    "backup create failed: rsync exited {code}: {message}\n"
                ));
                let _ = std::fs::remove_dir_all(&dest);
                out.code = 1;
                return out;
            }
            if !message.is_empty() {
                out.stdout.push_str(&message);
                out.stdout.push('\n');
                out.stderr.push_str(&format!(
                    "backup create: rsync exited {code} (tolerated): {}\n",
                    stax_sync::replicate::rsync_reported(&stderr, 6)
                ));
            }
            capture_state(&dest, env, &mut out);
            report_sources(
                &backup_adapter_sources(&dest, previous.as_deref(), env),
                &mut out,
            );
            let (total_files, jsonl_files, bytes) = measure(&dest);
            let megabytes = bytes as f64 / f64::from(1_u32 << 20);
            out.stdout.push_str(&format!(
                "  Done: {total_files} files ({jsonl_files} JSONL), {megabytes:.1} MB\n"
            ));
        }
        Spawn::NotFound => {
            out.stdout
                .push_str("  rsync not found — falling back to shutil copy\n");
            copytree_ignoring(claude_dir, &dest, &ignore_names());
            capture_state(&dest, env, &mut out);
            report_sources(
                &backup_adapter_sources(&dest, previous.as_deref(), env),
                &mut out,
            );
            let (total_files, _, _) = measure(&dest);
            out.stdout
                .push_str(&format!("  Done: {total_files} files\n"));
        }
        Spawn::TimedOut => {
            out.stdout.push_str("  Backup timed out (>10 min).\n");
            out.stderr
                .push_str("backup create failed: rsync timed out after 600s\n");
            let _ = std::fs::remove_dir_all(&dest);
            out.code = 1;
            return out;
        }
    }

    prune_backups(&root, keep, &mut out);

    // Replication is last and non-fatal to the local backup. The plan is the
    // sync agent's `stax_sync::replicate`; only the reporting is here.
    if let Some(url) = to_url.filter(|value| !value.is_empty())
        && !replicate(&dest, url, previous.as_deref(), runner, &mut out)
    {
        out.code = 1;
    }
    out
}

/// `shutil.ignore_patterns(*[e.rstrip("/") for e in excludes])` — the names the
/// `rsync not found` fallback skips.
fn ignore_names() -> Vec<&'static str> {
    EXCLUDES
        .iter()
        .map(|exclude| exclude.trim_end_matches('/'))
        .collect()
}

/// `_replicate_backup` — the reporting half; the argv is `stax_sync`'s.
fn replicate<R: Runner + ?Sized>(
    dest: &Path,
    to_url: &str,
    previous: Option<&Path>,
    runner: &R,
    out: &mut Output,
) -> bool {
    // The target is re-parsed rather than re-derived: `target.host` is
    // `user@host` (the user is part of the field, not stripped), and it is the
    // string BOTH printed lines interpolate. Hand-rolling that split printed
    // `box:` where the reference printed `u@box:` — caught by the argv differ's
    // `A-create-to-ssh-prev` scenario, which is the only case that carries a
    // user, and fixed by using the sync agent's parser instead of a second one.
    let target = match stax_sync::ssh_store::parse_ssh_url(to_url) {
        Ok(target) => target,
        Err(message) => {
            out.stdout
                .push_str(&format!("  Invalid --to destination: {message}\n"));
            return false;
        }
    };
    let plan = stax_sync::replicate::plan_for(
        &target,
        &file_name(dest),
        &display(dest),
        previous.map(file_name).as_deref(),
    );
    let host = target.host.clone();
    out.stdout.push_str(&format!(
        "  Replicating → {host}:{}\n",
        plan.remote_dir.clone()
    ));
    match runner.run(&plan.mkdir_argv, 60) {
        Spawn::Completed { code: 0, .. } => {}
        Spawn::Completed { stderr, .. } => {
            // The second line is not decoration. An ssh refusal is the FIRST
            // thing that touches the network, so it is the branch users
            // actually hit, and exit 1 has to read as "the copy did not reach
            // the remote" rather than "you have no backup". The port shipped
            // without it because it was written against the other worktree's
            // older `cli.py`; the argv differ's `A-create-to-ssh-mkdir`
            // scenario is what caught the missing line.
            out.stdout.push_str(&format!(
                "  Could not create remote dir: {}\n",
                stderr.trim()
            ));
            out.stdout
                .push_str("  The local backup is intact — re-run to retry.\n");
            return false;
        }
        // `subprocess.run` of a missing `ssh` raises FileNotFoundError, which
        // `_replicate_backup` does NOT catch around the mkdir — it propagates.
        // Reproduced as the same non-zero outcome rather than a panic; recorded.
        Spawn::NotFound | Spawn::TimedOut => {
            out.stdout.push_str("  Could not create remote dir: \n");
            out.stdout
                .push_str("  The local backup is intact — re-run to retry.\n");
            return false;
        }
    }
    match runner.run(&plan.rsync_argv, 3600) {
        Spawn::Completed { code: 0, .. } => {
            out.stdout
                .push_str(&format!("  Replicated to {host}:{}\n", plan.remote_dir));
            true
        }
        Spawn::Completed { stderr, .. } => {
            let detail: String = stderr.trim().chars().take(300).collect();
            out.stdout
                .push_str(&format!("  Replication failed: {detail}\n"));
            out.stdout
                .push_str("  The local backup is intact — re-run to retry.\n");
            false
        }
        Spawn::NotFound => {
            out.stdout
                .push_str("  rsync not found — cannot replicate (local backup is intact).\n");
            false
        }
        Spawn::TimedOut => {
            out.stdout
                .push_str("  Replication timed out (>1h). Local backup is intact.\n");
            false
        }
    }
}

/// `_capture_state(dest)`.
fn capture_state(dest: &Path, env: &Env, out: &mut Output) {
    let state_dest = dest.join("stackunderflow-state");
    let _ = std::fs::create_dir_all(&state_dest);
    let mut copied: Vec<&str> = Vec::new();
    let mut skipped: Vec<&str> = Vec::new();
    for artifact in CRITICAL_ARTIFACTS {
        let src = env.state_dir.join(artifact);
        if !src.is_file() {
            skipped.push(artifact);
            continue;
        }
        let result = if artifact.ends_with(".db") {
            sqlite_backup(&src, &state_dest.join(artifact))
        } else {
            std::fs::copy(&src, state_dest.join(artifact))
                .map(|_| ())
                .map_err(|err| err.to_string())
        };
        match result {
            Ok(()) => copied.push(artifact),
            Err(message) => {
                skipped.push(artifact);
                out.stderr.push_str(&format!(
                    "backup create: could not capture {artifact}: {message}\n"
                ));
            }
        }
    }
    if !copied.is_empty() {
        out.stdout
            .push_str(&format!("  State: captured {}\n", copied.join(", ")));
    }
    if !skipped.is_empty() {
        out.stdout.push_str(&format!(
            "  State: MISSING {} — `backup verify` will flag this\n",
            skipped.join(", ")
        ));
    }
}

/// `sqlite3.Connection.backup` — the online-backup API, page for page.
fn sqlite_backup(src: &Path, dest: &Path) -> Result<(), String> {
    let source = rusqlite::Connection::open(src).map_err(|err| err.to_string())?;
    let mut target = rusqlite::Connection::open(dest).map_err(|err| err.to_string())?;
    let backup =
        rusqlite::backup::Backup::new(&source, &mut target).map_err(|err| err.to_string())?;
    backup
        .run_to_completion(1024, std::time::Duration::from_millis(0), None)
        .map_err(|err| err.to_string())
}

/// `_backup_adapter_sources(dest, previous)` — `(adapter, original, subdir)`.
fn backup_adapter_sources(
    dest: &Path,
    previous: Option<&Path>,
    env: &Env,
) -> Vec<(String, String, String)> {
    let mut copied: Vec<(String, String, String)> = Vec::new();
    let src_base = dest.join("sources");
    let main_payload = py_resolve(&env.claude_dir);
    for adapter in stax_adapters::registry::registered() {
        for (index, root) in adapter.source_roots().into_iter().enumerate() {
            if !root.exists() {
                continue;
            }
            let resolved = py_resolve(&root);
            if resolved == main_payload || resolved.starts_with(&main_payload) {
                continue;
            }
            let subdir = format!("{index}-{}", file_name(&root));
            let target = src_base.join(adapter.name()).join(&subdir);
            let _ = std::fs::create_dir_all(&target);
            let mut argv = vec!["rsync".to_owned(), "-a".to_owned()];
            if let Some(previous) = previous {
                let prev_root = previous.join("sources").join(adapter.name()).join(&subdir);
                if prev_root.exists() {
                    argv.push("--link-dest".to_owned());
                    argv.push(display(&prev_root));
                }
            }
            if root.is_dir() {
                argv.push(format!("{}/", display(&root)));
            } else {
                argv.push(display(&root));
            }
            argv.push(format!("{}/", display(&target)));
            match SystemRunner.run(&argv, 600) {
                Spawn::Completed { code: 0, .. } => {}
                Spawn::Completed { .. } | Spawn::TimedOut => continue,
                Spawn::NotFound => {
                    if root.is_dir() {
                        copytree_ignoring(&root, &target, &[]);
                    } else {
                        let _ = std::fs::copy(&root, target.join(file_name(&root)));
                    }
                }
            }
            copied.push((
                adapter.name().to_owned(),
                display(&root),
                format!("{}/{subdir}", adapter.name()),
            ));
        }
    }
    if !copied.is_empty() {
        let mut names: Vec<&str> = copied.iter().map(|(name, _, _)| name.as_str()).collect();
        names.sort_unstable();
        names.dedup();
        let manifest = stax_core::queries::pyjson::Value::Object(
            names
                .into_iter()
                .map(|name| {
                    let mut entries: Vec<(String, stax_core::queries::pyjson::Value)> = copied
                        .iter()
                        .filter(|(adapter, _, _)| adapter == name)
                        .map(|(_, original, sub)| {
                            (
                                sub.clone(),
                                stax_core::queries::pyjson::Value::Str(original.clone()),
                            )
                        })
                        .collect();
                    // `sort_keys=True` sorts every level, not just the top.
                    entries.sort_by(|a, b| a.0.cmp(&b.0));
                    (
                        name.to_owned(),
                        stax_core::queries::pyjson::Value::Object(entries),
                    )
                })
                .collect(),
        );
        let _ = std::fs::write(
            src_base.join("manifest.json"),
            stax_core::queries::pyjson::dumps_indent2(&manifest),
        );
    }
    copied
}

/// `_report_sources(copied)`.
fn report_sources(copied: &[(String, String, String)], out: &mut Output) {
    let mut agents: Vec<&str> = copied.iter().map(|(name, _, _)| name.as_str()).collect();
    agents.sort_unstable();
    agents.dedup();
    if agents.is_empty() {
        out.stdout
            .push_str("  Sources: no other agents with data on this machine.\n");
    } else {
        out.stdout.push_str(&format!(
            "  Sources: {} root(s) from {} other agent(s): {}\n",
            copied.len(),
            agents.len(),
            agents.join(", ")
        ));
    }
}

/// `_prune_backups(keep)`.
fn prune_backups(root: &Path, keep: u64, out: &mut Output) {
    if !root.exists() {
        return;
    }
    let mut dirs: Vec<PathBuf> = sorted_entries(root)
        .into_iter()
        .filter(|path| path.is_dir())
        .collect();
    dirs.sort_by_key(|path| file_name(path));
    let keep = usize::try_from(keep).unwrap_or(usize::MAX);
    while dirs.len() > keep {
        let old = dirs.remove(0);
        let _ = std::fs::remove_dir_all(&old);
        out.stdout
            .push_str(&format!("  Pruned old backup: {}\n", file_name(&old)));
    }
}

/// `shutil.copytree(src, dest, dirs_exist_ok=True, ignore=ignore_patterns(*names))`.
///
/// `ignore_patterns` matches with `fnmatch` against the *entry name* at every
/// level; the names here are literals, so the match is equality.
fn copytree_ignoring(src: &Path, dest: &Path, ignore: &[&str]) {
    let _ = std::fs::create_dir_all(dest);
    let Ok(reader) = std::fs::read_dir(src) else {
        return;
    };
    for entry in reader.flatten() {
        let path = entry.path();
        let name = file_name(&path);
        if ignore.contains(&name.as_str()) {
            continue;
        }
        if path.is_dir() {
            copytree_ignoring(&path, &dest.join(&name), ignore);
        } else {
            let _ = std::fs::copy(&path, dest.join(&name));
        }
    }
}

// ── backup restore ───────────────────────────────────────────────────────────

/// `cmd` as `backup_restore` assembles it.
///
/// `sources/` and `stackunderflow-state/` are backup-internal — restoring them
/// INTO `~/.claude` would pollute it, so they are excluded here and nowhere
/// else.
#[must_use]
pub fn restore_rsync_argv(source: &Path, dest: &Path) -> Vec<String> {
    vec![
        "rsync".to_owned(),
        "-a".to_owned(),
        "--exclude".to_owned(),
        "sources/".to_owned(),
        "--exclude".to_owned(),
        "stackunderflow-state/".to_owned(),
        format!("{}/", display(source)),
        format!("{}/", display(dest)),
    ]
}

/// `backup_restore`.
///
/// Every early return is `return`, not `sys.exit(1)` — an invalid name, a
/// missing backup and a declined confirmation all exit **0**. Ported as found.
pub fn restore<R: Runner + ?Sized, C: Confirm + ?Sized>(
    name: &str,
    dry_run: bool,
    env: &Env,
    runner: &R,
    confirm: &mut C,
) -> Output {
    let mut out = Output::default();
    let root = env.backup_root();
    let source = py_resolve(&root.join(name));
    let mut prefix = py_resolve(&root).into_os_string();
    prefix.push(std::path::MAIN_SEPARATOR_STR);
    if !display(&source).starts_with(&*prefix.to_string_lossy()) {
        out.stdout.push_str("  Invalid backup name.\n");
        return out;
    }
    if !source.exists() {
        out.stdout.push_str(&format!(
            "  Backup '{name}' not found. Run: stax backup list\n"
        ));
        return out;
    }

    let dest = &env.claude_dir;
    let (total_files, _, _) = measure(&source);

    if dry_run {
        out.stdout.push_str(&format!(
            "  Would restore {total_files} files from {} → {}\n",
            display(&source),
            display(dest)
        ));
        return out;
    }

    let prompt = format!(
        "  This will overwrite files in {}. Continue? [y/N]: ",
        display(dest)
    );
    match confirm.confirm(&prompt, &mut out.stdout) {
        Some(true) => {}
        Some(false) => return out,
        None => {
            // `click.Abort` in standalone mode: `Aborted!` on stderr, exit 1.
            out.stderr.push_str("Aborted!\n");
            out.code = 1;
            return out;
        }
    }

    out.stdout.push_str(&format!(
        "  Restoring {total_files} files from {} → {}\n",
        display(&source),
        display(dest)
    ));
    let argv = restore_rsync_argv(&source, dest);
    match runner.run(&argv, 300) {
        Spawn::Completed { code: 0, .. } => out.stdout.push_str("  Restore complete.\n"),
        Spawn::Completed { stderr, .. } => {
            out.stdout
                .push_str(&format!("  rsync error: {}\n", stderr.trim()));
        }
        Spawn::NotFound => {
            copytree_ignoring(&source, dest, &["sources", "stackunderflow-state"]);
            out.stdout.push_str("  Restore complete (via shutil).\n");
        }
        // `subprocess.TimeoutExpired` is NOT caught here (it IS in `create`), so
        // the reference dies with a traceback at exit **1**. The port declines
        // to crash — a traceback is not portable — but it must not turn a
        // failure into a success either: the exit code is the half of that
        // contract a script actually reads, and this branch used to `return`
        // the default 0, so `stax backup restore` reported a timed-out restore
        // as done. DIV-256, and the residue is now stdout-only (one extra
        // `  rsync error: ` line where the reference emits a traceback on
        // stderr) rather than a changed answer.
        Spawn::TimedOut => {
            out.stdout.push_str("  rsync error: \n");
            out.code = 1;
        }
    }
    out
}

// ── backup auto ──────────────────────────────────────────────────────────────

/// `com.staxtrace.backup` — the launchd label and the plist file stem.
pub const PLIST_ID: &str = "com.staxtrace.backup";

/// The pre-rename label, still torn down by `--auto off`.
///
/// This one is REGISTERED WITH launchd on machines that enabled the schedule
/// before the rename. Changing only the constant would leave that job loaded
/// and running forever, invisible to every command that now looks for the new
/// label — disabling backups would silently not disable them.
pub const PLIST_ID_LEGACY: &str = "com.stackunderflow.backup";

/// The launchd plist `backup auto --enable` writes on Darwin, byte for byte.
///
/// Pure so it can be diffed against the reference's own output on a Linux host:
/// `tests/plist_golden.rs` compares it to a plist generated by the real
/// `cli.py` with `platform.system` faked. Nothing here writes or loads anything.
#[must_use]
pub fn darwin_plist(su_bin: &str, state_dir: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{PLIST_ID}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{su_bin}</string>
        <string>backup</string>
        <string>create</string>
        <string>--label</string>
        <string>auto</string>
        <string>--keep</string>
        <string>10</string>
    </array>
    <key>StartCalendarInterval</key>
    <dict>
        <key>Hour</key>
        <integer>3</integer>
        <key>Minute</key>
        <integer>0</integer>
    </dict>
    <key>StandardOutPath</key>
    <string>{state_dir}/backup.log</string>
    <key>StandardErrorPath</key>
    <string>{state_dir}/backup.log</string>
</dict>
</plist>"#
    )
}

/// The crontab line the non-Darwin branch prints (and never installs).
#[must_use]
pub fn cron_line(su_bin: &str) -> String {
    format!("0 3 * * * {su_bin} backup create --label auto --keep 10")
}

/// `backup_auto`.
///
/// The Darwin leg writes `~/Library/LaunchAgents/<id>.plist` and shells out to
/// `launchctl`; it is unreachable on this host and, per the tranche brief, is
/// never executed against a real system anywhere. What is proven instead is the
/// plist's bytes ([`darwin_plist`]) and the argv `launchctl` would receive
/// ([`launchctl_argv`]).
#[must_use]
pub fn auto(enable: bool, env: &Env) -> Output {
    if env.system == "Darwin" {
        return auto_darwin(enable, env);
    }
    let Some(su_bin) = &env.stackunderflow_bin else {
        return Output::ok("  Can't find stackunderflow in PATH.\n");
    };
    // Both branches print the same line; only the instruction differs.
    let line = cron_line(&display(su_bin));
    let header = if enable {
        "  Add this to your crontab (crontab -e):\n"
    } else {
        "  Remove this line from your crontab (crontab -e):\n"
    };
    Output::ok(format!("{header}\n  {line}\n"))
}

/// `["launchctl", "load"|"unload", str(plist_path)]`.
#[must_use]
pub fn launchctl_argv(action: &str, plist_path: &Path) -> Vec<String> {
    vec![
        "launchctl".to_owned(),
        action.to_owned(),
        display(plist_path),
    ]
}

/// The Darwin leg of [`auto`], expressed as the bytes it would write.
fn auto_darwin(enable: bool, env: &Env) -> Output {
    let plist_dir = env.home.join("Library").join("LaunchAgents");
    let plist_path = plist_dir.join(format!("{PLIST_ID}.plist"));
    let legacy_path = plist_dir.join(format!("{PLIST_ID_LEGACY}.plist"));
    if !enable {
        // Tear down BOTH generations: a machine that enabled backups before the
        // rename has the old label loaded, and leaving it would mean "disabled"
        // that keeps running.
        let mut disabled = false;
        for path in [&plist_path, &legacy_path] {
            if path.exists() {
                let _ = SystemRunner.run(&launchctl_argv("unload", path), NO_TIMEOUT);
                let _ = std::fs::remove_file(path);
                disabled = true;
            }
        }
        if disabled {
            return Output::ok("  Automatic backups disabled.\n");
        }
        return Output::ok("  Automatic backups are not enabled.\n");
    }
    // Enabling replaces a pre-rename job rather than racing it.
    if legacy_path.exists() {
        let _ = SystemRunner.run(&launchctl_argv("unload", &legacy_path), NO_TIMEOUT);
        let _ = std::fs::remove_file(&legacy_path);
    }
    let Some(su_bin) = &env.stackunderflow_bin else {
        return Output::ok("  Can't find stackunderflow in PATH. Install it first.\n");
    };
    let content = darwin_plist(&display(su_bin), &display(&env.state_dir));
    let _ = std::fs::create_dir_all(&plist_dir);
    let _ = std::fs::write(&plist_path, content);
    let _ = SystemRunner.run(&launchctl_argv("load", &plist_path), NO_TIMEOUT);
    Output::ok(format!(
        "  Daily backup enabled (3:00 AM). Keeps last 10.\n  Plist: {}\n",
        display(&plist_path)
    ))
}

/// `str(path)` — the bytes Python prints for a path.
fn display(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Scratch(PathBuf);

    impl Scratch {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "stax-backup-{tag}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            std::fs::remove_dir_all(&dir).ok();
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn path(&self) -> &Path {
            &self.0
        }

        fn file(&self, relative: &str, contents: &str) {
            let path = self.0.join(relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, contents).unwrap();
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).ok();
        }
    }

    #[test]
    fn a_missing_root_and_an_empty_root_print_the_same_line() {
        let scratch = Scratch::new("empty");
        let missing = scratch.path().join("backups");
        assert_eq!(list(&missing).stdout, NO_BACKUPS);
        std::fs::create_dir_all(&missing).unwrap();
        assert_eq!(list(&missing).stdout, NO_BACKUPS);
    }

    #[test]
    fn the_count_includes_stray_files_but_the_listing_does_not() {
        let scratch = Scratch::new("stray");
        let root = scratch.path().join("backups");
        std::fs::create_dir_all(root.join("2026-07-30_120000/x")).unwrap();
        std::fs::write(root.join("2026-07-30_120000/x/a.jsonl"), "hi\n").unwrap();
        std::fs::write(root.join("notadir"), "").unwrap();
        let out = list(&root).stdout;
        assert!(
            out.starts_with(&format!("  2 backup(s) in {}\n\n", root.display())),
            "{out}"
        );
        assert!(
            out.ends_with("  2026-07-30_120000  (1 files, 0.0 MB)\n"),
            "{out}"
        );
        assert_eq!(out.lines().count(), 3, "header, blank, one row: {out}");
    }

    #[test]
    fn hidden_entries_count_toward_the_totals() {
        let scratch = Scratch::new("hidden");
        let root = scratch.path().join("backups");
        std::fs::create_dir_all(root.join("b/.hidden")).unwrap();
        std::fs::write(root.join("b/.hidden/a.jsonl"), "x").unwrap();
        let out = list(&root).stdout;
        assert!(
            out.contains("(1 files,"),
            "pathlib rglob has no dot rule: {out}"
        );
    }

    #[test]
    fn megabytes_round_the_way_python_formats() {
        let scratch = Scratch::new("size");
        let root = scratch.path().join("backups");
        std::fs::create_dir_all(root.join("b")).unwrap();
        // 1.5 MiB exactly.
        std::fs::write(root.join("b/blob"), vec![0_u8; 1_572_864]).unwrap();
        assert!(list(&root).stdout.contains("(0 files, 1.5 MB)"));
    }

    #[test]
    fn verify_with_no_backups_exits_one() {
        let scratch = Scratch::new("verify-none");
        let root = scratch.path().join("backups");
        let out = verify(&root, None);
        assert_eq!(out.code, 1);
        assert_eq!(
            out.stdout,
            "  No backups to verify. Run: stax backup create\n"
        );
    }

    #[test]
    fn an_escaping_name_is_rejected_before_the_existence_test() {
        let scratch = Scratch::new("verify-escape");
        let root = scratch.path().join("backups");
        std::fs::create_dir_all(&root).unwrap();
        let out = verify(&root, Some("../etc"));
        assert_eq!(out.code, 1);
        assert_eq!(out.stdout, "  Invalid backup name.\n");
    }

    #[test]
    fn a_missing_named_backup_names_itself() {
        let scratch = Scratch::new("verify-missing");
        let root = scratch.path().join("backups");
        std::fs::create_dir_all(&root).unwrap();
        let out = verify(&root, Some("nope"));
        assert_eq!(out.code, 1);
        assert_eq!(
            out.stdout,
            "  Backup 'nope' not found. Run: stax backup list\n"
        );
    }

    #[test]
    fn an_incomplete_backup_lists_every_artifact_and_logs_to_stderr() {
        let scratch = Scratch::new("verify-incomplete");
        let root = scratch.path().join("backups");
        std::fs::create_dir_all(root.join("2026-07-30_120000/x")).unwrap();
        std::fs::write(root.join("2026-07-30_120000/x/a.jsonl"), "hi").unwrap();
        let out = verify(&root, None);
        assert_eq!(out.code, 1);
        assert_eq!(
            out.stdout,
            concat!(
                "  Verifying 2026-07-30_120000\n",
                "    store.db         MISSING\n",
                "    search_index.db  MISSING\n",
                "    qa_pairs.db      MISSING\n",
                "    tags.json        MISSING\n",
                "  Incomplete: missing 4 of 4 artifact(s): store.db, search_index.db, qa_pairs.db, tags.json\n",
            )
        );
        assert_eq!(
            out.stderr,
            "backup verify: 2026-07-30_120000 missing store.db, search_index.db, qa_pairs.db, tags.json\n"
        );
    }

    #[test]
    fn a_complete_backup_passes_even_when_the_artifacts_are_nested() {
        let scratch = Scratch::new("verify-ok");
        let root = scratch.path().join("backups");
        for artifact in CRITICAL_ARTIFACTS {
            scratch.file(
                &format!("backups/2026-07-30_120000/stackunderflow-state/{artifact}"),
                "x",
            );
        }
        let out = verify(&root, None);
        assert_eq!(out.code, 0);
        assert!(out.stderr.is_empty());
        assert_eq!(
            out.stdout,
            concat!(
                "  Verifying 2026-07-30_120000\n",
                "    store.db         ok\n",
                "    search_index.db  ok\n",
                "    qa_pairs.db      ok\n",
                "    tags.json        ok\n",
                "  OK: all 4 critical artifacts present.\n",
            )
        );
    }

    #[test]
    fn an_empty_name_is_falsy_and_means_latest() {
        // `if name:` — not `if name is not None:`. Found by the parity row,
        // not by reading: the port rejected `--name ''` as an escaping path
        // where Python verified the newest backup and exited 0.
        let scratch = Scratch::new("verify-empty-name");
        let root = scratch.path().join("backups");
        for artifact in CRITICAL_ARTIFACTS {
            scratch.file(&format!("backups/2026-07-30_120000/{artifact}"), "x");
        }
        let empty = verify(&root, Some(""));
        assert_eq!(empty.code, 0);
        assert_eq!(empty, verify(&root, None));
    }

    #[test]
    fn latest_is_the_last_directory_by_name() {
        let scratch = Scratch::new("latest");
        let root = scratch.path().join("backups");
        for name in [
            "2026-07-01_000000",
            "2026-07-30_120000",
            "2026-06-01_000000",
        ] {
            std::fs::create_dir_all(root.join(name)).unwrap();
        }
        std::fs::write(root.join("zzz-a-file"), "").unwrap();
        assert_eq!(
            latest_backup(&root).map(|path| file_name(&path)),
            Some("2026-07-30_120000".to_owned()),
            "a stray file sorting last must not become the latest backup"
        );
    }

    #[test]
    fn resolve_folds_dotdot_out_of_the_root() {
        let scratch = Scratch::new("resolve");
        let root = scratch.path().join("backups");
        std::fs::create_dir_all(&root).unwrap();
        let escaped = py_resolve(&root.join("../elsewhere"));
        assert!(!escaped.starts_with(py_resolve(&root)), "{escaped:?}");
    }
}
