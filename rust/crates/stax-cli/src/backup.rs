//! `stax backup list` / `stax backup verify` — `cli.py:1404`–`:1422` and
//! `:1356`–`:1401`, plus `_latest_backup` and `_CRITICAL_ARTIFACTS`.
//!
//! The read-only leg of the `backup` group. `create` / `restore` / `auto` shell
//! out to `rsync`, `launchctl` and `crontab`; those are tranche 2 and get the
//! argv-differ treatment rather than an output diff, because the interesting
//! contract there is *what command line was assembled*, not what was printed.
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

/// `_CRITICAL_ARTIFACTS` — order matters, it is the print order.
pub const CRITICAL_ARTIFACTS: [&str; 4] =
    ["store.db", "search_index.db", "qa_pairs.db", "tags.json"];

/// `stax backup` — back up and restore session data.
#[derive(Debug, Args)]
pub struct BackupArgs {
    /// The subcommand.
    #[command(subcommand)]
    pub verb: BackupVerb,
}

/// The ported `backup` verbs. `create` / `restore` / `auto` are tranche 2.
#[derive(Debug, Subcommand)]
pub enum BackupVerb {
    /// List existing backups.
    List,
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
        BackupVerb::List => Ok(list(&root)),
        BackupVerb::Verify { name } => Ok(verify(&root, name.as_deref())),
    }
}

// ── backup list ──────────────────────────────────────────────────────────────

const NO_BACKUPS: &str = "  No backups yet. Run: stackunderflow backup create\n";

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
        let (files, bytes) = measure(entry);
        let megabytes = bytes as f64 / f64::from(1_u32 << 20);
        out.push_str(&format!(
            "  {}  ({files} files, {megabytes:.1} MB)\n",
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

/// `sum(1 for _ in b.rglob("*.jsonl"))` and
/// `sum(f.stat().st_size for f in b.rglob("*") if f.is_file())`, in one walk.
///
/// The `.jsonl` count is over **every** matched entry, files and directories
/// alike — `rglob` does not filter by kind and the reference does not either.
fn measure(root: &Path) -> (u64, u64) {
    let mut files = 0_u64;
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
                files += 1;
            }
            // `symlink_metadata`: `rglob` does not follow directory symlinks
            // (CPython 3.12), so neither does this walk.
            let Ok(meta) = std::fs::symlink_metadata(&path) else {
                continue;
            };
            if meta.is_dir() {
                stack.push(path);
            } else if meta.is_file() {
                bytes += meta.len();
            }
        }
    }
    (files, bytes)
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
                    "  Backup '{name}' not found. Run: stackunderflow backup list\n"
                ));
            }
            target
        }
        None => match latest_backup(root) {
            Some(target) => target,
            None => {
                return Output::exit1(
                    "  No backups to verify. Run: stackunderflow backup create\n",
                );
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
            "  No backups to verify. Run: stackunderflow backup create\n"
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
            "  Backup 'nope' not found. Run: stackunderflow backup list\n"
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
