"""v030 migration: live read-path indexes (mart ``ts`` covering + ``projects.slug``).

v030 is index-only — two ``CREATE INDEX IF NOT EXISTS`` statements, no table
created, altered or dropped. These tests pin its guarantees:

  1. A fresh store reaches ``user_version`` 30 with both indexes present.
  2. A genuine v29 store upgrades cleanly and gains ONLY the two indexes —
     no table and no column changes anywhere.
  3. Re-applying is a no-op (no ``_ADD_COLUMN_GUARDS`` entry needed).
  4. The mart index survives a mart rebuild: builders clear rows with
     ``DELETE FROM``, never ``DROP TABLE``, so the index outlives
     ``rebuild_from_scratch``.
  5. The mart index is actually *usable* by the live latency window predicate
     (``WHERE ts >= ?``) — an index the planner ignores is dead weight.
"""

from __future__ import annotations

import sqlite3
from pathlib import Path

from stackunderflow.store import db, schema

_NEW_INDEXES = {"idx_message_tool_mart_ts", "idx_projects_slug"}


def _tables(conn) -> set[str]:
    rows = conn.execute("SELECT name FROM sqlite_master WHERE type IN ('table', 'view')").fetchall()
    return {r["name"] for r in rows}


def _columns(conn, table: str) -> set[str]:
    return {r["name"] for r in conn.execute(f"PRAGMA table_info({table})").fetchall()}


def _indexes(conn) -> set[str]:
    rows = conn.execute("SELECT name FROM sqlite_master WHERE type = 'index'").fetchall()
    return {r["name"] for r in rows}


def _apply_up_to(conn: sqlite3.Connection, target: int) -> None:
    for version, path in schema._discover():
        if version > target:
            break
        if path.suffix == ".sql":
            conn.executescript(path.read_text())
        else:
            schema._run_python_migration(conn, version, path)


def test_v030_fresh_store_reaches_version_30_with_both_indexes(tmp_path: Path) -> None:
    conn = db.connect(tmp_path / "store.db")
    try:
        schema.apply(conn)
        assert conn.execute("PRAGMA user_version").fetchone()[0] == schema.CURRENT_VERSION
        assert schema.CURRENT_VERSION >= 30
        assert _NEW_INDEXES.issubset(_indexes(conn))
    finally:
        conn.close()


def test_v030_index_key_columns(tmp_path: Path) -> None:
    """``idx_message_tool_mart_ts`` covers ``(ts, message_id, tool_name)`` in
    that order — the leading ``ts`` is what makes the window predicate a seek,
    and the two payload columns are what keep the fetch index-only."""
    conn = db.connect(tmp_path / "store.db")
    try:
        schema.apply(conn)
        cols = [r["name"] for r in conn.execute("PRAGMA index_info(idx_message_tool_mart_ts)").fetchall()]
        assert cols == ["ts", "message_id", "tool_name"]
        slug_cols = [r["name"] for r in conn.execute("PRAGMA index_info(idx_projects_slug)").fetchall()]
        assert slug_cols == ["slug"]
    finally:
        conn.close()


def test_v030_upgrades_a_genuine_v29_store_adding_only_indexes(tmp_path: Path) -> None:
    """Apply the real chain to v29, snapshot, then apply ONLY v030 and prove it
    adds the two indexes and touches nothing else."""
    conn = db.connect(tmp_path / "store.db")
    try:
        _apply_up_to(conn, 29)
        assert conn.execute("PRAGMA user_version").fetchone()[0] == 29
        before_tables = _tables(conn)
        before_cols = {t: _columns(conn, t) for t in before_tables}
        before_indexes = _indexes(conn)
        assert not (_NEW_INDEXES & before_indexes), "v030 indexes must not pre-exist at v29"

        v030_path = next(p for v, p in schema._discover() if v == 30)
        conn.executescript(v030_path.read_text())

        assert conn.execute("PRAGMA user_version").fetchone()[0] == 30
        # No table added, removed or altered.
        assert _tables(conn) == before_tables
        for table, cols in before_cols.items():
            assert _columns(conn, table) == cols, f"{table} columns changed"
        # Exactly the two indexes added.
        assert _indexes(conn) - before_indexes == _NEW_INDEXES
    finally:
        conn.close()


def test_v030_idempotent_reapply(tmp_path: Path) -> None:
    """No ``_ADD_COLUMN_GUARDS`` entry — ``IF NOT EXISTS`` carries idempotence.

    Both a plain second ``schema.apply`` and a rewound ``user_version`` (the
    partial-application shape) must run the body again without raising.
    """
    conn = db.connect(tmp_path / "store.db")
    try:
        schema.apply(conn)
        schema.apply(conn)
        conn.execute("PRAGMA user_version = 29")  # rewind: v030 looks pending again
        schema.apply(conn)  # re-runs the body; IF NOT EXISTS makes it a no-op
        assert conn.execute("PRAGMA user_version").fetchone()[0] == schema.CURRENT_VERSION
        assert _NEW_INDEXES.issubset(_indexes(conn))
        assert 30 not in schema._ADD_COLUMN_GUARDS
    finally:
        conn.close()


def test_v030_mart_index_survives_a_row_clear(tmp_path: Path) -> None:
    """Mart rebuilds ``DELETE FROM`` rather than ``DROP TABLE``, so the index
    must still be there (and still usable) afterwards."""
    conn = db.connect(tmp_path / "store.db")
    try:
        schema.apply(conn)
        conn.execute(
            "INSERT INTO projects (provider, slug, display_name, first_seen, last_modified) "
            "VALUES ('claude', '-alpha', '-alpha', 0.0, 0.0)"
        )
        pid = conn.execute("SELECT id FROM projects WHERE slug = '-alpha'").fetchone()[0]
        conn.execute(
            "INSERT INTO message_tool_mart "
            "(message_id, project_id, session_id, ts, day, tool_name, file_path, byte_count, call_index) "
            "VALUES (1, ?, 's1', '2026-07-01T00:00:00Z', '2026-07-01', 'Read', NULL, NULL, 0)",
            (pid,),
        )
        conn.execute("DELETE FROM message_tool_mart")  # what rebuild_from_scratch does
        assert _NEW_INDEXES.issubset(_indexes(conn))
        assert conn.execute("SELECT COUNT(*) FROM message_tool_mart").fetchone()[0] == 0
    finally:
        conn.close()


def test_v030_mart_index_is_used_by_the_live_window_predicate(tmp_path: Path) -> None:
    """The planner must actually pick the index for ``WHERE ts >= ?`` — the
    exact predicate ``services.live._latency_samples`` opens with."""
    conn = db.connect(tmp_path / "store.db")
    try:
        schema.apply(conn)
        plan = conn.execute(
            "EXPLAIN QUERY PLAN "
            "SELECT message_id, tool_name, session_id FROM message_tool_mart WHERE ts >= ?",
            ("2026-07-01T00:00:00Z",),
        ).fetchall()
        detail = " ".join(str(r["detail"]) for r in plan)
        assert "idx_message_tool_mart_ts" in detail, detail
        assert "SEARCH" in detail, detail
    finally:
        conn.close()
