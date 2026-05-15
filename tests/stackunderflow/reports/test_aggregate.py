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


# ── usage_events-driven path (post-backfill) ────────────────────────────────


def _seed_usage_event(
    conn: sqlite3.Connection,
    *,
    project_id: int,
    session_id: str,
    ts: str,
    cost_usd: float,
    model: str = "claude-opus-4-7",
    speed: str = "standard",
    input_tokens: int = 0,
    output_tokens: int = 0,
    source_message_fk: int = 0,
) -> None:
    """Seed one row directly into ``usage_events``.

    The aggregator's mart path keys off ``cost_usd`` and ``session_id`` —
    this helper sets just enough columns to exercise that path without
    going through the normalizer.
    """
    conn.execute(
        "INSERT INTO usage_events (source_message_fk, provider, project_id, "
        "                          session_id, ts, day, model, speed, "
        "                          input_tokens, output_tokens, cost_usd, "
        "                          role) "
        "VALUES (?,?,?,?,?,?,?,?,?,?,?,?)",
        (
            source_message_fk,
            "claude",
            project_id,
            session_id,
            ts,
            ts[:10],
            model,
            speed,
            input_tokens,
            output_tokens,
            cost_usd,
            "assistant",
        ),
    )


class TestBuildReportUsageEventsPath:
    """build_report reads from ``usage_events.cost_usd`` once it's populated.

    The mart path is the v0.7.0+ contract: normalised cost lives on
    ``usage_events`` and the aggregator sums it. Recomputing off
    (input_tokens, output_tokens, model) — the legacy path — mis-prices
    rows the live pricer doesn't recognise and drops the priority-tier
    multiplier, which is exactly the bug this regression test pins.
    """

    @pytest.fixture(autouse=True)
    def _setup(self, conn):
        self.conn = conn
        self.pa = _seed_project(conn, "proj-a")
        self.pb = _seed_project(conn, "proj-b")
        # proj-a — 3 events in 2 sessions, $5 + $3 + $2 = $10 total
        _seed_usage_event(
            conn, project_id=self.pa, session_id="s-a1",
            ts="2026-05-01T10:00:00+00:00", cost_usd=5.0,
            source_message_fk=1001,
        )
        _seed_usage_event(
            conn, project_id=self.pa, session_id="s-a1",
            ts="2026-05-01T11:00:00+00:00", cost_usd=3.0,
            source_message_fk=1002,
        )
        _seed_usage_event(
            conn, project_id=self.pa, session_id="s-a2",
            ts="2026-05-02T10:00:00+00:00", cost_usd=2.0,
            source_message_fk=1003,
        )
        # proj-b — 1 event in 1 session, $4
        _seed_usage_event(
            conn, project_id=self.pb, session_id="s-b1",
            ts="2026-05-02T11:00:00+00:00", cost_usd=4.0,
            source_message_fk=1004,
        )
        conn.commit()

    def test_total_cost_matches_sum_cost_usd(self):
        """The headline equivalence: total_cost == SUM(usage_events.cost_usd)."""
        scope = Scope(since=None, until=None, label="all")
        report = build_report(self.conn, scope=scope, include=None, exclude=None)
        sql_total = self.conn.execute(
            "SELECT COALESCE(SUM(cost_usd), 0.0) FROM usage_events"
        ).fetchone()[0]
        assert report["total_cost"] == pytest.approx(sql_total, abs=0.01)
        assert report["total_cost"] == pytest.approx(14.0, abs=0.01)
        assert report["total_messages"] == 4
        assert report["total_sessions"] == 3  # s-a1, s-a2, s-b1

    def test_scoped_window_matches_sum_cost_usd(self):
        """Day-2-only window must also agree with direct SQL."""
        scope = Scope(
            since="2026-05-02T00:00:00+00:00",
            until="2026-05-02T23:59:59+00:00",
            label="2026-05-02",
        )
        report = build_report(self.conn, scope=scope, include=None, exclude=None)
        sql_total = self.conn.execute(
            "SELECT COALESCE(SUM(cost_usd), 0.0) FROM usage_events "
            "WHERE ts >= ? AND ts <= ?",
            (scope.since, scope.until),
        ).fetchone()[0]
        assert report["total_cost"] == pytest.approx(sql_total, abs=0.01)
        assert report["total_cost"] == pytest.approx(6.0, abs=0.01)  # 2.0 + 4.0
        assert report["total_messages"] == 2
        assert report["total_sessions"] == 2

    def test_per_project_breakdown_sums_correctly(self):
        scope = Scope(since=None, until=None, label="all")
        report = build_report(self.conn, scope=scope, include=None, exclude=None)
        by_slug = {p["name"]: p for p in report["by_project"]}
        assert by_slug["proj-a"]["cost"] == pytest.approx(10.0, abs=0.01)
        assert by_slug["proj-a"]["messages"] == 3
        assert by_slug["proj-a"]["sessions"] == 2
        assert by_slug["proj-b"]["cost"] == pytest.approx(4.0, abs=0.01)
        assert by_slug["proj-b"]["sessions"] == 1

    def test_include_filter_on_usage_events_path(self):
        scope = Scope(since=None, until=None, label="all")
        report = build_report(
            self.conn, scope=scope, include=["proj-a"], exclude=None
        )
        assert len(report["by_project"]) == 1
        assert report["by_project"][0]["name"] == "proj-a"
        assert report["total_cost"] == pytest.approx(10.0, abs=0.01)

    def test_exclude_filter_on_usage_events_path(self):
        scope = Scope(since=None, until=None, label="all")
        report = build_report(
            self.conn, scope=scope, include=None, exclude=["proj-b"]
        )
        assert len(report["by_project"]) == 1
        assert report["by_project"][0]["name"] == "proj-a"


