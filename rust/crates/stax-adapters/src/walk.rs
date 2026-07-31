//! `pathlib` directory walking, with Python's ordering.
//!
//! Nine of the twenty adapters enumerate with the same four idioms —
//! `sorted(root.iterdir())`, `sorted(dir.glob("*.jsonl"))`,
//! `sorted(root.rglob("*.chat"))`, and "read the first line of the file" — and
//! the ordering of each one is load-bearing: the parity harness diffs record
//! streams line-for-line, and a walk that visits `b/` before `a/` produces the
//! same set in a different order.
//!
//! [`crate::claude`] and [`crate::codex`] each carry a private `read_dir_sorted`
//! from the batch that landed them. Those stay where they are — a single
//! directory listing sorts identically whether you compare `PathBuf`s or
//! strings, so there is nothing to reconcile. The recursive walks are the ones
//! that need a shared home, because they do *not*:
//!
//! > Python sorts `Path` objects by `str(path)`. Rust's `PathBuf: Ord` compares
//! > component by component. Those disagree the moment a name contains a
//! > character below `/` (0x2F): `sorted()` puts `a.db` before `a/b.db`
//! > (`.` is 0x2E), while `PathBuf` ordering puts `a/b.db` first. Every
//! > recursive helper here sorts by the path *string*, which is what
//! > `continue_adapter._walk_db_files` and `kiro.enumerate` actually do.

use std::path::{Path, PathBuf};

/// `sorted(root.iterdir())` — one directory level, sorted, never failing.
///
/// An unreadable root yields an empty vector: enumeration must degrade to "no
/// sessions", never to an error.
#[must_use]
pub fn read_dir_sorted(root: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut paths: Vec<PathBuf> = entries.flatten().map(|entry| entry.path()).collect();
    paths.sort();
    paths
}

/// `sorted(p for p in root.iterdir() if p.is_dir())`.
#[must_use]
pub fn child_dirs(root: &Path) -> Vec<PathBuf> {
    read_dir_sorted(root)
        .into_iter()
        .filter(|path| path.is_dir())
        .collect()
}

/// `sorted(dir.glob("*<suffix>"))` — one level, files and directories alike.
///
/// `pathlib.Path.glob` (unlike the `glob` module) does not hide dotfiles, so
/// neither does this: a `.hidden.jsonl` is enumerated by both implementations.
#[must_use]
pub fn glob_suffix(dir: &Path, suffix: &str) -> Vec<PathBuf> {
    read_dir_sorted(dir)
        .into_iter()
        .filter(|path| {
            path.file_name()
                .is_some_and(|name| name.to_string_lossy().ends_with(suffix))
        })
        .collect()
}

/// `sorted(root.rglob("*<suffix>"))` / `sorted(root.glob("**/*<suffix>"))`.
///
/// Both spellings mean the same walk: this directory and every subdirectory,
/// symlinked directories excluded (`pathlib`'s `**` does not follow them), with
/// the whole result sorted by path string.
#[must_use]
pub fn rglob_suffix(root: &Path, suffix: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    collect_recursive(root, &mut out);
    out.retain(|path| {
        path.file_name()
            .is_some_and(|name| name.to_string_lossy().ends_with(suffix))
    });
    sort_by_string(&mut out);
    out
}

/// `sorted(root.rglob("*"))` — every entry beneath `root`, files and
/// directories, sorted by path string.
#[must_use]
pub fn rglob_all(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    collect_recursive(root, &mut out);
    sort_by_string(&mut out);
    out
}

/// Depth-first collection; the caller sorts. Symlinked directories are listed
/// but not descended into, matching `pathlib`'s `**`.
fn collect_recursive(root: &Path, out: &mut Vec<PathBuf>) {
    for path in read_dir_sorted(root) {
        let is_symlink = std::fs::symlink_metadata(&path).is_ok_and(|meta| meta.is_symlink());
        out.push(path.clone());
        if !is_symlink && path.is_dir() {
            collect_recursive(&path, out);
        }
    }
}

/// Sort by `str(path)`, which is what `sorted()` over `Path` objects does.
///
/// **DIVERGENCE (recorded, unreachable in practice).** A path that is not valid
/// UTF-8 sorts here by its lossy rendering (`U+FFFD` for each bad byte) where
/// Python sorts by its `surrogateescape` rendering. Both are stable; they can
/// only disagree on a session file whose *name* is mojibake.
fn sort_by_string(paths: &mut [PathBuf]) {
    paths.sort_by(|a, b| a.to_string_lossy().cmp(&b.to_string_lossy()));
}

