//! A store shaped like a fresh v030 one, plus the fake adapter the ingest tests
//! drive it with.
//!
//! The mart DDL is reused from [`crate::marts::testdb`] rather than re-typed —
//! the writer's post-commit step really does call `refresh_all_marts`, so the
//! eight mart tables have to be here, and two copies of that schema would drift.
//! The five tables the *ingest* layer owns are then replaced with their real
//! v030 shapes (the mart schema carries a cut-down `messages`/`projects`/
//! `sessions`/`usage_events` that has no partitioning, no `message_count`, and
//! none of the columns `insert_event` binds).
//!
//! The partition scaffolding matches what `schema.apply()` leaves on a fresh
//! store, verified against one: `_messages_id_seq` seeded at 1, a
//! `messages_unknown` partition, the `messages` view, and the
//! `messages_insert_route` trigger. The current-month partition a real
//! `schema.apply()` also creates is deliberately absent — the writer must be
//! able to mint it, and a test store that pre-creates every month it will need
//! proves nothing about `ensure_partition`.

use rusqlite::Connection;
use serde_json::json;
use stax_adapters::base::{Record, SessionRef, SourceAdapter, SourceKind, Speed};

use crate::normalize::NormalizeContext;

/// The v030 shapes of the tables the ingest layer writes.
const INGEST_SCHEMA: &str = r"
    DROP TABLE messages;
    DROP TABLE projects;
    DROP TABLE sessions;
    DROP TABLE usage_events;

    CREATE TABLE projects (
        id            INTEGER PRIMARY KEY,
        provider      TEXT NOT NULL,
        slug          TEXT NOT NULL,
        path          TEXT,
        display_name  TEXT NOT NULL,
        first_seen    REAL,
        last_modified REAL,
        UNIQUE (provider, slug));
    CREATE INDEX idx_projects_slug ON projects(slug);

    CREATE TABLE sessions (
        id            INTEGER PRIMARY KEY,
        project_id    INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
        session_id    TEXT NOT NULL,
        message_count INTEGER NOT NULL DEFAULT 0,
        first_ts      TEXT,
        last_ts       TEXT,
        UNIQUE (project_id, session_id));

    CREATE TABLE usage_events (
        id                  INTEGER PRIMARY KEY,
        source_message_fk   INTEGER,
        provider            TEXT NOT NULL,
        account             TEXT NOT NULL DEFAULT 'default',
        project_id          INTEGER NOT NULL,
        session_id          TEXT NOT NULL,
        ts                  TEXT NOT NULL,
        day                 TEXT NOT NULL,
        model               TEXT NOT NULL DEFAULT '',
        speed               TEXT NOT NULL DEFAULT 'standard',
        input_tokens        INTEGER NOT NULL DEFAULT 0,
        output_tokens       INTEGER NOT NULL DEFAULT 0,
        cache_read_tokens   INTEGER NOT NULL DEFAULT 0,
        cache_create_tokens INTEGER NOT NULL DEFAULT 0,
        reasoning_tokens    INTEGER NOT NULL DEFAULT 0,
        cost_usd            REAL NOT NULL DEFAULT 0.0,
        cost_source         TEXT NOT NULL DEFAULT 'rate_card',
        role                TEXT NOT NULL DEFAULT 'assistant',
        raw_extras          TEXT);
    CREATE UNIQUE INDEX uniq_events_msg ON usage_events(source_message_fk);

    CREATE TABLE ingest_log (
        id               INTEGER PRIMARY KEY,
        file_path        TEXT NOT NULL,
        provider         TEXT NOT NULL,
        session_id       TEXT,
        storage_kind     TEXT NOT NULL DEFAULT 'file'
            CHECK (storage_kind IN ('file', 'database')),
        mtime            REAL NOT NULL,
        size             INTEGER NOT NULL,
        processed_offset INTEGER,
        last_rowid       INTEGER,
        last_ingest_ts   REAL,
        UNIQUE (file_path, session_id));
    CREATE UNIQUE INDEX idx_ingest_log_file_unique
        ON ingest_log(file_path) WHERE session_id IS NULL;
    CREATE UNIQUE INDEX idx_ingest_log_session_unique
        ON ingest_log(file_path, session_id) WHERE session_id IS NOT NULL;

    CREATE TABLE _messages_id_seq (
        rowid_kind INTEGER PRIMARY KEY CHECK (rowid_kind = 1),
        next_id    INTEGER NOT NULL);
    INSERT INTO _messages_id_seq (rowid_kind, next_id) VALUES (1, 1);
";

/// A fresh store: mart tables + the real ingest tables + one partition.
pub fn store() -> Connection {
    let conn = Connection::open_in_memory().expect("in-memory store");
    conn.execute_batch(crate::marts::testdb::SCHEMA)
        .expect("mart schema");
    conn.execute_batch(INGEST_SCHEMA).expect("ingest schema");
    // `messages_unknown` exists on a fresh Python store, and creating it here is
    // also what brings the `messages` view into being — `run_ingest`'s
    // `COUNT(*)` probe needs it before the first record lands.
    super::writer::ensure_partition(&conn, "messages_unknown").expect("seed partition");
    conn
}

