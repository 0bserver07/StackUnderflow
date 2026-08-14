//! Port of `stackunderflow/ingest/writer.py` — one file → one transaction →
//! one `ingest_log` row.
//!
//! # Rows follow records, not refs (commit 232ac37 — CURRENT contract)
//!
//! [`ingest_file`] creates the `projects` / `sessions` rows on the **first
//! record the adapter yields**, never before. Enumerating a file is not
//! evidence that it holds a conversation: an adapter names a project from
//! whatever metadata the source hands it (a Codex rollout with no `cwd` falls
//! back to a synthetic `codex-<uuid>` slug), and a file that then reads out
//! empty used to leave that project + session behind forever — the `ingest_log`
//! row marks even a zero-record file processed, and the enumerate pass skips
//! unchanged files, so nothing ever came back to clean up. Deferring the upsert
//! keeps the skip-unchanged fast path intact while making "no records" mean "no
//! rows". A `SessionRef` is a claim that a file exists; a `Record` is evidence
//! there is something in it, and only evidence creates rows.
//!
//! # The `count_added` mart gate (commit 49d9798 — CURRENT contract)
//!
//! The post-commit [`crate::marts::watermark::refresh_all_marts`] is gated on
//! **messages inserted**, not on events inserted. A messages-only ingest — a
//! provider whose normalizer drops rows (`codex` with `model=None`) or has none
//! at all (`antigravity`) — inserts zero usage events, and the old gate meant
//! the CLI/API ingest path then seeded no marts whatsoever for it. Message
//! inserts matter on their own: the dims pass and the coverage seed both read
//! `messages`.
//!
//! # v008 partitioning
//!
//! `messages` is a UNION-ALL view over per-month `messages_YYYYMM` partition
//! tables. Writes route through [`partition_for`] and land in the matching
//! partition; the per-row id comes from the global `_messages_id_seq` so ids
//! stay unique across partitions (preserving `usage_events.uniq_events_msg`).
//! A timestamp in a month with no partition yet creates the table + indexes and
//! rebuilds the view **inside the same per-file transaction**.
//!
//! # Deviation: diagnostics are returned, not logged
//!
//! Python calls `_log.warning` / `_log.debug` on the three swallowed-failure
//! paths. This workspace has no logging facade and adding one for three call
//! sites would be a dependency decision the campaign has not taken, so the
//! messages land in [`FileReport::notes`] instead. Same information, same
//! non-fatal semantics, and a test can assert on it — which is strictly more
//! than a `debug` line nobody reads gives you.

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params_from_iter};
use stax_adapters::base::{Record, SessionRef, SourceAdapter, SourceKind};

use super::pyraw;
use super::{Clock, guard};
use crate::normalize::{self, MsgRow, NormalizeContext};

/// Columns every partition table exposes — kept in sync with the migration's
/// `_PARTITION_COLUMNS`.
///
/// Duplicated from the migration on the Python side too (the migration module
/// is loaded by pathname, not as a package member). Future schema additions to
/// `messages` must update both lists, every existing partition table, and the
/// view rebuild.
pub const PARTITION_COLUMNS: [&str; 17] = [
    "id",
    "session_fk",
    "seq",
    "timestamp",
    "role",
    "model",
    "input_tokens",
    "output_tokens",
    "cache_create_tokens",
    "cache_read_tokens",
    "content_text",
    "tools_json",
    "raw_json",
    "is_sidechain",
    "uuid",
    "parent_uuid",
    "speed",
];

/// Default literals for the INSTEAD OF INSERT trigger — kept in sync with the
/// migration's identically-named map.
///
/// NOT NULL + DEFAULT columns need an explicit `COALESCE` in the trigger
/// because `NEW.col` is NULL when the original INSERT didn't supply it.
const COLUMN_DEFAULTS: [(&str, &str); 8] = [
    ("input_tokens", "0"),
    ("output_tokens", "0"),
    ("cache_create_tokens", "0"),
    ("cache_read_tokens", "0"),
    ("content_text", "''"),
    ("tools_json", "'[]'"),
    ("is_sidechain", "0"),
    ("speed", "'standard'"),
];

/// `_normalize_new_messages`'s chunk size.
///
/// SQLite has a per-statement parameter limit (~32K by default, 999 on older
/// builds). Ingest batches are typically far under 1K rows; Python caps at 500
/// "just to be paranoid" and so does this.
const NORMALIZE_CHUNK: usize = 500;

/// The `messages → sessions → projects` join the normalizer's row dict comes
/// from. Same shape the watcher and the backfill pass use.
const MSG_JOIN_SQL: &str = "
    SELECT m.id            AS id,
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
";

/// What one [`ingest_file`] call did.
///
/// Python's `ingest_file` returns `None`; `run_ingest` measures its effect with
/// a `SELECT COUNT(*) FROM messages` either side of the call. That measurement
/// is ported as-is (it is what populates the per-provider counts), and this
/// struct is the *additional* detail the swallowed-failure paths would
/// otherwise throw away.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FileReport {
    /// `count_added` — `messages` rows this call inserted.
    pub messages_added: u64,
    /// `usage_events` rows the per-record normalize hook inserted.
    pub events_inserted: u64,
    /// Events `uniq_events_msg` rejected as already-converted.
    pub events_skipped_duplicate: u64,
    /// Rows whose normalizer raised. Python logs these at DEBUG and drops them.
    pub rows_raised: u64,
    /// Whether the marts were refreshed (i.e. `count_added` was non-zero).
    pub marts_refreshed: bool,
    /// The messages Python would have written to the log.
    pub notes: Vec<String>,
}

