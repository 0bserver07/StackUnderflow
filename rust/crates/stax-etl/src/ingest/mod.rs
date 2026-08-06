//! The ingest layer — port of `stackunderflow/ingest/`.
//!
//! ```text
//! enumerate  →  skip-unchanged  →  ingest_file  →  post-ingest hooks  →  auto-reindex
//!   (refs)      (ingest_log)      (1 txn/file)      (materialize)          (wave 6)
//! ```
//!
//! Three files in Python (`__init__.py`, `enumerate.py`, `writer.py`, 962 lines
//! together) plus the watcher and the CLI freshness helper. The pieces:
//!
//! * [`enumerate`] — fan every adapter's `SessionRef`s into one lazy sequence.
//! * [`run_ingest`] — the pass: compare `(mtime, size)` against `ingest_log`,
//!   skip / tail-read / full-reparse, then the hooks and the reindex.
//! * [`writer`] — one file → one transaction → one `ingest_log` row, with the
//!   lazy project/session upsert and the per-record normalize hook.
//! * [`hooks`] — `PostIngestHook`, the trait that replaces Python's
//!   `getattr(adapter, "materialize_metadata")`.
//! * [`teams`] — the Claude hook's body: `adapters/claude_teams.py`, which is
//!   what fills `sessions.{team_id, spawned_by_session_id, spawn_prompt,
//!   agent_role}` and `agent_teams` (DIV-042).
//! * [`outcomes`] — the hook's second call: `link_commits_to_sessions`, the
//!   ingest half of `services/outcome_attribution.py`.
//! * [`reindex`] — `auto_reindex_touched`, interface ported, index builds
//!   deferred to wave 6.
//! * [`watcher`] — the `notify` filesystem watcher (non-wasm targets).
//!
//! # Why this lives in `stax-etl` and not a new crate
//!
//! The crate's own charter names it: *"the transactional writer with its
//! watermarks, and the filesystem watcher (`notify`)"* (`lib.rs`, from
//! `docs/specs/rust-port.md` §3). More concretely, the writer's per-record hook
//! calls [`crate::normalize`] and its post-commit step calls
//! [`crate::marts::watermark::refresh_all_marts`]; a separate `stax-ingest`
//! crate would put a crate boundary through the middle of one transaction and
//! give two owners to the `usage_events` insert that `normalize::pass` and the
//! writer already share. The cost is one promoted dependency
//! (`stax-adapters`, dev → normal), and the graph stays acyclic because
//! `stax-adapters` depends on neither `stax-etl` nor `stax-core`.
//!
//! # The clock is injected
//!
//! `ingest_log.last_ingest_ts` is `time.time()` and the mart watermark stamp is
//! `datetime.now(UTC).isoformat()`. Both arrive through [`Clock`] — finding 5
//! (`set_var` is `unsafe` under Rust 2024; this workspace forbids `unsafe`)
//! makes pure-function-plus-injection law for the campaign, and it is also the
//! only way the parity harness can make two runs produce identical bytes in a
//! column that is otherwise wall-clock noise.

pub mod enumerate;
pub mod hooks;
pub mod lock;
pub mod outcomes;
pub mod pyraw;
pub mod reindex;
pub mod teams;
pub mod writer;

// `notify` has no wasm32 backend. Everything else in this module is portable, so
// the watcher is the only thing wave 9 loses — see the target-gated dependency
// in `Cargo.toml`.
#[cfg(not(target_arch = "wasm32"))]
pub mod watcher;

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension};
use stax_adapters::base::{SessionRef, SourceAdapter, SourceKind};

use crate::normalize::NormalizeContext;
pub use reindex::{ReindexConfig, ReindexReport};
pub use writer::{FileReport, ingest_file};

/// The two clock reads the ingest layer makes, injected.
pub trait Clock {
    /// `time.time()` — `ingest_log.last_ingest_ts`, a REAL column.
    fn unix_seconds(&self) -> f64;
    /// `datetime.now(UTC).isoformat()` — `mart_watermark.last_refresh_ts`.
    fn iso_utc(&self) -> String;
}

