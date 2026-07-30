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

from stackunderflow.routes.commands import (
    get_commands,
    get_commands_daily,
    get_tool_distribution,
)
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
    assert set(payload.keys()) == {"commands", "total", "offset", "limit", "currency"}
    assert payload["total"] == 3
    assert payload["offset"] == 0
    assert payload["limit"] == 50  # default
    assert len(payload["commands"]) == 3
    assert payload["currency"]["code"] == "USD"
    assert payload["currency"]["rate_from_usd"] == 1.0

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

    resp = get_dashboard_data()
    ui = resp["statistics"]["user_interactions"]
    assert "command_details" not in ui, "command_details leaked into dashboard-data"
    # §D2: tool_count_distribution moved to /api/tool-distribution.
    assert "tool_count_distribution" not in ui, (
        "tool_count_distribution leaked into dashboard-data"
    )
    # Summary fields must survive.
    assert ui["user_commands_analyzed"] == 3
    assert ui["avg_tools_per_command"] == 2.0


# ── /api/tool-distribution (§D2) ────────────────────────────────────────────

@pytest.mark.asyncio
async def test_tool_distribution_returns_dict(tmp_path, monkeypatch):
    """§D2: /api/tool-distribution serves the bucket map split off dashboard-data."""
    store_db = tmp_path / "store.db"
    slug = "-tcd-ok"
    _seed_project(store_db, slug)
    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)
    monkeypatch.setattr("stackunderflow.deps.current_log_path", f"/fake/{slug}")

    fake_stats = {
        "user_interactions": {
            "user_commands_analyzed": 5,
            "tool_count_distribution": {"0": 2, "1": 1, "5": 2},
        },
    }
    monkeypatch.setattr(
        "stackunderflow.routes.commands.queries.get_project_stats",
        lambda conn, *, project_id, tz_offset=0: ([], fake_stats),
    )
    payload = await get_tool_distribution()
    assert payload == {"tool_count_distribution": {"0": 2, "1": 1, "5": 2}}


@pytest.mark.asyncio
async def test_tool_distribution_empty_when_missing(tmp_path, monkeypatch):
    """Empty user_interactions / missing key → ``{}`` (chart shows empty state)."""
    store_db = tmp_path / "store.db"
    slug = "-tcd-empty"
    _seed_project(store_db, slug)
    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)
    monkeypatch.setattr("stackunderflow.deps.current_log_path", f"/fake/{slug}")

    monkeypatch.setattr(
        "stackunderflow.routes.commands.queries.get_project_stats",
        lambda conn, *, project_id, tz_offset=0: ([], {}),
    )
    payload = await get_tool_distribution()
    assert payload == {"tool_count_distribution": {}}


@pytest.mark.asyncio
async def test_tool_distribution_400_without_project(monkeypatch):
    monkeypatch.setattr("stackunderflow.deps.current_log_path", None)
    with pytest.raises(HTTPException) as exc_info:
        await get_tool_distribution()
    assert exc_info.value.status_code == 400


# ── /api/tool-distribution provider/model filters (#33) ─────────────────────

def _seed_two_provider_project(store_db, slug: str) -> dict[str, int]:
    """Same slug under two providers — returns {provider: project_id}."""
    conn = db.connect(store_db)
    schema.apply(conn)
    ids: dict[str, int] = {}
    for provider in ("claude", "cursor"):
        cur = conn.execute(
            "INSERT INTO projects (provider, slug, display_name, first_seen, last_modified) "
            "VALUES (?, ?, ?, ?, ?)",
            (provider, slug, slug, 0.0, 0.0),
        )
        ids[provider] = int(cur.lastrowid)
    conn.commit()
    conn.close()
    return ids


