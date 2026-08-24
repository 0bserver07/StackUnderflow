//! Where the data directory lives — the Rust half of Python's `settings.app_dir()`.
//!
//! Resolution order, post-rename (the staxtrace identity campaign):
//!
//! 1. `$STAXTRACE_HOME` — the product's name.
//! 2. `$STACKUNDERFLOW_HOME` — the reference's name, honored forever: every
//!    existing script, hook and remote invocation keeps working unchanged.
//! 3. `~/.staxtrace` **when it exists** — a machine that migrated.
//! 4. `~/.stackunderflow` **when it exists** — a machine that has not; this is
//!    the no-data-movement guarantee. The campaign renames identifiers, never
//!    moves bytes; migrating is one `mv` the *user* runs, after which resolution
//!    finds the new name by itself.
//! 5. `~/.staxtrace` — a fresh machine starts under the product's name.
//!
//! [`resolve_app_dir`] keeps the reference's exact two-step semantics
//! (env-else-default) as the pure, parity-pinned core; the existence dance
//! lives only in [`app_dir`], the impure edge.

use std::env;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

/// The environment variable that re-points the whole application at a dataset.
///
/// Mirrors `stackunderflow.settings.APP_DIR_ENV`. Still honored — see the
/// module doc's resolution order.
pub const APP_DIR_ENV: &str = "STACKUNDERFLOW_HOME";

/// The post-rename spelling; wins over [`APP_DIR_ENV`] when both are set.
pub const APP_DIR_ENV_STAXTRACE: &str = "STAXTRACE_HOME";

/// The data directory holding the store, the indexes, and `config.json`.
///
/// See the module doc for the five-step order. Unlike Python this is resolved
/// on every call rather than bound at import, which is strictly more
/// permissive: no caller can observe a stale value.
#[must_use]
pub fn app_dir() -> PathBuf {
    let new_env = env::var_os(APP_DIR_ENV_STAXTRACE);
    let old_env = env::var_os(APP_DIR_ENV);
    let raw = new_env
        .as_deref()
        .filter(|value| !value.is_empty())
        .or(old_env.as_deref());
    let home = home_dir();
    if raw.is_some() {
        return resolve_app_dir(raw, home.as_deref());
    }
    let Some(home) = home else {
        return resolve_app_dir(None, None);
    };
    let staxtrace = home.join(".staxtrace");
    if staxtrace.is_dir() {
        return staxtrace;
    }
    let stackunderflow = home.join(".stackunderflow");
    if stackunderflow.is_dir() {
        return stackunderflow;
    }
    staxtrace
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

// ── the rename shim ─────────────────────────────────────────────────────────

/// Read a knob by its **staxtrace** name, falling back to the StackUnderflow
/// one.
///
/// Pass the suffix — `env_var("OLLAMA_URL")` tries `STAXTRACE_OLLAMA_URL`, then
/// `STACKUNDERFLOW_OLLAMA_URL`. The old spelling is honored **forever**: it is
/// baked into users' shells, scripts, CI, and the ssh command this project
/// sends to other machines, and a rename that silently stops reading it turns
/// a configured install into a defaulted one with no error to see.
///
/// An empty value counts as set, matching `std::env::var` — only absence falls
/// through.
#[must_use]
pub fn env_var(suffix: &str) -> Option<String> {
    env::var(format!("STAXTRACE_{suffix}"))
        .ok()
        .or_else(|| env::var(format!("STACKUNDERFLOW_{suffix}")).ok())
}

/// The [`env_var`] shim over `OsString`, for the two callers that need it.
#[must_use]
pub fn env_var_os(suffix: &str) -> Option<std::ffi::OsString> {
    env::var_os(format!("STAXTRACE_{suffix}"))
        .or_else(|| env::var_os(format!("STACKUNDERFLOW_{suffix}")))
}
