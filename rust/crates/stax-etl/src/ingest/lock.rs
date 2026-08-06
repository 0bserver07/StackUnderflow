//! Port of `stackunderflow/etl/lock.py` — the single-watcher invariant.
//!
//! Two `stax start` invocations against one store would run two watchers,
//! racing on ingest and mart refresh. The fence is the reference's: an
//! OS-level advisory lock on `<app_dir>/server.lock`, exclusive and
//! non-blocking, held for the watcher's lifetime and released by the kernel
//! on fd close (process exit covers the abnormal case).
//!
//! `fcntl.flock(LOCK_EX | LOCK_NB)` is [`File::try_lock`] here — stable std
//! since 1.89, no new dependency, no `unsafe`. On Linux both are flock(2) on
//! the same inode, so a Python holder fences a Rust contender and vice versa:
//! that cross-implementation fencing is exactly what the flip's rollback path
//! (Python resumes on the same store) depends on, and the reason this module
//! exists rather than an in-process mutex.
//!
//! The file's content (`<pid>\n<start_ts>\n`) is informational only — read by
//! `stax_reports::etl_status::read_lock_holder` for the status route's
//! `lock_held_by`. The flock is the actual gate; the metadata is never
//! trusted as one. Stale-detect mirrors the reference: a recorded PID that no
//! longer maps to a live process is truncated so the *display* stays honest,
//! while the kernel keeps the final answer either way.

use std::fs::{File, OpenOptions, TryLockError};
use std::io::{Seek as _, SeekFrom, Write as _};
use std::path::{Path, PathBuf};

use stax_core::queries::pytime;

/// A held watcher lock. Keep it referenced for the watcher's lifetime.
///
/// Dropping releases the OS lock (the fd closes) and removes the metadata
/// file — the reference's `release_watcher_lock`, which it registers with
/// `atexit`; Rust's drop-at-scope-end is the same hook with better coverage.
pub struct LockHandle {
    file: Option<File>,
    path: PathBuf,
}

impl Drop for LockHandle {
    fn drop(&mut self) {
        drop(self.file.take());
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Try to acquire the watcher singleton lock at `path`.
///
/// `Some` on success. `None` when another live process already holds it — the
/// caller serves HTTP without spawning a watcher — and on any filesystem
/// failure, each with the reference's warning (to stderr, where its logging
/// goes). Failure to *write the metadata* keeps the lock, as the reference
/// does: a blank `lock_held_by` is better than two watchers.
#[must_use]
pub fn acquire_watcher_lock(path: &Path) -> Option<LockHandle> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
        && let Err(err) = std::fs::create_dir_all(parent)
    {
        eprintln!(
            "etl.lock: could not create {} parent: {err}",
            path.display()
        );
        return None;
    }

    // Stale-detect, informational only (see module docs).
    if let Some(pid) = recorded_pid(path)
        && !pid_alive(pid)
    {
        let _ = std::fs::write(path, "");
    }

    let file = match OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
    {
        Ok(file) => file,
        Err(err) => {
            eprintln!("etl.lock: could not open {}: {err}", path.display());
            return None;
        }
    };

    match file.try_lock() {
        Ok(()) => {}
        Err(TryLockError::WouldBlock) => return None,
        Err(TryLockError::Error(err)) => {
            eprintln!("etl.lock: could not lock {}: {err}", path.display());
            return None;
        }
    }

    let mut handle = LockHandle {
        file: Some(file),
        path: path.to_path_buf(),
    };
    if let Some(file) = handle.file.as_mut() {
        let metadata = format!(
            "{}\n{}\n",
            std::process::id(),
            pytime::isoformat_utc(pytime::now_micros())
        );
        let written = file
            .set_len(0)
            .and_then(|()| file.seek(SeekFrom::Start(0)).map(drop))
            .and_then(|()| file.write_all(metadata.as_bytes()))
            .and_then(|()| file.flush());
        if let Err(err) = written {
            eprintln!(
                "etl.lock: could not write metadata to {}: {err}",
                path.display()
            );
        }
    }
    Some(handle)
}

/// First line of the lock file as a PID, `None` on every failure — the local
/// twin of `stax_reports::etl_status::read_lock_holder`, re-derived here
/// because the dependency points the other way.
fn recorded_pid(path: &Path) -> Option<u32> {
    let text = std::fs::read_to_string(path).ok()?;
    text.lines().next()?.trim().parse().ok()
}

/// `os.kill(pid, 0)` without a syscall the workspace forbids: on Linux —
/// the deployment — `/proc/<pid>` existing is process liveness. Elsewhere the
/// stale-detect is skipped (`true` = never truncate) and the flock still
/// answers correctly; only the informational display can go stale.
fn pid_alive(pid: u32) -> bool {
    if cfg!(target_os = "linux") {
        Path::new(&format!("/proc/{pid}")).exists()
    } else {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("stax-lock-{}-{name}", std::process::id()))
    }

    #[test]
    fn second_acquire_fails_while_held() {
        let path = scratch("contend").join("server.lock");
        let first = acquire_watcher_lock(&path).expect("first acquire");
        // flock is per open-file-description: a second fd in the same process
        // contends exactly as a second process would.
        assert!(
            acquire_watcher_lock(&path).is_none(),
            "the singleton invariant"
        );
        drop(first);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn drop_releases_and_clears_metadata() {
        let path = scratch("release").join("server.lock");
        let handle = acquire_watcher_lock(&path).expect("acquire");
        assert_eq!(
            recorded_pid(&path),
            Some(std::process::id()),
            "metadata records the holder"
        );
        drop(handle);
        assert!(!path.exists(), "release removes the metadata file");
        let second = acquire_watcher_lock(&path).expect("reacquire after release");
        drop(second);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn a_stale_pid_is_cleared_not_trusted() {
        let dir = scratch("stale");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("server.lock");
        // No live process has PID 0's successor space up at u32::MAX on Linux
        // (pid_max caps far below); the file exists, the flock does not.
        std::fs::write(&path, format!("{}\nleftover\n", u32::MAX)).unwrap();
        let handle = acquire_watcher_lock(&path).expect("stale lock is reclaimable");
        assert_eq!(recorded_pid(&path), Some(std::process::id()));
        drop(handle);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
