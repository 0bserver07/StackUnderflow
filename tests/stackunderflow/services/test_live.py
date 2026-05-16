"""Tests for ``services.live`` — Spec 13.

Covers the four read-side helpers that drive the live observability
tab: ``recent_events`` / ``recent_tool_calls`` (incremental SSE
fetchers), ``rolling_burn`` (rolling cost + month-end projection),
``tool_latency_percentiles`` (per-tool P50/P95/P99 from
``messages.timestamp`` deltas), and the ``snapshot`` helper that wraps
all three for ``GET /api/live/stats``.

Tests build a fresh in-memory store via ``schema.apply`` and seed it
with a known shape so the percentile / burn math is exact rather than
data-dependent.
"""

from __future__ import annotations

import sqlite3
from datetime import UTC, datetime, timedelta

import pytest

from stackunderflow.services import live
from stackunderflow.store import db, schema


# ── fixtures ────────────────────────────────────────────────────────────


@pytest.fixture()
def conn(tmp_path) -> sqlite3.Connection:
    """Fresh schema-applied store for every test."""
    c = db.connect(tmp_path / "store.db")
    schema.apply(c)
    return c


def _project(conn: sqlite3.Connection, *, slug: str = "-alpha") -> int:
    cur = conn.execute(
        "INSERT INTO projects (provider, slug, display_name, first_seen, last_modified) "
        "VALUES ('claude', ?, ?, 0.0, 0.0)",
        (slug, slug),
    )
    return int(cur.lastrowid)


def _session(conn: sqlite3.Connection, *, project_id: int, sid: str = "s1") -> int:
    cur = conn.execute(
        "INSERT INTO sessions (project_id, session_id, first_ts, last_ts, message_count) "
        "VALUES (?, ?, ?, ?, ?)",
        (project_id, sid, "2026-04-01T00:00:00Z", "2026-04-01T00:00:00Z", 1),
    )
    return int(cur.lastrowid)


_seq_counter: dict[int, int] = {}


def _message(
    conn: sqlite3.Connection,
    *,
    session_fk: int,
    timestamp: str = "2026-05-15T12:00:00Z",
    raw_json: str = "{}",
) -> int:
    """Insert a message; auto-increment ``seq`` per session.

    v008: ``messages`` is a UNION-ALL view; ``cur.lastrowid`` doesn't
    propagate the trigger's nested INSERT id, so we read the freshly-
    allocated id from ``_messages_id_seq``.
    """
    seq = _seq_counter.get(session_fk, 0)
    _seq_counter[session_fk] = seq + 1
    conn.execute(
        "INSERT INTO messages "
        "(session_fk, seq, timestamp, role, model, "
        " input_tokens, output_tokens, cache_create_tokens, cache_read_tokens, "
        " content_text, tools_json, raw_json, is_sidechain) "
        "VALUES (?, ?, ?, 'assistant', 'claude-sonnet-4-5',"
        " 0, 0, 0, 0, '', '[]', ?, 0)",
        (session_fk, seq, timestamp, raw_json),
    )
    return int(conn.execute(
        "SELECT next_id - 1 FROM _messages_id_seq WHERE rowid_kind = 1"
    ).fetchone()[0])


def _event(
    conn: sqlite3.Connection,
    *,
    source_message_fk: int,
    project_id: int,
    session_id: str = "s1",
    ts: str = "2026-05-15T12:00:00Z",
    cost_usd: float = 0.0,
) -> int:
    cur = conn.execute(
        "INSERT INTO usage_events "
        "(source_message_fk, provider, account, project_id, session_id, ts, day, "
        " model, speed, input_tokens, output_tokens, cache_read_tokens, "
        " cache_create_tokens, cost_usd, cost_source, role, raw_extras) "
        "VALUES (?, 'claude', 'default', ?, ?, ?, ?, "
        " 'claude-sonnet-4-5', 'standard', 0, 0, 0, 0, ?, 'rate_card', 'assistant', NULL)",
        (source_message_fk, project_id, session_id, ts, ts[:10], cost_usd),
    )
    return int(cur.lastrowid)


def _tool_call(
    conn: sqlite3.Connection,
    *,
    message_id: int,
    project_id: int,
    session_id: str = "s1",
    ts: str = "2026-05-15T12:00:00Z",
    tool_name: str = "Read",
    file_path: str | None = None,
    byte_count: int | None = None,
    call_index: int = 0,
) -> int:
    cur = conn.execute(
        "INSERT INTO message_tool_mart "
        "(message_id, project_id, session_id, ts, day, "
        " tool_name, file_path, byte_count, call_index) "
        "VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        (
            message_id,
            project_id,
            session_id,
            ts,
            ts[:10],
            tool_name,
            file_path,
            byte_count,
            call_index,
        ),
    )
    return int(cur.lastrowid)


@pytest.fixture(autouse=True)
def _reset_seq():
    """Per-test reset of the session→seq counter."""
    _seq_counter.clear()
    yield
    _seq_counter.clear()


