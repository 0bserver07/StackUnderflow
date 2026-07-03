"""v028 migration: ``sync_identity`` + ``sync_outbox`` (opt-in multi-device sync).

v028 adds two additive tables for Phase 1 of #100. These tests pin its
guarantees:

  1. Both tables exist with the spec's columns after ``schema.apply``.
  2. ``schema.apply`` bumps ``PRAGMA user_version`` to the current head (>=28).
  3. The migration is **purely additive** — applied to a genuine v27 store it
     adds ONLY ``sync_identity`` + ``sync_outbox`` and alters no existing table
     (the "sync-off store is byte-identical" invariant).
  4. ``sync_identity`` is single-row (CHECK id = 1).
  5. The loader is reentrant (partial prior run recovers via ``_ADD_COLUMN_GUARDS``).
"""

from __future__ import annotations

import sqlite3
from pathlib import Path

from stackunderflow.store import db, schema


def _tables(conn) -> set[str]:
    rows = conn.execute(
        "SELECT name FROM sqlite_master WHERE type IN ('table', 'view')"
    ).fetchall()
    return {r["name"] for r in rows}


def _columns(conn, table: str) -> set[str]:
    return {r["name"] for r in conn.execute(f"PRAGMA table_info({table})").fetchall()}


def _apply_up_to(conn: sqlite3.Connection, target: int) -> None:
    for version, path in schema._discover():
        if version > target:
            break
        if path.suffix == ".sql":
            conn.executescript(path.read_text())
        else:
            schema._run_python_migration(conn, version, path)


def test_v028_creates_sync_tables(tmp_path: Path) -> None:
    conn = db.connect(tmp_path / "store.db")
    try:
        schema.apply(conn)
        tables = _tables(conn)
        assert "sync_identity" in tables
        assert "sync_outbox" in tables
        assert _columns(conn, "sync_identity") == {
            "id", "device_uuid", "key_fingerprint", "bucket_url",
            "endpoint_url", "layout_version", "created_at",
        }
        assert _columns(conn, "sync_outbox") == {
            "shard_key", "content_hash", "generation", "dirty",
            "last_pushed_hash", "last_pushed_ts",
        }
    finally:
        conn.close()


def test_v028_user_version_bumped(tmp_path: Path) -> None:
    conn = db.connect(tmp_path / "store.db")
    try:
        schema.apply(conn)
        assert conn.execute("PRAGMA user_version").fetchone()[0] == schema.CURRENT_VERSION
        assert schema.CURRENT_VERSION >= 28
    finally:
        conn.close()


def test_v028_is_purely_additive_on_a_genuine_v27_store(tmp_path: Path) -> None:
    """Apply the real chain to v27, snapshot the schema, then apply v28 and prove
    it adds ONLY the two sync tables and changes no existing table."""
    conn = db.connect(tmp_path / "store.db")
    try:
        _apply_up_to(conn, 27)
        assert conn.execute("PRAGMA user_version").fetchone()[0] == 27
        before_tables = _tables(conn)
        before_cols = {t: _columns(conn, t) for t in before_tables}

        schema.apply(conn)  # v27 → head

        assert conn.execute("PRAGMA user_version").fetchone()[0] == schema.CURRENT_VERSION
        after_tables = _tables(conn)
        # Exactly the two new tables were added.
        assert after_tables - before_tables == {"sync_identity", "sync_outbox"}
        # Every pre-existing table kept its exact column set (nothing altered).
        for table, cols in before_cols.items():
            assert _columns(conn, table) == cols, f"{table} columns changed"
    finally:
        conn.close()


def test_v028_sync_identity_is_single_row(tmp_path: Path) -> None:
    conn = db.connect(tmp_path / "store.db")
    try:
        schema.apply(conn)
        conn.execute(
            "INSERT INTO sync_identity (id, device_uuid, key_fingerprint, bucket_url, created_at) "
            "VALUES (1, 'd', 'fp', 's3://b', 't')"
        )
        try:
            conn.execute(
                "INSERT INTO sync_identity (id, device_uuid, key_fingerprint, bucket_url, created_at) "
                "VALUES (2, 'd2', 'fp2', 's3://b2', 't2')"
            )
            raise AssertionError("expected CHECK (id = 1) to reject a second row")
        except sqlite3.IntegrityError:
            pass
    finally:
        conn.close()


def test_v028_reentrant_when_tables_already_exist(tmp_path: Path) -> None:
    """Partial-application recovery: tables present but ``user_version`` behind."""
    conn = db.connect(tmp_path / "store.db")
    try:
        schema.apply(conn)  # full chain → tables now exist
        conn.execute("PRAGMA user_version = 27")  # rewind so v028 looks pending
        conn.commit()
        schema.apply(conn)  # must not raise "table already exists"
        assert conn.execute("PRAGMA user_version").fetchone()[0] == schema.CURRENT_VERSION
        assert {"sync_identity", "sync_outbox"}.issubset(_tables(conn))
    finally:
        conn.close()


def test_v028_idempotent_reapply(tmp_path: Path) -> None:
    conn = db.connect(tmp_path / "store.db")
    try:
        schema.apply(conn)
        schema.apply(conn)  # second call must not raise
        assert conn.execute("PRAGMA user_version").fetchone()[0] == schema.CURRENT_VERSION
    finally:
        conn.close()
