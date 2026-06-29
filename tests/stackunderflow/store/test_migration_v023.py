"""v023 migration: Overview rate dims on ``project_mart`` + cache tokens on ``tool_mart``.

Closes the "Overview shows 0 on the mart fast-path" gap for the cache /
interruption / errors blocks and per-tool cache cost (ui-perf #20). The
migration is **additive** — no existing table touched. These tests pin its
guarantees:

  1. The 7 ``project_mart`` columns + 2 ``tool_mart`` columns exist with the
     declared type / NOT NULL / DEFAULT.
  2. ``schema.apply`` bumps ``PRAGMA user_version`` to the current head (>=23).
  3. INSERTs that omit the new columns land the DEFAULT (0 / '{}') — the
     state a pre-v023 row holds until a ``--force`` rebuild re-derives them.
  4. The loader is reentrant — running it on a DB where the columns already
     exist (manual ALTER / partial prior run) is a no-op and still bumps
     ``user_version`` via the ``_ADD_COLUMN_GUARDS`` entry.
"""

from __future__ import annotations

from pathlib import Path

from stackunderflow.store import db, schema


def _column_info(conn, table: str, column: str) -> dict | None:
    for r in conn.execute(f"PRAGMA table_info({table})").fetchall():
        if r["name"] == column:
            return {"type": r["type"], "notnull": r["notnull"], "dflt_value": r["dflt_value"]}
    return None


_PROJECT_MART_INT_COLS = (
    "total_records",
    "total_errors",
    "total_cache_read_messages",
    "total_commands_followed_by_interruption",
    "total_command_tools",
    "total_command_steps",
)
_TOOL_MART_INT_COLS = ("cache_read", "cache_create")


def test_v023_adds_project_mart_columns(tmp_path: Path) -> None:
    conn = db.connect(tmp_path / "store.db")
    try:
        schema.apply(conn)
        for col in _PROJECT_MART_INT_COLS:
            info = _column_info(conn, "project_mart", col)
            assert info is not None, f"project_mart.{col} missing after migration"
            assert info["type"].upper() == "INTEGER"
            assert info["notnull"] == 1
            assert str(info["dflt_value"]) == "0"
        # errors_by_category is a JSON text column defaulting to '{}'.
        cat = _column_info(conn, "project_mart", "errors_by_category")
        assert cat is not None
        assert cat["type"].upper() == "TEXT"
        assert cat["notnull"] == 1
        assert str(cat["dflt_value"]).strip("'") == "{}"
    finally:
        conn.close()


def test_v023_adds_tool_mart_cache_columns(tmp_path: Path) -> None:
    conn = db.connect(tmp_path / "store.db")
    try:
        schema.apply(conn)
        for col in _TOOL_MART_INT_COLS:
            info = _column_info(conn, "tool_mart", col)
            assert info is not None, f"tool_mart.{col} missing after migration"
            assert info["type"].upper() == "INTEGER"
            assert info["notnull"] == 1
            assert str(info["dflt_value"]) == "0"
    finally:
        conn.close()


def test_v023_user_version_bumped(tmp_path: Path) -> None:
    conn = db.connect(tmp_path / "store.db")
    try:
        schema.apply(conn)
        assert conn.execute("PRAGMA user_version").fetchone()[0] == schema.CURRENT_VERSION
        assert schema.CURRENT_VERSION >= 23
    finally:
        conn.close()


def test_v023_insert_without_new_columns_defaults(tmp_path: Path) -> None:
    """Rows that omit the v023 columns read 0 / '{}' via the DEFAULTs."""
    conn = db.connect(tmp_path / "store.db")
    try:
        schema.apply(conn)
        conn.execute(
            "INSERT INTO project_mart "
            "(project_id, provider, slug, display_name, total_messages) "
            "VALUES (1, 'claude', 'alpha', 'Alpha', 9)"
        )
        row = conn.execute(
            "SELECT total_records, total_errors, errors_by_category, "
            "       total_cache_read_messages, total_command_tools "
            "FROM project_mart"
        ).fetchone()
        assert row["total_records"] == 0
        assert row["total_errors"] == 0
        assert row["errors_by_category"] == "{}"
        assert row["total_cache_read_messages"] == 0
        assert row["total_command_tools"] == 0

        conn.execute(
            "INSERT INTO tool_mart "
            "(day, project_id, provider, tool_name, event_count, cost_usd, "
            " tokens_in, tokens_out, session_count) "
            "VALUES ('2026-04-01', 1, 'claude', 'Read', 7, 0.05, 700, 300, 2)"
        )
        trow = conn.execute("SELECT cache_read, cache_create FROM tool_mart").fetchone()
        assert trow["cache_read"] == 0
        assert trow["cache_create"] == 0
    finally:
        conn.close()


def test_v023_reentrant_when_columns_already_exist(tmp_path: Path) -> None:
    """Partial-application recovery: columns present but ``user_version`` behind."""
    conn = db.connect(tmp_path / "store.db")
    try:
        schema.apply(conn)  # full chain → v023 columns now exist
        conn.execute("PRAGMA user_version = 22")  # rewind so v023 looks pending
        conn.commit()
        schema.apply(conn)  # must not raise "duplicate column name"
        assert conn.execute("PRAGMA user_version").fetchone()[0] == schema.CURRENT_VERSION
        assert _column_info(conn, "project_mart", "total_records") is not None
        assert _column_info(conn, "tool_mart", "cache_read") is not None
    finally:
        conn.close()


def test_v023_idempotent_reapply(tmp_path: Path) -> None:
    conn = db.connect(tmp_path / "store.db")
    try:
        schema.apply(conn)
        schema.apply(conn)  # second call must not raise
        assert conn.execute("PRAGMA user_version").fetchone()[0] == schema.CURRENT_VERSION
        cols = [r["name"] for r in conn.execute("PRAGMA table_info(project_mart)").fetchall()]
        assert cols.count("total_records") == 1
        tcols = [r["name"] for r in conn.execute("PRAGMA table_info(tool_mart)").fetchall()]
        assert tcols.count("cache_read") == 1
    finally:
        conn.close()
