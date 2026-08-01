//! `routes/context_budget.py` — 1 endpoint, wave 5.
//!
//! | Item | Method | FastAPI path | axum path | State |
//! |---|---|---|---|---|
//! | `RS-5-060` | `GET` | `/api/context-budget` | `/api/context-budget` | **ported** |
//!
//! All the arithmetic is in [`crate::services::context_budget`]; this module is
//! the 20 lines of Python around it — resolve the slug, pick a branch, render.
//!
//! # Three branches, and the one a careless port drops
//!
//! ```text
//! project is None                     → estimate_global_budget()
//! slug not in the store               → 404 "Unknown project slug: {project}"
//! slug known, on-disk path gone/empty → estimate_global_budget()   ← this one
//! slug known, path present            → estimate_context_budget(path)
//! ```
//!
//! The fourth line is the obvious one and the third is the one that gets lost:
//! a project the store knows about but whose directory has been deleted (or
//! whose `path` column was never populated) answers **200 with the global
//! shape**, not a 404 and not an error. It is not a corner case on the harness
//! corpus either — every one of the 335 `projects` rows in
//! `rust/.parity-state/fresh/store.db` has `path IS NULL`, so on that store
//! *every* known slug takes this branch and the fourth is unreachable. A port
//! that 404'd here would look green on the unknown-slug case and wrong on
//! everything else.
//!
//! `Path(row.path) if row.path else None` is Python truthiness, so an EMPTY
//! `path` is the same as a NULL one.
//!
//! # `fetchone()` over a slug that names several rows — ported bug and all
//!
//! `queries.get_project(conn, slug=…)` is
//! `SELECT … FROM projects WHERE slug = ?` with **no `ORDER BY`** and a
//! `.fetchone()`. The schema's `UNIQUE(provider, slug)` means one row per
//! *provider*, so a project the user has opened in Claude Code and Codex has
//! two rows with the same slug and — potentially — different `path` values.
//! This picks whichever the planner yields first (v030's `idx_projects_slug` is
//! a slug-only index, so that is the lowest `rowid`), which is an arbitrary
//! provider.
//!
//! Elsewhere in the tree this exact bug is *fixed*: `routes/data.py`'s
//! `_filtered_project_ids` and `routes/sessions.py` both call
//! `get_projects_by_slug` and bind every id. Here it is not fixed, so the port
//! is not either — same SQL, same first-row-wins, recorded as **DIV-101** with
//! the evidence rather than quietly repaired.
//!
//! # `schema.apply(conn)` is not ported — DIV-102
//!
//! The reference runs the migration ladder on every request. This crate does
//! not write to the store from a GET (the rule `routes/search.rs` states for
//! the FTS sidecar), and on any store the server has already booted against the
//! call is a no-op. The one behaviour it buys — answering on a store with no
//! `projects` table at all — is recorded, not reproduced.
//!
//! # `$HOME`, not `$STACKUNDERFLOW_HOME`
//!
//! The service reads `~/.claude*`, and `~` is `Path.home()` in the reference —
//! the OS home, unrelated to the data directory the server was started with.
//! [`home_dir`] resolves it once per request and injects it, exactly as
//! `routes/projects.rs` does for `~/.claude/projects`; the service itself never
//! touches the environment.

use std::path::{Path, PathBuf};

use axum::Router;
use axum::extract::{RawQuery, State};
use axum::http::StatusCode;
use axum::routing::get;
use rusqlite::Connection;
use serde_json::Value;

use crate::json::{HandlerResult, HttpError, JsonBody, join_failure};
use crate::qs::Query;
use crate::services::context_budget::{
    estimate_context_budget, estimate_global_budget, py_path_str,
};
use crate::state::AppState;

/// Mount this module's endpoints onto `router`.
///
/// Called once, from [`super::register_all`], at this module's
/// `include_router` position (16th of the 34).
pub fn register(router: Router<AppState>) -> Router<AppState> {
    router.route("/api/context-budget", get(get_context_budget))
}

/// `GET /api/context-budget`.
///
/// `project: str | None = None` — a starlette scalar, so a repeated
/// `?project=a&project=b` takes the LAST value, and `?project=` is the empty
/// string, which is *not* `None`: it goes to the store as a slug, misses, and
/// 404s. Only an absent parameter takes the global branch.
///
/// The body is SQLite plus a directory walk over `~/.claude`, so it runs on
/// `spawn_blocking` for the same reason `routes/data.rs::get_stats` does.
async fn get_context_budget(
    State(state): State<AppState>,
    RawQuery(raw): RawQuery,
) -> HandlerResult {
    let query = Query::parse(raw.as_deref().unwrap_or_default());
    let project = query.get("project").map(str::to_owned);
    let home = home_dir();

    let worker = state.clone();
    let payload = tokio::task::spawn_blocking(move || match project {
        // `if project is None: return JSONResponse(estimate_global_budget()…)`.
        None => Ok(estimate_global_budget(&home).to_dict()),
        Some(slug) => project_budget(&worker, &slug, &home),
    })
    .await
    .map_err(|err| join_failure(&err))??;

    Ok(JsonBody::ok(payload))
}

