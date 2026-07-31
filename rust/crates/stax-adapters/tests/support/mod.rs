//! Shared test scaffolding: temp homes, fixture paths, and the Python
//! reference runner.
//!
//! Every helper here writes only inside a temp directory it created. The
//! campaign's live dataset and the checked-in fixture packs are read-only: the
//! claude fixture pack is reached through a *symlinked* projects root rather
//! than a copy, so both implementations parse the identical bytes on disk.
#![allow(
    dead_code,
    reason = "each integration-test binary compiles this module separately and \
    uses a different subset of it"
)]

use std::path::{Path, PathBuf};
use std::process::Command;

/// A directory removed when it goes out of scope.
///
/// Hand-rolled instead of pulling in `tempfile`: this crate's whole dependency
/// surface is `serde_json` + `anyhow`, and a shared `Cargo.lock` in a
/// many-agent campaign is worth keeping small.
pub struct TempDir {
    path: PathBuf,
}

impl TempDir {
    /// Create a uniquely-named directory under the system temp dir.
    pub fn new(tag: &str) -> Self {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |delta| delta.subsec_nanos());
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "stax-adapters-{tag}-{}-{nanos}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).expect("create temp dir");
        Self { path }
    }

    /// The directory itself.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Write `contents` to `relative`, creating parent directories.
    pub fn write(&self, relative: &str, contents: &str) -> PathBuf {
        let target = self.path.join(relative);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).expect("create parent");
        }
        std::fs::write(&target, contents).expect("write fixture");
        target
    }

    /// Create an empty directory at `relative`.
    pub fn mkdir(&self, relative: &str) -> PathBuf {
        let target = self.path.join(relative);
        std::fs::create_dir_all(&target).expect("create dir");
        target
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// The repository root — `rust/crates/stax-adapters` up three levels.
pub fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(3)
        .expect("repo root")
        .to_path_buf()
}

/// A checked-in fixture path, e.g. `tests/mock-data/codex-sessions`.
pub fn fixture(relative: &str) -> PathBuf {
    repo_root().join(relative)
}

/// The interpreter that drives the Python reference adapters.
///
/// `$STAX_PARITY_PYTHON` when set, else the campaign's venv beside the
/// worktree. `None` means "no reference available" and the parity tests report
/// that instead of failing — a Rust checkout without the Python tree next to it
/// must still build and test.
pub fn reference_python() -> Option<PathBuf> {
    if let Some(explicit) = std::env::var_os("STAX_PARITY_PYTHON") {
        let path = PathBuf::from(explicit);
        return path.is_file().then_some(path);
    }
    let candidate = repo_root()
        .parent()?
        .join("StackUnderflow")
        .join(".venv")
        .join("bin")
        .join("python");
    candidate.is_file().then_some(candidate)
}

/// Run `parity/python_reference.py` with the worktree on `PYTHONPATH`.
///
/// Returns its stdout. A non-zero exit is a test failure, not a skip: if the
/// reference is present it must work.
pub fn run_python_reference(args: &[&str]) -> String {
    let python = reference_python().expect("reference interpreter");
    let script = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("parity")
        .join("python_reference.py");
    let output = Command::new(python)
        .arg(&script)
        .args(args)
        .env("PYTHONPATH", repo_root())
        // The reference must not inherit a CLAUDE_CONFIG_DIR the harness did
        // not set: every claude test injects its own root explicitly.
        .env_remove("CLAUDE_CONFIG_DIR")
        .output()
        .expect("run python reference");
    assert!(
        output.status.success(),
        "python reference failed ({}):\n{}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("reference output is utf-8")
}

/// Print the reason a parity test is being skipped, so a green run that proved
/// nothing still says so out loud.
pub fn note_missing_reference(test: &str) {
    eprintln!(
        "SKIP {test}: no Python reference interpreter \
         (set STAX_PARITY_PYTHON to enable the parity diff)"
    );
}

/// Symlink `target` to `link` — used to expose a read-only fixture directory as
/// a claude projects root without copying it.
#[cfg(unix)]
pub fn symlink_dir(target: &Path, link: &Path) {
    std::os::unix::fs::symlink(target, link).expect("symlink fixture root");
}

/// Compare two multi-line dumps and fail with the first differing line.
pub fn assert_same_lines(label: &str, python: &str, rust: &str) {
    let py_lines: Vec<&str> = python.lines().collect();
    let rs_lines: Vec<&str> = rust.lines().collect();
    for (index, (py, rs)) in py_lines.iter().zip(rs_lines.iter()).enumerate() {
        assert_eq!(
            py,
            rs,
            "{label}: line {} differs\n  python: {py}\n  rust:   {rs}",
            index + 1
        );
    }
    assert_eq!(
        py_lines.len(),
        rs_lines.len(),
        "{label}: python produced {} lines, rust {}",
        py_lines.len(),
        rs_lines.len()
    );
}