/// `SELECT COUNT(*) FROM <table>`.
pub fn count(conn: &Connection, table: &str) -> i64 {
    conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
        row.get(0)
    })
    .unwrap_or_else(|err| panic!("counting {table}: {err}"))
}

/// The unprimed pricing context — `etl backfill`'s state (DIV-016).
///
/// Delegates to `normalize::test_support::ctx` rather than locating
/// `models.toml` again: DIV-035's lesson is that a second copy of a shared thing
/// manufactures differences, and a second copy of the rate-card *path* would let
/// the ingest tests price against a manifest the normalizer tests never see.
pub fn ctx() -> NormalizeContext {
    crate::normalize::test_support::ctx()
}

/// A `file`-kind ref pointing at `/tmp/<session_id>.jsonl`.
pub fn session_ref(
    provider: &str,
    slug: &str,
    session_id: &str,
    mtime: f64,
    size: u64,
) -> SessionRef {
    SessionRef::file(
        provider,
        slug,
        session_id,
        format!("/tmp/{session_id}.jsonl"),
        mtime,
        size,
    )
}

/// A user turn: no tokens, so no normalizer yields an event for it.
pub fn record(seq: i64, timestamp: &str) -> Record {
    Record {
        provider: "claude".into(),
        session_id: "s1".into(),
        seq,
        timestamp: timestamp.into(),
        role: "user".into(),
        model: None,
        input_tokens: 0,
        output_tokens: 0,
        cache_create_tokens: 0,
        cache_read_tokens: 0,
        content_text: "hello".into(),
        tools: Vec::new(),
        cwd: None,
        is_sidechain: false,
        uuid: format!("s1:{seq}"),
        parent_uuid: None,
        raw: json!({"seq": seq}),
        speed: Speed::Standard,
    }
}

/// An assistant turn with tokens and a priced model — one `usage_events` row.
pub fn billable_record(seq: i64) -> Record {
    Record {
        role: "assistant".into(),
        model: Some("claude-sonnet-4-5-20250929".into()),
        input_tokens: 1_000,
        output_tokens: 100,
        timestamp: "2026-04-25T00:00:00+00:00".into(),
        ..record(seq, "2026-04-25T00:00:00+00:00")
    }
}

/// An adapter that hands back a fixed ref list and a fixed record list.
///
/// `read_into` honours `since_offset` the way every real adapter does — records
/// at or below the watermark are skipped — so a resume test exercises the
/// writer's contract rather than a mock that always replays everything.
pub struct FakeAdapter {
    name: String,
    refs: Vec<SessionRef>,
    records: Vec<Record>,
}

impl FakeAdapter {
    /// No refs; drives [`crate::ingest::ingest_file`] directly.
    pub fn new(name: &str, records: Vec<Record>) -> Self {
        Self {
            name: name.to_string(),
            refs: Vec::new(),
            records,
        }
    }

    /// One ref, for the `run_ingest` tests.
    pub fn new_with_ref(name: &str, session: SessionRef, records: Vec<Record>) -> Self {
        Self {
            name: name.to_string(),
            refs: vec![session],
            records,
        }
    }

    /// `count` refs and no records — for the enumerate-order tests.
    pub fn with_refs(name: &str, count: usize) -> Self {
        let refs = (0..count)
            .map(|index| session_ref(name, "-a-proj", &format!("s{index}"), 1.0, 10))
            .collect();
        Self {
            name: name.to_string(),
            refs,
            records: Vec::new(),
        }
    }
}

impl SourceAdapter for FakeAdapter {
    fn name(&self) -> &str {
        &self.name
    }

    fn enumerate(&self) -> Vec<SessionRef> {
        self.refs.clone()
    }

    fn read_into(&self, _session: &SessionRef, since_offset: i64, sink: &mut dyn FnMut(Record)) {
        for record in &self.records {
            // "strictly past this seq" — the contract `since_offset == 0` opts
            // out of by meaning "yield everything".
            if since_offset != 0 && record.seq <= since_offset {
                continue;
            }
            sink(record.clone());
        }
    }
}

/// A `database`-kind adapter — resumes by rowid, not byte offset.
pub struct FakeDbAdapter(pub FakeAdapter);

impl SourceAdapter for FakeDbAdapter {
    fn name(&self) -> &str {
        self.0.name()
    }
    fn enumerate(&self) -> Vec<SessionRef> {
        self.0
            .enumerate()
            .into_iter()
            .map(|mut session| {
                session.source_kind = SourceKind::Database;
                session
            })
            .collect()
    }
    fn read_into(&self, session: &SessionRef, since_offset: i64, sink: &mut dyn FnMut(Record)) {
        self.0.read_into(session, since_offset, sink);
    }
}