/// Ingest all new records from `session` in a single transaction.
///
/// For `SourceKind::File` the `ingest_log` row stores `processed_offset =
/// max(seq)` (the byte position of the last yielded line), or `file_size` when
/// the file yielded nothing. For `SourceKind::Database` the row stores
/// `last_rowid = max(record.seq)`, and the next pass resumes from that rowid
/// keyed on `(file_path, session_id)`.
///
/// # Errors
/// Any SQLite error, after the transaction has been rolled back — Python's
/// `except: ROLLBACK; raise`, so the `ingest_log` is left untouched and the file
/// is re-read next pass. The adapter itself cannot fail: `read_into` has no
/// `Result`, which is the Rust half of "enumerate/read never raise".
pub fn ingest_file(
    conn: &Connection,
    adapter: &dyn SourceAdapter,
    session: &SessionRef,
    since_offset: i64,
    ctx: &NormalizeContext,
    clock: &dyn Clock,
) -> Result<FileReport> {
    let mut report = FileReport::default();
    conn.execute_batch("BEGIN")?;
    let outcome = ingest_body(
        conn,
        adapter,
        session,
        since_offset,
        ctx,
        clock,
        &mut report,
    );
    match outcome {
        Ok(()) => conn.execute_batch("COMMIT")?,
        Err(err) => {
            conn.execute_batch("ROLLBACK")?;
            return Err(err);
        }
    }

    // Refresh marts AFTER the per-file commit, so the mart upserts run against
    // fully-committed events. Each mart is watermarked + idempotent on its own —
    // if marts can't refresh (registry empty, SQL error), Python logs at DEBUG
    // and moves on; the next pass catches up.
    if report.messages_added > 0 {
        match crate::marts::watermark::refresh_all_marts(conn, &clock.iso_utc()) {
            Ok(_) => report.marts_refreshed = true,
            Err(err) => report.notes.push(format!(
                "ingest.writer: refresh_all_marts after {} failed: {err}",
                session.provider
            )),
        }
    }
    Ok(report)
}

/// The body of the transaction — split out so `?` can unwind into the ROLLBACK.
fn ingest_body(
    conn: &Connection,
    adapter: &dyn SourceAdapter,
    session: &SessionRef,
    since_offset: i64,
    ctx: &NormalizeContext,
    clock: &dyn Clock,
    report: &mut FileReport,
) -> Result<()> {
    // Deferred until the first yielded record — see the module docs.
    let mut session_fk: Option<i64> = None;
    let mut max_ts: Option<String> = None;
    // The batch's earliest timestamp, for `first_ts`. Writing max into both
    // (the pre-fix shape) collapsed every bulk-ingested session to zero
    // duration: on a first read the whole file is one batch, so
    // `first_ts = COALESCE(first_ts, max)` pinned start == end. Only sessions
    // ingested incrementally from birth (a live watcher) escaped, which is why
    // claude looked fine and codex looked broken.
    let mut min_ts: Option<String> = None;
    // The highest `record.seq` observed in this batch. For both source kinds the
    // semantic on the next ingest is "give me records strictly past this seq" —
    // a rowid for database mode, the byte offset of the last line for file mode.
    let mut max_seq: i64 = since_offset;
    let mut count_added: u64 = 0;
    // The new message rowids, so the post-insert normalize pass only walks the
    // rows this batch added rather than re-scanning the table.
    let mut new_message_ids: Vec<i64> = Vec::new();

    // `adapter.read` is a generator in Python. `read_into` is the streaming half
    // of the Rust contract, and the closure is where the per-record work goes —
    // a `collect()` here would put a 128 MB rollout in memory, which is exactly
    // what the generator exists to avoid.
    let mut pending: Result<()> = Ok(());
    adapter.read_into(session, since_offset, &mut |record: Record| {
        if pending.is_err() {
            return;
        }
        if let Err(err) = handle_record(
            conn,
            session,
            &record,
            &mut session_fk,
            &mut min_ts,
            &mut max_ts,
            &mut max_seq,
            &mut count_added,
            &mut new_message_ids,
        ) {
            pending = Err(err);
        }
    });
    pending?;

    if session_fk.is_none() {
        // Zero records: create nothing. An ALREADY-KNOWN project still gets its
        // `last_modified` bumped — the file did change on disk, and "last
        // active" ordering shouldn't regress just because this pass read nothing
        // out of it. A pure UPDATE matches no row for an unknown project, so no
        // ghost appears.
        conn.execute(
            "UPDATE projects SET last_modified = MAX(last_modified, ?) \
             WHERE provider = ? AND slug = ?",
            rusqlite::params![session.file_mtime, session.provider, session.project_slug],
        )?;
    }

    if count_added > 0 {
        let newest = max_ts.clone().unwrap_or_default();
        let oldest = min_ts.clone().unwrap_or_default();
        // `last_ts` advances to the batch's newest; `first_ts` retreats to the
        // batch's oldest — never the same value unless the batch really is one
        // instant. Empty-string guards on both sides: a record with no
        // parseable timestamp must not pin either bound.
        conn.execute(
            "UPDATE sessions SET message_count = message_count + ?1, \
             last_ts = COALESCE(MAX(COALESCE(last_ts, ''), ?2), last_ts), \
             first_ts = CASE \
               WHEN ?3 = '' THEN first_ts \
               WHEN first_ts IS NULL OR first_ts = '' THEN ?3 \
               ELSE MIN(first_ts, ?3) END \
             WHERE id = ?4",
            rusqlite::params![count_added as i64, newest, oldest, session_fk],
        )?;
    }

    write_ingest_log(conn, session, max_seq, count_added, clock)?;

    // The per-file normalize + insert hook. Converts the messages we just
    // inserted into `usage_events` rows in the same transaction. Idempotent via
    // `uniq_events_msg`; a no-op when the provider has no normalizer.
    //
    // Python wraps this in `except Exception` and logs a warning — never fail
    // ingest because of normalize. The messages stay committed and `etl
    // backfill` recovers the events later.
    if !new_message_ids.is_empty() {
        match normalize_new_messages(conn, ctx, &session.provider, &new_message_ids, report) {
            Ok(()) => {}
            Err(err) => report.notes.push(format!(
                "ingest.writer: normalize failed for {} ({}): {err} — messages still \
                 committed; run `stackunderflow etl backfill` to recover",
                session.provider,
                session.file_path.display()
            )),
        }
    }

    report.messages_added = count_added;
    Ok(())
}