/// The real clock.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn unix_seconds(&self) -> f64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0.0, |elapsed| elapsed.as_secs_f64())
    }

    fn iso_utc(&self) -> String {
        stax_core::queries::pytime::isoformat_utc(stax_core::queries::pytime::now_micros())
    }
}

/// A pinned clock — for tests and for the parity harness, where a wall-clock
/// column would be the only diff in an otherwise byte-identical table.
#[derive(Debug, Clone)]
pub struct FixedClock {
    unix: f64,
    iso: String,
}

impl FixedClock {
    /// Pin both reads.
    #[must_use]
    pub fn new(unix: f64, iso: impl Into<String>) -> Self {
        Self {
            unix,
            iso: iso.into(),
        }
    }
}

impl Clock for FixedClock {
    fn unix_seconds(&self) -> f64 {
        self.unix
    }
    fn iso_utc(&self) -> String {
        self.iso.clone()
    }
}

/// What one [`run_ingest`] pass did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IngestReport {
    /// `{provider: messages_added}` — Python's return value, in `dict`
    /// insertion order (first ref of that provider that was not skipped).
    pub counts: Vec<(String, i64)>,
    /// The slugs that gained messages, in first-touch order.
    ///
    /// Python's is a `set`, so its iteration order is hash order. Each slug is
    /// reindexed independently, so this is an order-only difference with no
    /// observable effect — recorded rather than papered over.
    pub touched_slugs: Vec<String>,
    /// Refs whose `(mtime, size)` matched the `ingest_log` row: the fast path.
    pub files_skipped: u64,
    /// Refs that reached [`ingest_file`].
    pub files_processed: u64,
    /// Refs whose size shrank below the logged size — truncation / rotation,
    /// re-read from 0.
    pub files_reparsed: u64,
    /// Events the per-file normalize hooks inserted, summed.
    pub events_inserted: u64,
    /// The `_logger` lines Python would have emitted (see [`writer`]'s note on
    /// why diagnostics are returned rather than logged).
    pub notes: Vec<String>,
    /// What the auto-reindex step did.
    pub reindex: ReindexReport,
}

/// Run one ingest pass across `adapters`. Port of `ingest.run_ingest`.
///
/// For each file, compare `(mtime, size)` against `ingest_log` and either skip,
/// tail-read, or full-reparse. Afterwards run each adapter's
/// [`hooks::PostIngestHook`] and refresh the indexes of every project that
/// gained messages.
///
/// # Errors
/// A ref whose provider is not in `adapters` (Python raises `KeyError` and does
/// not catch it), or any SQLite error from the skip-unchanged probe. Errors from
/// [`ingest_file`] propagate too — Python does not fence that call here; the
/// *watcher* fences it, which is a different call site and is ported as such.
pub fn run_ingest(
    conn: &Connection,
    adapters: &[Box<dyn SourceAdapter>],
    ctx: &NormalizeContext,
    clock: &dyn Clock,
    reindex_config: &ReindexConfig<'_>,
) -> Result<IngestReport> {
    let mut report = IngestReport::default();

    for session in enumerate::iter_refs(adapters) {
        let Some(since) = resume_offset(conn, &session, &mut report)? else {
            report.files_skipped += 1;
            continue;
        };

        let adapter = enumerate::lookup(adapters, &session.provider)?;
        // The pre/post `COUNT(*)` is Python's own measurement of what the writer
        // did, and it is ported rather than replaced by `FileReport::
        // messages_added` because it is what fills the counts dict — including
        // the case where the two could disagree (they cannot today; if they ever
        // do, the port should disagree the same way).
        let pre = message_count(conn)?;
        let file_report = ingest_file(conn, adapter, &session, since, ctx, clock)?;
        let post = message_count(conn)?;
        let added = post - pre;
        report.files_processed += 1;
        report.events_inserted += file_report.events_inserted;
        report.notes.extend(file_report.notes);

        match report
            .counts
            .iter_mut()
            .find(|(provider, _)| *provider == session.provider)
        {
            Some(entry) => entry.1 += added,
            None => report.counts.push((session.provider.clone(), added)),
        }
        if added > 0 && !report.touched_slugs.contains(&session.project_slug) {
            report.touched_slugs.push(session.project_slug.clone());
        }
    }

    // Per-adapter post-ingest hook. Claude uses it to materialise agent-team
    // metadata so the Agents tab JOINs instead of re-parsing `raw_json` on every
    // render. Each call is fenced — a hook hiccup must never break the pass.
    // `HookEnv::live()` is `_claude_home()`, read here rather than inside the
    // hook so a test can point it somewhere small — see [`hooks::HookEnv`].
    let provider_names: Vec<&str> = adapters.iter().map(|a| a.name()).collect();
    report.notes.extend(hooks::run_all(
        conn,
        &provider_names,
        &hooks::HookEnv::live(),
    ));

    if !report.touched_slugs.is_empty() {
        report.reindex =
            reindex::auto_reindex_touched(conn, &report.touched_slugs, reindex_config)?;
    }
    Ok(report)
}

