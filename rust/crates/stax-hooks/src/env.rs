//! Everything a hook fire reads from the machine, resolved once and injected.
//!
//! Finding 5 of the campaign ledger is law here: `std::env::set_var` is `unsafe`
//! in Rust 2024 and this workspace forbids `unsafe`, so the only testable shape
//! is a pure function over an explicit environment. It buys something else too —
//! the hook path's *entire* contact with ambient state is this one struct, which
//! is what makes "does a hook write anything?" an answerable question rather
//! than a grep.
//!
//! Resolution mirrors the reference exactly:
//!
//! | field | Python |
//! |---|---|
//! | `store_path` | `deps.store_path` = `settings.app_dir()/store.db` |
//! | `app_dir` | `proactive._app_dir()` = `deps.store_path.parent` |
//! | `weights` | `Settings().discovery_rank_weights`, env → file → `0.5,0.2,0.3` |
//! | `now_micros` | `datetime.now(UTC)` |
//! | `cwd` | `os.getcwd()`, for `os.path.abspath` |
//! | `proactive_enabled` | `deps.config["proactive_enabled"]` |
//! | `kill_switch` | `$STACKUNDERFLOW_PROACTIVE_DISABLED` |
//! | `recall_timeout` | `$STACKUNDERFLOW_RECALL_TIMEOUT` |

use std::path::PathBuf;

use stax_core::queries::pyjson;
use stax_core::queries::{pytime, rank};
use stax_core::settings;

/// The resolved environment for one hook fire.
#[derive(Debug, Clone)]
pub struct HookEnv {
    /// `deps.store_path`.
    pub store_path: PathBuf,
    /// `settings.app_dir()` — where the two proactive JSON files live.
    pub app_dir: PathBuf,
    /// `Settings().discovery_rank_weights`, parsed.
    pub weights: (f64, f64, f64),
    /// `datetime.now(UTC)` in microseconds since the epoch.
    pub now_micros: i64,
    /// `os.getcwd()` — `os.path.abspath` resolves relative paths against it.
    pub cwd: PathBuf,
    /// `config.json`, or `None` when absent/corrupt (`settings._load`).
    pub config: Option<pyjson::Value>,
    /// `$STACKUNDERFLOW_PROACTIVE_DISABLED`, raw.
    pub proactive_disabled: Option<String>,
    /// `$STACKUNDERFLOW_RECALL_TIMEOUT`, raw.
    pub recall_timeout: Option<String>,
    /// The `memory` CLI the recall hook shells. Bare `stackunderflow` in the
    /// reference — the bare name IS the portability contract (`recall.py:275`).
    pub memory_bin: String,
    /// `Settings().proactive_*`, resolved env → file → default.
    ///
    /// Resolved eagerly because `Policy.from_settings()` in the reference reads
    /// all five in one go, and a hook that reads a setting twice can observe two
    /// different values if the file changes underneath it.
    pub proactive: ProactiveSettings,
}

/// The five `proactive_*` knobs (`settings.py:190-206`).
#[derive(Debug, Clone, PartialEq)]
pub struct ProactiveSettings {
    /// `proactive_enabled` — OPT-IN, false by default.
    pub enabled: bool,
    /// `proactive_types` — the comma-separated allowlist. The DEFAULT is
    /// `"command-cluster,file-risk"`, which does **not** include
    /// `error-signature`: the Phase-2 nudge is off even in governed mode until
    /// a user names it.
    pub types: String,
    /// `proactive_max_per_session`.
    pub max_per_session: i64,
    /// `proactive_cooldown_hours` — an `int` default, so the env leg casts with
    /// `int(raw)`; the consumer then reads it through `float()`.
    pub cooldown_hours: f64,
    /// `proactive_dismiss_suppress_after` — file-only, no env leg (`env=None`).
    pub dismiss_suppress_after: i64,
}

impl Default for ProactiveSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            types: "command-cluster,file-risk".to_string(),
            max_per_session: 3,
            cooldown_hours: 24.0,
            dismiss_suppress_after: 3,
        }
    }
}

impl HookEnv {
    /// Resolve from the real process environment.
    #[must_use]
    pub fn from_process() -> Self {
        let app_dir = settings::app_dir();
        let config = read_config(&app_dir);
        let weights = resolve_weights(
            stax_core::settings::env_var("DISCOVERY_RANK_WEIGHTS")
                .ok_or(())
                .ok()
                .as_deref(),
            config.as_ref(),
        );
        let proactive = resolve_proactive(&|name| std::env::var(name).ok(), config.as_ref());
        Self {
            store_path: app_dir.join("store.db"),
            weights,
            app_dir,
            now_micros: pytime::now_micros(),
            cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")),
            config,
            proactive_disabled: stax_core::settings::env_var("PROACTIVE_DISABLED")
                .ok_or(())
                .ok(),
            recall_timeout: stax_core::settings::env_var("RECALL_TIMEOUT")
                .ok_or(())
                .ok(),
            // Post-split: the memory CLI is the native binary. The reference
            // spawned `stackunderflow` (recall.rs's header records why, and
            // named this field the seam for deciding otherwise) — since
            // f6ac5f6 that bare name resolves to nothing a Rust-only machine
            // has, so every recall spawn failed silently.
            memory_bin: "stax".to_string(),
            proactive,
        }
    }

