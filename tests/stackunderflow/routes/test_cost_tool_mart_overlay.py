"""Wave 5 — ``/api/cost-data`` tool_costs block reads from ``tool_mart``.

Tests the overlay path: when ``tool_mart`` has rows for the project,
the route reshapes them into the legacy ``tool_costs`` JSON shape so
the dashboard chart consumer doesn't notice the swap. Empty-mart
fallback is also exercised — the aggregator's ``tool_costs`` survives
intact when no mart rows exist.
"""

from __future__ import annotations

import pytest

from stackunderflow.routes.cost import get_cost_data
from stackunderflow.store import db, schema


def _connect(store_db):
    conn = db.connect(store_db)
    schema.apply(conn)
    return conn


def _insert_project(conn, provider, slug):
    cur = conn.execute(
        "INSERT INTO projects (provider, slug, display_name, "
        "first_seen, last_modified) "
        "VALUES (?, ?, ?, 0.0, 0.0)",
        (provider, slug, slug),
    )
    return int(cur.lastrowid)


def _insert_project_mart(conn, *, project_id, provider, slug):
    """Inserting a project_mart row enables the mart-aware fast path."""
    conn.execute(
        "INSERT INTO project_mart "
        "(project_id, provider, slug, display_name, first_ts, last_ts, "
        " total_messages, total_sessions, total_input_tokens, "
        " total_output_tokens, total_cache_read, total_cache_create, "
        " total_cost_usd) "
        "VALUES (?, ?, ?, ?, '2026-04-01', '2026-04-30', 1, 1, "
        "100, 50, 0, 0, 0.5)",
        (project_id, provider, slug, slug),
    )


def _insert_daily_mart(conn, *, project_id, day, **kw):
    """Daily-mart row is needed because cost.py only overlays when the
    daily mart has rows for the project."""
    conn.execute(
        "INSERT INTO daily_mart "
        "(day, project_id, provider, model, speed, "
        " input_tokens, output_tokens, cache_read, cache_create, "
        " message_count, session_count, cost_usd) "
        "VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        (day, project_id, kw.get("provider", "claude"),
         kw.get("model", "claude-sonnet-4-5"), kw.get("speed", "standard"),
         kw.get("input_tokens", 0), kw.get("output_tokens", 0),
         kw.get("cache_read", 0), kw.get("cache_create", 0),
         kw.get("message_count", 0), kw.get("session_count", 0),
         kw.get("cost_usd", 0.0)),
    )


def _insert_tool_mart(conn, *, project_id, day, provider, tool_name, **kw):
    conn.execute(
        "INSERT INTO tool_mart "
        "(day, project_id, provider, tool_name, "
        " event_count, calls_total, cost_usd, tokens_in, tokens_out, session_count) "
        "VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        (day, project_id, provider, tool_name,
         kw.get("event_count", 0), kw.get("calls_total", 0),
         kw.get("cost_usd", 0.0),
         kw.get("tokens_in", 0), kw.get("tokens_out", 0),
         kw.get("session_count", 0)),
    )


@pytest.mark.asyncio
async def test_cost_data_overlays_tool_costs_from_tool_mart(tmp_path, monkeypatch):
    """``tool_mart`` rows replace the aggregator's ``tool_costs`` block."""
    store_db = tmp_path / "store.db"
    slug = "-tool-overlay"
    conn = _connect(store_db)
    pid = _insert_project(conn, "claude", slug)
    _insert_project_mart(conn, project_id=pid, provider="claude", slug=slug)
    _insert_daily_mart(conn, project_id=pid, day="2026-04-01", cost_usd=0.10)
    # Two tool rows for the same project — the route should sum them
    # and surface a {tool_name: {...}} shape. ``calls_total`` is the
    # non-distinct occurrence count (v012); it can exceed ``event_count``.
    _insert_tool_mart(
        conn, project_id=pid, day="2026-04-01", provider="claude",
        tool_name="Read",
        event_count=10, calls_total=27, cost_usd=0.05,
        tokens_in=1000, tokens_out=500, session_count=2,
    )
    _insert_tool_mart(
        conn, project_id=pid, day="2026-04-01", provider="claude",
        tool_name="Edit",
        event_count=4, calls_total=4, cost_usd=0.02,
        tokens_in=400, tokens_out=200, session_count=1,
    )
    conn.commit()
    conn.close()
    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)
    monkeypatch.setattr("stackunderflow.deps.current_log_path", f"/fake/{slug}")
    bogus = {
        "session_costs": [], "command_costs": [],
        # Aggregator emits a junk Read row; mart overlay must blast it.
        "tool_costs": {"BOGUS_TOOL": {"calls": 9999, "cost": 999.0}},
        "token_composition": {"daily": {}, "totals": {}, "per_session": {}},
        "outliers": {}, "retry_signals": [], "session_efficiency": [],
        "error_cost": {}, "trends": {},
    }
    monkeypatch.setattr(
        "stackunderflow.routes.cost.queries.get_project_stats",
        lambda conn, *, project_id, tz_offset=0: ([], bogus),
    )
    payload = await get_cost_data()
    tc = payload["tool_costs"]
    assert "BOGUS_TOOL" not in tc
    assert set(tc) == {"Read", "Edit"}
    assert tc["Read"]["calls"] == 10
    assert tc["Read"]["calls_total"] == 27
    assert tc["Read"]["cost"] == 0.05
    assert tc["Read"]["input_tokens"] == 1000
    assert tc["Read"]["output_tokens"] == 500
    assert tc["Edit"]["calls"] == 4
    assert tc["Edit"]["calls_total"] == 4
    assert tc["Edit"]["cost"] == 0.02