@pytest.mark.asyncio
async def test_tool_distribution_provider_filter_narrows_project_ids(tmp_path, monkeypatch):
    """#33: ``?provider=`` narrows the slug's (provider, slug) rows before the
    stats sweep, so the distribution reflects only the requested provider."""
    store_db = tmp_path / "store.db"
    slug = "-tcd-provider"
    ids = _seed_two_provider_project(store_db, slug)
    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)
    monkeypatch.setattr("stackunderflow.deps.current_log_path", f"/fake/{slug}")

    per_ids_dist = {
        (ids["claude"],): {"1": 4},
        (ids["cursor"],): {"3": 9},
        (ids["claude"], ids["cursor"]): {"1": 4, "3": 9},
    }

    def fake_stats(conn, *, project_id, tz_offset=0):  # noqa: ARG001
        dist = per_ids_dist[tuple(sorted(project_id))]
        return [], {"user_interactions": {"tool_count_distribution": dist}}

    monkeypatch.setattr(
        "stackunderflow.routes.commands.queries.get_project_stats",
        fake_stats,
    )

    both = await get_tool_distribution()
    assert both == {"tool_count_distribution": {"1": 4, "3": 9}}

    claude_only = await get_tool_distribution(provider=["Claude"])  # case-insensitive
    assert claude_only == {"tool_count_distribution": {"1": 4}}

    cursor_only = await get_tool_distribution(provider=["cursor"])
    assert cursor_only == {"tool_count_distribution": {"3": 9}}


@pytest.mark.asyncio
async def test_tool_distribution_provider_filter_excluding_all_returns_empty(
    tmp_path, monkeypatch,
):
    """A provider filter that matches no row → shape-stable empty map."""
    store_db = tmp_path / "store.db"
    slug = "-tcd-provider-miss"
    _seed_project(store_db, slug)
    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)
    monkeypatch.setattr("stackunderflow.deps.current_log_path", f"/fake/{slug}")

    def fail_stats(conn, *, project_id, tz_offset=0):  # noqa: ARG001
        raise AssertionError("stats sweep must not run when the filter excludes every row")

    monkeypatch.setattr(
        "stackunderflow.routes.commands.queries.get_project_stats",
        fail_stats,
    )
    payload = await get_tool_distribution(provider=["gemini"])
    assert payload == {"tool_count_distribution": {}}


@pytest.mark.asyncio
async def test_tool_distribution_model_filter_recomputes_from_command_details(
    tmp_path, monkeypatch,
):
    """#33: ``?model=`` rebuilds the distribution from the per-command
    ``command_details`` rows so only commands attributed to the selected
    model(s) are counted — interruptions stay excluded, mirroring the
    aggregator's canonical distribution."""
    store_db = tmp_path / "store.db"
    slug = "-tcd-model"
    _seed_project(store_db, slug)
    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)
    monkeypatch.setattr("stackunderflow.deps.current_log_path", f"/fake/{slug}")

    details = [
        {"model": "claude-opus-4-8", "tools_used": 2, "is_interruption": False},
        {"model": "claude-opus-4-8", "tools_used": 2, "is_interruption": False},
        {"model": "claude-opus-4-8", "tools_used": 0, "is_interruption": False},
        # Interruption — never counted, even when the model matches.
        {"model": "claude-opus-4-8", "tools_used": 5, "is_interruption": True},
        # Different model — dropped by the filter.
        {"model": "claude-haiku-4-5", "tools_used": 1, "is_interruption": False},
    ]
    fake_stats = {
        "user_interactions": {
            "tool_count_distribution": {"0": 1, "1": 1, "2": 2, "5": 1},
            "command_details": details,
        },
    }
    monkeypatch.setattr(
        "stackunderflow.routes.commands.queries.get_project_stats",
        lambda conn, *, project_id, tz_offset=0: ([], fake_stats),
    )

    payload = await get_tool_distribution(model=["Claude-Opus-4-8"])  # case-insensitive
    assert payload == {"tool_count_distribution": {2: 2, 0: 1}}

    # A model that matches nothing → empty map, not the all-model fallback.
    payload = await get_tool_distribution(model=["gpt-5"])
    assert payload == {"tool_count_distribution": {}}


# ── route registration ──────────────────────────────────────────────────────

def test_commands_route_registered_on_app():
    from stackunderflow.server import app

    from tests.conftest import app_route_paths

    assert "/api/commands" in app_route_paths(app)


def test_tool_distribution_route_registered_on_app():
    from stackunderflow.server import app

    from tests.conftest import app_route_paths

    assert "/api/tool-distribution" in app_route_paths(app)


