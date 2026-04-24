"""Tests for ``/api/commands`` — spec §D1 pagination endpoint.

Covers:
  * Default pagination shape (`{commands, total, offset, limit}`).
  * All five sort keys: cost, tokens, tools, steps, time (desc + asc).
  * Offset advances the page, ``limit`` caps at 500.
  * 400 when no project selected; 404 on unknown slug.
  * Dashboard regression: ``user_interactions.command_details`` is no longer
    shipped by ``/api/dashboard-data`` (spec §D1).
  * ``/api/commands`` is registered on the FastAPI app.
"""
from __future__ import annotations

import pytest
from fastapi import HTTPException

from stackunderflow.routes.commands import get_commands
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


def _make_record(
    *,
    kind: str,
    ts: str,
    session_id: str = "s1",
    content: str = "",
    model: str = "N/A",
    tokens: dict | None = None,
    tools: list | None = None,
) -> Record:
    return Record(
        session_id=session_id,
        kind=kind,
        timestamp=ts,
        model=model,
        content=content,
        tokens=tokens or {"input": 0, "output": 0, "cache_creation": 0, "cache_read": 0},
        tools=tools or [],
        is_error=False,
        error_category=None,
        is_interruption=False,
        has_tool_result=False,
        uuid=f"u-{kind}-{ts}",
        parent_uuid=None,
        is_sidechain=False,
        message_id=f"m-{kind}-{ts}",
        cwd="/tmp",
        raw_data={},
    )


def _make_interaction(
    *,
    iid: str,
    ts: str,
    prompt: str,
    tool_count: int,
    steps: int,
    output_tokens: int,
    model: str = "claude-sonnet-4-20250514",
) -> Interaction:
    cmd = _make_record(kind="user", ts=ts, content=prompt)
    responses: list[Record] = []
    for step in range(steps):
        responses.append(
            _make_record(
                kind="assistant",
                ts=f"{ts[:-1]}{step}Z",
                content=f"step-{step}",
                model=model,
                tokens={
                    "input": 10,
                    "output": output_tokens,
                    "cache_creation": 0,
                    "cache_read": 0,
                },
            )
        )
    return Interaction(
        interaction_id=iid,
        command=cmd,
        responses=responses,
        tool_results=[],
        session_id="s1",
        start_time=ts,
        end_time=f"{ts[:-1]}9Z",
        model=model,
        tool_count=tool_count,
        assistant_steps=steps,
    )


def _three_command_dataset() -> EnrichedDataset:
    """Three interactions with deliberately different sort signatures so each
    sort key produces a distinct ordering."""
    ix_a = _make_interaction(
        iid="IX-A",
        ts="2026-04-20T10:00:00Z",
        prompt="cheap prompt",
        tool_count=1,
        steps=1,
        output_tokens=10,  # low cost, low tokens, low tools, low steps, earliest
    )
    ix_b = _make_interaction(
        iid="IX-B",
        ts="2026-04-21T10:00:00Z",
        prompt="medium prompt",
        tool_count=5,
        steps=3,
        output_tokens=100,  # middle on every axis
    )
    ix_c = _make_interaction(
        iid="IX-C",
        ts="2026-04-22T10:00:00Z",
        prompt="expensive prompt",
        tool_count=20,
        steps=10,
        output_tokens=5000,  # highest cost/tokens/tools/steps, latest
    )
    all_records = [ix_a.command, ix_b.command, ix_c.command]
    for ix in (ix_a, ix_b, ix_c):
        all_records.extend(ix.responses)
    return EnrichedDataset(
        records=all_records,
        interactions=[ix_a, ix_b, ix_c],
        sessions={},
    )


def _configure_store(tmp_path, monkeypatch, slug: str, dataset: EnrichedDataset | None):
    store_db = tmp_path / "store.db"
    _seed_project(store_db, slug)
    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)
    monkeypatch.setattr("stackunderflow.deps.current_log_path", f"/fake/{slug}")

    def fake_build(conn, *, project_id):  # noqa: ARG001
        if dataset is None:
            return None, ""
        return dataset, f"/fake/{slug}"

    monkeypatch.setattr(
        "stackunderflow.routes.commands.queries.build_enriched_dataset",
        fake_build,
    )


# ── pagination shape ─────────────────────────────────────────────────────────

@pytest.mark.asyncio
async def test_commands_default_shape(tmp_path, monkeypatch):
    _configure_store(tmp_path, monkeypatch, "-cmd-shape", _three_command_dataset())
    payload = await get_commands()
    assert set(payload.keys()) == {"commands", "total", "offset", "limit"}
    assert payload["total"] == 3
    assert payload["offset"] == 0
    assert payload["limit"] == 50  # default
    assert len(payload["commands"]) == 3

    row = payload["commands"][0]
    expected_keys = {
        "interaction_id", "session_id", "timestamp", "prompt_preview",
        "cost", "tokens", "tools_used", "steps", "models_used", "had_error",
    }
    assert expected_keys.issubset(row.keys())


# ── sort keys ────────────────────────────────────────────────────────────────

@pytest.mark.asyncio
@pytest.mark.parametrize("sort_key", ["cost", "tokens", "tools", "steps", "time"])
async def test_commands_sort_desc_puts_expensive_first(tmp_path, monkeypatch, sort_key):
    _configure_store(tmp_path, monkeypatch, f"-cmd-s-{sort_key}", _three_command_dataset())
    payload = await get_commands(sort=sort_key, order="desc")
    ids = [c["interaction_id"] for c in payload["commands"]]
    # IX-C is highest on every axis in the fixture.
    assert ids[0] == "IX-C"


