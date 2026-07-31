//! Strictly read-only access to `store.db`.
//!
//! The campaign rule is that the live dataset is read-only to Rust
//! (`docs/specs/rust-port.md` §5), so wave 0 hands out exactly one kind of
//! handle: one opened with `SQLITE_OPEN_READ_ONLY`, which makes "we did not
//! touch the store" an enforcement rather than a promise — every write attempt
//! comes back `SQLITE_READONLY` from SQLite itself.
//!
//! The URI carries `immutable=0` explicitly. That is SQLite's default, but
//! stating it is the point: `immutable=1` would let SQLite skip locking and
//! ignore the `-wal`, and against a store that a live watcher is appending to
//! that reads stale — or torn — pages. `immutable=0` means the reader honors the
//! WAL and sees the same committed state Python's `mode=ro` reader sees.

use std::fmt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OpenFlags};

use crate::settings;

/// A read-only connection to a StackUnderflow store.
pub struct Store {
    conn: Connection,
    path: PathBuf,
}

impl fmt::Debug for Store {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Store")
            .field("path", &self.path)
            .field("mode", &"read-only")
            .finish()
    }
}

/// What kind of `sqlite_master` object a row count came from.
///
/// The distinction is load-bearing rather than cosmetic: `messages` is a
/// UNION-ALL *view* over the monthly partitions (§6b), so its count overlaps
/// with the `messages_YYYYMM` tables listed beside it. Tagging the kind keeps a
/// reader from summing the column and double-counting 383K rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectKind {
    /// An ordinary table.
    Table,
    /// A view — its rows are also counted under the tables it selects from.
    View,
}

impl ObjectKind {
    /// The `sqlite_master.type` spelling, which is also what `status` prints.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Table => "table",
            Self::View => "view",
        }
    }
}

impl fmt::Display for ObjectKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad(self.as_str())
    }
}

/// One `sqlite_master` object and its `COUNT(*)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectCount {
    /// The object name, as stored in `sqlite_master`.
    pub name: String,
    /// Table or view.
    pub kind: ObjectKind,
    /// Live `COUNT(*)`, not a cached estimate.
    pub rows: i64,
}

impl Store {
    /// Open the store named by `$STACKUNDERFLOW_HOME` (see [`settings`]).
    ///
    /// # Errors
    /// When the resolved path does not exist, or SQLite refuses the file.
    pub fn open_default() -> Result<Self> {
        Self::open_read_only(&settings::store_path())
    }

    /// Open `path` read-only.
    ///
    /// # Errors
    /// When `path` does not exist — a read-only open of a missing file is an
    /// error rather than the silent create Python's `store.db.connect` performs
    /// — or when SQLite rejects the file.
    pub fn open_read_only(path: &Path) -> Result<Self> {
        if !path.exists() {
            bail!("no store at {}", path.display());
        }
        let flags = OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_URI
            | OpenFlags::SQLITE_OPEN_NO_MUTEX;
        let conn = Connection::open_with_flags(sqlite_uri(path), flags)
            .with_context(|| format!("opening {} read-only", path.display()))?;
        Ok(Self {
            conn,
            path: path.to_path_buf(),
        })
    }

    /// The path this store was opened from.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The read-only connection, for queries later waves add.
    ///
    /// Handing this out is safe by construction: the handle carries
    /// `SQLITE_OPEN_READ_ONLY`, so no caller can turn it into a writer.
    #[must_use]
    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    /// `PRAGMA user_version` — the migration level (schema v030 as of this wave).
    ///
    /// # Errors
    /// When the pragma cannot be read.
    pub fn schema_version(&self) -> Result<i64> {
        self.conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .context("reading PRAGMA user_version")
    }

