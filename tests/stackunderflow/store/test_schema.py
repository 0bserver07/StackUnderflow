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
        assert version == 1
    finally:
        conn.close()


def test_apply_is_idempotent(tmp_path: Path) -> None:
    conn = db.connect(tmp_path / "store.db")
    try:
        schema.apply(conn)
        schema.apply(conn)  # second call must not raise
        assert conn.execute("PRAGMA user_version").fetchone()[0] == 1
    finally:
        conn.close()


def test_current_version_constant() -> None:
    assert schema.CURRENT_VERSION == 1
