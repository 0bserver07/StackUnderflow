from pathlib import Path

from stackunderflow.store import db, schema


def _tables(conn) -> set[str]:
    rows = conn.execute("SELECT name FROM sqlite_master WHERE type='table'").fetchall()
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
    assert schema.CURRENT_VERSION == 7


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
