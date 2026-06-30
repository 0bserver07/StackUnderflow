"""Audit #12 — ``GET /api/projects`` server-side pagination.

Covers the ``limit`` / ``offset`` / ``total_count`` / ``has_more`` contract the
frontend pages against, the default-cap + hard-max clamping rules, and a
pagination-path perf guard so the mart fast-path stays under 100ms when a page
slice is requested (the slice must not reintroduce a full per-project scan).
"""

from __future__ import annotations

import json
import time

import pytest

from stackunderflow.routes.projects import (
    PROJECTS_DEFAULT_LIMIT,
    PROJECTS_MAX_LIMIT,
    get_projects,
)
from stackunderflow.store import db, schema


def _connect(store_db):
    conn = db.connect(store_db)
    schema.apply(conn)
    return conn


def _insert_project(conn, *, provider, slug, last_modified=0.0):
    cur = conn.execute(
        "INSERT INTO projects (provider, slug, display_name, first_seen, last_modified) "
        "VALUES (?, ?, ?, ?, ?)",
        (provider, slug, slug, 0.0, last_modified),
    )
    return int(cur.lastrowid)


def _insert_project_mart(conn, *, project_id, provider, slug, **kw):
    conn.execute(
        "INSERT INTO project_mart "
        "(project_id, provider, slug, display_name, first_ts, last_ts, "
        " total_messages, total_sessions, total_input_tokens, total_output_tokens, "
        " total_cache_read, total_cache_create, total_cost_usd, "
        " total_user_messages, total_assistant_messages, total_tool_use_messages, "
        " total_tool_result_messages, total_commands) "
        "VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        (
            project_id,
            provider,
            slug,
            slug,
            kw.get("first_ts"),
            kw.get("last_ts"),
            kw.get("total_messages", 0),
            kw.get("total_sessions", 0),
            kw.get("total_input_tokens", 0),
            kw.get("total_output_tokens", 0),
            kw.get("total_cache_read", 0),
            kw.get("total_cache_create", 0),
            kw.get("total_cost_usd", 0.0),
            kw.get("total_user_messages", 0),
            kw.get("total_assistant_messages", 0),
            kw.get("total_tool_use_messages", 0),
            kw.get("total_tool_result_messages", 0),
            kw.get("total_commands", 0),
        ),
    )


def _seed_n(store_db, n, *, with_mart=False):
    """Seed ``n`` projects ``proj-000..`` with ascending ``last_modified``.

    Default sort is ``last_modified`` descending, so the API returns them
    newest-first: ``proj-{n-1} .. proj-0``.
    """
    conn = _connect(store_db)
    for i in range(n):
        pid = _insert_project(conn, provider="claude", slug=f"proj-{i:03d}", last_modified=float(i))
        if with_mart:
            _insert_project_mart(
                conn,
                project_id=pid,
                provider="claude",
                slug=f"proj-{i:03d}",
                total_input_tokens=1000,
                total_cost_usd=0.1,
            )
    conn.commit()
    conn.close()


async def _call(**kw):
    response = await get_projects(**kw)
    return json.loads(response.body.decode("utf-8"))


# ── defaults: omitting limit preserves the "all projects" response ───────────


@pytest.mark.asyncio
async def test_default_returns_all_with_total_and_echoed_bounds(tmp_path, monkeypatch):
    store_db = tmp_path / "store.db"
    _seed_n(store_db, 3)
    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)

    body = await _call()

    assert len(body["projects"]) == 3
    assert body["total_count"] == 3
    assert body["has_more"] is False
    # Omitted limit resolves to the large default cap, offset to 0.
    assert body["limit"] == PROJECTS_DEFAULT_LIMIT
    assert body["offset"] == 0


# ── limit / offset slice the result and report has_more ──────────────────────


@pytest.mark.asyncio
async def test_limit_returns_first_page_and_flags_more(tmp_path, monkeypatch):
    store_db = tmp_path / "store.db"
    _seed_n(store_db, 5)
    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)

    body = await _call(limit=2)

    assert [p["dir_name"] for p in body["projects"]] == ["proj-004", "proj-003"]
    assert body["total_count"] == 5
    assert body["limit"] == 2
    assert body["offset"] == 0
    assert body["has_more"] is True