/// The first line of `path`, terminator included — `fh.readline()` in binary
/// mode.
///
/// `None` for an unreadable file, which is every caller's "no metadata, fall
/// back to the filename stem" branch.
#[must_use]
pub fn first_line(path: &Path) -> Option<Vec<u8>> {
    use std::io::BufRead;
    let file = std::fs::File::open(path).ok()?;
    let mut reader = std::io::BufReader::new(file);
    let mut line = Vec::new();
    reader.read_until(b'\n', &mut line).ok()?;
    Some(line)
}

/// `Path.name`, as a `String` — `""` for a path that ends in `..` or is empty.
#[must_use]
pub fn dir_name(path: &Path) -> String {
    path.file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned()
}

/// `Path.stem` — the file name with its last suffix removed.
#[must_use]
pub fn file_stem(path: &Path) -> String {
    let name = dir_name(path);
    // `Path::file_stem` splits at the *first* dot for names that begin with
    // one; `pathlib.PurePath.stem` never treats a leading dot as a suffix
    // boundary, so `.env.jsonl` stems to `.env` in both, but `.jsonl` stems to
    // `.jsonl` in Python and to `.jsonl` here — hence the explicit rfind.
    match name.rfind('.') {
        Some(index) if index > 0 => name[..index].to_string(),
        _ => name,
    }
}

/// `Path.with_suffix(new)` — replace the last suffix, or append when the name
/// has none (`droid`'s `foo.jsonl` → `foo.settings.json`).
#[must_use]
pub fn with_suffix(path: &Path, new_suffix: &str) -> PathBuf {
    let stem = file_stem(path);
    path.with_file_name(format!("{stem}{new_suffix}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Scratch(PathBuf);

    impl Scratch {
        fn new(tag: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "stax-walk-{tag}-{}-{:?}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |delta| delta.subsec_nanos())
            ));
            std::fs::create_dir_all(&path).expect("scratch");
            Self(path)
        }
        fn touch(&self, relative: &str) {
            let target = self.0.join(relative);
            std::fs::create_dir_all(target.parent().expect("parent")).expect("mkdir");
            std::fs::write(&target, b"{}\n").expect("write");
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn recursive_globs_sort_by_string_the_way_python_does() {
        let scratch = Scratch::new("rglob");
        scratch.touch("a.jsonl");
        scratch.touch("a/b.jsonl");
        scratch.touch("a/c.txt");
        let found: Vec<String> = rglob_suffix(&scratch.0, ".jsonl")
            .iter()
            .map(|path| {
                path.strip_prefix(&scratch.0)
                    .expect("prefix")
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        // `sorted()` over Path objects compares `str(path)`: '.' (0x2e) sorts
        // before '/' (0x2f), so the top-level file comes first. Sorting
        // `PathBuf`s would put `a/b.jsonl` first and silently reorder the
        // record stream.
        assert_eq!(found, vec!["a.jsonl", "a/b.jsonl"]);
    }

    #[test]
    fn one_level_helpers_ignore_what_python_ignores() {
        let scratch = Scratch::new("glob");
        scratch.touch("keep.jsonl");
        scratch.touch(".hidden.jsonl");
        scratch.touch("skip.txt");
        scratch.touch("nested/deep.jsonl");
        let names: Vec<String> = glob_suffix(&scratch.0, ".jsonl")
            .iter()
            .map(|path| dir_name(path))
            .collect();
        // Dotfiles included (pathlib, not glob); nested files excluded.
        assert_eq!(names, vec![".hidden.jsonl", "keep.jsonl"]);
        let dirs: Vec<String> = child_dirs(&scratch.0)
            .iter()
            .map(|path| dir_name(path))
            .collect();
        assert_eq!(dirs, vec!["nested"], "only `nested/` is a directory");
    }

    #[test]
    fn a_missing_root_walks_to_nothing_rather_than_failing() {
        let missing = std::env::temp_dir().join("stax-walk-does-not-exist");
        let _ = std::fs::remove_dir_all(&missing);
        assert!(read_dir_sorted(&missing).is_empty());
        assert!(rglob_all(&missing).is_empty());
        assert!(rglob_suffix(&missing, ".jsonl").is_empty());
        assert!(first_line(&missing).is_none());
    }

    #[test]
    fn stem_and_with_suffix_match_pathlib() {
        assert_eq!(file_stem(Path::new("/a/session.jsonl")), "session");
        assert_eq!(file_stem(Path::new("/a/foo.bar.jsonl")), "foo.bar");
        assert_eq!(file_stem(Path::new("/a/plain")), "plain");
        assert_eq!(file_stem(Path::new("/a/.jsonl")), ".jsonl");
        assert_eq!(
            with_suffix(Path::new("/a/session.jsonl"), ".settings.json"),
            Path::new("/a/session.settings.json")
        );
        assert_eq!(
            with_suffix(Path::new("/a/plain"), ".settings.json"),
            Path::new("/a/plain.settings.json")
        );
    }
}
