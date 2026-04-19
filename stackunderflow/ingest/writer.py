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
    """
    conn.execute("BEGIN")
    try:
        project_id = _upsert_project(conn, ref)
        session_fk = _upsert_session(conn, project_id, ref)

        max_ts: str | None = None
        count_added = 0
        for record in adapter.read(ref, since_offset=since_offset):
            changes = _insert_message(conn, session_fk, record)
            if changes:
                count_added += 1
                if max_ts is None or record.timestamp > max_ts:
                    max_ts = record.timestamp

        if count_added:
            conn.execute(
                "UPDATE sessions SET message_count = message_count + ?, "
                "                     last_ts = COALESCE(MAX(COALESCE(last_ts, ''), ?), last_ts), "
                "                     first_ts = COALESCE(first_ts, ?) "
                "WHERE id = ?",
                (count_added, max_ts or "", max_ts or "", session_fk),
            )

        conn.execute(
            "INSERT INTO ingest_log (file_path, provider, mtime, size, processed_offset, last_ingest_ts) "
            "VALUES (?, ?, ?, ?, ?, ?) "
            "ON CONFLICT(file_path) DO UPDATE SET "
            "  mtime=excluded.mtime, size=excluded.size, "
            "  processed_offset=excluded.processed_offset, "
            "  last_ingest_ts=excluded.last_ingest_ts",
            (
                str(ref.file_path),
                ref.provider,
                ref.file_mtime,
                ref.file_size,
                ref.file_size,
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
    assert cur.lastrowid is not None
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
    assert cur.lastrowid is not None
    return cur.lastrowid


def _insert_message(conn: sqlite3.Connection, session_fk: int, rec: Record) -> int:
    cur = conn.execute(
        "INSERT OR IGNORE INTO messages ("
        "  session_fk, seq, timestamp, role, model, "
        "  input_tokens, output_tokens, cache_create_tokens, cache_read_tokens, "
        "  content_text, tools_json, raw_json, is_sidechain, uuid, parent_uuid"
        ") VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
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
        ),
    )
    return cur.rowcount
