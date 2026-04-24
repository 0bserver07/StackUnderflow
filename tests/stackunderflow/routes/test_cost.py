"""Tests for the split cost routes — spec §A3.

Covers:

* ``GET /api/cost-data`` returns exactly the 9 analytics keys, defaults
  missing ones to empty containers, and respects ``log_path`` / 400s.
* ``GET /api/interaction/{id}`` serialises an ``Interaction`` and 404s on
  unknown ids.
* ``GET /api/dashboard-data`` no longer surfaces the 9 analytics keys.
"""
from __future__ import annotations

import json

import pytest
from fastapi import HTTPException

from stackunderflow.routes.cost import COST_KEYS, get_cost_data, get_interaction
from stackunderflow.routes.data import get_dashboard_data
from stackunderflow.stats.enricher import EnrichedDataset, Interaction, Record
from stackunderflow.store import db, schema


# ── helpers ──────────────────────────────────────────────────────────────────

def _seed_project(store_db, slug: str) -> None:
    conn = db.connect(store_db)
    schema.apply(conn)
    conn.execute(
        "INSERT INTO projects (provider, slug, display_name, first_seen, last_modified) "
        "VALUES (?, ?, ?, ?, ?)",
        ("claude", slug, slug, 0.0, 0.0),
    )
    conn.commit()
    conn.close()


def _fake_stats() -> dict:
    """Full stats dict with all 9 analytics keys + overview/tools so we can
    verify which subset each route returns."""
    return {
        # kept in /api/dashboard-data
        "overview": {"project_name": "demo"},
        "tools": {"usage_counts": {}, "error_counts": {}, "error_rates": {}},
        "sessions": {"count": 0},
        "daily_stats": [],
        "hourly_pattern": [],
        "errors": {"total": 0},
        "models": {},
        "user_interactions": [],
        "cache": {"hit_rate": 0.0},
        # moved to /api/cost-data
        "session_costs": [{"session_id": "s1", "cost": 1.0}],
        "command_costs": [{"interaction_id": "i1", "cost": 0.5}],
        "tool_costs": {"Read": {"calls": 1, "cost": 0.0}},
        "token_composition": {"daily": {}, "totals": {}, "per_session": {}},
        "outliers": {"high_tool_commands": [], "high_step_commands": []},
        "retry_signals": [{"interaction_id": "i1", "tool": "Bash"}],
        "session_efficiency": [{"session_id": "s1", "classification": "ok"}],
        "error_cost": {"estimated_retry_cost": 0.12, "top_error_commands": []},
        "trends": {"current": {}, "prior": {}},
    }


def _fake_record(content: str = "hi") -> Record:
    return Record(
        session_id="sess-1",
        kind="user",
        timestamp="2026-04-23T00:00:00Z",
        model="N/A",
        content=content,
        tokens={"input": 0, "output": 0, "cache_creation": 0, "cache_read": 0},
        tools=[],
        is_error=False,
        error_category=None,
        is_interruption=False,
        has_tool_result=False,
        uuid="u1",
        parent_uuid=None,
        is_sidechain=False,
        message_id="m1",
        cwd="/tmp",
        raw_data={"foo": "bar"},
    )


# ── /api/cost-data ────────────────────────────────────────────────────────────

@pytest.mark.asyncio
async def test_cost_data_returns_nine_keys_and_nothing_else(tmp_path, monkeypatch):
    store_db = tmp_path / "store.db"
    slug = "-cost-proj"
    _seed_project(store_db, slug)

    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)
    monkeypatch.setattr("stackunderflow.deps.current_log_path", f"/fake/{slug}")

    def fake_stats(conn, *, project_id, tz_offset=0):  # noqa: ARG001
        return [], _fake_stats()

    monkeypatch.setattr(
        "stackunderflow.routes.cost.queries.get_project_stats",
        fake_stats,
    )

    payload = await get_cost_data()
    assert set(payload.keys()) == set(COST_KEYS)
    assert payload["session_costs"] == [{"session_id": "s1", "cost": 1.0}]
    assert payload["error_cost"]["estimated_retry_cost"] == 0.12
    assert payload["retry_signals"][0]["interaction_id"] == "i1"


@pytest.mark.asyncio
async def test_cost_data_defaults_missing_keys_to_empty_containers(tmp_path, monkeypatch):
    """If summarise() returns a truncated stats dict the route still answers
    with every expected key — dict-shaped sections default to {}, list-shaped
    to []."""
    store_db = tmp_path / "store.db"
    slug = "-sparse-proj"
    _seed_project(store_db, slug)

    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)
    monkeypatch.setattr("stackunderflow.deps.current_log_path", f"/fake/{slug}")

    monkeypatch.setattr(
        "stackunderflow.routes.cost.queries.get_project_stats",
        lambda conn, *, project_id, tz_offset=0: ([], {}),
    )

    payload = await get_cost_data()
    assert payload["session_costs"] == []
    assert payload["command_costs"] == []
    assert payload["retry_signals"] == []
    assert payload["session_efficiency"] == []
    assert payload["tool_costs"] == {}
    assert payload["token_composition"] == {}
    assert payload["outliers"] == {}
    assert payload["error_cost"] == {}
    assert payload["trends"] == {}


@pytest.mark.asyncio
async def test_cost_data_400_without_project(monkeypatch):
    monkeypatch.setattr("stackunderflow.deps.current_log_path", None)
    with pytest.raises(HTTPException) as exc_info:
        await get_cost_data()
    assert exc_info.value.status_code == 400