# ── max-id watermarks ──────────────────────────────────────────────────


class TestMaxIds:
    def test_empty_store_returns_zero(self, conn):
        assert live.max_event_id(conn) == 0
        assert live.max_tool_call_id(conn) == 0

    def test_with_rows_returns_highest(self, conn):
        pid = _project(conn)
        sfk = _session(conn, project_id=pid)
        m1 = _message(conn, session_fk=sfk)
        m2 = _message(conn, session_fk=sfk)
        e1 = _event(conn, source_message_fk=m1, project_id=pid)
        e2 = _event(conn, source_message_fk=m2, project_id=pid)
        t1 = _tool_call(conn, message_id=m1, project_id=pid)
        t2 = _tool_call(conn, message_id=m2, project_id=pid)
        assert live.max_event_id(conn) == max(e1, e2)
        assert live.max_tool_call_id(conn) == max(t1, t2)


# ── recent_events / recent_tool_calls ──────────────────────────────────


class TestRecentEvents:
    def test_empty_store_returns_empty_list(self, conn):
        assert live.recent_events(conn) == []
        assert live.recent_tool_calls(conn) == []

    def test_returns_only_rows_after_watermark(self, conn):
        pid = _project(conn)
        sfk = _session(conn, project_id=pid)
        m1 = _message(conn, session_fk=sfk)
        m2 = _message(conn, session_fk=sfk)
        m3 = _message(conn, session_fk=sfk)
        e1 = _event(conn, source_message_fk=m1, project_id=pid, cost_usd=0.01)
        _event(conn, source_message_fk=m2, project_id=pid, cost_usd=0.02)
        _event(conn, source_message_fk=m3, project_id=pid, cost_usd=0.03)
        # since e1 → only e2 + e3 should appear, oldest first.
        rows = live.recent_events(conn, since_id=e1)
        assert len(rows) == 2
        assert rows[0]["cost_usd"] == 0.02
        assert rows[1]["cost_usd"] == 0.03
        # project_slug joined from projects table.
        assert rows[0]["project_slug"] == "-alpha"

    def test_respects_limit(self, conn):
        pid = _project(conn)
        sfk = _session(conn, project_id=pid)
        for _ in range(5):
            mid = _message(conn, session_fk=sfk)
            _event(conn, source_message_fk=mid, project_id=pid)
        rows = live.recent_events(conn, limit=2)
        assert len(rows) == 2

    def test_tool_calls_filtered_by_id(self, conn):
        pid = _project(conn)
        sfk = _session(conn, project_id=pid)
        m1 = _message(conn, session_fk=sfk)
        t1 = _tool_call(conn, message_id=m1, project_id=pid, tool_name="Read")
        t2 = _tool_call(
            conn, message_id=m1, project_id=pid, tool_name="Edit", call_index=1
        )
        rows = live.recent_tool_calls(conn, since_id=t1)
        assert len(rows) == 1
        assert rows[0]["id"] == t2
        assert rows[0]["tool_name"] == "Edit"


# ── rolling_burn ───────────────────────────────────────────────────────


class TestRollingBurn:
    def test_empty_store_returns_zeros(self, conn):
        now = datetime(2026, 5, 15, 12, 0, tzinfo=UTC)
        out = live.rolling_burn(conn, now=now)
        assert out["window_cost"] == 0.0
        assert out["per_minute"] == 0.0
        assert out["per_hour"] == 0.0
        assert out["today_cost"] == 0.0
        assert out["month_to_date"] == 0.0
        assert out["projected_month_end"] == 0.0
        assert out["window_minutes"] == 5
        assert out["ts"] == now.isoformat()

    def test_window_cost_sums_only_recent_events(self, conn):
        pid = _project(conn)
        sfk = _session(conn, project_id=pid)
        now = datetime(2026, 5, 15, 12, 0, tzinfo=UTC)
        # Inside the 5-min window:
        m1 = _message(conn, session_fk=sfk)
        _event(conn, source_message_fk=m1, project_id=pid,
               ts=(now - timedelta(minutes=2)).isoformat(), cost_usd=0.10)
        m2 = _message(conn, session_fk=sfk)
        _event(conn, source_message_fk=m2, project_id=pid,
               ts=(now - timedelta(minutes=4)).isoformat(), cost_usd=0.20)
        # Outside the 5-min window (still today):
        m3 = _message(conn, session_fk=sfk)
        _event(conn, source_message_fk=m3, project_id=pid,
               ts=(now - timedelta(minutes=10)).isoformat(), cost_usd=0.50)
        out = live.rolling_burn(conn, window_minutes=5, now=now)
        assert out["window_cost"] == pytest.approx(0.30)
        assert out["per_minute"] == pytest.approx(0.06)
        assert out["per_hour"] == pytest.approx(3.6)
        # today_cost includes the older event since both are same UTC day.
        assert out["today_cost"] == pytest.approx(0.80)

    def test_month_end_projection_extrapolates(self, conn):
        pid = _project(conn)
        sfk = _session(conn, project_id=pid)
        # Day 10 of a 31-day month → 21 days remaining. With $1.00 MTD,
        # daily avg = $0.10, projected = $1.00 + $0.10 * 21 = $3.10.
        now = datetime(2026, 5, 10, 12, 0, tzinfo=UTC)
        m1 = _message(conn, session_fk=sfk)
        _event(conn, source_message_fk=m1, project_id=pid,
               ts=(now - timedelta(days=2)).isoformat(), cost_usd=1.00)
        out = live.rolling_burn(conn, window_minutes=5, now=now)
        assert out["month_to_date"] == pytest.approx(1.00)
        assert out["projected_month_end"] == pytest.approx(3.10)