@pytest.mark.asyncio
async def test_cost_data_keeps_aggregator_tool_costs_when_mart_empty(
    tmp_path, monkeypatch,
):
    """Empty ``tool_mart`` → aggregator's ``tool_costs`` survives intact."""
    store_db = tmp_path / "store.db"
    slug = "-tool-fallback"
    conn = _connect(store_db)
    pid = _insert_project(conn, "claude", slug)
    _insert_project_mart(conn, project_id=pid, provider="claude", slug=slug)
    _insert_daily_mart(conn, project_id=pid, day="2026-04-01", cost_usd=0.10)
    # No tool_mart rows on purpose.
    conn.commit()
    conn.close()
    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)
    monkeypatch.setattr("stackunderflow.deps.current_log_path", f"/fake/{slug}")
    aggregator_tool_costs = {
        "Read": {"calls": 7, "cost": 0.03,
                 "input_tokens": 700, "output_tokens": 300,
                 "cache_read_tokens": 0, "cache_creation_tokens": 0},
    }
    monkeypatch.setattr(
        "stackunderflow.routes.cost.queries.get_project_stats",
        lambda conn, *, project_id, tz_offset=0: ([], {
            "session_costs": [], "command_costs": [],
            "tool_costs": aggregator_tool_costs,
            "token_composition": {"daily": {}, "totals": {}, "per_session": {}},
            "outliers": {}, "retry_signals": [], "session_efficiency": [],
            "error_cost": {}, "trends": {},
        }),
    )
    payload = await get_cost_data()
    # Aggregator output preserved verbatim — same shape, same numbers.
    assert payload["tool_costs"] == aggregator_tool_costs


def _seed_windowed_project(tmp_path, monkeypatch, slug: str) -> None:
    """Project with daily+tool mart rows on two days a month apart.

    daily_mart's max day (2026-04-30) is the #24 window anchor; the tool rows
    split one-per-day so a 7d window keeps only the 04-30 row.
    """
    store_db = tmp_path / "store.db"
    conn = _connect(store_db)
    pid = _insert_project(conn, "claude", slug)
    _insert_project_mart(conn, project_id=pid, provider="claude", slug=slug)
    _insert_daily_mart(conn, project_id=pid, day="2026-04-01", cost_usd=0.10)
    _insert_daily_mart(conn, project_id=pid, day="2026-04-30", cost_usd=0.20)
    _insert_tool_mart(
        conn, project_id=pid, day="2026-04-01", provider="claude",
        tool_name="Read",
        event_count=10, calls_total=27, cost_usd=0.05,
        tokens_in=1000, tokens_out=500, session_count=2,
    )
    _insert_tool_mart(
        conn, project_id=pid, day="2026-04-30", provider="claude",
        tool_name="Edit",
        event_count=4, calls_total=4, cost_usd=0.02,
        tokens_in=400, tokens_out=200, session_count=1,
    )
    conn.commit()
    conn.close()
    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)
    monkeypatch.setattr("stackunderflow.deps.current_log_path", f"/fake/{slug}")
    monkeypatch.setattr(
        "stackunderflow.routes.cost.queries.get_project_stats",
        lambda conn, *, project_id, tz_offset=0: ([], {
            "session_costs": [], "command_costs": [],
            "tool_costs": {"AGGREGATOR_TOOL": {"calls": 9999, "cost": 999.0}},
            "token_composition": {"daily": {}, "totals": {}, "per_session": {}},
            "outliers": {}, "retry_signals": [], "session_efficiency": [],
            "error_cost": {}, "trends": {},
        }),
    )