/// The blocking body of the `project=<slug>` branch.
///
/// # Errors
/// A 404 for an unknown slug, or a 500 if the store cannot be opened or read.
fn project_budget(state: &AppState, project: &str, home: &Path) -> Result<Value, HttpError> {
    let conn = state
        .connect()
        .map_err(|err| HttpError::new(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))?;
    let row = get_project(&conn, project)
        .map_err(|err| HttpError::new(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))?;
    drop(conn);

    // `if row is None: raise HTTPException(404, f"Unknown project slug: {project}")`.
    let Some(stored_path) = row else {
        return Err(HttpError::not_found(format!(
            "Unknown project slug: {project}"
        )));
    };

    // `project_dir = Path(row.path) if row.path else None` — NULL and "" alike
    // are falsy, and the normalising `Path(...)` is what `exists()` is asked
    // about, so the same normalisation runs here.
    let project_dir = stored_path
        .filter(|path| !path.is_empty())
        .map(|path| py_path_str(&path));
    match project_dir {
        // `if project_dir is None or not project_dir.exists():` → the GLOBAL
        // shape, deliberately: "the CLAUDE.md slice will simply be zero".
        Some(dir) if Path::new(&dir).exists() => {
            Ok(estimate_context_budget(Path::new(&dir), home).to_dict())
        }
        _ => Ok(estimate_global_budget(home).to_dict()),
    }
}

/// `queries.get_project(conn, slug=…)` — `fetchone`, narrowed to `path`.
///
/// The full column list is kept so the emitted statement matches the reference
/// byte for byte in a query log, and the missing `ORDER BY` is deliberate: see
/// the module docs and DIV-101.
fn get_project(conn: &Connection, slug: &str) -> rusqlite::Result<Option<Option<String>>> {
    let mut stmt = conn.prepare(
        "SELECT id, provider, slug, path, display_name, first_seen, last_modified \
         FROM projects WHERE slug = ?",
    )?;
    let mut rows = stmt.query([slug])?;
    match rows.next()? {
        Some(row) => Ok(Some(row.get(3)?)),
        None => Ok(None),
    }
}

