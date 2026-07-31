//! The normalize pass — `messages → usage_events` over a whole store.
//!
//! This is the **events half** of `etl/backfill.py`: the streaming join, the
//! per-chunk transaction, the `INSERT OR IGNORE` idempotence, the WAL
//! checkpoint, and the poison-row `except`. It is deliberately not the whole
//! orchestrator — `refresh_all_marts`, `rebuild_from_scratch`, the watermarks
//! and the progress plumbing belong to RS-3-001 and to the mart items, and
//! stubbing them here would put two owners on one file.
//!
//! Everything the events half does is load-bearing for parity:
//!
//! * **The SQL shape.** `messages` is a UNION-ALL view over sixteen monthly
//!   partitions and SQLite does not push join predicates into the arms (§6b).
//!   The `m.id > ? … ORDER BY m.id LIMIT ?` keyset walk is what makes the scan
//!   linear; a `LIMIT/OFFSET` rewrite would re-detonate the July hangs.
//! * **The provider filter.** `tuple(sorted(normalizers))` — twenty keys — so a
//!   provider with no normalizer (antigravity) is filtered in SQL, not skipped
//!   in Python. 412 antigravity rows on the maintainer's store never reach a
//!   normalizer on either side.
//! * **`uniq_events_msg`.** The UNIQUE index on `source_message_fk` is the
//!   idempotence contract: `INSERT OR IGNORE` turns an already-converted
//!   message into a counted *skip*, which is what makes a re-run a no-op and an
//!   interrupted run resumable. [`tests::a_second_pass_inserts_nothing_and_
//!   counts_every_row_as_a_skip`] pins it.
//! * **The poison-row swallow.** A normalizer that raises costs the row, not
//!   the run.

use rusqlite::{Connection, Result as SqlResult, params_from_iter};
use stax_core::queries::pyjson::Value as PyValue;

use super::base::UsageEvent;
use super::row::MsgRow;
use super::{NormalizeContext, Normalizer};

/// `_CHUNK_SIZE` — rows per streamed chunk, and per transaction.
pub const CHUNK_SIZE: i64 = 5_000;

/// `_CHECKPOINT_EVERY_CHUNKS` — fold the WAL back every N chunks so a
/// full-store pass cannot grow it into the gigabytes (observed: 1.5 GB after an
/// interrupted `--force`, which then degraded every reader).
pub const CHECKPOINT_EVERY_CHUNKS: u64 = 5;

/// The columns `_run_normalizers` selects, in its order.
const SELECTED_COLUMNS: [&str; 20] = [
    "id",
    "session_fk",
    "seq",
    "timestamp",
    "role",
    "model",
    "input_tokens",
    "output_tokens",
    "cache_read_tokens",
    "cache_create_tokens",
    "content_text",
    "tools_json",
    "raw_json",
    "is_sidechain",
    "uuid",
    "parent_uuid",
    "speed",
    "session_id",
    "project_id",
    "provider",
];

/// What one pass did — the events half of `BackfillReport`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PassReport {
    /// Rows written to `usage_events`.
    pub events_inserted: u64,
    /// Rows the UNIQUE index rejected because the message was already converted.
    pub events_skipped_duplicate: u64,
    /// `messages` rows streamed.
    pub messages_seen: u64,
    /// Rows whose normalizer raised — silently dropped, counted here because
    /// Python only logs them at DEBUG and a silent drop that nobody counts is
    /// how a parity gap hides.
    pub rows_raised: u64,
}