/// The skip / tail-read / full-reparse decision.
///
/// `Ok(None)` means "unchanged — skip". `Ok(Some(offset))` is the
/// `since_offset` to resume from.
fn resume_offset(
    conn: &Connection,
    session: &SessionRef,
    report: &mut IngestReport,
) -> Result<Option<i64>> {
    let path = stax_core::queries::paths::path_to_string(&session.file_path);
    match session.source_kind {
        SourceKind::Database => {
            let prior: Option<(f64, i64, Option<i64>)> = conn
                .query_row(
                    "SELECT mtime, size, last_rowid FROM ingest_log \
                     WHERE file_path = ? AND session_id = ?",
                    rusqlite::params![path, session.session_id],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .optional()?;
            if let Some((mtime, size, last_rowid)) = prior {
                if is_unchanged(mtime, size, session) {
                    return Ok(None);
                }
                // A NULL `last_rowid` on a database row would make Python pass
                // `None` as `since_offset` and the adapter's comparison would
                // raise; the writer never writes NULL there, so 0 is the only
                // reachable reading of "no watermark yet".
                return Ok(Some(last_rowid.unwrap_or(0)));
            }
            Ok(Some(0))
        }
        SourceKind::File => {
            let prior: Option<(f64, i64, Option<i64>)> = conn
                .query_row(
                    "SELECT mtime, size, processed_offset FROM ingest_log \
                     WHERE file_path = ? AND session_id IS NULL",
                    [&path],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .optional()?;
            let Some((mtime, size, processed_offset)) = prior else {
                return Ok(Some(0));
            };
            if is_unchanged(mtime, size, session) {
                return Ok(None);
            }
            if (session.file_size as i64) < size {
                // Truncation / rotation — full reparse from 0. The DELETE is a
                // bare statement in autocommit mode on both sides
                // (`db.connect(..., isolation_level=None)`), so it lands before
                // the writer's own `BEGIN`.
                conn.execute(
                    "DELETE FROM ingest_log WHERE file_path = ? AND session_id IS NULL",
                    [&path],
                )?;
                report.files_reparsed += 1;
                return Ok(Some(0));
            }
            Ok(Some(processed_offset.unwrap_or(0)))
        }
    }
}

/// `prior["mtime"] == ref.file_mtime and prior["size"] == ref.file_size`.
///
/// Both sides compare an `st_mtime` float for exact equality. That is Python's
/// `==` on two `float`s that came from the same `stat()` call formula, and
/// `stax_adapters::base::mtime_seconds` reproduces CPython's `sec + 1e-9*nsec`
/// bit-for-bit, so the comparison agrees. Clippy's float-cmp lint is exactly
/// what is wanted *against*: rounding here would make a changed file look
/// unchanged.
#[allow(
    clippy::float_cmp,
    reason = "the fast path IS an exact float equality on both sides; a \
    tolerance would silently skip a modified file"
)]
fn is_unchanged(mtime: f64, size: i64, session: &SessionRef) -> bool {
    mtime == session.file_mtime && size == session.file_size as i64
}

