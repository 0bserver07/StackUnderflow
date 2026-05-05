"""Integration test — every mart against a 100-event synthetic workload.

Locks in:

* the mart-watermark contract (each mart's ``last_event_id`` is
  persisted independently)
* cost-conservation: SUM(daily_mart.cost_usd) == SUM(usage_events.cost_usd)
* row-count invariants for daily_mart and session_mart
* the registry wires every Wave 2B builder
"""

from __future__ import annotations

import sqlite3

from stackunderflow.etl import marts as marts_pkg
from stackunderflow.etl.marts.daily import DailyMartBuilder
from stackunderflow.etl.marts.model_day import ModelDayMartBuilder
from stackunderflow.etl.marts.project import ProjectMartBuilder
from stackunderflow.etl.marts.provider_day import ProviderDayMartBuilder
from stackunderflow.etl.marts.session import SessionMartBuilder
from stackunderflow.etl.watermark import (
    get_watermark,
    refresh_all_marts,
    set_watermark,
)

from .conftest import insert_event

# 3 days × 2 providers × 3 models — but events themselves are 100,
# distributed deterministically.
_DAYS = ["2024-01-01", "2024-01-02", "2024-01-03"]
_PROVIDERS = [("claude", 1, "sess-1"), ("codex", 2, "sess-2")]
_MODELS = ["sonnet", "opus", "haiku"]


def _seed_100_events(conn: sqlite3.Connection) -> float:
    """Insert 100 deterministic events. Returns the total cost_usd."""
    total_cost = 0.0
    eid = 0
    for i in range(100):
        eid += 1
        day = _DAYS[i % len(_DAYS)]
        provider, project_id, base_session = _PROVIDERS[i % len(_PROVIDERS)]
        model = _MODELS[i % len(_MODELS)]
        # Mix in a second session per provider to exercise DISTINCT counts.
        session_id = base_session if i % 2 == 0 else f"{base_session}-alt"
        cost = round(0.001 * (i + 1), 6)
        total_cost += cost
        # Alternate roles so is_one_shot can fire occasionally.
        role = "user" if i % 4 == 0 else "assistant"
        insert_event(
            conn, event_id=eid,
            project_id=project_id, provider=provider,
            session_id=session_id,
            ts=f"{day}T00:00:{i % 60:02d}Z",
            day=day, model=model,
            input_tokens=10 * (i + 1),
            output_tokens=5 * (i + 1),
            cost_usd=cost,
            role=role,
        )
    return total_cost


def test_registry_lists_all_five(conn) -> None:
    """marts.all() must expose every Wave 2B builder by spec name."""
    assert set(marts_pkg.all().keys()) == {
        "daily", "session", "project", "provider_day", "model_day",
    }


def test_full_pipeline_consistency(conn) -> None:
    """Every mart yields rows with totals consistent with usage_events."""
    total_cost = _seed_100_events(conn)

    DailyMartBuilder().refresh(conn, since_event_id=0)
    SessionMartBuilder().refresh(conn, since_event_id=0)
    ProjectMartBuilder().refresh(conn, since_event_id=0)
    ProviderDayMartBuilder().refresh(conn, since_event_id=0)
    ModelDayMartBuilder().refresh(conn, since_event_id=0)

    # ── daily_mart row count = unique (day, project, provider, model, speed) combos
    expected_daily = conn.execute(
        "SELECT COUNT(*) AS n FROM ("
        "  SELECT DISTINCT day, project_id, provider, model, speed "
        "  FROM usage_events"
        ")"
    ).fetchone()["n"]
    actual_daily = conn.execute(
        "SELECT COUNT(*) AS n FROM daily_mart"
    ).fetchone()["n"]
    assert actual_daily == expected_daily

    # ── cost conservation ────────────────────────────────────────────────
    daily_cost = conn.execute(
        "SELECT COALESCE(SUM(cost_usd), 0) AS s FROM daily_mart"
    ).fetchone()["s"]
    events_cost = conn.execute(
        "SELECT COALESCE(SUM(cost_usd), 0) AS s FROM usage_events"
    ).fetchone()["s"]
    assert abs(daily_cost - events_cost) < 1e-6
    assert abs(events_cost - total_cost) < 1e-6

    # provider_day cost also matches the per-day per-provider total
    pd_cost = conn.execute(
        "SELECT COALESCE(SUM(cost_usd), 0) AS s FROM provider_day_mart"
    ).fetchone()["s"]
    assert abs(pd_cost - events_cost) < 1e-6

    # model_day cost too
    md_cost = conn.execute(
        "SELECT COALESCE(SUM(cost_usd), 0) AS s FROM model_day_mart"
    ).fetchone()["s"]
    assert abs(md_cost - events_cost) < 1e-6

    # project_mart total_cost_usd sum matches as well
    pm_cost = conn.execute(
        "SELECT COALESCE(SUM(total_cost_usd), 0) AS s FROM project_mart"
    ).fetchone()["s"]
    assert abs(pm_cost - events_cost) < 1e-6

    # ── session_mart row count = unique session_ids in events ────────────
    expected_sessions = conn.execute(
        "SELECT COUNT(DISTINCT session_id) AS n FROM usage_events"
    ).fetchone()["n"]
    actual_sessions = conn.execute(
        "SELECT COUNT(*) AS n FROM session_mart"
    ).fetchone()["n"]
    assert actual_sessions == expected_sessions


