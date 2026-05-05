"""DailyMartBuilder — incremental + idempotent + session_count correctness."""

from __future__ import annotations

from stackunderflow.etl.marts.daily import DailyMartBuilder

from .conftest import insert_event


def test_empty_events_returns_zero(conn) -> None:
    """Empty source table → mart stays empty, refresh returns 0."""
    new = DailyMartBuilder().refresh(conn, since_event_id=0)
    assert new == 0
    rows = conn.execute("SELECT * FROM daily_mart").fetchall()
    assert rows == []


def test_full_backfill_aggregates_per_key(conn) -> None:
    """Two events on the same day/project/provider/model collapse into one row."""
    insert_event(conn, event_id=1, input_tokens=100, output_tokens=50, cost_usd=0.01)
    insert_event(conn, event_id=2, input_tokens=200, output_tokens=100, cost_usd=0.02)
    new = DailyMartBuilder().refresh(conn, since_event_id=0)
    assert new == 2
    rows = conn.execute("SELECT * FROM daily_mart").fetchall()
    assert len(rows) == 1
    r = rows[0]
    assert r["input_tokens"] == 300
    assert r["output_tokens"] == 150
    assert r["message_count"] == 2
    assert r["session_count"] == 1
    assert r["cost_usd"] == 0.03


def test_incremental_no_double_counting(conn) -> None:
    """Refreshing the same event_id twice does not double the totals."""
    insert_event(conn, event_id=1, input_tokens=100, cost_usd=0.01)
    b = DailyMartBuilder()
    b.refresh(conn, since_event_id=0)
    # Re-running with the same since=0 is wrong (would double-count) — the
    # caller passes the persisted watermark. We assert the *correct* path
    # (since=last_max) is a no-op:
    b.refresh(conn, since_event_id=1)
    row = conn.execute("SELECT * FROM daily_mart").fetchone()
    assert row["input_tokens"] == 100
    assert row["message_count"] == 1


def test_incremental_appends_new_window(conn) -> None:
    """A second window's events get added to the existing row."""
    insert_event(conn, event_id=1, input_tokens=100, cost_usd=0.01)
    b = DailyMartBuilder()
    w1 = b.refresh(conn, since_event_id=0)
    insert_event(conn, event_id=2, input_tokens=300, cost_usd=0.05)
    w2 = b.refresh(conn, since_event_id=w1)
    assert w2 == 2
    row = conn.execute("SELECT * FROM daily_mart").fetchone()
    assert row["input_tokens"] == 400
    assert row["message_count"] == 2
    assert abs(row["cost_usd"] - 0.06) < 1e-9


def test_session_count_stays_unique_across_windows(conn) -> None:
    """COUNT(DISTINCT session_id) is recomputed — not naively summed.

    Without the recompute pass, a session that produces events on the
    same day in two refresh windows would count as 2 (1 + 1). The
    correct answer is 1.
    """
    # Window 1: 2 events from sess-1
    insert_event(conn, event_id=1, session_id="sess-1")
    insert_event(conn, event_id=2, session_id="sess-1")
    b = DailyMartBuilder()
    w1 = b.refresh(conn, since_event_id=0)
    # Window 2: 2 more events from same sess-1 same day
    insert_event(conn, event_id=3, session_id="sess-1")
    insert_event(conn, event_id=4, session_id="sess-1")
    b.refresh(conn, since_event_id=w1)
    row = conn.execute("SELECT * FROM daily_mart").fetchone()
    assert row["session_count"] == 1, "same session across windows must stay 1"
    assert row["message_count"] == 4


def test_session_count_two_distinct_sessions_across_windows(conn) -> None:
    """Two sessions on the same day → session_count = 2 even after splits."""
    insert_event(conn, event_id=1, session_id="sess-1")
    b = DailyMartBuilder()
    w1 = b.refresh(conn, since_event_id=0)
    insert_event(conn, event_id=2, session_id="sess-2")
    b.refresh(conn, since_event_id=w1)
    row = conn.execute("SELECT * FROM daily_mart").fetchone()
    assert row["session_count"] == 2


def test_rebuild_from_scratch_matches_incremental(conn) -> None:
    """Full rebuild produces the same final state as incremental refresh."""
    insert_event(conn, event_id=1, input_tokens=100, cost_usd=0.01)
    insert_event(conn, event_id=2, input_tokens=200, cost_usd=0.02)
    insert_event(conn, event_id=3, input_tokens=300, cost_usd=0.03,
                 day="2024-01-02")
    b = DailyMartBuilder()
    b.refresh(conn, since_event_id=0)
    incremental = sorted(
        tuple(dict(r).items())
        for r in conn.execute(
            "SELECT * FROM daily_mart ORDER BY day"
        ).fetchall()
    )
    b.rebuild_from_scratch(conn)
    rebuilt = sorted(
        tuple(dict(r).items())
        for r in conn.execute(
            "SELECT * FROM daily_mart ORDER BY day"
        ).fetchall()
    )
    assert incremental == rebuilt


def test_separate_keys_create_separate_rows(conn) -> None:
    """Different model/provider/speed combinations produce distinct rows."""
    insert_event(conn, event_id=1, model="sonnet", provider="claude")
    insert_event(conn, event_id=2, model="opus", provider="claude")
    insert_event(conn, event_id=3, model="sonnet", provider="claude",
                 speed="fast")
    DailyMartBuilder().refresh(conn, since_event_id=0)
    n = conn.execute("SELECT COUNT(*) AS n FROM daily_mart").fetchone()["n"]
    assert n == 3