/// `SELECT COUNT(*) FROM messages` — over the UNION-ALL view, as Python's is.
fn message_count(conn: &Connection) -> Result<i64> {
    Ok(conn.query_row("SELECT COUNT(*) FROM messages", [], |row| row.get(0))?)
}

/// The campaign's write-side guard.
///
/// `stax_core::store` hands out read-only handles on purpose — the live dataset
/// is read-only to this campaign (`docs/specs/rust-port.md` §5). The ingest
/// layer is the first thing that *must* write, so the guard moves from the
/// connection flags to the path: anything under the live dataset is refused
/// outright, and everything else gets the PRAGMAs `store/db.py::connect` sets.
pub mod guard {
    use std::path::Path;

    use anyhow::{Context, Result, bail};
    use rusqlite::Connection;

    /// The live dataset's directory name. A substring match rather than a path
    /// comparison because the dataset is reachable through several symlinks and
    /// a copy of it under a different mount is still the thing not to write to.
    pub const LIVE_DATASET_MARKER: &str = "stackunderflow-data";

    /// Open `path` read-write with the store's standard PRAGMAs.
    ///
    /// `journal_mode = WAL`, `synchronous = NORMAL`, `foreign_keys = ON` — the
    /// three `store/db.py::connect` sets. `foreign_keys` is not cosmetic: the
    /// message partitions declare `REFERENCES sessions(id) ON DELETE CASCADE`,
    /// so an orphan insert raises on the Python side and must raise here too.
    ///
    /// # Errors
    /// A path under the live dataset, or any SQLite error.
    pub fn open_read_write(path: &Path) -> Result<Connection> {
        let text = path.to_string_lossy();
        if text.contains(LIVE_DATASET_MARKER) {
            bail!(
                "refusing to open {text} read-write: the live dataset is READ-ONLY for \
                 this campaign (docs/specs/rust-port.md §5). Work on a copy."
            );
        }
        open_unchecked(path)
    }

    /// Open `path` read-write for the RESIDENT watcher — no live-dataset fence.
    ///
    /// The flip (2026-08-05, maintainer's standing order): the Rust watcher is
    /// the sanctioned writer of the live dataset now, which inverts §5's
    /// premise for exactly one caller — `stax start`'s supervised boot. Every
    /// harness, differ, and parity bin stays on [`open_read_write`] and stays
    /// fenced; a test that wants the live store still cannot have it.
    ///
    /// # Errors
    /// Any SQLite error.
    pub fn open_resident(path: &Path) -> Result<Connection> {
        open_unchecked(path)
    }

    fn open_unchecked(path: &Path) -> Result<Connection> {
        let text = path.to_string_lossy();
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let conn = Connection::open(path).with_context(|| format!("opening {text}"))?;
        conn.execute_batch(
            "PRAGMA journal_mode = WAL; \
             PRAGMA synchronous = NORMAL; \
             PRAGMA foreign_keys = ON;",
        )?;
        Ok(conn)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn the_live_dataset_cannot_be_opened_for_writing() {
            let err = open_read_write(Path::new(
                "/media/x/dev_dev/year26/jul26/stackunderflow-data/store.db",
            ))
            .unwrap_err()
            .to_string();
            assert!(err.contains("READ-ONLY for this campaign"), "{err}");
        }

        #[test]
        fn a_scratch_path_opens_with_the_stores_pragmas() {
            let dir =
                std::env::temp_dir().join(format!("stax-ingest-guard-{}", std::process::id()));
            let path = dir.join("store.db");
            let conn = open_read_write(&path).unwrap();
            let journal: String = conn
                .query_row("PRAGMA journal_mode", [], |r| r.get(0))
                .unwrap();
            assert_eq!(journal, "wal");
            let fk: i64 = conn
                .query_row("PRAGMA foreign_keys", [], |r| r.get(0))
                .unwrap();
            assert_eq!(fk, 1, "the partitions' REFERENCES must be enforced");
            drop(conn);
            let _ = std::fs::remove_dir_all(&dir);
        }
    }
}

