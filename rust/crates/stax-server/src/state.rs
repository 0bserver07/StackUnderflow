//! Injected application state — the Rust half of `stackunderflow/deps.py`.
//!
//! `deps.py` is a module of globals: `store_path`, `config`, `BASE_DIR`, the
//! five lazily-built services, and two mutable strings (`current_project_path`,
//! `current_log_path`) that `POST /api/project-by-dir` writes and half a dozen
//! GET handlers read. Route modules `import stackunderflow.deps as deps` and
//! reach in.
//!
//! None of that survives the port as written, and not for taste: finding 5 of
//! `rust/ARCHITECT-STATE.md` is campaign law — `std::env::set_var` is `unsafe`
//! in Rust 2024, the workspace `forbid(unsafe_code)`s, so every setting is a
//! pure function of injected inputs. So the globals become one [`AppState`]
//! that `axum` hands each handler. The behaviour is identical (one process, one
//! shared current-project) and the testability is not: a test builds a state
//! pointing at a fixture store without touching the environment, and two states
//! can coexist in one process, which the parity harness uses.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};

use anyhow::Result;
use rusqlite::Connection;

/// The settings the HTTP layer reads, resolved once at startup.
///
/// Python resolves `env → config.json → default` on *every* attribute read
/// (`settings._Opt.__get__`). Resolving once at startup is a deliberate,
/// recorded narrowing: no shipped endpoint reads a setting that another request
/// could have changed mid-flight (`cfg` writes go through the store and the
/// currency memo, which is invalidated explicitly), and a per-read `os.getenv`
/// is not something a `forbid(unsafe_code)` crate can honour anyway.
#[derive(Debug, Clone)]
pub struct Config {
    /// `Settings.currency` — the display currency (`STACKUNDERFLOW_CURRENCY`).
    pub currency: String,
    /// `Settings.max_date_range_days` — echoed in several `config` blocks.
    pub max_date_range_days: i64,
    /// `Settings.port` — what the CORS allow-list is built from in Python.
    pub port: u16,
}

impl Default for Config {
    /// The three built-in defaults from `settings.Settings`.
    fn default() -> Self {
        Self {
            currency: "USD".to_owned(),
            max_date_range_days: 30,
            port: 8081,
        }
    }
}

impl Config {
    /// Resolve `env → config.json → default`, with both legs injected.
    ///
    /// `env` is a lookup closure rather than [`std::env::var`] so the resolution
    /// rules can be tested without mutating process state, and `config_json` is
    /// the already-parsed file (`None` when absent or corrupt, which is exactly
    /// what `settings._load()` returns).
    ///
    /// Casting matches `_Opt._cast`: an `int` setting whose env value does not
    /// parse falls back to the *default*, it does not error.
    #[must_use]
    pub fn resolve(
        env: &dyn Fn(&str) -> Option<String>,
        config_json: Option<&serde_json::Value>,
    ) -> Self {
        let defaults = Self::default();
        let file = |key: &str| config_json.and_then(|v| v.get(key));

        let currency = env("STACKUNDERFLOW_CURRENCY")
            .or_else(|| {
                file("currency")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned)
            })
            .unwrap_or(defaults.currency);

        let max_date_range_days = env("MAX_DATE_RANGE_DAYS")
            .map(|raw| raw.parse().unwrap_or(defaults.max_date_range_days))
            .or_else(|| file("max_date_range_days").and_then(serde_json::Value::as_i64))
            .unwrap_or(defaults.max_date_range_days);

        let port = env("PORT")
            .map(|raw| raw.parse().unwrap_or(defaults.port))
            .or_else(|| {
                file("port")
                    .and_then(serde_json::Value::as_i64)
                    .and_then(|n| u16::try_from(n).ok())
            })
            .unwrap_or(defaults.port);

        Self {
            currency,
            max_date_range_days,
            port,
        }
    }

    /// Read `config.json` out of `app_dir` and resolve against the real
    /// environment.
    ///
    /// A missing or unparseable file is not an error: `settings._load()`
    /// swallows both and falls through to defaults, so this does too.
    #[must_use]
    pub fn load(app_dir: &Path) -> Self {
        let parsed = std::fs::read_to_string(app_dir.join("config.json"))
            .ok()
            .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok());
        Self::resolve(&|key| std::env::var(key).ok(), parsed.as_ref())
    }
}

/// `deps.current_project_path` / `deps.current_log_path` — the one piece of
/// genuinely mutable, request-writable server state.
///
/// Both are `None` until `POST /api/project` or `POST /api/project-by-dir` sets
/// them; `_require_project()` 400s until then. `log_path` is a `String` and not
/// a `PathBuf` on purpose — Python stores `str(log_path)` and can legitimately
/// store `""` for a provider with no on-disk log dir, and `Path("")` would
/// silently become `"."`.
#[derive(Debug, Clone, Default)]
pub struct CurrentProject {
    /// `deps.current_project_path`.
    pub project_path: Option<String>,
    /// `deps.current_log_path`.
    pub log_path: Option<String>,
}

#[derive(Debug)]
struct Inner {
    store_path: PathBuf,
    package_dir: PathBuf,
    config: Config,
    project: RwLock<CurrentProject>,
    is_reindexing: AtomicBool,
}

/// Everything a handler needs, cloned cheaply into every request.
///
/// `Clone` is an `Arc` bump; axum requires `Clone + Send + Sync + 'static` for
/// state and clones it per request.
#[derive(Debug, Clone)]
pub struct AppState {
    inner: Arc<Inner>,
}