/// Run the normalize pass over `conn`. Port of `backfill._run_normalizers`.
///
/// # Errors
/// Any SQLite error. A chunk that fails is rolled back; earlier chunks stay
/// committed and the next pass resumes through `uniq_events_msg`, exactly as
/// Python's `except: ROLLBACK; raise` leaves things.
pub fn run(conn: &Connection, ctx: &NormalizeContext) -> SqlResult<PassReport> {
    let registry = super::all();
    let providers = super::registered_providers();
    let mut report = PassReport::default();
    if providers.is_empty() {
        return Ok(report);
    }

    let placeholders = vec!["?"; providers.len()].join(",");
    let select_sql = format!(
        "SELECT m.id            AS id,
                m.session_fk    AS session_fk,
                m.seq           AS seq,
                m.timestamp     AS timestamp,
                m.role          AS role,
                m.model         AS model,
                m.input_tokens  AS input_tokens,
                m.output_tokens AS output_tokens,
                m.cache_read_tokens AS cache_read_tokens,
                m.cache_create_tokens AS cache_create_tokens,
                m.content_text  AS content_text,
                m.tools_json    AS tools_json,
                m.raw_json      AS raw_json,
                m.is_sidechain  AS is_sidechain,
                m.uuid          AS uuid,
                m.parent_uuid   AS parent_uuid,
                m.speed         AS speed,
                s.session_id    AS session_id,
                s.project_id    AS project_id,
                p.provider      AS provider
           FROM messages m
           JOIN sessions s ON s.id = m.session_fk
           JOIN projects p ON p.id = s.project_id
          WHERE p.provider IN ({placeholders})
            AND m.id > ?
          ORDER BY m.id
          LIMIT ?"
    );

    let mut last_id: i64 = 0;
    let mut chunks_done: u64 = 0;

    loop {
        let chunk = fetch_chunk(conn, &select_sql, &providers, last_id)?;
        if chunk.is_empty() {
            break;
        }
        let chunk_len = chunk.len() as i64;

        conn.execute_batch("BEGIN")?;
        let outcome = (|| -> SqlResult<()> {
            for row in &chunk {
                last_id = match row.get("id") {
                    Some(PyValue::Int(id)) => *id,
                    // `int(msg_row["id"])` — the column is INTEGER PRIMARY KEY,
                    // so anything else is impossible; a stall is worse than a
                    // loud stop, so this is a hard error rather than a skip.
                    other => {
                        return Err(rusqlite::Error::InvalidParameterName(format!(
                            "messages.id is not an integer: {other:?}"
                        )));
                    }
                };
                report.messages_seen += 1;

                let provider = super::row::str_or_empty(row, "provider");
                let Some(normalizer) = registry
                    .iter()
                    .find(|(key, _)| *key == provider)
                    .map(|(_, n)| *n)
                else {
                    // Filtered out at the SQL level above, but defensive.
                    continue;
                };

                let (inserted, skipped, raised) = normalize_and_insert(conn, ctx, normalizer, row)?;
                report.events_inserted += inserted;
                report.events_skipped_duplicate += skipped;
                report.rows_raised += raised;
            }
            Ok(())
        })();
        match outcome {
            Ok(()) => conn.execute_batch("COMMIT")?,
            Err(err) => {
                conn.execute_batch("ROLLBACK")?;
                return Err(err);
            }
        }

        chunks_done += 1;
        if chunks_done.is_multiple_of(CHECKPOINT_EVERY_CHUNKS) {
            // PASSIVE never blocks; a busy reader defers it to the next pass.
            // Best-effort on the Python side too, so a hiccup is not fatal.
            let _ = conn.execute_batch("PRAGMA wal_checkpoint(PASSIVE)");
        }

        if chunk_len < CHUNK_SIZE {
            break;
        }
    }

    Ok(report)
}

/// One chunk of joined rows past `last_id`.
fn fetch_chunk(
    conn: &Connection,
    select_sql: &str,
    providers: &[&str],
    last_id: i64,
) -> SqlResult<Vec<MsgRow>> {
    let mut stmt = conn.prepare_cached(select_sql)?;
    let mut binds: Vec<rusqlite::types::Value> = providers
        .iter()
        .map(|p| rusqlite::types::Value::Text((*p).to_string()))
        .collect();
    binds.push(rusqlite::types::Value::Integer(last_id));
    binds.push(rusqlite::types::Value::Integer(CHUNK_SIZE));

    let rows = stmt.query_map(params_from_iter(binds), |row| {
        let mut out = MsgRow::new();
        for (index, name) in SELECTED_COLUMNS.iter().enumerate() {
            out.insert(*name, sqlite_to_py(row.get_ref(index)?));
        }
        Ok(out)
    })?;
    rows.collect()
}