#[cfg(test)]
pub(crate) mod testdb;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unchanged_file_is_skipped_and_a_grown_one_tail_reads() {
        let conn = testdb::store();
        let clock = FixedClock::new(1_700_000_100.0, "2026-07-31T00:00:00+00:00");
        let adapters: Vec<Box<dyn SourceAdapter>> =
            vec![Box::new(testdb::FakeAdapter::new_with_ref(
                "claude",
                testdb::session_ref("claude", "-a-proj", "s1", 1_700_000_000.0, 240),
                vec![
                    testdb::record(0, "2026-04-01T00:00:00Z"),
                    testdb::record(120, "2026-04-01T00:01:00Z"),
                ],
            ))];

        let first = run_ingest(
            &conn,
            &adapters,
            &testdb::ctx(),
            &clock,
            &ReindexConfig::default(),
        )
        .unwrap();
        assert_eq!(first.counts, [("claude".to_string(), 2)]);
        assert_eq!(first.files_processed, 1);
        assert_eq!(first.files_skipped, 0);
        assert_eq!(first.touched_slugs, ["-a-proj"]);

        // Same (mtime, size) → the fast path, and the writer is never entered.
        let second = run_ingest(
            &conn,
            &adapters,
            &testdb::ctx(),
            &clock,
            &ReindexConfig::default(),
        )
        .unwrap();
        assert_eq!(second.files_skipped, 1);
        assert_eq!(second.files_processed, 0);
        assert!(second.counts.is_empty(), "a skipped ref adds no counts key");
        assert_eq!(testdb::count(&conn, "messages"), 2);
    }

    #[test]
    fn a_grown_file_resumes_from_the_stored_offset() {
        let conn = testdb::store();
        let clock = FixedClock::new(1_700_000_100.0, "2026-07-31T00:00:00+00:00");
        let session = testdb::session_ref("claude", "-a-proj", "s1", 1_700_000_000.0, 240);
        let adapters: Vec<Box<dyn SourceAdapter>> =
            vec![Box::new(testdb::FakeAdapter::new_with_ref(
                "claude",
                session,
                vec![
                    testdb::record(0, "2026-04-01T00:00:00Z"),
                    testdb::record(120, "2026-04-01T00:01:00Z"),
                ],
            ))];
        run_ingest(
            &conn,
            &adapters,
            &testdb::ctx(),
            &clock,
            &ReindexConfig::default(),
        )
        .unwrap();

        // The file grew: two more lines, and the adapter honours since_offset.
        let grown = testdb::session_ref("claude", "-a-proj", "s1", 1_700_000_500.0, 480);
        let adapters: Vec<Box<dyn SourceAdapter>> =
            vec![Box::new(testdb::FakeAdapter::new_with_ref(
                "claude",
                grown,
                vec![
                    testdb::record(0, "2026-04-01T00:00:00Z"),
                    testdb::record(120, "2026-04-01T00:01:00Z"),
                    testdb::record(240, "2026-04-01T00:02:00Z"),
                    testdb::record(360, "2026-04-01T00:03:00Z"),
                ],
            ))];
        let report = run_ingest(
            &conn,
            &adapters,
            &testdb::ctx(),
            &clock,
            &ReindexConfig::default(),
        )
        .unwrap();
        assert_eq!(
            report.counts,
            [("claude".to_string(), 2)],
            "only the new lines"
        );
        assert_eq!(testdb::count(&conn, "messages"), 4);
        let offset: i64 = conn
            .query_row("SELECT processed_offset FROM ingest_log", [], |r| r.get(0))
            .unwrap();
        assert_eq!(offset, 360);
    }

    #[test]
    fn a_truncated_file_is_reparsed_from_zero() {
        let conn = testdb::store();
        let clock = FixedClock::new(1_700_000_100.0, "2026-07-31T00:00:00+00:00");
        let adapters: Vec<Box<dyn SourceAdapter>> =
            vec![Box::new(testdb::FakeAdapter::new_with_ref(
                "claude",
                testdb::session_ref("claude", "-a-proj", "s1", 1_700_000_000.0, 240),
                vec![
                    testdb::record(0, "2026-04-01T00:00:00Z"),
                    testdb::record(120, "2026-04-01T00:01:00Z"),
                ],
            ))];
        run_ingest(
            &conn,
            &adapters,
            &testdb::ctx(),
            &clock,
            &ReindexConfig::default(),
        )
        .unwrap();

        // Rotated: smaller than the logged size. The surviving line keeps its
        // timestamp, which is what a real rotation looks like.
        let adapters: Vec<Box<dyn SourceAdapter>> =
            vec![Box::new(testdb::FakeAdapter::new_with_ref(
                "claude",
                testdb::session_ref("claude", "-a-proj", "s1", 1_700_000_900.0, 60),
                vec![testdb::record(0, "2026-04-01T00:00:00Z")],
            ))];
        let report = run_ingest(
            &conn,
            &adapters,
            &testdb::ctx(),
            &clock,
            &ReindexConfig::default(),
        )
        .unwrap();
        assert_eq!(report.files_reparsed, 1);
        // seq 0 already exists for this session IN THIS PARTITION, so the
        // re-read is absorbed by UNIQUE (session_fk, seq).
        //
        // "in this partition" is load-bearing and is a v008 property, not a port
        // one: the UNIQUE index lives on each `messages_YYYYMM` table, so the
        // *same* (session_fk, seq) re-read under a timestamp that moved to a new
        // month lands as a second row. Measured on both sides; recorded in the
        // ledger rather than silently normalised away.
        assert_eq!(testdb::count(&conn, "messages"), 2);
        assert_eq!(
            testdb::count(&conn, "ingest_log"),
            1,
            "the row was re-created"
        );
    }

    #[test]
    fn a_database_kind_ref_resumes_by_rowid_through_the_whole_pass() {
        // The `(file_path, session_id)` key and the `last_rowid` watermark, end
        // to end — a single .vscdb hosting many conversations is why they exist.
        let conn = testdb::store();
        let clock = FixedClock::new(1_700_000_100.0, "2026-07-31T00:00:00+00:00");
        let adapters: Vec<Box<dyn SourceAdapter>> = vec![Box::new(testdb::FakeDbAdapter(
            testdb::FakeAdapter::new_with_ref(
                "cursor",
                testdb::session_ref("cursor", "-a-proj", "s1", 1_700_000_000.0, 4096),
                vec![
                    testdb::record(11, "2026-04-01T00:00:00Z"),
                    testdb::record(12, "2026-04-01T00:01:00Z"),
                ],
            ),
        ))];
        let report = run_ingest(
            &conn,
            &adapters,
            &testdb::ctx(),
            &clock,
            &ReindexConfig::default(),
        )
        .unwrap();
        assert_eq!(report.counts, [("cursor".to_string(), 2)]);
        let (kind, offset, rowid, session_id): (String, Option<i64>, Option<i64>, Option<String>) =
            conn.query_row(
                "SELECT storage_kind, processed_offset, last_rowid, session_id FROM ingest_log",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!(kind, "database");
        assert_eq!(offset, None);
        assert_eq!(rowid, Some(12));
        assert_eq!(
            session_id.as_deref(),
            Some("s1"),
            "keyed per session, not per file"
        );

        // Unchanged (mtime, size) → skipped, as for a file ref.
        let second = run_ingest(
            &conn,
            &adapters,
            &testdb::ctx(),
            &clock,
            &ReindexConfig::default(),
        )
        .unwrap();
        assert_eq!(second.files_skipped, 1);
    }

    #[test]
    fn a_ref_whose_provider_is_not_registered_is_a_hard_error() {
        // Python raises KeyError here and does not catch it: a silently dropped
        // file is worse than a loud stop.
        let conn = testdb::store();
        let clock = FixedClock::new(1_700_000_100.0, "2026-07-31T00:00:00+00:00");
        let adapters: Vec<Box<dyn SourceAdapter>> =
            vec![Box::new(testdb::FakeAdapter::new_with_ref(
                "claude",
                // The adapter is named "claude" but hands back a ref claiming
                // "ghost" — the registry and the enumeration disagree.
                testdb::session_ref("ghost", "-a-proj", "s1", 1_700_000_000.0, 240),
                vec![testdb::record(0, "2026-04-01T00:00:00Z")],
            ))];
        let err = run_ingest(
            &conn,
            &adapters,
            &testdb::ctx(),
            &clock,
            &ReindexConfig::default(),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("No adapter registered for provider"), "{err}");
    }

    #[test]
    fn counts_carry_a_zero_for_a_changed_file_that_yielded_nothing() {
        // `counts[provider] = counts.get(provider, 0) + added` runs for every
        // ref that was NOT skipped, so a changed-but-empty file is a 0 entry —
        // which is different from being absent.
        let conn = testdb::store();
        let clock = FixedClock::new(1_700_000_100.0, "2026-07-31T00:00:00+00:00");
        let adapters: Vec<Box<dyn SourceAdapter>> =
            vec![Box::new(testdb::FakeAdapter::new_with_ref(
                "claude",
                testdb::session_ref("claude", "-a-proj", "s1", 1_700_000_000.0, 240),
                vec![],
            ))];
        let report = run_ingest(
            &conn,
            &adapters,
            &testdb::ctx(),
            &clock,
            &ReindexConfig::default(),
        )
        .unwrap();
        assert_eq!(report.counts, [("claude".to_string(), 0)]);
        assert!(report.touched_slugs.is_empty(), "0 added touches nothing");
        assert_eq!(testdb::count(&conn, "projects"), 0);
    }

    #[test]
    fn the_pass_ends_with_the_post_ingest_hooks_and_the_reindex_seam() {
        let conn = testdb::store();
        let clock = FixedClock::new(1_700_000_100.0, "2026-07-31T00:00:00+00:00");
        // `codex`, deliberately: `run_ingest` reads the LIVE `_claude_home()`,
        // so a fake adapter called `claude` would send this unit test through
        // the developer's real 1.1 GB `~/.claude/projects` (measured: 5.3 s a
        // run). The claude hook's own dispatch is proven against an injected
        // three-line home in `hooks::tests`, and end to end by `ingest-parity.sh`.
        let adapters: Vec<Box<dyn SourceAdapter>> =
            vec![Box::new(testdb::FakeAdapter::new_with_ref(
                "codex",
                testdb::session_ref("codex", "-a-proj", "s1", 1_700_000_000.0, 240),
                vec![testdb::billable_record(0)],
            ))];
        let report = run_ingest(
            &conn,
            &adapters,
            &testdb::ctx(),
            &clock,
            &ReindexConfig::default(),
        )
        .unwrap();
        // The claude hook ran (and is the wave-5/6 stub, so it succeeded silently).
        assert!(
            !report
                .notes
                .iter()
                .any(|n| n.contains("materialize_metadata failed")),
            "{:?}",
            report.notes
        );
        // The reindex walked its full shape and found the project…
        assert_eq!(report.reindex.slugs_indexed, ["-a-proj"]);
        // …and indexed nothing, because wave 6 has registered no sinks.
        assert!(report.reindex.indexed.is_empty());
        assert_eq!(report.events_inserted, 1);
    }
}