@pytest.mark.asyncio
async def test_cost_data_404_when_slug_not_in_store(tmp_path, monkeypatch):
    store_db = tmp_path / "store.db"
    # schema applied but no projects inserted
    conn = db.connect(store_db)
    schema.apply(conn)
    conn.close()

    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)
    monkeypatch.setattr("stackunderflow.deps.current_log_path", "/fake/-missing")

    with pytest.raises(HTTPException) as exc_info:
        await get_cost_data()
    assert exc_info.value.status_code == 404


@pytest.mark.asyncio
async def test_cost_data_respects_log_path_query(tmp_path, monkeypatch):
    store_db = tmp_path / "store.db"
    slug = "-explicit-proj"
    _seed_project(store_db, slug)

    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)
    monkeypatch.setattr("stackunderflow.deps.current_log_path", None)  # no default

    monkeypatch.setattr(
        "stackunderflow.routes.cost.queries.get_project_stats",
        lambda conn, *, project_id, tz_offset=0: ([], _fake_stats()),
    )

    payload = await get_cost_data(log_path=f"/anywhere/{slug}")
    assert "session_costs" in payload


# ── /api/interaction/{id} ─────────────────────────────────────────────────────

@pytest.mark.asyncio
async def test_interaction_returns_enriched_payload(tmp_path, monkeypatch):
    store_db = tmp_path / "store.db"
    slug = "-ix-proj"
    _seed_project(store_db, slug)

    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)
    monkeypatch.setattr("stackunderflow.deps.current_log_path", f"/fake/{slug}")

    cmd = _fake_record("please refactor")
    assistant = Record(
        session_id="sess-1",
        kind="assistant",
        timestamp="2026-04-23T00:00:01Z",
        model="claude-sonnet-4-20250514",
        content="working on it",
        tokens={"input": 100, "output": 50, "cache_creation": 0, "cache_read": 0},
        tools=[],
        is_error=False,
        error_category=None,
        is_interruption=False,
        has_tool_result=False,
        uuid="u2",
        parent_uuid="u1",
        is_sidechain=False,
        message_id="m2",
        cwd="/tmp",
        raw_data={},
    )
    ix = Interaction(
        interaction_id="IX-ABC123",
        command=cmd,
        responses=[assistant],
        tool_results=[],
        session_id="sess-1",
        start_time=cmd.timestamp,
        end_time=assistant.timestamp,
        model="claude-sonnet-4-20250514",
        tool_count=0,
        assistant_steps=1,
    )
    ds = EnrichedDataset(records=[cmd, assistant], interactions=[ix], sessions={})

    monkeypatch.setattr(
        "stackunderflow.routes.cost.queries.build_enriched_dataset",
        lambda conn, *, project_id: (ds, f"/fake/{slug}"),
    )

    payload = await get_interaction("IX-ABC123")
    assert payload["interaction_id"] == "IX-ABC123"
    assert payload["assistant_steps"] == 1
    assert payload["command"]["content"] == "please refactor"
    assert "raw_data" not in payload["command"]  # raw_data stripped
    assert len(payload["responses"]) == 1
    assert payload["responses"][0]["model"] == "claude-sonnet-4-20250514"
    # must round-trip cleanly through JSON
    json.dumps(payload)


@pytest.mark.asyncio
async def test_interaction_404_when_not_found(tmp_path, monkeypatch):
    store_db = tmp_path / "store.db"
    slug = "-none-proj"
    _seed_project(store_db, slug)

    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)
    monkeypatch.setattr("stackunderflow.deps.current_log_path", f"/fake/{slug}")

    ds = EnrichedDataset(records=[], interactions=[], sessions={})
    monkeypatch.setattr(
        "stackunderflow.routes.cost.queries.build_enriched_dataset",
        lambda conn, *, project_id: (ds, f"/fake/{slug}"),
    )

    with pytest.raises(HTTPException) as exc_info:
        await get_interaction("does-not-exist")
    assert exc_info.value.status_code == 404
    assert "does-not-exist" in exc_info.value.detail


@pytest.mark.asyncio
async def test_interaction_400_without_project(monkeypatch):
    monkeypatch.setattr("stackunderflow.deps.current_log_path", None)
    with pytest.raises(HTTPException) as exc_info:
        await get_interaction("anything")
    assert exc_info.value.status_code == 400


# ── /api/dashboard-data regression ────────────────────────────────────────────

@pytest.mark.asyncio
async def test_dashboard_data_no_longer_carries_cost_keys(tmp_path, monkeypatch):
    store_db = tmp_path / "store.db"
    slug = "-dash-proj"
    _seed_project(store_db, slug)

    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)
    monkeypatch.setattr("stackunderflow.deps.current_log_path", f"/fake/{slug}")

    monkeypatch.setattr(
        "stackunderflow.routes.data.queries.get_project_stats",
        lambda conn, *, project_id, tz_offset=0: ([], _fake_stats()),
    )

    resp = await get_dashboard_data()
    stats = resp["statistics"]
    for key in COST_KEYS:
        assert key not in stats, f"dashboard-data still exposes cost key: {key}"
    # the surviving sections must still be present
    for kept in ("overview", "tools", "sessions", "daily_stats", "hourly_pattern",
                 "errors", "models", "user_interactions", "cache"):
        assert kept in stats


# ── server-level route registration ───────────────────────────────────────────

def test_new_routes_registered_on_app():
    from stackunderflow.server import app
    paths = {getattr(r, "path", "") for r in app.routes}
    assert "/api/cost-data" in paths
    assert "/api/interaction/{interaction_id}" in paths