# ── /api/commands/daily (#25 — windowed Commands KPI source) ─────────────────


def _seed_command_day_rows(store_db, *, pid: int, rows: list[tuple[str, int]]) -> None:
    """Insert ``command_day_mart`` rows ``[(day, command_count), ...]`` directly.

    Read-path isolation: the builder's own materialisation is covered by
    ``etl/marts/test_command_day_mart.py``, so the route tests exercise the
    reader/endpoint against pre-seeded mart rows.
    """
    conn = db.connect(store_db)
    schema.apply(conn)
    conn.executemany(
        "INSERT INTO command_day_mart (day, project_id, command_count) VALUES (?, ?, ?)",
        [(day, pid, n) for day, n in rows],
    )
    conn.commit()
    conn.close()


def _project_id_for_slug(store_db, slug: str) -> int:
    conn = db.connect(store_db)
    try:
        row = conn.execute("SELECT id FROM projects WHERE slug = ?", (slug,)).fetchone()
        return int(row[0])
    finally:
        conn.close()


@pytest.mark.asyncio
async def test_commands_daily_project_scoped(tmp_path, monkeypatch):
    """With a project active, the series is scoped to that slug's ids."""
    store_db = tmp_path / "store.db"
    slug = "-cmd-daily"
    _seed_project(store_db, slug)
    pid = _project_id_for_slug(store_db, slug)
    _seed_command_day_rows(
        store_db, pid=pid,
        rows=[("2026-04-01", 3), ("2026-04-02", 5), ("2026-04-03", 2)],
    )
    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)
    monkeypatch.setattr("stackunderflow.deps.current_log_path", f"/fake/{slug}")

    payload = await get_commands_daily()
    assert payload["scope"] == "project"
    assert payload["total"] == 10
    assert payload["daily"] == [
        {"date": "2026-04-01", "commands": 3},
        {"date": "2026-04-02", "commands": 5},
        {"date": "2026-04-03", "commands": 2},
    ]


@pytest.mark.asyncio
async def test_commands_daily_global_sums_across_projects(tmp_path, monkeypatch):
    """No project active → cross-project; per-day counts sum across projects."""
    store_db = tmp_path / "store.db"
    conn = db.connect(store_db)
    schema.apply(conn)
    conn.execute(
        "INSERT INTO projects (id, provider, slug, display_name, first_seen, last_modified) "
        "VALUES (1, 'claude', 'a', 'a', 0, 0)"
    )
    conn.execute(
        "INSERT INTO projects (id, provider, slug, display_name, first_seen, last_modified) "
        "VALUES (2, 'codex', 'b', 'b', 0, 0)"
    )
    conn.executemany(
        "INSERT INTO command_day_mart (day, project_id, command_count) VALUES (?, ?, ?)",
        [("2026-04-01", 1, 3), ("2026-04-01", 2, 4), ("2026-04-02", 1, 5)],
    )
    conn.commit()
    conn.close()
    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)
    monkeypatch.setattr("stackunderflow.deps.current_log_path", None)

    payload = await get_commands_daily()
    assert payload["scope"] == "global"
    # 2026-04-01 sums project 1 (3) + project 2 (4) = 7; 2026-04-02 = 5.
    assert payload["daily"] == [
        {"date": "2026-04-01", "commands": 7},
        {"date": "2026-04-02", "commands": 5},
    ]
    assert payload["total"] == 12


@pytest.mark.asyncio
async def test_commands_daily_empty_when_mart_unbuilt(tmp_path, monkeypatch):
    """Mart not yet backfilled → empty series (caller falls back to lifetime)."""
    store_db = tmp_path / "store.db"
    conn = db.connect(store_db)
    schema.apply(conn)
    conn.close()
    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)
    monkeypatch.setattr("stackunderflow.deps.current_log_path", None)

    payload = await get_commands_daily()
    assert payload == {"daily": [], "total": 0, "scope": "global"}


def test_commands_daily_route_registered_on_app():
    from stackunderflow.server import app

    from tests.conftest import app_route_paths

    assert "/api/commands/daily" in app_route_paths(app)