/// One record: lazy upsert on the first, then insert + watermark bookkeeping.
#[allow(
    clippy::too_many_arguments,
    reason = "the loop body's locals, threaded \
    through a closure boundary `read_into` imposes; bundling them into a struct \
    would hide which ones the lazy upsert mutates"
)]
fn handle_record(
    conn: &Connection,
    session: &SessionRef,
    record: &Record,
    session_fk: &mut Option<i64>,
    min_ts: &mut Option<String>,
    max_ts: &mut Option<String>,
    max_seq: &mut i64,
    count_added: &mut u64,
    new_message_ids: &mut Vec<i64>,
) -> Result<()> {
    if session_fk.is_none() {
        // First record of this file: now we know there is something to attach.
        // Both upserts are idempotent, so a resumed read of an already-known
        // file just re-finds the existing rows.
        let project_id = upsert_project(conn, session)?;
        *session_fk = Some(upsert_session(conn, project_id, session)?);
    }
    let fk = session_fk.expect("set immediately above");
    let (changes, msg_id) = insert_message(conn, fk, record)?;
    if changes {
        *count_added += 1;
        if let Some(id) = msg_id {
            new_message_ids.push(id);
        }
        // `record.timestamp > max_ts` is a Python string comparison, which is a
        // code-point-wise ordering — `str::cmp` is byte-wise over UTF-8, and the
        // two agree for every string because UTF-8 preserves code-point order.
        if max_ts
            .as_ref()
            .is_none_or(|current| record.timestamp > *current)
        {
            *max_ts = Some(record.timestamp.clone());
        }
        // Empty timestamps never bound the batch — an unstamped record would
        // otherwise win every min comparison and blank `first_ts`.
        if !record.timestamp.is_empty()
            && min_ts
                .as_ref()
                .is_none_or(|current| record.timestamp < *current)
        {
            *min_ts = Some(record.timestamp.clone());
        }
        if record.seq > *max_seq {
            *max_seq = record.seq;
        }
    }
    Ok(())
}

/// The `ingest_log` upsert — the watermark, per source kind.
fn write_ingest_log(
    conn: &Connection,
    session: &SessionRef,
    max_seq: i64,
    count_added: u64,
    clock: &dyn Clock,
) -> Result<()> {
    let path = stax_core::queries::paths::path_to_string(&session.file_path);
    match session.source_kind {
        SourceKind::Database => {
            // Database-backed sources resume by rowid keyed on (file_path,
            // session_id). The partial unique index covers `session_id IS NOT
            // NULL` rows; `processed_offset` stays NULL.
            conn.execute(
                "INSERT INTO ingest_log \
                 (file_path, provider, session_id, storage_kind, \
                  mtime, size, processed_offset, last_rowid, last_ingest_ts) \
                 VALUES (?, ?, ?, 'database', ?, ?, NULL, ?, ?) \
                 ON CONFLICT(file_path, session_id) WHERE session_id IS NOT NULL \
                 DO UPDATE SET \
                   mtime=excluded.mtime, size=excluded.size, \
                   storage_kind=excluded.storage_kind, \
                   processed_offset=NULL, \
                   last_rowid=excluded.last_rowid, \
                   last_ingest_ts=excluded.last_ingest_ts",
                rusqlite::params![
                    path,
                    session.provider,
                    session.session_id,
                    session.file_mtime,
                    session.file_size as i64,
                    max_seq,
                    clock.unix_seconds(),
                ],
            )?;
        }
        SourceKind::File => {
            // File-backed sources resume from the highest seq observed (= the
            // byte offset of the last yielded line). `session_id` is NULL so a
            // single .jsonl is one ingest_log row regardless of how many
            // sessions live inside it.
            //
            // First-time ingest with no records: store `file_size` so we don't
            // re-scan empty / non-conversational files on every pass.
            let stored_offset = if count_added > 0 {
                max_seq
            } else {
                session.file_size as i64
            };
            conn.execute(
                "INSERT INTO ingest_log \
                 (file_path, provider, session_id, storage_kind, \
                  mtime, size, processed_offset, last_rowid, last_ingest_ts) \
                 VALUES (?, ?, NULL, 'file', ?, ?, ?, NULL, ?) \
                 ON CONFLICT(file_path) WHERE session_id IS NULL \
                 DO UPDATE SET \
                   mtime=excluded.mtime, size=excluded.size, \
                   storage_kind=excluded.storage_kind, \
                   processed_offset=excluded.processed_offset, \
                   last_rowid=NULL, \
                   last_ingest_ts=excluded.last_ingest_ts",
                rusqlite::params![
                    path,
                    session.provider,
                    session.file_mtime,
                    session.file_size as i64,
                    stored_offset,
                    clock.unix_seconds(),
                ],
            )?;
        }
    }
    Ok(())
}

