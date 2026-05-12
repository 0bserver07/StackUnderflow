"""v010 migration: ``captured_events`` — the hybrid-capture hook sink.

Spec at ``.notes/specs/05-hybrid-capture-hooks.md``. The migration is
additive (no existing table touched) and uses ``CREATE TABLE IF NOT
EXISTS`` so it coexists with the hook handler's own
``ensure_captured_events_table`` bootstrap — a user can install hooks and
start capturing before the dashboard ever runs ``schema.apply``.
"""

from __future__ import annotations

import sqlite3
from pathlib import Path

import pytest

from stackunderflow.store import db, schema

_EXPECTED_COLUMNS = ("id", "ts", "project_id", "session_id", "hook_id", "event_kind", "payload_json")


@pytest.fixture
def conn(tmp_path: Path) -> sqlite3.Connection:
    c = db.connect(tmp_path / "store.db")
    schema.apply(c)
    yield c
    c.close()


class TestV010:
    def test_current_version_is_at_least_10(self) -> None:
        assert schema.CURRENT_VERSION >= 10

    def test_apply_lands_on_current_version(self, conn: sqlite3.Connection) -> None:
        assert conn.execute("PRAGMA user_version").fetchone()[0] == schema.CURRENT_VERSION

    def test_captured_events_table_shape(self, conn: sqlite3.Connection) -> None:
        cols = [r["name"] for r in conn.execute("PRAGMA table_info(captured_events)").fetchall()]
        assert tuple(cols) == _EXPECTED_COLUMNS
        # NOT NULL on the columns the spec marks NOT NULL.
        info = {r["name"]: r for r in conn.execute("PRAGMA table_info(captured_events)").fetchall()}
        for not_null_col in ("ts", "hook_id", "event_kind", "payload_json"):
            assert info[not_null_col]["notnull"] == 1, not_null_col
        # nullable: project_id, session_id
        assert info["project_id"]["notnull"] == 0
        assert info["session_id"]["notnull"] == 0

    def test_indexes_present(self, conn: sqlite3.Connection) -> None:
        idx = {r["name"] for r in conn.execute("PRAGMA index_list(captured_events)").fetchall()}
        assert "idx_captured_events_session" in idx
        assert "idx_captured_events_kind" in idx
        # the UNIQUE(ts, hook_id, session_id) constraint → an autoindex
        unique_idx = [r for r in conn.execute("PRAGMA index_list(captured_events)").fetchall() if r["unique"]]
        assert unique_idx, "expected a UNIQUE index for (ts, hook_id, session_id)"

    def test_unique_constraint_dedupes(self, conn: sqlite3.Connection) -> None:
        conn.execute(
            "INSERT INTO captured_events (ts, hook_id, event_kind, payload_json) VALUES "
            "('2026-05-12T00:00:00Z', 'stackunderflow-stop', 'boundary', '{}')"
        )
        # Same (ts, hook_id, session_id=NULL) — SQLite treats NULLs as distinct
        # for UNIQUE, so this is allowed (and that's fine — best-effort dedup).
        conn.execute(
            "INSERT INTO captured_events (ts, hook_id, event_kind, payload_json) VALUES "
            "('2026-05-12T00:00:00Z', 'stackunderflow-stop', 'boundary', '{}')"
        )
        # But with a concrete session_id, a re-insert collides.
        conn.execute(
            "INSERT INTO captured_events (ts, session_id, hook_id, event_kind, payload_json) VALUES "
            "('2026-05-12T00:00:01Z', 's1', 'stackunderflow-stop', 'boundary', '{}')"
        )
        with pytest.raises(sqlite3.IntegrityError):
            conn.execute(
                "INSERT INTO captured_events (ts, session_id, hook_id, event_kind, payload_json) VALUES "
                "('2026-05-12T00:00:01Z', 's1', 'stackunderflow-stop', 'boundary', '{\"x\":1}')"
            )

    def test_reapply_is_idempotent(self, conn: sqlite3.Connection) -> None:
        before = conn.execute("PRAGMA user_version").fetchone()[0]
        schema.apply(conn)  # again
        assert conn.execute("PRAGMA user_version").fetchone()[0] == before

    def test_additive_does_not_disturb_existing_tables(self, conn: sqlite3.Connection) -> None:
        # A handful of pre-v010 tables that must still be present.
        names = {r["name"] for r in conn.execute(
            "SELECT name FROM sqlite_master WHERE type IN ('table', 'view')"
        ).fetchall()}
        for table in ("projects", "sessions", "messages", "usage_events", "session_mart"):
            assert table in names

    def test_coexists_with_handler_bootstrap(self, tmp_path: Path) -> None:
        # Handler creates the table first (no user_version bump)…
        from stackunderflow.hooks.handlers import ensure_captured_events_table

        c = db.connect(tmp_path / "h.db")
        try:
            ensure_captured_events_table(c)
            assert c.execute("PRAGMA user_version").fetchone()[0] == 0
            # …then the migration runs and must not choke on the existing table.
            schema.apply(c)
            assert c.execute("PRAGMA user_version").fetchone()[0] == schema.CURRENT_VERSION
            cols = [r["name"] for r in c.execute("PRAGMA table_info(captured_events)").fetchall()]
            assert tuple(cols) == _EXPECTED_COLUMNS
        finally:
            c.close()