impl AppState {
    /// Build state around an explicit store path and package directory.
    ///
    /// Nothing is read from the environment here — `store_path` is what
    /// `deps.store_path` would have resolved to and `package_dir` is
    /// `deps.BASE_DIR`, the `stackunderflow/` package root that `static/` and
    /// `data/models.toml` hang off. The binary does the resolving; every test
    /// injects.
    #[must_use]
    pub fn new(store_path: PathBuf, package_dir: PathBuf, config: Config) -> Self {
        Self {
            inner: Arc::new(Inner {
                store_path,
                package_dir,
                config,
                project: RwLock::new(CurrentProject::default()),
                is_reindexing: AtomicBool::new(false),
            }),
        }
    }

    /// `deps.store_path`.
    #[must_use]
    pub fn store_path(&self) -> &Path {
        &self.inner.store_path
    }

    /// `deps.BASE_DIR` — the `stackunderflow/` package directory.
    #[must_use]
    pub fn package_dir(&self) -> &Path {
        &self.inner.package_dir
    }

    /// `BASE_DIR/static` — the `StaticFiles` mount root.
    #[must_use]
    pub fn static_dir(&self) -> PathBuf {
        self.inner.package_dir.join("static")
    }

    /// `BASE_DIR/static/react/index.html` — the SPA entry every page route
    /// `FileResponse`s.
    #[must_use]
    pub fn spa_index(&self) -> PathBuf {
        self.static_dir().join("react").join("index.html")
    }

    /// The resolved settings.
    #[must_use]
    pub fn config(&self) -> &Config {
        &self.inner.config
    }

    /// `deps.is_reindexing`.
    #[must_use]
    pub fn is_reindexing(&self) -> bool {
        self.inner.is_reindexing.load(Ordering::Relaxed)
    }

    /// Set `deps.is_reindexing`.
    pub fn set_reindexing(&self, value: bool) {
        self.inner.is_reindexing.store(value, Ordering::Relaxed);
    }

    /// A snapshot of the current project.
    ///
    /// # Panics
    /// Only if a previous holder panicked while writing, which would mean the
    /// process is already unwinding.
    #[must_use]
    pub fn current_project(&self) -> CurrentProject {
        self.inner
            .project
            .read()
            .expect("current-project lock poisoned")
            .clone()
    }

    /// Replace the current project — what `POST /api/project{,-by-dir}` does.
    ///
    /// # Panics
    /// See [`Self::current_project`].
    pub fn set_current_project(&self, project: CurrentProject) {
        *self
            .inner
            .project
            .write()
            .expect("current-project lock poisoned") = project;
    }

    /// Open the store the way `store/db.py::connect` does.
    ///
    /// Read-*write*, with `journal_mode=WAL`, `synchronous=NORMAL` and
    /// `foreign_keys=ON`, because that is what the reference opens and the
    /// pragmas are observable (a read-only handle cannot even run the first
    /// one). The live-dataset guard in `stax_etl::ingest::guard` still applies,
    /// so pointing this at `…/stackunderflow-data` fails loudly instead of
    /// writing to it.
    ///
    /// # Errors
    /// Whatever the guard or SQLite rejects.
    pub fn connect(&self) -> Result<Connection> {
        stax_etl::ingest::guard::open_read_write(&self.inner.store_path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_env(_: &str) -> Option<String> {
        None
    }

    #[test]
    fn defaults_match_settings_py() {
        let cfg = Config::resolve(&no_env, None);
        assert_eq!(cfg.currency, "USD");
        assert_eq!(cfg.max_date_range_days, 30);
        assert_eq!(cfg.port, 8081);
    }

    #[test]
    fn env_beats_file_beats_default() {
        let file = serde_json::json!({"currency": "EUR", "max_date_range_days": 7, "port": 9000});
        let cfg = Config::resolve(&no_env, Some(&file));
        assert_eq!(cfg.currency, "EUR");
        assert_eq!(cfg.max_date_range_days, 7);
        assert_eq!(cfg.port, 9000);

        let env = |key: &str| match key {
            "STACKUNDERFLOW_CURRENCY" => Some("GBP".to_owned()),
            _ => None,
        };
        let cfg = Config::resolve(&env, Some(&file));
        assert_eq!(cfg.currency, "GBP");
        assert_eq!(cfg.max_date_range_days, 7);
    }

    #[test]
    fn an_uncastable_env_int_falls_back_to_the_default_not_the_file() {
        // `_Opt.__get__` returns `self._cast(raw)` the moment the env var
        // exists, and `_cast` swallows the ValueError into `self.default`. The
        // file leg is never reached. Reproduced exactly, wrong-looking as it is.
        let file = serde_json::json!({"max_date_range_days": 7});
        let env = |key: &str| (key == "MAX_DATE_RANGE_DAYS").then(|| "not-a-number".to_owned());
        let cfg = Config::resolve(&env, Some(&file));
        assert_eq!(cfg.max_date_range_days, 30);
    }

    #[test]
    fn current_project_round_trips_through_the_lock() {
        let state = AppState::new(
            PathBuf::from("/nonexistent/store.db"),
            PathBuf::from("/nonexistent/static"),
            Config::default(),
        );
        assert!(state.current_project().log_path.is_none());
        state.set_current_project(CurrentProject {
            project_path: Some("/tmp/p".to_owned()),
            log_path: Some(String::new()),
        });
        // The empty string is a real value, distinct from "unset" — a provider
        // with no on-disk log dir stores exactly this.
        assert_eq!(state.current_project().log_path.as_deref(), Some(""));
    }

    #[test]
    fn spa_index_is_the_react_build_entry() {
        let state = AppState::new(
            PathBuf::from("/s/store.db"),
            PathBuf::from("/pkg"),
            Config::default(),
        );
        assert_eq!(
            state.spa_index(),
            PathBuf::from("/pkg/static/react/index.html")
        );
        assert_eq!(state.static_dir(), PathBuf::from("/pkg/static"));
    }
}
