"""v012 migration: add ``tool_mart.calls_total`` — non-distinct tool-call count.

Closes HANDOFF item #6. ``tool_mart.event_count`` is the *distinct*
``(message, tool)`` pair count (the 1/N cost-attribution unit); the
pre-Wave-5 aggregator's ``tool_costs`` block reported ``calls`` as the
*non-distinct* occurrence count. ``calls_total`` restores that signal.

The migration is **additive** — no existing tables touched. These tests
pin its guarantees:

  1. The column exists with type INTEGER, NOT NULL, DEFAULT 0.
  2. ``schema.apply`` bumps ``PRAGMA user_version`` to the current head.
  3. A ``tool_mart`` INSERT that omits ``calls_total`` lands ``0`` via the
     DEFAULT — i.e. rows that predate the migration read ``0`` until a
     ``--force`` rebuild re-derives them.
  4. The migration loader is reentrant — running it on a DB where the
     column already exists (manual ALTER / partial prior run) is a no-op
     and still bumps ``user_version``.
"""

from __future__ import annotations

from pathlib import Path

from stackunderflow.store import db, schema


def _column_info(conn, table: str, column: str) -> dict | None:
    rows = conn.execute(f"PRAGMA table_info({table})").fetchall()
    for r in rows:
        if r["name"] == column:
            # PRAGMA table_info: cid, name, type, notnull, dflt_value, pk
            return {
                "type": r["type"],
                "notnull": r["notnull"],
                "dflt_value": r["dflt_value"],
            }
    return None


def test_v012_adds_calls_total_column(tmp_path: Path) -> None:
    """``tool_mart.calls_total`` exists after ``schema.apply``: INTEGER, NOT NULL, DEFAULT 0."""
    conn = db.connect(tmp_path / "store.db")
    try:
        schema.apply(conn)
        info = _column_info(conn, "tool_mart", "calls_total")
        assert info is not None, "tool_mart.calls_total missing after migration"
        assert info["type"].upper() == "INTEGER"
        assert info["notnull"] == 1
        # SQLite reports the literal default verbatim.
        assert str(info["dflt_value"]) == "0"
    finally:
        conn.close()


def test_v012_user_version_bumped(tmp_path: Path) -> None:
    """``schema.apply`` lands ``user_version`` on the current head."""
    conn = db.connect(tmp_path / "store.db")
    try:
        schema.apply(conn)
        assert (
            conn.execute("PRAGMA user_version").fetchone()[0]
            == schema.CURRENT_VERSION
        )
        assert schema.CURRENT_VERSION >= 12
    finally:
        conn.close()


def test_v012_insert_without_calls_total_defaults_zero(tmp_path: Path) -> None:
    """A ``tool_mart`` INSERT that omits ``calls_total`` gets ``0``.

    This is the state a row that predates v012 lands in after the
    migration runs: the ALTER backfills the new column with its DEFAULT,
    so existing rows read ``0`` until a ``MartBuilder.rebuild_from_scratch``
    re-derives the true count.
    """
    conn = db.connect(tmp_path / "store.db")
    try:
        schema.apply(conn)
        conn.execute(
            "INSERT INTO tool_mart "
            "(day, project_id, provider, tool_name, "
            " event_count, cost_usd, tokens_in, tokens_out, session_count) "
            "VALUES ('2026-04-01', 1, 'claude', 'Read', 7, 0.05, 700, 300, 2)"
        )
        row = conn.execute(
            "SELECT event_count, calls_total FROM tool_mart"
        ).fetchone()
        assert row["event_count"] == 7
        assert row["calls_total"] == 0
    finally:
        conn.close()


def test_v012_reentrant_when_column_already_exists(tmp_path: Path) -> None:
    """Partial-application recovery: column present but ``user_version`` behind.

    Simulates the case where the ALTER ran (manually, or a crashed prior
    attempt) but ``user_version`` wasn't bumped. ``schema.apply`` must
    detect the existing column via ``_ADD_COLUMN_GUARDS`` and bump the
    version instead of re-running the ALTER (which would raise
    "duplicate column name").
    """
    conn = db.connect(tmp_path / "store.db")
    try:
        schema.apply(conn)  # full chain → calls_total now exists
        # Rewind the recorded version to just below v012 so the loader
        # treats v012 as pending again — but the column is already there.
        conn.execute("PRAGMA user_version = 11")
        conn.commit()
        # Must not raise; must land on the current head again.
        schema.apply(conn)
        assert (
            conn.execute("PRAGMA user_version").fetchone()[0]
            == schema.CURRENT_VERSION
        )
        assert _column_info(conn, "tool_mart", "calls_total") is not None
    finally:
        conn.close()


def test_v012_idempotent_reapply(tmp_path: Path) -> None:
    """``schema.apply`` twice on the same DB is safe."""
    conn = db.connect(tmp_path / "store.db")
    try:
        schema.apply(conn)
        schema.apply(conn)  # second call must not raise
        assert (
            conn.execute("PRAGMA user_version").fetchone()[0]
            == schema.CURRENT_VERSION
        )
        # Column still present and singular (no duplicate ALTER).
        cols = [
            r["name"]
            for r in conn.execute("PRAGMA table_info(tool_mart)").fetchall()
        ]
        assert cols.count("calls_total") == 1
    finally:
        conn.close()
