import sqlite3
from pathlib import Path

from stackunderflow.store import db, schema


def _tables(conn) -> set[str]:
    # v008 turns ``messages`` into a UNION-ALL view over ``messages_YYYYMM``
    # partition tables; cover both kinds so existing checks against
    # ``"messages"`` keep working.
    rows = conn.execute(
        "SELECT name FROM sqlite_master WHERE type IN ('table', 'view')"
    ).fetchall()
    return {r["name"] for r in rows}


def test_apply_creates_all_tables(tmp_path: Path) -> None:
    conn = db.connect(tmp_path / "store.db")
    try:
        schema.apply(conn)
        assert {"projects", "sessions", "messages", "ingest_log"}.issubset(_tables(conn))
    finally:
        conn.close()


def test_apply_sets_user_version(tmp_path: Path) -> None:
    conn = db.connect(tmp_path / "store.db")
    try:
        schema.apply(conn)
        version = conn.execute("PRAGMA user_version").fetchone()[0]
        assert version == schema.CURRENT_VERSION
        assert version == schema.CURRENT_VERSION
    finally:
        conn.close()


def test_apply_is_idempotent(tmp_path: Path) -> None:
    conn = db.connect(tmp_path / "store.db")
    try:
        schema.apply(conn)
        schema.apply(conn)  # second call must not raise
        assert conn.execute("PRAGMA user_version").fetchone()[0] == schema.CURRENT_VERSION
        assert conn.execute("PRAGMA user_version").fetchone()[0] == schema.CURRENT_VERSION
    finally:
        conn.close()


def test_current_version_constant() -> None:
    # v006 added the ETL foundation (usage_events + 5 marts + watermark).
    # v007 added Wave 5 lower-grain marts (tool_mart + command_mart).
    # v008 partitioned messages into messages_YYYYMM tables behind a view.
    # v009 added discovery_telemetry (citation-feedback loop).
    # v010 added captured_events (opt-in hybrid-capture hook sink).
    # v011 added the per-message-grain message_tool_mart.
    # (>= rather than == so a later migration wave doesn't have to touch this;
    #  the strong invariant — apply() lands on CURRENT_VERSION — is checked by
    #  test_apply_sets_user_version.)
    assert schema.CURRENT_VERSION >= 11


def test_v011_creates_message_tool_mart(tmp_path: Path) -> None:
    conn = db.connect(tmp_path / "store.db")
    try:
        schema.apply(conn)
        assert "message_tool_mart" in _tables(conn)
        cols = {r["name"] for r in conn.execute("PRAGMA table_info(message_tool_mart)").fetchall()}
        assert cols == {
            "id", "message_id", "project_id", "session_id", "ts", "day",
            "tool_name", "file_path", "byte_count", "call_index",
        }
        # The lookup indexes the spec calls for.
        idx = {r["name"] for r in conn.execute(
            "SELECT name FROM sqlite_master WHERE type='index' "
            "AND tbl_name='message_tool_mart'"
        ).fetchall()}
        assert {
            "idx_message_tool_mart_session",
            "idx_message_tool_mart_project",
            "idx_message_tool_mart_file",
            "idx_message_tool_mart_tool_day",
        }.issubset(idx)
        # UNIQUE(message_id, tool_name, call_index) is the dedup key the
        # builder's INSERT OR IGNORE relies on.
        conn.execute(
            "INSERT INTO projects (id, provider, slug, display_name, first_seen, last_modified) "
            "VALUES (1, 'claude', 'p', 'p', 0, 0)"
        )
        conn.execute(
            "INSERT INTO message_tool_mart "
            "(message_id, project_id, session_id, ts, day, tool_name, file_path, byte_count, call_index) "
            "VALUES (1, 1, 's', 't', 'd', 'Read', '/a', NULL, 0)"
        )
        try:
            conn.execute(
                "INSERT INTO message_tool_mart "
                "(message_id, project_id, session_id, ts, day, tool_name, file_path, byte_count, call_index) "
                "VALUES (1, 1, 's', 't', 'd', 'Read', '/b', NULL, 0)"
            )
            raise AssertionError("expected UNIQUE violation on (message_id, tool_name, call_index)")
        except sqlite3.IntegrityError:
            pass
    finally:
        conn.close()


def test_v002_migration_preserves_existing_rows(tmp_path: Path) -> None:
    """Apply v001, seed an ingest_log row, then run the v002 migration and
    confirm the row survived with session_id=NULL and storage_kind='file'."""
    conn = db.connect(tmp_path / "store.db")
    try:
        # Apply only v001 by hand-running its migration (simulate a pre-v002 db).
        v001_sql = (
            Path(__file__).resolve().parents[3]
            / "stackunderflow" / "store" / "migrations" / "v001_initial.sql"
        ).read_text()
        conn.executescript(v001_sql)
        conn.execute(
            "INSERT INTO ingest_log "
            "(file_path, provider, mtime, size, processed_offset, last_ingest_ts) "
            "VALUES (?, ?, ?, ?, ?, ?)",
            ("/tmp/old.jsonl", "claude", 1.0, 100, 100, 1700000000.0),
        )
        conn.commit()

        # Now apply the full migration chain (which should run only v002).
        schema.apply(conn)

        rows = list(conn.execute(
            "SELECT file_path, provider, session_id, storage_kind, "
            "mtime, size, processed_offset, last_rowid "
            "FROM ingest_log"
        ))
        assert len(rows) == 1
        r = rows[0]
        assert r["file_path"] == "/tmp/old.jsonl"
        assert r["provider"] == "claude"
        assert r["session_id"] is None
        assert r["storage_kind"] == "file"
        assert r["processed_offset"] == 100
        assert r["last_rowid"] is None
        assert conn.execute("PRAGMA user_version").fetchone()[0] == schema.CURRENT_VERSION
        assert conn.execute("PRAGMA user_version").fetchone()[0] == schema.CURRENT_VERSION
    finally:
        conn.close()
