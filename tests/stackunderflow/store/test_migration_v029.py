"""v029 — Phase 2 pull tables: ``sync_cursors`` + ``sync_remote_devices`` + the
five ``<mart>_remote`` landing tables.

Asserts the migration is additive (no existing table touched), lands
``user_version`` on the current head, recovers from a partial application via the
``_ADD_COLUMN_GUARDS`` entry, and — crucially — that each landing table's columns
are exactly ``device_uuid`` + the serialized shard columns, so the DDL can never
drift from ``sync/serialize.py``.
"""

from __future__ import annotations

import sqlite3
from pathlib import Path

from stackunderflow.store import db, schema
from stackunderflow.sync import serialize

_NEW_TABLES = {
    "sync_cursors",
    "sync_remote_devices",
    "daily_mart_remote",
    "provider_day_mart_remote",
    "model_day_mart_remote",
    "project_mart_remote",
    "session_mart_remote",
}


def _tables(conn) -> set[str]:
    return {
        r["name"]
        for r in conn.execute(
            "SELECT name FROM sqlite_master WHERE type IN ('table', 'view')"
        ).fetchall()
    }


def _columns(conn, table) -> list[str]:
    return [r["name"] for r in conn.execute(f"PRAGMA table_info({table})").fetchall()]


def test_v029_creates_all_pull_tables(tmp_path: Path) -> None:
    conn = db.connect(tmp_path / "store.db")
    try:
        schema.apply(conn)
        assert _NEW_TABLES.issubset(_tables(conn))
    finally:
        conn.close()


def test_v029_user_version_bumped(tmp_path: Path) -> None:
    conn = db.connect(tmp_path / "store.db")
    try:
        schema.apply(conn)
        assert conn.execute("PRAGMA user_version").fetchone()[0] == schema.CURRENT_VERSION
        assert schema.CURRENT_VERSION >= 29
    finally:
        conn.close()


def test_v029_landing_columns_match_shard_columns(tmp_path: Path) -> None:
    """Each ``<mart>_remote`` table = ``device_uuid`` + the family's shard columns.

    This is the contract the pull upsert relies on (it INSERTs ``('device_uuid',)
    + shard.columns``); if the DDL and ``serialize._SPECS`` ever diverge this fails.
    """
    conn = db.connect(tmp_path / "store.db")
    try:
        schema.apply(conn)
        for family, shard_cols in serialize.SHARD_COLUMNS.items():
            table = serialize.remote_table(family)
            assert _columns(conn, table) == ["device_uuid", *shard_cols], (
                f"{table} columns drifted from the {family} shard columns"
            )
    finally:
        conn.close()


def test_v029_is_additive_leaves_existing_tables_unchanged(tmp_path: Path) -> None:
    """The local marts + the v028 sync tables are untouched (no new columns)."""
    conn = db.connect(tmp_path / "store.db")
    try:
        schema.apply(conn)
        # Local daily_mart keeps its project_id key — the re-key lives only in the
        # remote twin, never in the local mart.
        assert "project_id" in _columns(conn, "daily_mart")
        assert "device_uuid" not in _columns(conn, "daily_mart")
        # v028 tables still present and unchanged.
        assert {"sync_identity", "sync_outbox"}.issubset(_tables(conn))
        assert "device_uuid" in _columns(conn, "sync_identity")
    finally:
        conn.close()


def test_v029_partial_application_recovers(tmp_path: Path) -> None:
    """Tables hand-created, ``user_version`` behind ⇒ apply bumps without erroring."""
    conn = db.connect(tmp_path / "store.db")
    try:
        schema.apply(conn)
        # Rewind so v029 looks pending even though its tables exist.
        conn.execute("PRAGMA user_version = 28")
        schema.apply(conn)  # guard short-circuits the body, still bumps the version
        assert conn.execute("PRAGMA user_version").fetchone()[0] == schema.CURRENT_VERSION
        assert _NEW_TABLES.issubset(_tables(conn))
    finally:
        conn.close()


def test_v029_landing_tables_reject_missing_device_uuid(tmp_path: Path) -> None:
    """``device_uuid`` is NOT NULL — provenance is mandatory on every landed row."""
    conn = db.connect(tmp_path / "store.db")
    try:
        schema.apply(conn)
        try:
            conn.execute(
                "INSERT INTO daily_mart_remote (day, provider, slug) "
                "VALUES ('2026-07-01', 'claude', 'alpha')"
            )
            raise AssertionError("expected NOT NULL violation on device_uuid")
        except sqlite3.IntegrityError:
            pass
    finally:
        conn.close()