/// The `sqlite3` type mapping Python's driver applies: INTEGER → `int`, REAL →
/// `float`, TEXT → `str`, NULL → `None`.
///
/// BLOB has no `pyjson::Value` home. It is unreachable for these twenty columns
/// on the maintainer's store (measured: `typeof(raw_json)` is `'text'` for all
/// 383,700 rows) and is mapped to a lossy-UTF-8 string, which is what
/// `_safe_load_raw`'s `decode("utf-8", errors="replace")` would have produced
/// had it been reached.
fn sqlite_to_py(value: rusqlite::types::ValueRef<'_>) -> PyValue {
    use rusqlite::types::ValueRef;
    match value {
        ValueRef::Null => PyValue::Null,
        ValueRef::Integer(n) => PyValue::Int(n),
        ValueRef::Real(x) => PyValue::Float(x),
        ValueRef::Text(bytes) | ValueRef::Blob(bytes) => {
            PyValue::Str(String::from_utf8_lossy(bytes).into_owned())
        }
    }
}

/// Dispatch one row and persist what it yields.
/// Returns `(inserted, skipped_duplicate, raised)`.
fn normalize_and_insert(
    conn: &Connection,
    ctx: &NormalizeContext,
    normalizer: &dyn Normalizer,
    row: &MsgRow,
) -> SqlResult<(u64, u64, u64)> {
    // `except Exception: return 0, 0` — never let a poison row stop the run.
    let Ok(events) = normalizer.normalize(ctx, row) else {
        return Ok((0, 0, 1));
    };
    let mut inserted = 0;
    let mut skipped = 0;
    for event in &events {
        if insert_event(conn, row, event)? {
            inserted += 1;
        } else {
            skipped += 1;
        }
    }
    Ok((inserted, skipped, 0))
}

/// Port of `ingest.writer.normalize_and_insert_event`. `true` when the row was
/// written, `false` when `uniq_events_msg` rejected it as a duplicate.
///
/// Every bind reproduces the writer's `event.get(k) or msg_row.get(k) or …`
/// chain. The `or`s are Python truthiness, so a `0` token count falls back the
/// same way a missing one does — which is invisible here (both sides are 0) and
/// would not be for a column where zero is meaningful.
fn insert_event(conn: &Connection, row: &MsgRow, event: &UsageEvent) -> SqlResult<bool> {
    use rusqlite::types::Value as SqlValue;

    let provider = first_truthy_str(&[&event.provider, &super::row::str_or_empty(row, "provider")]);
    let account = if event.account.is_empty() {
        "default".to_string()
    } else {
        event.account.clone()
    };
    let session_id = first_truthy_str(&[
        &event.session_id,
        &super::row::str_or_empty(row, "session_id"),
    ]);
    let ts = first_truthy_str(&[&event.ts, &super::row::str_or_empty(row, "timestamp")]);
    let day = if event.day.is_empty() {
        day_of(&ts)
    } else {
        event.day.clone()
    };
    let model = first_truthy_str(&[&event.model, &super::row::str_or_empty(row, "model")]);
    let speed = {
        let candidate = first_truthy_str(&[&event.speed, &super::row::str_or_empty(row, "speed")]);
        if candidate.is_empty() {
            "standard".to_string()
        } else {
            candidate
        }
    };
    // `event.get("project_id") or msg_row.get("project_id")` — no coercion on
    // either side, so the value's Python type reaches the bind.
    let project_id = if event.project_id.is_truthy() {
        py_to_sqlite(&event.project_id)
    } else {
        py_to_sqlite(row.get("project_id").unwrap_or(&PyValue::Null))
    };
    let source_fk = py_to_sqlite(row.get("id").unwrap_or(&PyValue::Null));
    let role = first_truthy_str(&[&event.role, &super::row::str_or_empty(row, "role")]);

    let mut stmt = conn.prepare_cached(
        "INSERT OR IGNORE INTO usage_events (
            source_message_fk, provider, account, project_id,
            session_id, ts, day, model, speed,
            input_tokens, output_tokens,
            cache_read_tokens, cache_create_tokens, reasoning_tokens,
            cost_usd, cost_source, role, raw_extras
        ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )?;
    let changed = stmt.execute(rusqlite::params![
        source_fk,
        provider,
        account,
        project_id,
        session_id,
        ts,
        day,
        model,
        speed,
        event.input_tokens,
        event.output_tokens,
        event.cache_read_tokens,
        event.cache_create_tokens,
        event.reasoning_tokens,
        event.cost_usd,
        event.cost_source.as_str(),
        role,
        event
            .raw_extras
            .as_ref()
            .map_or(SqlValue::Null, |text| SqlValue::Text(text.clone())),
    ])?;
    Ok(changed > 0)
}