def test_watermark_contract_persisted_per_mart(conn) -> None:
    """``mart_watermark`` records each mart's ``last_event_id`` independently."""
    _seed_100_events(conn)
    # Use the Wave-1 helper so the integration mirrors what the watcher
    # / backfill orchestrator will do in production.
    processed = refresh_all_marts(conn)
    assert set(processed) == {
        "daily", "session", "project", "provider_day", "model_day",
    }
    # Every mart consumed all 100 events on the first run.
    assert all(v == 100 for v in processed.values())

    # Each mart's watermark is at the highest event id (100).
    for name in ("daily", "session", "project", "provider_day", "model_day"):
        assert get_watermark(conn, name) == 100

    # A second refresh with no new events is a clean no-op.
    processed = refresh_all_marts(conn)
    assert all(v == 0 for v in processed.values())
    for name in ("daily", "session", "project", "provider_day", "model_day"):
        assert get_watermark(conn, name) == 100


def test_watermark_helpers_round_trip(conn) -> None:
    """``set_watermark`` then ``get_watermark`` round-trips cleanly."""
    assert get_watermark(conn, "daily") == 0
    set_watermark(conn, "daily", 42)
    assert get_watermark(conn, "daily") == 42
    set_watermark(conn, "daily", 99)
    assert get_watermark(conn, "daily") == 99


def _snapshot_marts(conn: sqlite3.Connection) -> dict:
    """Return per-mart row sets, with float columns rounded for comparison."""
    out: dict = {}
    for name in ("daily", "session", "project", "provider_day", "model_day"):
        rows = []
        # noqa via inline-on-the-fstring: the name comes from a hardcoded
        # literal tuple — there is no user input.
        sql = f"SELECT * FROM {name}_mart"  # noqa: S608
        for r in conn.execute(sql).fetchall():
            d = dict(r)
            for k, v in list(d.items()):
                if isinstance(v, float):
                    d[k] = round(v, 6)
            rows.append(tuple(sorted(d.items())))
        out[name] = sorted(rows)
    return out


def test_two_window_incremental_matches_full(conn) -> None:
    """Split the 100 events into two real windows and verify equivalence."""
    # Insert first 50 events.
    cost_w1 = 0.0
    for i in range(50):
        eid = i + 1
        day = _DAYS[i % len(_DAYS)]
        provider, project_id, base_session = _PROVIDERS[i % len(_PROVIDERS)]
        model = _MODELS[i % len(_MODELS)]
        session_id = base_session if i % 2 == 0 else f"{base_session}-alt"
        cost = round(0.001 * (i + 1), 6)
        cost_w1 += cost
        insert_event(
            conn, event_id=eid, project_id=project_id, provider=provider,
            session_id=session_id, ts=f"{day}T00:00:{i % 60:02d}Z",
            day=day, model=model,
            input_tokens=10 * (i + 1), output_tokens=5 * (i + 1),
            cost_usd=cost,
            role="user" if i % 4 == 0 else "assistant",
        )

    # Refresh window 1.
    refresh_all_marts(conn)

    # Insert second 50 events.
    for i in range(50, 100):
        eid = i + 1
        day = _DAYS[i % len(_DAYS)]
        provider, project_id, base_session = _PROVIDERS[i % len(_PROVIDERS)]
        model = _MODELS[i % len(_MODELS)]
        session_id = base_session if i % 2 == 0 else f"{base_session}-alt"
        cost = round(0.001 * (i + 1), 6)
        insert_event(
            conn, event_id=eid, project_id=project_id, provider=provider,
            session_id=session_id, ts=f"{day}T00:00:{i % 60:02d}Z",
            day=day, model=model,
            input_tokens=10 * (i + 1), output_tokens=5 * (i + 1),
            cost_usd=cost,
            role="user" if i % 4 == 0 else "assistant",
        )

    # Refresh window 2 — this exercises the additive + recompute paths.
    refresh_all_marts(conn)

    incremental = _snapshot_marts(conn)

    # Now rebuild from scratch and compare.
    for cls in (
        DailyMartBuilder, SessionMartBuilder, ProjectMartBuilder,
        ProviderDayMartBuilder, ModelDayMartBuilder,
    ):
        cls().rebuild_from_scratch(conn)

    one_shot = _snapshot_marts(conn)

    for name in one_shot:
        assert one_shot[name] == incremental[name], (
            f"two-window incremental != one-shot rebuild for {name}_mart"
        )

    # And final cost-conservation check holds across the two-window path.
    daily_cost = conn.execute(
        "SELECT COALESCE(SUM(cost_usd), 0) AS s FROM daily_mart"
    ).fetchone()["s"]
    events_cost = conn.execute(
        "SELECT COALESCE(SUM(cost_usd), 0) AS s FROM usage_events"
    ).fetchone()["s"]
    assert abs(daily_cost - events_cost) < 1e-6