class TestBuildReportFallback:
    """When ``usage_events`` is empty the aggregator falls back to messages.

    This is the fresh-install / pre-backfill contract: the SQL path that
    reads off ``messages`` + ``compute_cost`` is preserved so a user who
    hasn't run ``stackunderflow backfill`` yet still sees their report.
    """

    def test_messages_path_used_when_usage_events_empty(self, conn):
        # Seed messages but NOT usage_events.
        pa = _seed_project(conn, "proj-leg")
        sa = _seed_session(conn, pa, "s-leg")
        _seed_msg(conn, sa, 0, "2026-04-15T10:00:00+00:00", "m1", 1000, 500)
        conn.commit()

        # Sanity: usage_events is empty.
        assert conn.execute("SELECT COUNT(*) FROM usage_events").fetchone()[0] == 0

        scope = Scope(since=None, until=None, label="all")
        report = build_report(conn, scope=scope, include=None, exclude=None)
        # Legacy path produces a (non-zero) cost via compute_cost off
        # the seeded model "m1" (unknown → 0.0); we only assert message
        # count + session count to lock the path was taken at all.
        assert report["total_messages"] == 1
        assert report["total_sessions"] == 1

    def test_mixed_store_prefers_usage_events_over_messages(self, conn):
        """When BOTH tables have rows, usage_events wins (cost is stored).

        The bug this fixes: previously, even with normalised events on
        ``usage_events``, the aggregator re-derived cost from messages
        and mis-priced 6×. The fix asserts: present, populated events
        table → use it.
        """
        pa = _seed_project(conn, "proj-mix")
        sa = _seed_session(conn, pa, "s-mix")
        _seed_msg(conn, sa, 0, "2026-05-01T10:00:00+00:00", "m1", 1000, 500)
        _seed_usage_event(
            conn, project_id=pa, session_id="s-mix",
            ts="2026-05-01T10:00:00+00:00", cost_usd=42.0,
            source_message_fk=2001,
        )
        conn.commit()

        scope = Scope(since=None, until=None, label="all")
        report = build_report(conn, scope=scope, include=None, exclude=None)
        # Cost matches the stored usage_events value, not the messages-derived one.
        assert report["total_cost"] == pytest.approx(42.0, abs=0.01)


if __name__ == "__main__":
    import unittest
    unittest.main()