@pytest.mark.asyncio
async def test_offset_pages_through_to_the_last_page(tmp_path, monkeypatch):
    store_db = tmp_path / "store.db"
    _seed_n(store_db, 5)
    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)

    page2 = await _call(limit=2, offset=2)
    assert [p["dir_name"] for p in page2["projects"]] == ["proj-002", "proj-001"]
    assert page2["has_more"] is True

    page3 = await _call(limit=2, offset=4)
    assert [p["dir_name"] for p in page3["projects"]] == ["proj-000"]
    assert page3["total_count"] == 5
    assert page3["has_more"] is False


@pytest.mark.asyncio
async def test_walking_pages_covers_every_project_once(tmp_path, monkeypatch):
    store_db = tmp_path / "store.db"
    _seed_n(store_db, 7)
    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)

    seen: list[str] = []
    offset = 0
    while True:
        body = await _call(limit=3, offset=offset)
        seen.extend(p["dir_name"] for p in body["projects"])
        if not body["has_more"]:
            break
        offset += body["limit"]

    assert sorted(seen) == [f"proj-{i:03d}" for i in range(7)]
    assert len(seen) == 7  # no overlap, no gaps


# ── clamping: hard max, min 1, non-negative offset ───────────────────────────


@pytest.mark.asyncio
async def test_limit_is_clamped_to_hard_max(tmp_path, monkeypatch):
    store_db = tmp_path / "store.db"
    _seed_n(store_db, 1)
    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)

    body = await _call(limit=10_000_000)

    assert body["limit"] == PROJECTS_MAX_LIMIT
    assert len(body["projects"]) == 1


@pytest.mark.asyncio
async def test_limit_zero_is_clamped_up_to_one(tmp_path, monkeypatch):
    store_db = tmp_path / "store.db"
    _seed_n(store_db, 3)
    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)

    body = await _call(limit=0)

    assert body["limit"] == 1
    assert len(body["projects"]) == 1
    assert body["total_count"] == 3
    assert body["has_more"] is True


@pytest.mark.asyncio
async def test_negative_offset_floors_to_zero(tmp_path, monkeypatch):
    store_db = tmp_path / "store.db"
    _seed_n(store_db, 3)
    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)

    body = await _call(limit=2, offset=-50)

    assert body["offset"] == 0
    assert [p["dir_name"] for p in body["projects"]] == ["proj-002", "proj-001"]


@pytest.mark.asyncio
async def test_offset_past_end_returns_empty_page_with_total(tmp_path, monkeypatch):
    store_db = tmp_path / "store.db"
    _seed_n(store_db, 2)
    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)

    body = await _call(limit=2, offset=10)

    assert body["projects"] == []
    assert body["total_count"] == 2
    assert body["has_more"] is False


# ── stats resolve only for the page, and the mart fast-path stays fast ────────


@pytest.mark.asyncio
async def test_stats_present_only_for_returned_page(tmp_path, monkeypatch):
    store_db = tmp_path / "store.db"
    _seed_n(store_db, 5, with_mart=True)
    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)

    body = await _call(include_stats=True, limit=2)

    assert len(body["projects"]) == 2
    for proj in body["projects"]:
        assert proj["stats"] is not None
        assert proj["stats"]["total_input_tokens"] == 1000


@pytest.mark.asyncio
async def test_paginated_mart_path_under_100ms(tmp_path, monkeypatch):
    """A page request over a 100-project mart store stays under 100ms — the
    slice must not reintroduce a full per-project pipeline scan."""
    store_db = tmp_path / "store.db"
    _seed_n(store_db, 100, with_mart=True)
    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)

    await get_projects(include_stats=True, limit=50)  # warm caches
    t0 = time.perf_counter()
    response = await get_projects(include_stats=True, limit=50)
    elapsed_ms = (time.perf_counter() - t0) * 1000

    body = json.loads(response.body.decode("utf-8"))
    assert len(body["projects"]) == 50
    assert body["total_count"] == 100
    assert body["has_more"] is True
    assert elapsed_ms < 100, f"slow: {elapsed_ms:.1f}ms"