    /// Every table and view in `sqlite_master` with its `COUNT(*)`, sorted by name.
    ///
    /// Sorting happens in SQL (`ORDER BY name`, BINARY collation) so the order is
    /// the engine's, not a locale's — the Python reference sorts the same way and
    /// the two outputs are compared byte for byte.
    ///
    /// # Errors
    /// When `sqlite_master` cannot be read, or a `COUNT(*)` fails.
    pub fn object_counts(&self) -> Result<Vec<ObjectCount>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT name, type FROM sqlite_master \
                 WHERE type IN ('table', 'view') \
                 ORDER BY name",
            )
            .context("listing sqlite_master")?;
        let objects = stmt
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .context("listing sqlite_master")?
            .collect::<rusqlite::Result<Vec<_>>>()
            .context("listing sqlite_master")?;

        objects
            .into_iter()
            .map(|(name, kind)| {
                let kind = match kind.as_str() {
                    "table" => ObjectKind::Table,
                    "view" => ObjectKind::View,
                    other => bail!("unexpected sqlite_master.type {other:?} for {name}"),
                };
                let rows = self.count_rows(&name)?;
                Ok(ObjectCount { name, kind, rows })
            })
            .collect()
    }

    /// `COUNT(*)` for one object.
    ///
    /// # Errors
    /// When the count fails — a corrupt page or a view over a dropped table.
    pub fn count_rows(&self, name: &str) -> Result<i64> {
        // Identifiers cannot be bound as parameters; quote and escape instead.
        let quoted = name.replace('"', "\"\"");
        self.conn
            .query_row(&format!("SELECT COUNT(*) FROM \"{quoted}\""), [], |row| {
                row.get(0)
            })
            .with_context(|| format!("counting rows in {name}"))
    }
}