    /// `discovery`'s clock term — `now_micros` as epoch seconds.
    #[must_use]
    pub fn now_epoch(&self) -> f64 {
        self.now_micros as f64 / 1_000_000.0
    }

    /// A `rank::Budget` for one of `inject`'s per-event token budgets.
    #[must_use]
    pub fn budget(&self, tokens: i64) -> rank::Budget {
        rank::Budget::at(tokens, self.weights, self.now_epoch())
    }
}

/// `settings._load()` — `config.json`, or nothing when absent or corrupt.
fn read_config(app_dir: &std::path::Path) -> Option<pyjson::Value> {
    let raw = std::fs::read_to_string(app_dir.join("config.json")).ok()?;
    pyjson::loads(&raw)
}

/// `Settings()`'s `_Opt.__get__` for the five `proactive_*` knobs — env, then
/// the config file, then the built-in default.
///
/// `getenv` is injected rather than read here so the resolution is testable
/// without `std::env::set_var`, which Rust 2024 makes `unsafe` (finding 5).
///
/// The env leg casts by the *default's* type, exactly as `_Opt._cast` does: a
/// `bool` default accepts `1/true/yes/on` (lowercased) and treats every other
/// string as false; an `int` default falls back to the default on a
/// non-integer. The file leg returns the stored value untouched — so a
/// `proactive_max_per_session` of `"7"` in `config.json` reaches the consumer as
/// a *string*, which `_as_int` then parses. That chain is preserved.
#[must_use]
pub fn resolve_proactive(
    getenv: &dyn Fn(&str) -> Option<String>,
    config: Option<&pyjson::Value>,
) -> ProactiveSettings {
    let defaults = ProactiveSettings::default();
    let file = |key: &str| config.and_then(|config| config.get(key));

    let enabled = match getenv("STACKUNDERFLOW_PROACTIVE_ENABLED") {
        Some(raw) => matches!(raw.to_lowercase().as_str(), "1" | "true" | "yes" | "on"),
        None => file("proactive_enabled").map_or(defaults.enabled, pyjson::Value::is_truthy),
    };
    let types = match getenv("STACKUNDERFLOW_PROACTIVE_TYPES") {
        Some(raw) => raw,
        None => match file("proactive_types") {
            Some(pyjson::Value::Str(value)) => value.clone(),
            // A non-string in the file is returned as-is by `_Opt.__get__` and
            // then rejected by `_parse_types`'s `isinstance(raw, str)` check,
            // which yields the FULL known set — not the default string. The
            // sentinel below reproduces that, because a `types` field of type
            // `String` cannot carry "a non-string was stored".
            Some(_) => NON_STRING_TYPES.to_string(),
            None => defaults.types.clone(),
        },
    };
    ProactiveSettings {
        enabled,
        types,
        max_per_session: cast_int(
            getenv("STACKUNDERFLOW_PROACTIVE_MAX_PER_SESSION").as_deref(),
            file("proactive_max_per_session"),
            defaults.max_per_session,
        ),
        cooldown_hours: cast_int(
            getenv("STACKUNDERFLOW_PROACTIVE_COOLDOWN_HOURS").as_deref(),
            file("proactive_cooldown_hours"),
            24,
        ) as f64,
        // `env=None` — the file-only leg.
        dismiss_suppress_after: cast_int(
            None,
            file("proactive_dismiss_suppress_after"),
            defaults.dismiss_suppress_after,
        ),
    }
}

/// The one value `proactive_types` can hold that is not a comma list: the
/// marker for "the config file stored a non-string", which `_parse_types` turns
/// into the full known-type set.
pub const NON_STRING_TYPES: &str = "\u{0}non-string";

/// `_Opt._cast` for an `int` default, followed by the consumer's `_as_int`.
fn cast_int(env: Option<&str>, file: Option<&pyjson::Value>, default: i64) -> i64 {
    if let Some(raw) = env {
        return raw.trim().parse::<i64>().unwrap_or(default);
    }
    match file {
        // `_as_int` rejects bools before ints, exactly like the reference.
        Some(pyjson::Value::Bool(_)) => default,
        Some(pyjson::Value::Int(value)) => *value,
        Some(pyjson::Value::Float(value)) => *value as i64,
        Some(pyjson::Value::Str(value)) => value.trim().parse::<i64>().unwrap_or(default),
        _ => default,
    }
}

