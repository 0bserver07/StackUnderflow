"""ProviderDayMartBuilder — (day, provider) rollup."""

from __future__ import annotations

from stackunderflow.etl.marts.provider_day import ProviderDayMartBuilder

from .conftest import insert_event


def test_empty_events(conn) -> None:
    new = ProviderDayMartBuilder().refresh(conn, since_event_id=0)
    assert new == 0
    assert conn.execute("SELECT COUNT(*) AS n FROM provider_day_mart").fetchone()["n"] == 0


def test_collapses_per_day_provider(conn) -> None:
    """Multiple events same (day, provider) collapse, regardless of model."""
    insert_event(conn, event_id=1, provider="claude", model="sonnet", cost_usd=0.01)
    insert_event(conn, event_id=2, provider="claude", model="opus", cost_usd=0.02)
    insert_event(conn, event_id=3, provider="codex", project_id=2,
                 session_id="sess-2", model="o4", cost_usd=0.05)
    ProviderDayMartBuilder().refresh(conn, since_event_id=0)
    rows = {(r["day"], r["provider"]): dict(r) for r in conn.execute(
        "SELECT * FROM provider_day_mart"
    )}
    assert len(rows) == 2
    assert rows[("2024-01-01", "claude")]["message_count"] == 2
    assert abs(rows[("2024-01-01", "claude")]["cost_usd"] - 0.03) < 1e-9
    assert rows[("2024-01-01", "claude")]["project_count"] == 1
    assert rows[("2024-01-01", "codex")]["project_count"] == 1


def test_session_and_project_distinct_count_across_windows(conn) -> None:
    """Two refresh windows with same session+project on the same day stay at 1."""
    insert_event(conn, event_id=1, session_id="sess-1", project_id=1)
    b = ProviderDayMartBuilder()
    w1 = b.refresh(conn, since_event_id=0)
    insert_event(conn, event_id=2, session_id="sess-1", project_id=1)
    b.refresh(conn, since_event_id=w1)
    row = conn.execute("SELECT * FROM provider_day_mart").fetchone()
    assert row["session_count"] == 1
    assert row["project_count"] == 1
    assert row["message_count"] == 2


def test_rebuild_matches_incremental(conn) -> None:
    insert_event(conn, event_id=1, cost_usd=0.05)
    insert_event(conn, event_id=2, cost_usd=0.10, day="2024-01-02")
    b = ProviderDayMartBuilder()
    b.refresh(conn, since_event_id=0)
    inc = sorted(
        tuple(dict(r).items())
        for r in conn.execute(
            "SELECT * FROM provider_day_mart ORDER BY day"
        ).fetchall()
    )
    b.rebuild_from_scratch(conn)
    out = sorted(
        tuple(dict(r).items())
        for r in conn.execute(
            "SELECT * FROM provider_day_mart ORDER BY day"
        ).fetchall()
    )
    assert inc == out