/// Build the `file:` URI SQLite opens, with `immutable=0` stated explicitly.
///
/// Only the three characters that would otherwise change the URI's meaning are
/// escaped (`%`, `?`, `#`); SQLite accepts everything else, spaces included,
/// verbatim.
fn sqlite_uri(path: &Path) -> String {
    let mut uri = String::from("file:");
    for ch in path.to_string_lossy().chars() {
        match ch {
            '%' => uri.push_str("%25"),
            '?' => uri.push_str("%3f"),
            '#' => uri.push_str("%23"),
            other => uri.push(other),
        }
    }
    uri.push_str("?immutable=0");
    uri
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    use rusqlite::ErrorCode;

    use super::*;

    /// A scratch directory that removes itself — no `tempfile` dependency this
    /// wave (§5 keeps wave-0 dependencies minimal).
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
            let path = std::env::temp_dir()
                .join(format!("stax-core-{}-{nanos}-{seq}", std::process::id()));
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

    /// A miniature of the real store: two partition tables and a UNION-ALL view
    /// over them, which is the shape `messages` has (§6b).
    fn build_fixture(path: &Path, user_version: i64) {
        let conn = Connection::open(path).expect("creating the fixture store");
        conn.execute_batch(
            "CREATE TABLE projects (id INTEGER PRIMARY KEY, slug TEXT);
             CREATE TABLE messages_202601 (id INTEGER PRIMARY KEY, body TEXT);
             CREATE TABLE messages_202602 (id INTEGER PRIMARY KEY, body TEXT);
             CREATE TABLE empty_mart (id INTEGER PRIMARY KEY);
             CREATE VIEW messages AS
                 SELECT * FROM messages_202601 UNION ALL SELECT * FROM messages_202602;
             INSERT INTO projects (slug) VALUES ('a'), ('b'), ('c');
             INSERT INTO messages_202601 (body) VALUES ('one'), ('two');
             INSERT INTO messages_202602 (body) VALUES ('three'), ('four'), ('five');",
        )
        .expect("populating the fixture store");
        conn.pragma_update(None, "user_version", user_version)
            .expect("stamping user_version");
        conn.close().expect("closing the fixture store");
    }

    #[test]
    fn reads_the_schema_version() {
        let scratch = Scratch::new();
        build_fixture(&scratch.db(), 30);

        let store = Store::open_read_only(&scratch.db()).expect("opening the fixture");
        assert_eq!(store.schema_version().expect("reading user_version"), 30);
    }

    #[test]
    fn counts_every_table_and_the_view_sorted_by_name() {
        let scratch = Scratch::new();
        build_fixture(&scratch.db(), 30);

        let store = Store::open_read_only(&scratch.db()).expect("opening the fixture");
        let counts = store.object_counts().expect("counting objects");

        let expected = vec![
            ObjectCount {
                name: "empty_mart".into(),
                kind: ObjectKind::Table,
                rows: 0,
            },
            ObjectCount {
                name: "messages".into(),
                kind: ObjectKind::View,
                rows: 5,
            },
            ObjectCount {
                name: "messages_202601".into(),
                kind: ObjectKind::Table,
                rows: 2,
            },
            ObjectCount {
                name: "messages_202602".into(),
                kind: ObjectKind::Table,
                rows: 3,
            },
            ObjectCount {
                name: "projects".into(),
                kind: ObjectKind::Table,
                rows: 3,
            },
        ];
        assert_eq!(counts, expected);
    }

    #[test]
    fn indexes_and_triggers_are_not_counted() {
        let scratch = Scratch::new();
        build_fixture(&scratch.db(), 30);
        {
            let conn = Connection::open(scratch.db()).expect("reopening the fixture");
            conn.execute_batch(
                "CREATE INDEX idx_projects_slug ON projects (slug);
                 CREATE TRIGGER t_noop AFTER INSERT ON projects BEGIN SELECT 1; END;",
            )
            .expect("adding an index and a trigger");
        }

        let store = Store::open_read_only(&scratch.db()).expect("opening the fixture");
        let names: Vec<_> = store
            .object_counts()
            .expect("counting objects")
            .into_iter()
            .map(|object| object.name)
            .collect();
        assert!(!names.iter().any(|name| name.starts_with("idx_")));
        assert!(!names.iter().any(|name| name.starts_with("t_noop")));
    }

    #[test]
    fn writes_are_refused_by_sqlite() {
        let scratch = Scratch::new();
        build_fixture(&scratch.db(), 30);

        let store = Store::open_read_only(&scratch.db()).expect("opening the fixture");
        for statement in [
            "INSERT INTO projects (slug) VALUES ('d')",
            "UPDATE projects SET slug = 'z'",
            "DELETE FROM projects",
            "CREATE TABLE sneaky (id INTEGER)",
            "DROP TABLE projects",
            "PRAGMA user_version = 999",
        ] {
            let error = store
                .conn()
                .execute_batch(statement)
                .expect_err(&format!("{statement} should have been refused"));
            match error {
                rusqlite::Error::SqliteFailure(err, _) => {
                    assert_eq!(err.code, ErrorCode::ReadOnly, "{statement} -> {err:?}");
                }
                other => panic!("{statement} failed with {other:?}, expected SQLITE_READONLY"),
            }
        }

        // …and the store still reads its original contents.
        assert_eq!(store.count_rows("projects").expect("re-counting"), 3);
    }

    #[test]
    fn a_missing_store_is_an_error_not_an_empty_database() {
        let scratch = Scratch::new();
        let missing = scratch.path.join("nope.db");

        let error = Store::open_read_only(&missing).expect_err("should refuse to open");
        assert!(error.to_string().contains("no store at"), "{error}");
        assert!(!missing.exists(), "read-only open must not create the file");
    }

    #[test]
    fn the_bundled_engine_has_fts5_compiled_in() {
        // §3 asks for rusqlite `features = ["fts5"]`, which does not exist; FTS5
        // rides along with `bundled`. Since the manifest cannot state it, the
        // test does: wave 1's memory crate is built on FTS5 + bm25.
        let conn = Connection::open_in_memory().expect("in-memory database");
        conn.execute_batch("CREATE VIRTUAL TABLE probe USING fts5(body)")
            .expect("SQLITE_ENABLE_FTS5 missing from the bundled build");
        conn.execute_batch("INSERT INTO probe (body) VALUES ('watermark parity')")
            .expect("inserting into the fts5 table");
        let hits: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM probe WHERE probe MATCH 'watermark'",
                [],
                |row| row.get(0),
            )
            .expect("matching");
        assert_eq!(hits, 1);
        let ranked: f64 = conn
            .query_row(
                "SELECT bm25(probe) FROM probe WHERE probe MATCH 'watermark'",
                [],
                |row| row.get(0),
            )
            .expect("bm25() missing from the bundled build");
        assert!(ranked < 0.0, "bm25 scores are negative in SQLite: {ranked}");
    }

    #[test]
    fn the_uri_states_immutable_zero_and_escapes_delimiters() {
        assert_eq!(
            sqlite_uri(Path::new("/data/su/store.db")),
            "file:/data/su/store.db?immutable=0"
        );
        assert_eq!(
            sqlite_uri(Path::new("/data/wei?rd#/100%/store.db")),
            "file:/data/wei%3frd%23/100%25/store.db?immutable=0"
        );
    }
}
