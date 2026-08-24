//! `python-legacy: api/__init__.py` — the public, store-backed library API.
//!
//! Python's package entry point re-exports three names so `import
//! stackunderflow; stackunderflow.list_projects()` works: [`list_projects`],
//! [`list_sessions`] and `process`. Two of the three are straight store reads
//! and are ported here. **`process` is not**, and the reason is structural
//! rather than a matter of effort — see [`PROCESS_IS_A_LATER_WAVE`].
//!
//! Two behaviours carry over exactly because callers depend on them:
//!
//! * A **missing store is not an error for [`list_projects`]** — a fresh
//!   install with no ingest returns an empty list, not a failure. The two
//!   session-scoped calls do the opposite and raise `KeyError(project_slug)`,
//!   including when the store file itself is absent ("still a not-found case
//!   from the caller's point of view", per the reference's docstring).
//! * The dicts drop `projects.id`. `ProjectRow` carries it, the public dict
//!   does not, and the key order is `slug, provider, display_name, path,
//!   first_seen, last_modified` — the reference's literal, not the column
//!   order, so anything serialising these agrees byte for byte.
//!
//! Read-only by construction, as everywhere else in this port: Python's
//! `db.connect` opens read-write and would *create* the file, but `api` only
//! ever reads and guards on `path.is_file()` first, so no observable behaviour
//! depends on the difference.

use std::path::Path;

use anyhow::{Result, bail};
use rusqlite::Connection;

use crate::queries::pyjson;
use crate::store::Store;

/// Why `api.process()` is not in this module.
///
/// `process(slug)` is `store.queries.get_project_stats`, which is
/// `build_enriched_dataset` (rehydrating every message's `raw_json` into
/// pipeline `RawEntry` objects) → `dedup → classifier → enricher` →
/// `stats.aggregator.summarise` + `stats.formatter.to_dicts`. That is the whole
/// analytics pipeline, and none of it exists in Rust yet: the pipeline modules
/// are wave-3 items and the aggregator/formatter are wave-5's. A `process` that
/// returned anything today would be inventing numbers, so the item stays open
/// with this note rather than shipping a stub that type-checks and lies.
pub const PROCESS_IS_A_LATER_WAVE: &str = "api.process() needs pipeline{dedup,classifier,enricher} + stats{aggregator,formatter}; \
     those are wave-3/wave-5 items (RS-3-*, RS-5-*). Not stubbed on purpose.";

/// `store.types.ProjectRow`.
#[derive(Debug, Clone, PartialEq)]
pub struct ProjectRow {
    /// `projects.id` — present on the row, absent from the public dict.
    pub id: i64,
    /// `"claude"`, `"codex"`, `"cursor"`, …
    pub provider: String,
    /// The canonical project slug.
    pub slug: String,
    /// The original log directory, when known.
    pub path: Option<String>,
    /// Human-facing name.
    pub display_name: String,
    /// Epoch seconds.
    pub first_seen: f64,
    /// Epoch seconds.
    pub last_modified: f64,
}

impl ProjectRow {
    /// The dict `api.list_projects` returns — `id` dropped, six keys, in order.
    #[must_use]
    pub fn to_dict(&self) -> pyjson::Value {
        pyjson::Value::Object(vec![
            ("slug".into(), pyjson::Value::Str(self.slug.clone())),
            ("provider".into(), pyjson::Value::Str(self.provider.clone())),
            (
                "display_name".into(),
                pyjson::Value::Str(self.display_name.clone()),
            ),
            (
                "path".into(),
                self.path
                    .as_ref()
                    .map_or(pyjson::Value::Null, |path| pyjson::Value::Str(path.clone())),
            ),
            ("first_seen".into(), pyjson::Value::Float(self.first_seen)),
            (
                "last_modified".into(),
                pyjson::Value::Float(self.last_modified),
            ),
        ])
    }
}

/// `store.types.SessionRow`.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionRow {
    /// `sessions.id` — the internal foreign key.
    pub id: i64,
    /// `sessions.project_id`.
    pub project_id: i64,
    /// The provider-facing session id.
    pub session_id: String,
    /// `sessions.first_ts`, `None` when null.
    pub first_ts: Option<String>,
    /// `sessions.last_ts`, `None` when null.
    pub last_ts: Option<String>,
    /// `sessions.message_count`.
    pub message_count: i64,
}

impl SessionRow {
    /// The dict `api.list_sessions` returns — four keys, in order.
    #[must_use]
    pub fn to_dict(&self) -> pyjson::Value {
        pyjson::Value::Object(vec![
            (
                "session_id".into(),
                pyjson::Value::Str(self.session_id.clone()),
            ),
            (
                "first_ts".into(),
                self.first_ts
                    .as_ref()
                    .map_or(pyjson::Value::Null, |ts| pyjson::Value::Str(ts.clone())),
            ),
            (
                "last_ts".into(),
                self.last_ts
                    .as_ref()
                    .map_or(pyjson::Value::Null, |ts| pyjson::Value::Str(ts.clone())),
            ),
            (
                "message_count".into(),
                pyjson::Value::Int(self.message_count),
            ),
        ])
    }
}