@pytest.mark.asyncio
async def test_commands_sort_asc_reverses_order(tmp_path, monkeypatch):
    _configure_store(tmp_path, monkeypatch, "-cmd-asc", _three_command_dataset())
    payload = await get_commands(sort="cost", order="asc")
    ids = [c["interaction_id"] for c in payload["commands"]]
    assert ids == ["IX-A", "IX-B", "IX-C"]


@pytest.mark.asyncio
async def test_commands_unknown_sort_falls_back_to_cost(tmp_path, monkeypatch):
    _configure_store(tmp_path, monkeypatch, "-cmd-bad-sort", _three_command_dataset())
    payload = await get_commands(sort="banana")
    assert payload["commands"][0]["interaction_id"] == "IX-C"  # cost-desc behaviour


# ── pagination slicing ──────────────────────────────────────────────────────

@pytest.mark.asyncio
async def test_commands_offset_and_limit(tmp_path, monkeypatch):
    _configure_store(tmp_path, monkeypatch, "-cmd-page", _three_command_dataset())
    payload = await get_commands(offset=1, limit=1, sort="cost", order="desc")
    assert payload["offset"] == 1
    assert payload["limit"] == 1
    assert payload["total"] == 3
    assert len(payload["commands"]) == 1
    assert payload["commands"][0]["interaction_id"] == "IX-B"


@pytest.mark.asyncio
async def test_commands_offset_past_end_returns_empty_slice(tmp_path, monkeypatch):
    _configure_store(tmp_path, monkeypatch, "-cmd-offend", _three_command_dataset())
    payload = await get_commands(offset=100, limit=50)
    assert payload["commands"] == []
    assert payload["total"] == 3


@pytest.mark.asyncio
async def test_commands_limit_clamps_to_500(tmp_path, monkeypatch):
    _configure_store(tmp_path, monkeypatch, "-cmd-clamp", _three_command_dataset())
    payload = await get_commands(limit=9999)
    assert payload["limit"] == 500


@pytest.mark.asyncio
async def test_commands_negative_offset_clamps_to_zero(tmp_path, monkeypatch):
    _configure_store(tmp_path, monkeypatch, "-cmd-neg", _three_command_dataset())
    payload = await get_commands(offset=-5, limit=50)
    assert payload["offset"] == 0


# ── empty + error paths ─────────────────────────────────────────────────────

@pytest.mark.asyncio
async def test_commands_returns_empty_when_dataset_missing(tmp_path, monkeypatch):
    _configure_store(tmp_path, monkeypatch, "-cmd-none", None)
    payload = await get_commands()
    assert payload == {"commands": [], "total": 0, "offset": 0, "limit": 50}


@pytest.mark.asyncio
async def test_commands_400_without_project(monkeypatch):
    monkeypatch.setattr("stackunderflow.deps.current_log_path", None)
    with pytest.raises(HTTPException) as exc_info:
        await get_commands()
    assert exc_info.value.status_code == 400


@pytest.mark.asyncio
async def test_commands_404_when_slug_missing(tmp_path, monkeypatch):
    store_db = tmp_path / "store.db"
    conn = db.connect(store_db)
    schema.apply(conn)
    conn.close()
    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)
    monkeypatch.setattr("stackunderflow.deps.current_log_path", "/fake/-missing")
    with pytest.raises(HTTPException) as exc_info:
        await get_commands()
    assert exc_info.value.status_code == 404


# ── dashboard-data regression ───────────────────────────────────────────────

@pytest.mark.asyncio
async def test_dashboard_data_drops_command_details(tmp_path, monkeypatch):
    """§D1: /api/dashboard-data must not ship user_interactions.command_details."""
    store_db = tmp_path / "store.db"
    slug = "-dash-slim"
    _seed_project(store_db, slug)
    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)
    monkeypatch.setattr("stackunderflow.deps.current_log_path", f"/fake/{slug}")

    big_details = [{"user_message": "x" * 4096} for _ in range(100)]
    fake_stats = {
        "overview": {"project_name": "demo"},
        "tools": {},
        "sessions": {},
        "daily_stats": {},
        "hourly_pattern": {},
        "errors": {},
        "models": {},
        "user_interactions": {
            "user_commands_analyzed": 3,
            "avg_tools_per_command": 2.0,
            "tool_count_distribution": {"0": 1, "1": 2},
            "command_details": big_details,
        },
        "cache": {},
    }

    monkeypatch.setattr(
        "stackunderflow.routes.data.queries.get_project_stats",
        lambda conn, *, project_id, tz_offset=0: ([], fake_stats),
    )

    resp = await get_dashboard_data()
    ui = resp["statistics"]["user_interactions"]
    assert "command_details" not in ui, "command_details leaked into dashboard-data"
    # Summary fields must survive.
    assert ui["user_commands_analyzed"] == 3
    assert ui["avg_tools_per_command"] == 2.0
    assert ui["tool_count_distribution"] == {"0": 1, "1": 2}


# ── route registration ──────────────────────────────────────────────────────

def test_commands_route_registered_on_app():
    from stackunderflow.server import app
    paths = {getattr(r, "path", "") for r in app.routes}
    assert "/api/commands" in paths
