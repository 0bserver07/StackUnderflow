"""v025 migration: the ``command_day_mart`` per-(day, project) command count.

ui-perf-audit #25 — the Overview "Commands" KPI read the lifetime
``project_mart.total_commands`` and so ignored the dashboard's date window. v025
adds the per-day source that lets the KPI window like Tokens/Cost. The migration
is **additive** (no existing table touched); these tests pin its structural
guarantees, mirroring ``test_migration_v024``:

  1. The ``command_day_mart`` table exists with the declared columns / types.
  2. ``schema.apply`` bumps ``PRAGMA user_version`` to the current head (>= 25).
  3. The PRIMARY KEY (day, project_id) rejects a duplicate.
  4. The loader is reentrant — running it on a DB where the table already exists
     (manual create / partial prior run) is a no-op and still bumps
     ``user_version`` via the ``_ADD_COLUMN_GUARDS`` entry.
  5. Idempotent re-apply does not duplicate the table.
"""

from __future__ import annotations

import sqlite3
from pathlib import Path

from stackunderflow.store import db, schema


def _column_info(conn, table: str, column: str) -> dict | None:
    for r in conn.execute(f"PRAGMA table_info({table})").fetchall():
        if r["name"] == column:
            return {"type": r["type"], "notnull": r["notnull"], "pk": r["pk"]}
    return None


def _table_exists(conn, name: str) -> bool:
    return (
        conn.execute(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name=?", (name,)
        ).fetchone()
        is not None
    )


def test_v025_creates_command_day_mart_table(tmp_path: Path) -> None:
    conn = db.connect(tmp_path / "store.db")
    try:
        schema.apply(conn)
        assert _table_exists(conn, "command_day_mart")
        day = _column_info(conn, "command_day_mart", "day")
        assert day is not None and day["type"].upper() == "TEXT" and day["notnull"] == 1
        pid = _column_info(conn, "command_day_mart", "project_id")
        assert pid is not None and pid["type"].upper() == "INTEGER" and pid["notnull"] == 1
        cc = _column_info(conn, "command_day_mart", "command_count")
        assert cc is not None and cc["type"].upper() == "INTEGER" and cc["notnull"] == 1
        # (day, project_id) compose the primary key.
        assert day["pk"] > 0 and pid["pk"] > 0
    finally:
        conn.close()


def test_v025_user_version_bumped(tmp_path: Path) -> None:
    conn = db.connect(tmp_path / "store.db")
    try:
        schema.apply(conn)
        assert conn.execute("PRAGMA user_version").fetchone()[0] == schema.CURRENT_VERSION
        assert schema.CURRENT_VERSION >= 25
    finally:
        conn.close()


def test_v025_primary_key_rejects_duplicate(tmp_path: Path) -> None:
    conn = db.connect(tmp_path / "store.db")
    try:
        schema.apply(conn)
        conn.execute(
            "INSERT INTO command_day_mart (day, project_id, command_count) "
            "VALUES ('2026-04-01', 1, 3)"
        )
        try:
            conn.execute(
                "INSERT INTO command_day_mart (day, project_id, command_count) "
                "VALUES ('2026-04-01', 1, 9)"
            )
            raise AssertionError("expected PRIMARY KEY violation on (day, project_id)")
        except sqlite3.IntegrityError:
            pass
        # Same day, different project IS allowed.
        conn.execute(
            "INSERT INTO command_day_mart (day, project_id, command_count) "
            "VALUES ('2026-04-01', 2, 4)"
        )
        assert conn.execute("SELECT COUNT(*) FROM command_day_mart").fetchone()[0] == 2
    finally:
        conn.close()


def test_v025_reentrant_when_table_already_exists(tmp_path: Path) -> None:
    """Partial-application recovery: table present but ``user_version`` behind."""
    conn = db.connect(tmp_path / "store.db")
    try:
        schema.apply(conn)  # full chain → command_day_mart exists
        conn.execute("PRAGMA user_version = 24")  # rewind so v025 looks pending
        conn.commit()
        schema.apply(conn)  # must not raise "table already exists"
        assert conn.execute("PRAGMA user_version").fetchone()[0] == schema.CURRENT_VERSION
        assert _table_exists(conn, "command_day_mart")
    finally:
        conn.close()


def test_v025_idempotent_reapply(tmp_path: Path) -> None:
    conn = db.connect(tmp_path / "store.db")
    try:
        schema.apply(conn)
        schema.apply(conn)  # second call must not raise
        assert conn.execute("PRAGMA user_version").fetchone()[0] == schema.CURRENT_VERSION
        n = conn.execute(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='command_day_mart'"
        ).fetchone()[0]
        assert n == 1
    finally:
        conn.close()