/// `a or b or ""` over strings.
fn first_truthy_str(candidates: &[&String]) -> String {
    candidates
        .iter()
        .find(|text| !text.is_empty())
        .map_or_else(String::new, |text| (*text).clone())
}

/// `writer._day_of` — the cheap slice ONLY, no `fromisoformat` fallback. It is
/// a *different* function from `normalize.base._day_from_ts` and is written to
/// be, so the two are ported separately rather than shared.
fn day_of(ts: &str) -> String {
    let chars: Vec<char> = ts.chars().collect();
    if chars.len() < 10 {
        return String::new();
    }
    if chars[4] == '-' && chars[7] == '-' {
        chars[..10].iter().collect()
    } else {
        String::new()
    }
}

fn py_to_sqlite(value: &PyValue) -> rusqlite::types::Value {
    use rusqlite::types::Value as SqlValue;
    match value {
        PyValue::Null => SqlValue::Null,
        PyValue::Bool(b) => SqlValue::Integer(i64::from(*b)),
        PyValue::Int(n) => SqlValue::Integer(*n),
        PyValue::Float(x) => SqlValue::Real(*x),
        PyValue::Str(text) => SqlValue::Text(text.clone()),
        // A list or dict has no SQLite representation; `sqlite3` raises
        // `InterfaceError`. Unreachable for `id` / `project_id`.
        other => SqlValue::Text(super::row::py_repr(other)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::normalize::test_support::ctx;

    /// The minimal slice of schema v030 the pass touches.
    fn store() -> Connection {
        let conn = Connection::open_in_memory().expect("in-memory store");
        conn.execute_batch(
            "CREATE TABLE projects (id INTEGER PRIMARY KEY, provider TEXT NOT NULL);
             CREATE TABLE sessions (id INTEGER PRIMARY KEY, session_id TEXT NOT NULL,
                                    project_id INTEGER NOT NULL);
             CREATE TABLE messages (
                id INTEGER PRIMARY KEY, session_fk INTEGER, seq INTEGER,
                timestamp TEXT, role TEXT, model TEXT,
                input_tokens INTEGER DEFAULT 0, output_tokens INTEGER DEFAULT 0,
                cache_read_tokens INTEGER DEFAULT 0, cache_create_tokens INTEGER DEFAULT 0,
                content_text TEXT DEFAULT '', tools_json TEXT DEFAULT '[]', raw_json TEXT,
                is_sidechain INTEGER DEFAULT 0, uuid TEXT, parent_uuid TEXT,
                speed TEXT DEFAULT 'standard');
             CREATE TABLE usage_events (
                id INTEGER PRIMARY KEY, source_message_fk INTEGER NOT NULL,
                provider TEXT NOT NULL, account TEXT NOT NULL DEFAULT 'default',
                project_id INTEGER NOT NULL, session_id TEXT NOT NULL,
                ts TEXT NOT NULL, day TEXT NOT NULL, model TEXT NOT NULL DEFAULT '',
                speed TEXT NOT NULL DEFAULT 'standard',
                input_tokens INTEGER NOT NULL DEFAULT 0,
                output_tokens INTEGER NOT NULL DEFAULT 0,
                cache_read_tokens INTEGER NOT NULL DEFAULT 0,
                cache_create_tokens INTEGER NOT NULL DEFAULT 0,
                cost_usd REAL NOT NULL DEFAULT 0.0,
                cost_source TEXT NOT NULL DEFAULT 'rate_card',
                role TEXT NOT NULL, raw_extras TEXT,
                reasoning_tokens INTEGER NOT NULL DEFAULT 0);
             CREATE UNIQUE INDEX uniq_events_msg ON usage_events(source_message_fk);
             INSERT INTO projects VALUES (1, 'claude'), (2, 'antigravity');
             INSERT INTO sessions VALUES (1, 'sess-a', 1), (2, 'sess-b', 2);
             INSERT INTO messages (id, session_fk, seq, timestamp, role, model,
                                   input_tokens, output_tokens)
               VALUES (1, 1, 0, '2026-04-25T00:00:00+00:00', 'assistant',
                       'claude-sonnet-4-5-20250929', 1000, 100),
                      (2, 1, 1, '2026-04-25T00:00:01+00:00', 'user', NULL, 0, 0),
                      (3, 1, 2, '2026-04-26T00:00:00+00:00', 'assistant',
                       'claude-sonnet-4-5-20250929', 0, 0),
                      (4, 2, 0, '2026-04-25T00:00:00+00:00', 'assistant', 'gemini-3-pro', 5, 5);",
        )
        .expect("schema");
        conn
    }

    #[test]
    fn only_billable_rows_of_registered_providers_become_events() {
        let conn = store();
        let report = run(&conn, &ctx()).expect("pass");
        // id 1 only: 2 is a user turn, 3 is all-zero, 4 is antigravity (no
        // normalizer, filtered in SQL).
        assert_eq!(report.events_inserted, 1);
        assert_eq!(report.messages_seen, 3, "antigravity never streams");
        assert_eq!(report.rows_raised, 0);
        let (fk, cost): (i64, f64) = conn
            .query_row(
                "SELECT source_message_fk, cost_usd FROM usage_events",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .expect("one event");
        assert_eq!(fk, 1);
        assert!(cost > 0.0);
    }

    #[test]
    fn a_second_pass_inserts_nothing_and_counts_every_row_as_a_skip() {
        let conn = store();
        let first = run(&conn, &ctx()).expect("first pass");
        let second = run(&conn, &ctx()).expect("second pass");
        assert_eq!(second.events_inserted, 0);
        assert_eq!(second.events_skipped_duplicate, first.events_inserted);
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM usage_events", [], |r| r.get(0))
            .expect("count");
        assert_eq!(count, 1, "uniq_events_msg is the idempotence contract");
    }

    #[test]
    fn the_keyset_walk_crosses_chunk_boundaries_without_losing_or_repeating_a_row() {
        let conn = store();
        // More rows than one chunk, so `m.id > ?` has to carry the cursor.
        let rows = CHUNK_SIZE + 17;
        {
            let mut stmt = conn
                .prepare(
                    "INSERT INTO messages (id, session_fk, seq, timestamp, role, model,
                                           input_tokens, output_tokens)
                     VALUES (?, 1, ?, '2026-05-01T00:00:00+00:00', 'assistant',
                             'claude-sonnet-4-5-20250929', 10, 1)",
                )
                .expect("prepare");
            for i in 0..rows {
                stmt.execute(rusqlite::params![100 + i, i]).expect("insert");
            }
        }
        let report = run(&conn, &ctx()).expect("pass");
        assert_eq!(report.events_inserted, (rows + 1) as u64);
        let distinct: i64 = conn
            .query_row(
                "SELECT COUNT(DISTINCT source_message_fk) FROM usage_events",
                [],
                |r| r.get(0),
            )
            .expect("count");
        assert_eq!(distinct, rows + 1);
    }

    #[test]
    fn a_poison_row_costs_the_row_not_the_run() {
        let conn = store();
        // A TEXT token column on an INTEGER-declared table: SQLite's type
        // affinity keeps 'abc' as TEXT, `int('abc')` raises, and the row is
        // dropped without stopping the pass.
        conn.execute(
            "INSERT INTO messages (id, session_fk, seq, timestamp, role, model, input_tokens)
             VALUES (99, 1, 9, '2026-04-25T00:00:00+00:00', 'assistant',
                     'claude-sonnet-4-5-20250929', 'abc')",
            [],
        )
        .expect("insert poison");
        let report = run(&conn, &ctx()).expect("pass survives");
        assert_eq!(report.rows_raised, 1);
        assert_eq!(report.events_inserted, 1, "the healthy row still lands");
    }

    #[test]
    fn day_of_is_the_cheap_slice_only() {
        assert_eq!(day_of("2026-04-25T00:00:00+00:00"), "2026-04-25");
        assert_eq!(day_of("2026-04-25"), "2026-04-25");
        assert_eq!(day_of("20260425T000000"), "", "no fromisoformat fallback");
        assert_eq!(day_of("short"), "");
    }
}
