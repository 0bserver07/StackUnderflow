//! Where the data directory lives — the Rust half of Python's `settings.app_dir()`.
//!
//! Resolution is deliberately identical to `stackunderflow/settings.py:22`:
//! `$STACKUNDERFLOW_HOME` when set (with a leading `~` expanded), otherwise
//! `~/.stackunderflow`. Every path the campaign touches derives from here, so
//! pointing both implementations at the same dataset stays one environment
//! variable — which is exactly how the parity harness runs them side by side.

use std::env;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

/// The environment variable that re-points the whole application at a dataset.
///
/// Mirrors `stackunderflow.settings.APP_DIR_ENV`.
pub const APP_DIR_ENV: &str = "STACKUNDERFLOW_HOME";

/// The data directory holding the store, the indexes, and `config.json`.
///
/// `$STACKUNDERFLOW_HOME` when set, else `~/.stackunderflow`. Unlike Python this
/// is resolved on every call rather than bound at import, which is strictly more
/// permissive: no caller can observe a stale value.
#[must_use]
pub fn app_dir() -> PathBuf {
    resolve_app_dir(env::var_os(APP_DIR_ENV).as_deref(), home_dir().as_deref())
}

/// The SQLite store — `app_dir()/store.db`, matching `deps.store_path`.
#[must_use]
pub fn store_path() -> PathBuf {
    app_dir().join("store.db")
}

/// The pure core of [`app_dir`], with the environment injected.
///
/// Split out because Rust 2024 makes `std::env::set_var` `unsafe` and this crate
/// forbids `unsafe`; testing resolution therefore has to happen without mutating
/// process state.
///
/// `raw` is the value of `$STACKUNDERFLOW_HOME` (unset when `None`) and `home`
/// the user's home directory. An empty `$STACKUNDERFLOW_HOME` counts as unset,
/// matching `os.environ.get` + truthiness on the Python side. A leading `~` or
/// `~/` expands against `home`; when `home` is unknown the `~` is left literal,
/// as `Path.expanduser()` does.
#[must_use]
pub fn resolve_app_dir(raw: Option<&OsStr>, home: Option<&Path>) -> PathBuf {
    match raw.filter(|value| !value.is_empty()) {
        Some(value) => expand_user(Path::new(value), home),
        None => match home {
            Some(home) => home.join(".stackunderflow"),
            None => PathBuf::from(".stackunderflow"),
        },
    }
}

/// Expand a leading `~` / `~/` against `home`, as `pathlib.Path.expanduser` does.
///
/// `~user` forms are *not* expanded (Python's are); they are left untouched. No
/// StackUnderflow path in the tree uses them, and honoring them would mean
/// reading the password database from a read-only bedrock crate.
fn expand_user(path: &Path, home: Option<&Path>) -> PathBuf {
    let Some(home) = home else {
        return path.to_path_buf();
    };
    let mut parts = path.components();
    match parts.next() {
        Some(std::path::Component::Normal(first)) if first == OsStr::new("~") => {
            home.join(parts.as_path())
        }
        _ => path.to_path_buf(),
    }
}

/// The user's home directory, or `None` when the platform cannot name one.
fn home_dir() -> Option<PathBuf> {
    #[allow(
        deprecated,
        reason = "std::env::home_dir is the platform-correct answer \
        on the 1.97.1 pin; the 2018-era deprecation is scheduled for removal upstream"
    )]
    env::home_dir()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home() -> PathBuf {
        PathBuf::from("/home/tester")
    }

    #[test]
    fn unset_env_falls_back_to_dot_stackunderflow_in_home() {
        let resolved = resolve_app_dir(None, Some(&home()));
        assert_eq!(resolved, PathBuf::from("/home/tester/.stackunderflow"));
    }

    #[test]
    fn empty_env_counts_as_unset() {
        let resolved = resolve_app_dir(Some(OsStr::new("")), Some(&home()));
        assert_eq!(resolved, PathBuf::from("/home/tester/.stackunderflow"));
    }

    #[test]
    fn absolute_env_wins() {
        let resolved = resolve_app_dir(Some(OsStr::new("/data/su")), Some(&home()));
        assert_eq!(resolved, PathBuf::from("/data/su"));
    }

    #[test]
    fn tilde_in_env_expands_like_pathlib() {
        let resolved = resolve_app_dir(Some(OsStr::new("~/alt-store")), Some(&home()));
        assert_eq!(resolved, PathBuf::from("/home/tester/alt-store"));

        let bare = resolve_app_dir(Some(OsStr::new("~")), Some(&home()));
        assert_eq!(bare, home());
    }

    #[test]
    fn tilde_user_is_left_literal() {
        // Divergence from Python's expanduser, recorded rather than silently
        // approximated: `~other` stays a relative path here.
        let resolved = resolve_app_dir(Some(OsStr::new("~other/su")), Some(&home()));
        assert_eq!(resolved, PathBuf::from("~other/su"));
    }

    #[test]
    fn store_lives_directly_under_the_app_dir() {
        let resolved = resolve_app_dir(Some(OsStr::new("/data/su")), Some(&home()));
        assert_eq!(
            resolved.join("store.db"),
            PathBuf::from("/data/su/store.db")
        );
    }
}