/// `_upsert_project` — find-or-insert, bumping `last_modified` on a hit.
fn upsert_project(conn: &Connection, session: &SessionRef) -> Result<i64> {
    let existing: Option<i64> = conn
        .query_row(
            "SELECT id FROM projects WHERE provider = ? AND slug = ?",
            rusqlite::params![session.provider, session.project_slug],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(id) = existing {
        conn.execute(
            "UPDATE projects SET last_modified = MAX(last_modified, ?) WHERE id = ?",
            rusqlite::params![session.file_mtime, id],
        )?;
        return Ok(id);
    }
    conn.execute(
        "INSERT INTO projects (provider, slug, path, display_name, first_seen, last_modified) \
         VALUES (?, ?, ?, ?, ?, ?)",
        rusqlite::params![
            session.provider,
            session.project_slug,
            // `path=None` on creation: the slug is all the writer knows. The
            // dashboard decodes a filesystem path from the slug when it needs
            // one (`pypath::decode_slug_to_path`).
            rusqlite::types::Null,
            session.project_slug,
            session.file_mtime,
            session.file_mtime,
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

/// `_upsert_session` — find-or-insert on `(project_id, session_id)`.
fn upsert_session(conn: &Connection, project_id: i64, session: &SessionRef) -> Result<i64> {
    let existing: Option<i64> = conn
        .query_row(
            "SELECT id FROM sessions WHERE project_id = ? AND session_id = ?",
            rusqlite::params![project_id, session.session_id],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(id) = existing {
        return Ok(id);
    }
    conn.execute(
        "INSERT INTO sessions (project_id, session_id) VALUES (?, ?)",
        rusqlite::params![project_id, session.session_id],
    )?;
    Ok(conn.last_insert_rowid())
}

/// `_insert_message` — one row into the partition for `record.timestamp`.
///
/// Returns `(inserted, new_id)`. `inserted` is false when `INSERT OR IGNORE`
/// hit the `UNIQUE (session_fk, seq)` conflict; the id reserved for that attempt
/// is then leaked (the sequence keeps moving forward), which Python does too and
/// documents as bounded by the number of duplicate attempts — 0 on a normal run.
fn insert_message(
    conn: &Connection,
    session_fk: i64,
    record: &Record,
) -> Result<(bool, Option<i64>)> {
    let partition = partition_for(&record.timestamp);
    ensure_partition(conn, &partition)?;
    let new_id = next_message_id(conn)?;
    // `partition` is produced by `partition_for` and re-validated by
    // `ensure_partition`, so it is one of `messages_YYYYMM` / `messages_unknown`
    // and cannot carry SQL.
    let sql = format!(
        "INSERT OR IGNORE INTO {partition} (\
           id, session_fk, seq, timestamp, role, model, \
           input_tokens, output_tokens, cache_create_tokens, cache_read_tokens, \
           content_text, tools_json, raw_json, is_sidechain, uuid, parent_uuid, \
           speed\
         ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"
    );
    let changed = conn.execute(
        &sql,
        rusqlite::params![
            new_id,
            session_fk,
            record.seq,
            record.timestamp,
            record.role,
            record.model,
            record.input_tokens,
            record.output_tokens,
            record.cache_create_tokens,
            record.cache_read_tokens,
            record.content_text,
            pyraw::dumps_str_list(&record.tools),
            pyraw::dumps_default(&record.raw),
            i64::from(record.is_sidechain),
            record.uuid,
            record.parent_uuid,
            record.speed.as_str(),
        ],
    )?;
    if changed > 0 {
        Ok((true, Some(new_id)))
    } else {
        Ok((false, None))
    }
}

/// `_partition_for` — the partition table name for an ISO-8601 timestamp.
///
/// `"2026-04-15T…"` → `"messages_202604"`. Empty or malformed falls back to
/// `"messages_unknown"` so no row is ever lost; the writer and the view both
/// treat it as a regular partition.
///
/// **DIV: `str.isdigit()` is Unicode-aware in Python and ASCII-only here.**
/// `"٢٠٢٦-٠٤".isdigit()` is `True`, and `\d` inside a `str` regex matches those
/// code points too, so CPython would mint a partition table named with Arabic-
/// Indic digits where this returns `messages_unknown`. Both sides store the row;
/// they disagree on which table. Unreachable for any ISO-8601 timestamp a source
/// actually writes.
#[must_use]
pub fn partition_for(timestamp: &str) -> String {
    let chars: Vec<char> = timestamp.chars().collect();
    if chars.len() < 7 || chars[4] != '-' {
        return "messages_unknown".to_string();
    }
    let year: String = chars[..4].iter().collect();
    let month: String = chars[5..7].iter().collect();
    if !(is_ascii_digits(&year) && is_ascii_digits(&month)) {
        return "messages_unknown".to_string();
    }
    format!("messages_{year}{month}")
}

fn is_ascii_digits(text: &str) -> bool {
    !text.is_empty() && text.chars().all(|ch| ch.is_ascii_digit())
}

/// `_PARTITION_NAME_RE` — `^messages_(\d{6}|unknown)$`, ASCII digits only.
fn is_valid_partition(name: &str) -> bool {
    let Some(rest) = name.strip_prefix("messages_") else {
        return false;
    };
    rest == "unknown" || (rest.len() == 6 && is_ascii_digits(rest))
}

/// `_ensure_partition` — create the partition + indexes and extend the view.
///
/// Returns `true` when a new partition was created. Cheap on the hot path: one
/// `SELECT FROM sqlite_master` per insert. The DDL + view rebuild only runs on
/// the first insert into a brand-new month.
///
/// # Errors
/// An invalid partition name (impossible via [`partition_for`], checked anyway
/// so the name can never be coerced into arbitrary DDL), or any SQLite error.
pub fn ensure_partition(conn: &Connection, partition: &str) -> Result<bool> {
    if !is_valid_partition(partition) {
        anyhow::bail!("Invalid partition name: {partition:?}");
    }
    let existing: Option<String> = conn
        .query_row(
            "SELECT name FROM sqlite_master WHERE type = 'table' AND name = ?",
            [partition],
            |row| row.get(0),
        )
        .optional()?;
    if existing.is_some() {
        return Ok(false);
    }

    conn.execute_batch(&format!(
        "CREATE TABLE {partition} (
            id                    INTEGER PRIMARY KEY,
            session_fk            INTEGER NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
            seq                   INTEGER NOT NULL,
            timestamp             TEXT NOT NULL,
            role                  TEXT NOT NULL,
            model                 TEXT,
            input_tokens          INTEGER NOT NULL DEFAULT 0,
            output_tokens         INTEGER NOT NULL DEFAULT 0,
            cache_create_tokens   INTEGER NOT NULL DEFAULT 0,
            cache_read_tokens     INTEGER NOT NULL DEFAULT 0,
            content_text          TEXT NOT NULL DEFAULT '',
            tools_json            TEXT NOT NULL DEFAULT '[]',
            raw_json              TEXT NOT NULL,
            is_sidechain          INTEGER NOT NULL DEFAULT 0,
            uuid                  TEXT,
            parent_uuid           TEXT,
            speed                 TEXT NOT NULL DEFAULT 'standard',
            UNIQUE (session_fk, seq)
        );
        CREATE INDEX IF NOT EXISTS idx_{partition}_session_seq ON {partition}(session_fk, seq);
        CREATE INDEX IF NOT EXISTS idx_{partition}_timestamp ON {partition}(timestamp);
        CREATE INDEX IF NOT EXISTS idx_{partition}_model ON {partition}(model);"
    ))?;
    rebuild_messages_view(conn)?;
    rebuild_messages_insert_trigger(conn)?;
    Ok(true)
}

/// `_list_partitions` — every partition table name, sorted.
fn list_partitions(conn: &Connection) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT name FROM sqlite_master WHERE type = 'table' \
         AND (name GLOB 'messages_[0-9][0-9][0-9][0-9][0-9][0-9]' \
              OR name = 'messages_unknown')",
    )?;
    let mut names: Vec<String> = stmt
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<_>>()?;
    names.sort();
    Ok(names)
}

/// `_rebuild_messages_view` — the UNION-ALL view spanning every partition.
///
/// Explicit columns guard against silent drift in any one partition, which is
/// also why the arms cannot be `SELECT *`.
fn rebuild_messages_view(conn: &Connection) -> Result<()> {
    let partitions = list_partitions(conn)?;
    if partitions.is_empty() {
        return Ok(());
    }
    let cols_csv = PARTITION_COLUMNS.join(", ");
    let union_sql = partitions
        .iter()
        .map(|table| format!("SELECT {cols_csv} FROM {table}"))
        .collect::<Vec<_>>()
        .join(" UNION ALL ");
    conn.execute("DROP VIEW IF EXISTS messages", [])?;
    conn.execute(&format!("CREATE VIEW messages AS {union_sql}"), [])?;
    Ok(())
}

/// `_rebuild_messages_insert_trigger` — the INSTEAD OF INSERT router.
///
/// Mirrors the trigger the v008 migration builds so callers that use the
/// `messages` name directly (fixture-seeding tests, ad hoc tooling) keep
/// working. Production writes route through [`insert_message`] and bypass it.
/// Rebuilt on every new partition so the WHEN clauses cover every active month.
fn rebuild_messages_insert_trigger(conn: &Connection) -> Result<()> {
    let partitions = list_partitions(conn)?;
    if partitions.is_empty() {
        return Ok(());
    }

    let cols_csv = PARTITION_COLUMNS.join(", ");
    let base_select = PARTITION_COLUMNS
        .iter()
        .map(|col| {
            if *col == "id" {
                "COALESCE(NEW.id, (SELECT next_id - 1 FROM _messages_id_seq WHERE rowid_kind = 1))"
                    .to_string()
            } else if let Some((_, default)) = COLUMN_DEFAULTS.iter().find(|(name, _)| name == col)
            {
                format!("COALESCE(NEW.{col}, {default})")
            } else {
                format!("NEW.{col}")
            }
        })
        .collect::<Vec<_>>()
        .join(", ");

    let known_months: Vec<&str> = partitions
        .iter()
        .filter(|name| *name != "messages_unknown")
        .map(|name| &name["messages_".len()..])
        .collect();

    let mut inserts: Vec<String> = Vec::new();
    for ym in &known_months {
        let yyyy_mm = format!("{}-{}", &ym[..4], &ym[4..]);
        inserts.push(format!(
            "INSERT OR IGNORE INTO messages_{ym} ({cols_csv}) \
             SELECT {base_select} \
             WHERE substr(NEW.timestamp, 1, 7) = '{yyyy_mm}';"
        ));
    }
    let fallback_where = if known_months.is_empty() {
        "1 = 1".to_string()
    } else {
        let known_list = known_months
            .iter()
            .map(|ym| format!("'{}-{}'", &ym[..4], &ym[4..]))
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "length(NEW.timestamp) < 7 \
             OR substr(NEW.timestamp, 5, 1) <> '-' \
             OR substr(NEW.timestamp, 1, 7) NOT IN ({known_list})"
        )
    };
    inserts.push(format!(
        "INSERT OR IGNORE INTO messages_unknown ({cols_csv}) \
         SELECT {base_select} \
         WHERE {fallback_where};"
    ));

    // DIV-456. `concat!`, not a `\` line continuation: the reference builds this
    // from four adjacent string literals whose 2nd and 3rd carry a LEADING TWO
    // SPACES, and a `\` continuation eats exactly those. The trigger's text is
    // stored verbatim in `sqlite_master`, so the difference is permanent in
    // every store the port's WRITER extended — invisible to `ingest-parity.sh`
    // (which diffs rows) and to `schema-differ.sh` (which never adds a
    // partition, and whose `v008.rs` copy has always had the spaces). The
    // import differ is the first thing that compared `sqlite_master` between two
    // independently WRITTEN stores, and it found this on its first run.
    let bump_sql = concat!(
        "UPDATE _messages_id_seq SET next_id = MAX(",
        "  next_id + (CASE WHEN NEW.id IS NULL THEN 1 ELSE 0 END),",
        "  COALESCE(NEW.id + 1, next_id)",
        ") WHERE rowid_kind = 1;"
    );
    let body = format!("{bump_sql}{}", inserts.join(""));

    conn.execute("DROP TRIGGER IF EXISTS messages_insert_route", [])?;
    conn.execute(
        &format!(
            "CREATE TRIGGER messages_insert_route INSTEAD OF INSERT ON messages BEGIN {body} END"
        ),
        [],
    )?;
    Ok(())
}

/// `_next_message_id` — atomically reserve the next global `messages.id`.
///
/// The caller is already inside a transaction, so the read-then-update pair is
/// serialised against any concurrent writer on the same connection.
///
/// # Errors
/// A missing `_messages_id_seq` row, which would mean the v008 migration never
/// ran. Python raises `RuntimeError` with the same message.
fn next_message_id(conn: &Connection) -> Result<i64> {
    let next: Option<i64> = conn
        .query_row(
            "SELECT next_id FROM _messages_id_seq WHERE rowid_kind = 1",
            [],
            |row| row.get(0),
        )
        .optional()?;
    let Some(next) = next else {
        anyhow::bail!("ingest.writer: _messages_id_seq is missing — run schema.apply() first");
    };
    conn.execute(
        "UPDATE _messages_id_seq SET next_id = next_id + 1 WHERE rowid_kind = 1",
        [],
    )?;
    Ok(next)
}

/// `_normalize_new_messages` — run the registered normalizer over `message_ids`.
///
/// Reads each row back via the standard `messages → sessions → projects` join so
/// the normalizer receives the full row it expects. `INSERT OR IGNORE` against
/// `uniq_events_msg` makes re-runs idempotent — a watcher cycle racing a
/// backfill won't double-insert.
///
/// Silently does nothing when the provider has no normalizer (beta-disabled
/// providers, or a new provider that ships before its normalizer). The
/// `messages` row still lands; the next `etl backfill` picks it up.
fn normalize_new_messages(
    conn: &Connection,
    ctx: &NormalizeContext,
    provider: &str,
    message_ids: &[i64],
    report: &mut FileReport,
) -> Result<()> {
    if message_ids.is_empty() {
        return Ok(());
    }
    let Some(normalizer) = normalize::get(provider) else {
        report.notes.push(format!(
            "ingest.writer: no normalizer registered for provider {provider:?} — skipping \
             (run `stackunderflow etl backfill` to materialise events later if a \
             normalizer is added)"
        ));
        return Ok(());
    };

    for chunk in message_ids.chunks(NORMALIZE_CHUNK) {
        let placeholders = vec!["?"; chunk.len()].join(",");
        let sql = format!("{MSG_JOIN_SQL} WHERE m.id IN ({placeholders}) ORDER BY m.id");
        let mut stmt = conn.prepare(&sql)?;
        let rows: Vec<MsgRow> = stmt
            .query_map(params_from_iter(chunk.iter().copied()), |row| {
                let mut out = MsgRow::new();
                for (index, name) in crate::normalize::pass::SELECTED_COLUMNS.iter().enumerate() {
                    out.insert(
                        *name,
                        crate::normalize::pass::sqlite_to_py(row.get_ref(index)?),
                    );
                }
                Ok(out)
            })?
            .collect::<rusqlite::Result<_>>()?;
        drop(stmt);

        for row in &rows {
            // `except Exception: continue` — a poison row costs the row, not the
            // batch. Python logs it at DEBUG.
            let Ok(events) = normalizer.normalize(ctx, row) else {
                report.rows_raised += 1;
                continue;
            };
            for event in &events {
                if crate::normalize::pass::insert_event(conn, row, event)? {
                    report.events_inserted += 1;
                } else {
                    report.events_skipped_duplicate += 1;
                }
            }
        }
    }
    Ok(())
}

/// The campaign's read-only guard, re-exported here so a caller that only pulls
/// the writer still has it.
pub use guard::open_read_write;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ingest::FixedClock;
    use crate::ingest::testdb;

    #[test]
    fn partition_for_maps_the_month_and_falls_back_on_junk() {
        assert_eq!(partition_for("2026-04-15T10:00:00Z"), "messages_202604");
        assert_eq!(partition_for("2026-04"), "messages_202604");
        assert_eq!(partition_for(""), "messages_unknown");
        assert_eq!(partition_for("2026/04/15"), "messages_unknown");
        assert_eq!(partition_for("abcd-ef-gh"), "messages_unknown");
        // Six chars is one short of the slice Python needs.
        assert_eq!(partition_for("2026-0"), "messages_unknown");
    }

    #[test]
    fn partition_names_are_validated_before_they_reach_ddl() {
        assert!(is_valid_partition("messages_202604"));
        assert!(is_valid_partition("messages_unknown"));
        assert!(!is_valid_partition("messages_2026"));
        assert!(!is_valid_partition("messages_x; DROP TABLE projects"));
        let conn = testdb::store();
        assert!(ensure_partition(&conn, "messages_x; DROP TABLE projects--").is_err());
    }

    #[test]
    fn a_new_month_creates_the_partition_and_extends_the_view() {
        let conn = testdb::store();
        assert!(ensure_partition(&conn, "messages_202604").unwrap());
        assert!(
            !ensure_partition(&conn, "messages_202604").unwrap(),
            "the second call is a no-op"
        );
        assert!(ensure_partition(&conn, "messages_202605").unwrap());
        // The view now spans both, and the trigger routes into both.
        let sql: String = conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type='view' AND name='messages'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(sql.contains("messages_202604"), "{sql}");
        assert!(sql.contains("messages_202605"), "{sql}");
        assert!(sql.contains("UNION ALL"), "{sql}");
        let trigger: String = conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type='trigger' \
                 AND name='messages_insert_route'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(trigger.contains("'2026-04'"), "{trigger}");
        assert!(trigger.contains("messages_unknown"), "{trigger}");
    }

    #[test]
    fn a_file_that_yields_nothing_creates_no_project_and_no_session() {
        // THE 232ac37 CONTRACT. A ref is a claim; a record is evidence.
        let conn = testdb::store();
        let adapter = testdb::FakeAdapter::new("claude", vec![]);
        let session = testdb::session_ref("claude", "-a-proj", "s1", 1_700_000_000.0, 4096);
        let report = ingest_file(
            &conn,
            &adapter,
            &session,
            0,
            &testdb::ctx(),
            &FixedClock::new(1_700_000_100.0, "2026-07-31T00:00:00+00:00"),
        )
        .unwrap();
        assert_eq!(report.messages_added, 0);
        assert_eq!(testdb::count(&conn, "projects"), 0, "no ghost project");
        assert_eq!(testdb::count(&conn, "sessions"), 0, "no ghost session");
        // …but the file IS marked processed, so the enumerate pass skips it.
        assert_eq!(testdb::count(&conn, "ingest_log"), 1);
        let offset: i64 = conn
            .query_row("SELECT processed_offset FROM ingest_log", [], |r| r.get(0))
            .unwrap();
        assert_eq!(offset, 4096, "an empty read stores file_size, not 0");
        assert!(
            !report.marts_refreshed,
            "count_added == 0 gates the refresh"
        );
    }

    #[test]
    fn the_first_record_is_what_mints_the_project_and_session() {
        let conn = testdb::store();
        let adapter =
            testdb::FakeAdapter::new("claude", vec![testdb::record(0, "2026-04-01T00:00:00Z")]);
        let session = testdb::session_ref("claude", "-a-proj", "s1", 1_700_000_000.0, 4096);
        let report = ingest_file(
            &conn,
            &adapter,
            &session,
            0,
            &testdb::ctx(),
            &FixedClock::new(1_700_000_100.0, "2026-07-31T00:00:00+00:00"),
        )
        .unwrap();
        assert_eq!(report.messages_added, 1);
        assert_eq!(testdb::count(&conn, "projects"), 1);
        assert_eq!(testdb::count(&conn, "sessions"), 1);
        assert_eq!(testdb::count(&conn, "messages_202604"), 1);
        // The session counters moved with the batch.
        let (count, first, last): (i64, String, String) = conn
            .query_row(
                "SELECT message_count, first_ts, last_ts FROM sessions",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(count, 1);
        assert_eq!(first, "2026-04-01T00:00:00Z");
        assert_eq!(last, "2026-04-01T00:00:00Z");
    }

    #[test]
    fn re_ingesting_the_same_records_adds_no_rows() {
        let conn = testdb::store();
        let records = vec![
            testdb::record(0, "2026-04-01T00:00:00Z"),
            testdb::record(120, "2026-04-01T00:01:00Z"),
        ];
        let adapter = testdb::FakeAdapter::new("claude", records);
        let session = testdb::session_ref("claude", "-a-proj", "s1", 1_700_000_000.0, 240);
        let clock = FixedClock::new(1_700_000_100.0, "2026-07-31T00:00:00+00:00");
        let first = ingest_file(&conn, &adapter, &session, 0, &testdb::ctx(), &clock).unwrap();
        assert_eq!(first.messages_added, 2);
        // A second pass from offset 0 re-reads the same lines; UNIQUE
        // (session_fk, seq) turns every one into a counted no-op.
        let second = ingest_file(&conn, &adapter, &session, 0, &testdb::ctx(), &clock).unwrap();
        assert_eq!(second.messages_added, 0);
        assert_eq!(testdb::count(&conn, "messages"), 2);
        assert_eq!(testdb::count(&conn, "projects"), 1);
        assert_eq!(testdb::count(&conn, "sessions"), 1);
        assert_eq!(testdb::count(&conn, "ingest_log"), 1);
    }

    #[test]
    fn the_watermark_is_the_last_seq_for_a_file_and_a_rowid_for_a_database() {
        let conn = testdb::store();
        let adapter = testdb::FakeAdapter::new(
            "claude",
            vec![
                testdb::record(0, "2026-04-01T00:00:00Z"),
                testdb::record(940, "2026-04-01T00:01:00Z"),
            ],
        );
        let session = testdb::session_ref("claude", "-a-proj", "s1", 1_700_000_000.0, 1024);
        let clock = FixedClock::new(1_700_000_100.0, "2026-07-31T00:00:00+00:00");
        ingest_file(&conn, &adapter, &session, 0, &testdb::ctx(), &clock).unwrap();
        let (offset, rowid): (i64, Option<i64>) = conn
            .query_row(
                "SELECT processed_offset, last_rowid FROM ingest_log",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(offset, 940, "the byte offset of the LAST yielded line");
        assert_eq!(rowid, None, "file kind leaves last_rowid NULL");

        // The database kind stores the mirror image.
        let mut db_session = testdb::session_ref("cursor", "-a-proj", "s9", 1_700_000_000.0, 1024);
        db_session.source_kind = SourceKind::Database;
        db_session.file_path = "/tmp/state.vscdb".into();
        let db_adapter =
            testdb::FakeAdapter::new("cursor", vec![testdb::record(77, "2026-04-01T00:02:00Z")]);
        ingest_file(&conn, &db_adapter, &db_session, 0, &testdb::ctx(), &clock).unwrap();
        let (offset, rowid): (Option<i64>, Option<i64>) = conn
            .query_row(
                "SELECT processed_offset, last_rowid FROM ingest_log WHERE session_id = 's9'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(offset, None, "database kind leaves processed_offset NULL");
        assert_eq!(rowid, Some(77));
    }

    #[test]
    fn a_resumed_read_across_a_batch_boundary_keeps_the_existing_rows() {
        // The codex model-seeding fix depends on a resumed read finding the
        // session row already there rather than minting a second one.
        let conn = testdb::store();
        let session = testdb::session_ref("claude", "-a-proj", "s1", 1_700_000_000.0, 2048);
        let clock = FixedClock::new(1_700_000_100.0, "2026-07-31T00:00:00+00:00");
        let first =
            testdb::FakeAdapter::new("claude", vec![testdb::record(0, "2026-04-01T00:00:00Z")]);
        ingest_file(&conn, &first, &session, 0, &testdb::ctx(), &clock).unwrap();
        let project_id: i64 = conn
            .query_row("SELECT id FROM projects", [], |r| r.get(0))
            .unwrap();
        let session_pk: i64 = conn
            .query_row("SELECT id FROM sessions", [], |r| r.get(0))
            .unwrap();

        // seq 120 past a watermark of 60: "strictly past this seq" is the
        // contract, so a record AT the watermark would be (correctly) skipped.
        let second =
            testdb::FakeAdapter::new("claude", vec![testdb::record(120, "2026-05-01T00:00:00Z")]);
        let report = ingest_file(&conn, &second, &session, 60, &testdb::ctx(), &clock).unwrap();
        assert_eq!(report.messages_added, 1);
        assert_eq!(testdb::count(&conn, "projects"), 1);
        assert_eq!(testdb::count(&conn, "sessions"), 1);
        assert_eq!(
            conn.query_row("SELECT id FROM projects", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            project_id
        );
        assert_eq!(
            conn.query_row("SELECT id FROM sessions", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            session_pk
        );
        // The May record crossed a month boundary → a second partition, and the
        // ids stayed globally unique across both.
        assert_eq!(testdb::count(&conn, "messages_202604"), 1);
        assert_eq!(testdb::count(&conn, "messages_202605"), 1);
        let ids: i64 = conn
            .query_row("SELECT COUNT(DISTINCT id) FROM messages", [], |r| r.get(0))
            .unwrap();
        assert_eq!(ids, 2);
        let (count, first_ts, last_ts): (i64, String, String) = conn
            .query_row(
                "SELECT message_count, first_ts, last_ts FROM sessions",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(count, 2, "message_count accumulates across batches");
        assert_eq!(
            first_ts, "2026-04-01T00:00:00Z",
            "first_ts is COALESCE-pinned"
        );
        assert_eq!(last_ts, "2026-05-01T00:00:00Z", "last_ts is MAX-advanced");
    }

    #[test]
    fn an_empty_read_of_a_known_project_still_bumps_last_modified() {
        let conn = testdb::store();
        let clock = FixedClock::new(1_700_000_100.0, "2026-07-31T00:00:00+00:00");
        let session = testdb::session_ref("claude", "-a-proj", "s1", 1_700_000_000.0, 2048);
        let adapter =
            testdb::FakeAdapter::new("claude", vec![testdb::record(0, "2026-04-01T00:00:00Z")]);
        ingest_file(&conn, &adapter, &session, 0, &testdb::ctx(), &clock).unwrap();

        let mut later = testdb::session_ref("claude", "-a-proj", "s2", 1_800_000_000.0, 4096);
        later.file_path = "/tmp/s2.jsonl".into();
        let empty = testdb::FakeAdapter::new("claude", vec![]);
        ingest_file(&conn, &empty, &later, 0, &testdb::ctx(), &clock).unwrap();
        let last_modified: f64 = conn
            .query_row("SELECT last_modified FROM projects", [], |r| r.get(0))
            .unwrap();
        assert!(
            (last_modified - 1_800_000_000.0).abs() < f64::EPSILON,
            "the pure UPDATE bumped the KNOWN project: {last_modified}"
        );
        assert_eq!(
            testdb::count(&conn, "sessions"),
            1,
            "…and minted no session"
        );
    }

    #[test]
    fn the_normalize_hook_writes_events_in_the_same_transaction() {
        let conn = testdb::store();
        let adapter = testdb::FakeAdapter::new("claude", vec![testdb::billable_record(0)]);
        let session = testdb::session_ref("claude", "-a-proj", "s1", 1_700_000_000.0, 2048);
        let report = ingest_file(
            &conn,
            &adapter,
            &session,
            0,
            &testdb::ctx(),
            &FixedClock::new(1_700_000_100.0, "2026-07-31T00:00:00+00:00"),
        )
        .unwrap();
        assert_eq!(report.messages_added, 1);
        assert_eq!(report.events_inserted, 1);
        assert_eq!(testdb::count(&conn, "usage_events"), 1);
        let cost: f64 = conn
            .query_row("SELECT cost_usd FROM usage_events", [], |r| r.get(0))
            .unwrap();
        assert!(cost > 0.0, "the pricer ran inside the ingest transaction");
        assert!(
            report.marts_refreshed,
            "count_added > 0 opens the mart gate"
        );
        let daily: i64 = conn
            .query_row("SELECT COUNT(*) FROM daily_mart", [], |r| r.get(0))
            .unwrap();
        assert_eq!(daily, 1, "the post-commit refresh advanced the marts");
    }

    #[test]
    fn a_provider_with_no_normalizer_still_lands_its_messages_and_seeds_marts() {
        // THE 49d9798 CONTRACT: the mart gate is count_added, not events.
        let conn = testdb::store();
        let adapter = testdb::FakeAdapter::new("antigravity", vec![testdb::billable_record(0)]);
        let mut session =
            testdb::session_ref("antigravity", "-a-proj", "s1", 1_700_000_000.0, 2048);
        session.provider = "antigravity".to_string();
        let report = ingest_file(
            &conn,
            &adapter,
            &session,
            0,
            &testdb::ctx(),
            &FixedClock::new(1_700_000_100.0, "2026-07-31T00:00:00+00:00"),
        )
        .unwrap();
        assert_eq!(report.messages_added, 1);
        assert_eq!(report.events_inserted, 0, "antigravity has no normalizer");
        assert!(
            report
                .notes
                .iter()
                .any(|note| note.contains("no normalizer registered")),
            "{:?}",
            report.notes
        );
        assert!(
            report.marts_refreshed,
            "a messages-only ingest still refreshes — the pre-49d9798 gate did not"
        );
    }

    #[test]
    fn a_malformed_timestamp_lands_in_the_unknown_partition() {
        let conn = testdb::store();
        let adapter = testdb::FakeAdapter::new("claude", vec![testdb::record(0, "not-a-date")]);
        let session = testdb::session_ref("claude", "-a-proj", "s1", 1_700_000_000.0, 2048);
        ingest_file(
            &conn,
            &adapter,
            &session,
            0,
            &testdb::ctx(),
            &FixedClock::new(1_700_000_100.0, "2026-07-31T00:00:00+00:00"),
        )
        .unwrap();
        assert_eq!(
            testdb::count(&conn, "messages_unknown"),
            1,
            "no row is ever lost"
        );
    }

    #[test]
    fn raw_json_is_written_with_pythons_separators_and_escapes() {
        let conn = testdb::store();
        let mut record = testdb::record(0, "2026-04-01T00:00:00Z");
        record.raw = serde_json::from_str(r#"{"z":1,"t":"café"}"#).unwrap();
        record.tools = vec!["Bash".to_string(), "Read".to_string()];
        let adapter = testdb::FakeAdapter::new("claude", vec![record]);
        let session = testdb::session_ref("claude", "-a-proj", "s1", 1_700_000_000.0, 2048);
        ingest_file(
            &conn,
            &adapter,
            &session,
            0,
            &testdb::ctx(),
            &FixedClock::new(1_700_000_100.0, "2026-07-31T00:00:00+00:00"),
        )
        .unwrap();
        let (raw, tools): (String, String) = conn
            .query_row("SELECT raw_json, tools_json FROM messages", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!(raw, "{\"z\": 1, \"t\": \"caf\\u00e9\"}");
        assert_eq!(tools, r#"["Bash", "Read"]"#);
    }
}