/// `Path.home()` — `$HOME` on POSIX, and nothing to do with the data directory.
///
/// Same helper, same fallback, as `routes/projects.rs`: a process with no home
/// at all resolves `/`, where the `~/.claude` probes simply miss and every
/// slice comes back zero.
fn home_dir() -> PathBuf {
    // WAVE 8 TRANCHE 3: moved to `stax_reports::context_budget::os_home` so the
    // CLI's `context-budget` verb resolves home the same way this route does.
    crate::services::context_budget::os_home()
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::state::Config;

    /// A store with a `projects` table and the rows a case needs.
    ///
    /// `schema.apply` is not ported (DIV-102), so the table is created here the
    /// way the migration ladder would have.
    fn store_with(tag: &str, line: u32, rows: &[(&str, &str, Option<&str>)]) -> AppState {
        let dir = std::env::temp_dir().join(format!(
            "stax-ctxbudget-route-{tag}-{}-{line}",
            std::process::id()
        ));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).expect("mkdir");
        let state = AppState::new(
            dir.join("store.db"),
            PathBuf::from("/nonexistent/pkg"),
            Config::default(),
        );
        let conn = state.connect().expect("open");
        conn.execute_batch(
            "CREATE TABLE projects (
                 id             INTEGER PRIMARY KEY,
                 provider       TEXT NOT NULL,
                 slug           TEXT NOT NULL,
                 path           TEXT,
                 display_name   TEXT NOT NULL,
                 first_seen     REAL NOT NULL,
                 last_modified  REAL NOT NULL,
                 UNIQUE (provider, slug));
             CREATE INDEX idx_projects_slug ON projects(slug);",
        )
        .expect("schema");
        for (provider, slug, path) in rows {
            conn.execute(
                "INSERT INTO projects (provider, slug, path, display_name, first_seen, last_modified) \
                 VALUES (?, ?, ?, ?, 0.0, 0.0)",
                rusqlite::params![provider, slug, path, slug],
            )
            .expect("insert");
        }
        drop(conn);
        state
    }

    fn slice_names(payload: &Value) -> Vec<&str> {
        payload["slices"]
            .as_array()
            .expect("slices")
            .iter()
            .map(|slice| slice["name"].as_str().unwrap_or_default())
            .collect()
    }

    #[test]
    fn an_unknown_slug_is_a_404_carrying_the_slug_verbatim() {
        let state = store_with("unknown", line!(), &[("claude", "demo", None)]);
        let err = project_budget(&state, "nope", Path::new("/nonexistent/home"))
            .expect_err("unknown slug");
        assert_eq!(
            err.body().render(),
            r#"{"detail":"Unknown project slug: nope"}"#
        );
    }

    #[test]
    fn an_empty_project_parameter_is_a_slug_lookup_and_not_an_omission() {
        // `?project=` arrives as `""`, which is not `None`; FastAPI hands the
        // handler the empty string and the store lookup misses.
        let query = Query::parse("project=");
        assert_eq!(query.get("project"), Some(""));
        let state = store_with("empty-param", line!(), &[("claude", "demo", None)]);
        let err =
            project_budget(&state, "", Path::new("/nonexistent/home")).expect_err("empty slug");
        assert_eq!(
            err.body().render(),
            r#"{"detail":"Unknown project slug: "}"#
        );
    }

    #[test]
    fn a_known_slug_with_a_null_path_falls_back_to_the_global_shape() {
        // This is the branch every row on the harness store takes.
        let state = store_with("null-path", line!(), &[("claude", "demo", None)]);
        let payload =
            project_budget(&state, "demo", Path::new("/nonexistent/home")).expect("200, not 404");
        assert_eq!(
            slice_names(&payload),
            vec!["system_prompt", "memory:global_CLAUDE.md"]
        );
        assert_eq!(payload["total_tokens"], Value::from(3000));
    }

    #[test]
    fn an_empty_path_string_is_falsy_exactly_like_a_null_one() {
        let state = store_with("empty-path", line!(), &[("claude", "demo", Some(""))]);
        let payload =
            project_budget(&state, "demo", Path::new("/nonexistent/home")).expect("global shape");
        assert!(!slice_names(&payload).contains(&"memory:project_CLAUDE.md"));
    }

    #[test]
    fn a_known_slug_whose_directory_is_gone_is_the_global_shape_not_a_404() {
        let state = store_with(
            "ghost",
            line!(),
            &[("claude", "ghost", Some("/nonexistent/project-dir"))],
        );
        let payload =
            project_budget(&state, "ghost", Path::new("/nonexistent/home")).expect("200, not 404");
        assert!(!slice_names(&payload).contains(&"memory:project_CLAUDE.md"));
    }

    #[test]
    fn a_known_slug_with_a_live_directory_gets_the_project_budget() {
        let dir = std::env::temp_dir().join(format!(
            "stax-ctxbudget-live-{}-{}",
            std::process::id(),
            line!()
        ));
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(dir.join("CLAUDE.md"), "a".repeat(400)).expect("write");
        let path = dir.to_string_lossy().into_owned();
        let state = store_with("live", line!(), &[("claude", "demo", Some(&path))]);

        let payload =
            project_budget(&state, "demo", Path::new("/nonexistent/home")).expect("project budget");
        assert_eq!(
            slice_names(&payload),
            vec![
                "system_prompt",
                "memory:project_CLAUDE.md",
                "memory:global_CLAUDE.md"
            ]
        );
        assert_eq!(payload["total_tokens"], Value::from(3100));
        assert_eq!(
            payload["slices"][1]["source_path"],
            Value::from(format!("{path}/CLAUDE.md"))
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_slug_naming_two_providers_takes_the_first_row_and_that_is_the_bug() {
        // DIV-101: `fetchone()` with no ORDER BY. The codex row is inserted
        // second, so it is never seen — its `path` would have produced a
        // project budget and the claude row's NULL produces the global one.
        let dir = std::env::temp_dir().join(format!(
            "stax-ctxbudget-multi-{}-{}",
            std::process::id(),
            line!()
        ));
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(dir.join("CLAUDE.md"), "a".repeat(400)).expect("write");
        let path = dir.to_string_lossy().into_owned();
        let state = store_with(
            "multi",
            line!(),
            &[("claude", "shared", None), ("codex", "shared", Some(&path))],
        );

        let payload =
            project_budget(&state, "shared", Path::new("/nonexistent/home")).expect("first row");
        assert!(
            !slice_names(&payload).contains(&"memory:project_CLAUDE.md"),
            "the codex row's path must NOT win: {payload}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_repeated_project_parameter_takes_the_last_occurrence() {
        // starlette's `QueryParams._dict` is a comprehension over the pair
        // list, so the last write wins — not the first.
        let query = Query::parse("project=a&project=b");
        assert_eq!(query.get("project"), Some("b"));
        // …and an absent parameter is the only route to the global branch.
        assert_eq!(Query::parse("").get("project"), None);
    }
}