# ── tool_latency_percentiles ──────────────────────────────────────────


class TestToolLatencyPercentiles:
    def test_empty_mart_returns_empty(self, conn):
        assert live.tool_latency_percentiles(conn) == []

    def test_p50_p95_from_known_distribution(self, conn, monkeypatch):
        """Seed 100 latencies (1..100s) for one tool; check P50 and P95.

        Using the nearest-rank percentile method: P50 of [1..100] = 50,
        P95 = 95. P99 = 99. This is the textbook math the spec calls
        for ("P95 percentile on a known-shape latency histogram").
        """
        # Freeze "now" so the 24h window catches all our seeded rows.
        now = datetime(2026, 5, 15, 12, 0, tzinfo=UTC)
        monkeypatch.setattr(live, "_now_utc", lambda: now)

        pid = _project(conn)
        sfk = _session(conn, project_id=pid)
        # For each pair of messages, the second's timestamp is N seconds
        # after the first; the tool_call attached to the first inherits
        # latency = N seconds.
        for delta_s in range(1, 101):
            t1 = now - timedelta(minutes=30, seconds=delta_s)
            t2 = t1 + timedelta(seconds=delta_s)
            m1 = _message(conn, session_fk=sfk, timestamp=t1.isoformat())
            _message(conn, session_fk=sfk, timestamp=t2.isoformat())
            _tool_call(
                conn,
                message_id=m1,
                project_id=pid,
                ts=t1.isoformat(),
                tool_name="Read",
                call_index=delta_s - 1,
            )

        results = live.tool_latency_percentiles(conn)
        assert len(results) == 1
        r = results[0]
        assert r["tool_name"] == "Read"
        assert r["samples"] == 100
        # Nearest-rank with N=100: index = floor(p/100 * 100), clamped.
        # P50 → values[50] = 51. P95 → values[95] = 96. P99 → values[99] = 100.
        assert r["p50"] == pytest.approx(51.0)
        assert r["p95"] == pytest.approx(96.0)
        assert r["p99"] == pytest.approx(100.0)

    def test_top_n_caps_results(self, conn, monkeypatch):
        now = datetime(2026, 5, 15, 12, 0, tzinfo=UTC)
        monkeypatch.setattr(live, "_now_utc", lambda: now)
        pid = _project(conn)
        sfk = _session(conn, project_id=pid)
        # 8 distinct tools, one sample each — top_n=3 should keep 3.
        for i, tool in enumerate(["A", "B", "C", "D", "E", "F", "G", "H"]):
            t1 = now - timedelta(minutes=30 + i)
            t2 = t1 + timedelta(seconds=1 + i)
            m1 = _message(conn, session_fk=sfk, timestamp=t1.isoformat())
            _message(conn, session_fk=sfk, timestamp=t2.isoformat())
            _tool_call(
                conn,
                message_id=m1,
                project_id=pid,
                ts=t1.isoformat(),
                tool_name=tool,
            )
        out = live.tool_latency_percentiles(conn, top_n=3)
        assert len(out) == 3

    def test_negative_deltas_are_dropped(self, conn, monkeypatch):
        """Out-of-order timestamps (clock skew) must not poison the percentile."""
        now = datetime(2026, 5, 15, 12, 0, tzinfo=UTC)
        monkeypatch.setattr(live, "_now_utc", lambda: now)
        pid = _project(conn)
        sfk = _session(conn, project_id=pid)
        t1 = now - timedelta(minutes=1)
        t2 = t1 - timedelta(seconds=5)  # next msg is BEFORE source — bogus
        m1 = _message(conn, session_fk=sfk, timestamp=t1.isoformat())
        _message(conn, session_fk=sfk, timestamp=t2.isoformat())
        _tool_call(conn, message_id=m1, project_id=pid, ts=t1.isoformat())
        out = live.tool_latency_percentiles(conn)
        # Negative delta dropped → tool drops out entirely (no samples).
        assert out == []


# ── snapshot ────────────────────────────────────────────────────────────


class TestSnapshot:
    def test_snapshot_returns_complete_shape(self, conn):
        out = live.snapshot(conn)
        assert set(out.keys()) == {"burn", "tool_latency", "watermarks"}
        assert out["watermarks"] == {"event_id": 0, "tool_call_id": 0}
        assert out["tool_latency"] == []
        assert out["burn"]["window_minutes"] == 5
