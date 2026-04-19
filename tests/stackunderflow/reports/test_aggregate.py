"""Tests for cross-project aggregation (store-backed)."""

import sqlite3
from pathlib import Path

import pytest

from stackunderflow.reports.aggregate import build_report
from stackunderflow.reports.scope import Scope
from stackunderflow.store import db, schema


@pytest.fixture
def conn(tmp_path: Path):
    c = db.connect(tmp_path / "store.db")
    schema.apply(c)
    yield c
    c.close()


def _seed_project(conn: sqlite3.Connection, slug: str) -> int:
    cur = conn.execute(
        "INSERT INTO projects (provider, slug, display_name, first_seen, last_modified) "
        "VALUES (?, ?, ?, ?, ?)",
        ("claude", slug, slug, 0.0, 0.0),
    )
    return cur.lastrowid


def _seed_session(conn: sqlite3.Connection, project_id: int, session_id: str) -> int:
    cur = conn.execute(
        "INSERT INTO sessions (project_id, session_id) VALUES (?, ?)",
        (project_id, session_id),
    )
    return cur.lastrowid


def _seed_msg(conn, session_fk, seq, ts, model, inp, out):
    conn.execute(
        "INSERT INTO messages (session_fk, seq, timestamp, role, model, "
        "input_tokens, output_tokens, raw_json) VALUES (?,?,?,?,?,?,?,?)",
        (session_fk, seq, ts, "assistant", model, inp, out, "{}"),
    )


class TestBuildReport:
    """build_report sums across projects within scope."""

    @pytest.fixture(autouse=True)
    def _setup(self, conn):
        self.conn = conn
        pa = _seed_project(conn, "proj-a")
        pb = _seed_project(conn, "proj-b")
        sa1 = _seed_session(conn, pa, "s-a1")
        sa2 = _seed_session(conn, pa, "s-a2")
        sb1 = _seed_session(conn, pb, "s-b1")
        # proj-a day 2026-04-15: session sa1, 10 messages, model m1
        _seed_msg(conn, sa1, 0, "2026-04-15T10:00:00+00:00", "m1", 1000, 500)
        # proj-a day 2026-04-16: session sa2, 20 messages, model m1
        _seed_msg(conn, sa2, 0, "2026-04-16T10:00:00+00:00", "m1", 2000, 1000)
        # proj-b day 2026-04-16: session sb1, 5 messages, model m1
        _seed_msg(conn, sb1, 0, "2026-04-16T11:00:00+00:00", "m1", 500, 250)
        conn.commit()

    def test_all_time_scope_sums_everything(self):
        scope = Scope(since=None, until=None, label="all time")
        report = build_report(self.conn, scope=scope, include=None, exclude=None)
        assert report["total_messages"] == 3  # 3 messages seeded
        assert report["total_sessions"] == 3  # 3 sessions
        assert len(report["by_project"]) == 2

    def test_scoped_excludes_earlier_day(self):
        scope = Scope(
            since="2026-04-16T00:00:00+00:00",
            until=None,
            label="from 2026-04-16",
        )
        report = build_report(self.conn, scope=scope, include=None, exclude=None)
        # Only 2026-04-16 messages: sa2 (proj-a) + sb1 (proj-b)
        assert report["total_messages"] == 2
        assert report["total_sessions"] == 2

    def test_include_filter(self):
        scope = Scope(since=None, until=None, label="all")
        report = build_report(self.conn, scope=scope, include=["proj-a"], exclude=None)
        assert len(report["by_project"]) == 1
        assert report["by_project"][0]["name"] == "proj-a"

    def test_exclude_filter(self):
        scope = Scope(since=None, until=None, label="all")
        report = build_report(self.conn, scope=scope, include=None, exclude=["proj-b"])
        assert len(report["by_project"]) == 1
        assert report["by_project"][0]["name"] == "proj-a"

    def test_per_project_rankings_sorted_by_cost_desc(self):
        scope = Scope(since=None, until=None, label="all")
        report = build_report(self.conn, scope=scope, include=None, exclude=None)
        costs = [p["cost"] for p in report["by_project"]]
        assert costs == sorted(costs, reverse=True)

    def test_empty_store_returns_zero_totals(self, tmp_path):
        c = db.connect(tmp_path / "empty.db")
        schema.apply(c)
        scope = Scope(since=None, until=None, label="all")
        report = build_report(c, scope=scope, include=None, exclude=None)
        c.close()
        assert report["total_messages"] == 0
        assert report["total_sessions"] == 0
        assert report["by_project"] == []


if __name__ == "__main__":
    import unittest
    unittest.main()