const PROJECT_COLUMNS: &str =
    "SELECT id, provider, slug, path, display_name, first_seen, last_modified FROM projects";

fn project_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ProjectRow> {
    Ok(ProjectRow {
        id: row.get(0)?,
        provider: row.get::<_, Option<String>>(1)?.unwrap_or_default(),
        slug: row.get::<_, Option<String>>(2)?.unwrap_or_default(),
        path: row.get(3)?,
        display_name: row.get::<_, Option<String>>(4)?.unwrap_or_default(),
        first_seen: row.get(5)?,
        last_modified: row.get(6)?,
    })
}

/// `store.queries.list_projects` — every project, newest `last_modified` first.
///
/// # Errors
/// When the query fails.
pub fn store_list_projects(conn: &Connection) -> Result<Vec<ProjectRow>> {
    let mut stmt = conn.prepare(&format!("{PROJECT_COLUMNS} ORDER BY last_modified DESC"))?;
    let rows = stmt
        .query_map([], project_from_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// `store.queries.get_project` — the **first** row with this slug.
///
/// No `ORDER BY` and no provider clause, so on a slug two providers share the
/// row SQLite happens to return first wins. Ported as found; `list_sessions`'s
/// `provider` argument exists precisely because of it.
///
/// # Errors
/// When the query fails.
pub fn store_get_project(conn: &Connection, slug: &str) -> Result<Option<ProjectRow>> {
    let mut stmt = conn.prepare(&format!("{PROJECT_COLUMNS} WHERE slug = ?"))?;
    let mut rows = stmt.query([slug])?;
    match rows.next()? {
        Some(row) => Ok(Some(project_from_row(row)?)),
        None => Ok(None),
    }
}

/// `store.queries.list_sessions(project_id=<int>)`.
///
/// # Errors
/// When the query fails.
pub fn store_list_sessions(conn: &Connection, project_id: i64) -> Result<Vec<SessionRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, project_id, session_id, first_ts, last_ts, message_count \
         FROM sessions WHERE project_id = ? ORDER BY last_ts DESC",
    )?;
    let rows = stmt
        .query_map([project_id], |row| {
            Ok(SessionRow {
                id: row.get(0)?,
                project_id: row.get(1)?,
                session_id: row.get::<_, Option<String>>(2)?.unwrap_or_default(),
                first_ts: row.get(3)?,
                last_ts: row.get(4)?,
                message_count: row.get(5)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// `api.list_projects(provider=None)` — every project, optionally filtered.
///
/// An absent store is `[]`, not an error. The filter is `provider is not None`,
/// so `Some("")` really does select the rows whose provider is the empty string
/// rather than meaning "no filter".
///
/// # Errors
/// When the store exists but cannot be read.
pub fn list_projects(store_path: &Path, provider: Option<&str>) -> Result<Vec<ProjectRow>> {
    if !store_path.is_file() {
        return Ok(Vec::new());
    }
    let store = Store::open_read_only(store_path)?;
    let mut rows = store_list_projects(store.conn())?;
    if let Some(wanted) = provider {
        rows.retain(|row| row.provider == wanted);
    }
    Ok(rows)
}

/// `api.list_sessions(project_slug, provider=None)`.
///
/// # Errors
/// `KeyError(project_slug)` in Python — here an error whose message is the slug,
/// raised when the store is absent or no project matches.
pub fn list_sessions(
    store_path: &Path,
    project_slug: &str,
    provider: Option<&str>,
) -> Result<Vec<SessionRow>> {
    if !store_path.is_file() {
        bail!("{project_slug}");
    }
    let store = Store::open_read_only(store_path)?;
    let project = resolve_project(store.conn(), project_slug, provider)?;
    store_list_sessions(store.conn(), project.id)
}

/// `api._resolve_project` — slug alone, or slug ⨯ provider.
///
/// # Errors
/// When no project matches (Python's `KeyError(slug)`).
pub fn resolve_project(
    conn: &Connection,
    slug: &str,
    provider: Option<&str>,
) -> Result<ProjectRow> {
    match provider {
        None => match store_get_project(conn, slug)? {
            Some(row) => Ok(row),
            None => bail!("{slug}"),
        },
        Some(wanted) => store_list_projects(conn)?
            .into_iter()
            .find(|row| row.slug == slug && row.provider == wanted)
            .ok_or_else(|| anyhow::anyhow!("{slug}")),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    struct Scratch {
        path: PathBuf,
    }

    impl Scratch {
        fn new() -> Self {
            static SEQ: AtomicU32 = AtomicU32::new(0);
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock before the epoch")
                .as_nanos();
            let seq = SEQ.fetch_add(1, Ordering::Relaxed);
            let path =
                std::env::temp_dir().join(format!("stax-api-{}-{nanos}-{seq}", std::process::id()));
            fs::create_dir_all(&path).expect("creating the scratch directory");
            Self { path }
        }

        fn db(&self) -> PathBuf {
            self.path.join("store.db")
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    fn seeded(scratch: &Scratch) {
        let conn = Connection::open(scratch.db()).expect("creating the store");
        conn.execute_batch(
            "CREATE TABLE projects (
               id INTEGER PRIMARY KEY, provider TEXT NOT NULL, slug TEXT NOT NULL,
               path TEXT, display_name TEXT NOT NULL, first_seen REAL NOT NULL,
               last_modified REAL NOT NULL, UNIQUE (provider, slug));
             CREATE TABLE sessions (
               id INTEGER PRIMARY KEY, project_id INTEGER NOT NULL,
               session_id TEXT NOT NULL, first_ts TEXT, last_ts TEXT,
               message_count INTEGER NOT NULL DEFAULT 0);
             INSERT INTO projects VALUES
               (1, 'claude', '-home-dev-alpha', '/logs/alpha', 'alpha', 1.5, 30.0),
               (2, 'codex',  '-home-dev-alpha', NULL,          'alpha', 2.5, 40.0),
               (3, 'claude', '-home-dev-beta',  '/logs/beta',  'beta',  3.5, 20.0);
             INSERT INTO sessions VALUES
               (1, 1, 'aaaa', '2026-01-02T09:00:00+00:00', '2026-01-02T10:00:00+00:00', 6),
               (2, 1, 'bbbb', NULL, NULL, 0),
               (3, 2, 'cccc', '2026-02-01T09:00:00+00:00', '2026-02-01T10:00:00+00:00', 2);",
        )
        .expect("seeding");
    }

    #[test]
    fn a_missing_store_is_an_empty_list_not_a_failure() {
        let scratch = Scratch::new();
        assert!(
            list_projects(&scratch.db(), None)
                .expect("a fresh install is not an error")
                .is_empty()
        );
        // …but the session-scoped call is a KeyError even then.
        let error = list_sessions(&scratch.db(), "-home-dev-alpha", None).expect_err("KeyError");
        assert_eq!(error.to_string(), "-home-dev-alpha");
    }

    #[test]
    fn projects_come_back_newest_first_and_filter_by_provider() {
        let scratch = Scratch::new();
        seeded(&scratch);
        let all = list_projects(&scratch.db(), None).expect("reads");
        assert_eq!(
            all.iter().map(|row| row.id).collect::<Vec<_>>(),
            vec![2, 1, 3],
            "ORDER BY last_modified DESC"
        );
        let codex = list_projects(&scratch.db(), Some("codex")).expect("reads");
        assert_eq!(codex.len(), 1);
        // `provider is not None` — an empty provider is a filter, not a default.
        assert!(
            list_projects(&scratch.db(), Some(""))
                .expect("reads")
                .is_empty()
        );
    }

    #[test]
    fn the_public_dict_drops_the_id_and_keeps_the_reference_key_order() {
        let scratch = Scratch::new();
        seeded(&scratch);
        let rows = list_projects(&scratch.db(), Some("claude")).expect("reads");
        let alpha = rows
            .iter()
            .find(|row| row.slug == "-home-dev-alpha")
            .expect("alpha");
        assert_eq!(
            pyjson::dumps_compact(&alpha.to_dict()),
            "{\"slug\":\"-home-dev-alpha\",\"provider\":\"claude\",\
             \"display_name\":\"alpha\",\"path\":\"/logs/alpha\",\
             \"first_seen\":1.5,\"last_modified\":30.0}"
        );
        let sessions = list_sessions(&scratch.db(), "-home-dev-alpha", Some("claude"))
            .expect("reads")
            .into_iter()
            .map(|row| pyjson::dumps_compact(&row.to_dict()))
            .collect::<Vec<_>>();
        assert_eq!(
            sessions[0],
            "{\"session_id\":\"aaaa\",\"first_ts\":\"2026-01-02T09:00:00+00:00\",\
             \"last_ts\":\"2026-01-02T10:00:00+00:00\",\"message_count\":6}"
        );
        assert_eq!(
            sessions[1],
            "{\"session_id\":\"bbbb\",\"first_ts\":null,\"last_ts\":null,\"message_count\":0}"
        );
    }

    /// `UNIQUE(provider, slug)` means one slug can exist twice. Without
    /// `provider` the reference takes whichever row SQLite returns first; with
    /// it, the constraint is honoured.
    #[test]
    fn a_slug_shared_by_two_providers_needs_the_provider_argument() {
        let scratch = Scratch::new();
        seeded(&scratch);
        let store = Store::open_read_only(&scratch.db()).expect("opens");
        assert_eq!(
            resolve_project(store.conn(), "-home-dev-alpha", Some("codex"))
                .expect("resolves")
                .id,
            2
        );
        assert_eq!(
            resolve_project(store.conn(), "-home-dev-alpha", None)
                .expect("resolves")
                .id,
            1,
            "no ORDER BY — the first row wins"
        );
        let error =
            resolve_project(store.conn(), "-home-dev-alpha", Some("cursor")).expect_err("KeyError");
        assert_eq!(error.to_string(), "-home-dev-alpha");
        // The sessions are the resolved project's, not the slug's.
        assert_eq!(
            list_sessions(&scratch.db(), "-home-dev-alpha", Some("codex"))
                .expect("reads")
                .len(),
            1
        );
    }
}
