"""Transactional writer: one file → one transaction → one ingest_log row."""

from __future__ import annotations

import json
import sqlite3
import time

from stackunderflow.adapters.base import Record, SessionRef, SourceAdapter


def ingest_file(
    conn: sqlite3.Connection,
    adapter: SourceAdapter,
    ref: SessionRef,
    *,
    since_offset: int = 0,
) -> None:
    """Ingest all new records from *ref* in a single transaction.

    Raises whatever the adapter raises; the transaction rolls back and
    the ingest_log is left untouched.

    For ``ref.source_kind == "file"`` the ingest_log row stores
    ``processed_offset = ref.file_size`` (byte position into a JSONL).
    For ``"database"`` the row stores ``last_rowid = max(record.seq)``
    seen in this batch — the next pass resumes from that rowid keyed on
    ``(file_path, session_id)``.
    """
    conn.execute("BEGIN")
    try:
        project_id = _upsert_project(conn, ref)
        session_fk = _upsert_session(conn, project_id, ref)

        max_ts: str | None = None
        # max_seq carries the highest record.seq we observed in this batch.
        # For both source kinds the semantic on the next ingest is "give me
        # records strictly past this seq" — for database mode that's a
        # rowid; for file mode that's the byte offset of the last line.
        max_seq: int = since_offset
        count_added = 0
        for record in adapter.read(ref, since_offset=since_offset):
            changes = _insert_message(conn, session_fk, record)
            if changes:
                count_added += 1
                if max_ts is None or record.timestamp > max_ts:
                    max_ts = record.timestamp
                if record.seq > max_seq:
                    max_seq = record.seq

        if count_added:
            conn.execute(
                "UPDATE sessions SET message_count = message_count + ?, "
                "                     last_ts = COALESCE(MAX(COALESCE(last_ts, ''), ?), last_ts), "
                "                     first_ts = COALESCE(first_ts, ?) "
                "WHERE id = ?",
                (count_added, max_ts or "", max_ts or "", session_fk),
            )

        if ref.source_kind == "database":
            # Database-backed sources resume by rowid keyed on (file_path,
            # session_id). The partial unique index covers session_id IS
            # NOT NULL rows; processed_offset stays NULL.
            conn.execute(
                "INSERT INTO ingest_log "
                "(file_path, provider, session_id, storage_kind, "
                " mtime, size, processed_offset, last_rowid, last_ingest_ts) "
                "VALUES (?, ?, ?, 'database', ?, ?, NULL, ?, ?) "
                "ON CONFLICT(file_path, session_id) WHERE session_id IS NOT NULL "
                "DO UPDATE SET "
                "  mtime=excluded.mtime, size=excluded.size, "
                "  storage_kind=excluded.storage_kind, "
                "  processed_offset=NULL, "
                "  last_rowid=excluded.last_rowid, "
                "  last_ingest_ts=excluded.last_ingest_ts",
                (
                    str(ref.file_path),
                    ref.provider,
                    ref.session_id,
                    ref.file_mtime,
                    ref.file_size,
                    max_seq,
                    time.time(),
                ),
            )
        else:
            # File-backed sources resume from the highest seq observed
            # (= byte offset of the last yielded line). session_id is NULL
            # so a single .jsonl is one ingest_log row regardless of how
            # many sessions live inside it. The partial unique index on
            # file_path WHERE session_id IS NULL is the conflict target.
            #
            # First-time ingest with no records: store the file_size so we
            # don't re-scan empty/non-conversational files on every pass.
            stored_offset = max_seq if count_added else ref.file_size
            conn.execute(
                "INSERT INTO ingest_log "
                "(file_path, provider, session_id, storage_kind, "
                " mtime, size, processed_offset, last_rowid, last_ingest_ts) "
                "VALUES (?, ?, NULL, 'file', ?, ?, ?, NULL, ?) "
                "ON CONFLICT(file_path) WHERE session_id IS NULL "
                "DO UPDATE SET "
                "  mtime=excluded.mtime, size=excluded.size, "
                "  storage_kind=excluded.storage_kind, "
                "  processed_offset=excluded.processed_offset, "
                "  last_rowid=NULL, "
                "  last_ingest_ts=excluded.last_ingest_ts",
                (
                    str(ref.file_path),
                    ref.provider,
                    ref.file_mtime,
                    ref.file_size,
                    stored_offset,
                    time.time(),
                ),
            )
        conn.execute("COMMIT")
    except Exception:
        conn.execute("ROLLBACK")
        raise


def _upsert_project(conn: sqlite3.Connection, ref: SessionRef) -> int:
    row = conn.execute(
        "SELECT id FROM projects WHERE provider = ? AND slug = ?",
        (ref.provider, ref.project_slug),
    ).fetchone()
    if row:
        conn.execute(
            "UPDATE projects SET last_modified = MAX(last_modified, ?) WHERE id = ?",
            (ref.file_mtime, row["id"]),
        )
        return row["id"]
    cur = conn.execute(
        "INSERT INTO projects (provider, slug, path, display_name, first_seen, last_modified) "
        "VALUES (?, ?, ?, ?, ?, ?)",
        (
            ref.provider,
            ref.project_slug,
            None,
            ref.project_slug,
            ref.file_mtime,
            ref.file_mtime,
        ),
    )
    assert cur.lastrowid is not None  # noqa: S101
    return cur.lastrowid


def _upsert_session(conn: sqlite3.Connection, project_id: int, ref: SessionRef) -> int:
    row = conn.execute(
        "SELECT id FROM sessions WHERE project_id = ? AND session_id = ?",
        (project_id, ref.session_id),
    ).fetchone()
    if row:
        return row["id"]
    cur = conn.execute(
        "INSERT INTO sessions (project_id, session_id) VALUES (?, ?)",
        (project_id, ref.session_id),
    )
    assert cur.lastrowid is not None  # noqa: S101
    return cur.lastrowid


def _insert_message(conn: sqlite3.Connection, session_fk: int, rec: Record) -> int:
    # ``speed`` carries Anthropic's priority/fast tier flag (PR #44).
    # Persisted to the messages table by v003 so SQL-driven cost paths
    # (get_global_stats, services/compare, reports/export, build_enriched_dataset)
    # can apply the 6× Opus multiplier without round-tripping raw_json.
    cur = conn.execute(
        "INSERT OR IGNORE INTO messages ("
        "  session_fk, seq, timestamp, role, model, "
        "  input_tokens, output_tokens, cache_create_tokens, cache_read_tokens, "
        "  content_text, tools_json, raw_json, is_sidechain, uuid, parent_uuid, "
        "  speed"
        ") VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        (
            session_fk,
            rec.seq,
            rec.timestamp,
            rec.role,
            rec.model,
            rec.input_tokens,
            rec.output_tokens,
            rec.cache_create_tokens,
            rec.cache_read_tokens,
            rec.content_text,
            json.dumps(list(rec.tools)),
            json.dumps(rec.raw, default=str),
            int(rec.is_sidechain),
            rec.uuid,
            rec.parent_uuid,
            rec.speed,
        ),
    )
    return cur.rowcount