@pytest.mark.asyncio
async def test_cost_data_range_windows_tool_costs(tmp_path, monkeypatch):
    """#24: ``?range=7d`` narrows tool_costs to the last 7 days anchored on
    the project's most recent daily_mart day, and flags the payload."""
    _seed_windowed_project(tmp_path, monkeypatch, "-tool-window")

    payload = await get_cost_data(range_="7d")
    # Anchor = 2026-04-30 → window [2026-04-24, …]: only the Edit row survives.
    assert set(payload["tool_costs"]) == {"Edit"}
    assert payload["tool_costs"]["Edit"]["calls"] == 4
    assert payload["tool_costs_windowed"] is True

    # No range → all-time rollup, flag off.
    payload_all = await get_cost_data()
    assert set(payload_all["tool_costs"]) == {"Read", "Edit"}
    assert payload_all["tool_costs_windowed"] is False

    # Explicit range=all behaves like no range.
    payload_all2 = await get_cost_data(range_="all")
    assert set(payload_all2["tool_costs"]) == {"Read", "Edit"}
    assert payload_all2["tool_costs_windowed"] is False


@pytest.mark.asyncio
async def test_cost_data_windowed_empty_replaces_all_time_block(tmp_path, monkeypatch):
    """#24: a window with no tool activity yields an EMPTY tool_costs block
    (still flagged windowed) — never the aggregator's all-time numbers."""
    store_db = tmp_path / "store.db"
    slug = "-tool-window-empty"
    conn = _connect(store_db)
    pid = _insert_project(conn, "claude", slug)
    _insert_project_mart(conn, project_id=pid, provider="claude", slug=slug)
    # Latest activity day is 2026-04-30 but the only tool row is a month older.
    _insert_daily_mart(conn, project_id=pid, day="2026-04-30", cost_usd=0.20)
    _insert_tool_mart(
        conn, project_id=pid, day="2026-03-30", provider="claude",
        tool_name="Read", event_count=10, cost_usd=0.05,
        tokens_in=1000, tokens_out=500, session_count=2,
    )
    conn.commit()
    conn.close()
    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)
    monkeypatch.setattr("stackunderflow.deps.current_log_path", f"/fake/{slug}")
    monkeypatch.setattr(
        "stackunderflow.routes.cost.queries.get_project_stats",
        lambda conn, *, project_id, tz_offset=0: ([], {
            "session_costs": [], "command_costs": [],
            "tool_costs": {"Read": {"calls": 10, "cost": 0.05}},
            "token_composition": {"daily": {}, "totals": {}, "per_session": {}},
            "outliers": {}, "retry_signals": [], "session_efficiency": [],
            "error_cost": {}, "trends": {},
        }),
    )

    payload = await get_cost_data(range_="7d")
    assert payload["tool_costs"] == {}
    assert payload["tool_costs_windowed"] is True


@pytest.mark.asyncio
async def test_cost_data_range_skipped_under_model_filter(tmp_path, monkeypatch):
    """#24/#57: a model filter disables the tool overlay entirely (tool_mart
    has no model dimension), so the window is NOT applied and the flag stays
    off — the frontend keeps its all-time/all-models badge."""
    _seed_windowed_project(tmp_path, monkeypatch, "-tool-window-model")

    payload = await get_cost_data(model=["claude-sonnet-4-5"], range_="7d")
    # Overlay skipped → the aggregator's all-time all-model block survives.
    assert set(payload["tool_costs"]) == {"AGGREGATOR_TOOL"}
    assert payload["tool_costs_windowed"] is False


@pytest.mark.asyncio
async def test_cost_data_unknown_range_is_400(tmp_path, monkeypatch):
    _seed_windowed_project(tmp_path, monkeypatch, "-tool-window-bad")
    from fastapi import HTTPException

    with pytest.raises(HTTPException) as exc_info:
        await get_cost_data(range_="90d")
    assert exc_info.value.status_code == 400


def test_tool_mart_shape_surfaces_v023_cache_tokens():
    """ui-perf #20: ``_tool_mart_to_aggregator_shape`` must pass the v023
    tool_mart cache tokens through (under the aggregator's
    ``cache_read_tokens`` / ``cache_creation_tokens`` field names) instead of
    the old hard-coded 0, so the ToolCost card shows a real per-tool cache
    cost — while a pre-v023 row that lacks the keys still falls back to 0."""
    from stackunderflow.routes.cost import _tool_mart_to_aggregator_shape

    shaped = _tool_mart_to_aggregator_shape(
        {
            "Read": {
                "calls": 3, "calls_total": 5, "cost": 0.12,
                "tokens_in": 100, "tokens_out": 40,
                "cache_read_tokens": 900, "cache_creation_tokens": 120,
            }
        }
    )
    assert shaped["Read"]["cache_read_tokens"] == 900
    assert shaped["Read"]["cache_creation_tokens"] == 120

    # Pre-v023 tool_mart row (no cache keys) → safe 0 fallback, no KeyError.
    legacy = _tool_mart_to_aggregator_shape(
        {"Edit": {"calls": 1, "cost": 0.0, "tokens_in": 0, "tokens_out": 0}}
    )
    assert legacy["Edit"]["cache_read_tokens"] == 0
    assert legacy["Edit"]["cache_creation_tokens"] == 0