/// `Settings().discovery_rank_weights` — env, then file, then `0.5,0.2,0.3`.
///
/// Same chain `stax-cli`'s `memory::resolve_weights` walks; duplicated as three
/// lines rather than reached for across a crate boundary the charter does not
/// have (`stax-hooks` must not depend on `stax-cli`).
#[must_use]
pub fn resolve_weights(env: Option<&str>, config: Option<&pyjson::Value>) -> (f64, f64, f64) {
    if let Some(raw) = env {
        return rank::parse_rank_weights(Some(raw));
    }
    match config.and_then(|config| config.get("discovery_rank_weights")) {
        Some(pyjson::Value::Str(value)) => rank::parse_rank_weights(Some(value)),
        _ => rank::parse_rank_weights(None),
    }
}

/// `os.path.abspath(path)` — `normpath(join(cwd, path))`, lexical only.
///
/// **Not** `Path::canonicalize` and not `discovery._resolve_input_path`: this
/// one never touches the filesystem and never resolves a symlink, which is what
/// `handlers._resolve_project_id` and `inject._slug_from_cwd` call. A hook's
/// `cwd` is usually a real directory, so the two agree in practice — but the
/// slug is a *database key*, and agreeing "in practice" is how a lookup misses.
#[must_use]
pub fn abspath(path: &str, cwd: &std::path::Path) -> String {
    let joined = if path.starts_with('/') {
        PathBuf::from(path)
    } else {
        cwd.join(path)
    };
    let text = joined.to_string_lossy().into_owned();
    normpath(&text)
}

/// `os.path.normpath` for POSIX: collapse `.`, resolve `..` lexically, squeeze
/// separators, and keep the leading-`//` special case CPython keeps.
#[must_use]
pub fn normpath(path: &str) -> String {
    if path.is_empty() {
        return ".".to_string();
    }
    let absolute = path.starts_with('/');
    // POSIX says exactly two leading slashes are implementation-defined and
    // CPython preserves them; three or more collapse to one.
    let leading = if absolute && path.starts_with("//") && !path.starts_with("///") {
        "//"
    } else if absolute {
        "/"
    } else {
        ""
    };

    let mut parts: Vec<&str> = Vec::new();
    for component in path.split('/') {
        match component {
            "" | "." => {}
            ".." => {
                // An absolute path can always drop a `..` (`/..` is `/`); a
                // relative one can only drop it when there is a real component
                // to cancel, otherwise the `..` is kept.
                if absolute || parts.last().is_some_and(|last| *last != "..") {
                    parts.pop();
                } else {
                    parts.push("..");
                }
            }
            other => parts.push(other),
        }
    }
    let joined = parts.join("/");
    if joined.is_empty() {
        if absolute {
            leading.to_string()
        } else {
            ".".to_string()
        }
    } else {
        format!("{leading}{joined}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn normpath_matches_posixpath() {
        // Each pair was produced by `python3 -c "import posixpath; ..."`.
        assert_eq!(normpath("/a/b/../c"), "/a/c");
        assert_eq!(normpath("/a/./b//c/"), "/a/b/c");
        assert_eq!(normpath("a/b/../../.."), "..");
        assert_eq!(normpath("/.."), "/");
        assert_eq!(normpath("//a/b"), "//a/b");
        assert_eq!(normpath("///a/b"), "/a/b");
        assert_eq!(normpath(""), ".");
        assert_eq!(normpath("."), ".");
        assert_eq!(normpath("/"), "/");
    }

    #[test]
    fn abspath_joins_against_the_cwd() {
        let cwd = Path::new("/home/u/proj");
        assert_eq!(abspath("src/app.py", cwd), "/home/u/proj/src/app.py");
        assert_eq!(abspath("/tmp/x", cwd), "/tmp/x");
        assert_eq!(abspath("../sibling", cwd), "/home/u/sibling");
        // No `~` expansion — `os.path.abspath` does not do one.
        assert_eq!(abspath("~/x", cwd), "/home/u/proj/~/x");
    }

    #[test]
    fn weights_walk_env_then_file_then_default() {
        let config = pyjson::loads(r#"{"discovery_rank_weights": "0.1,0.2,0.7"}"#);
        assert_eq!(
            resolve_weights(Some("0.9,0.05,0.05"), config.as_ref()),
            (0.9, 0.05, 0.05)
        );
        assert_eq!(resolve_weights(None, config.as_ref()), (0.1, 0.2, 0.7));
        assert_eq!(resolve_weights(None, None), (0.5, 0.2, 0.3));
        // A malformed env value falls back — it does not error.
        assert_eq!(resolve_weights(Some("nope"), None), (0.5, 0.2, 0.3));
    }
}
