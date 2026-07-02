"""Tests for ``/api/worktrees`` — the worktree-intelligence route.

``services/worktrees.py`` is owned by a parallel campaign agent (wt-core),
so these tests monkeypatch its two entry points at the ROUTE module's
import site — but they construct REAL ``WorktreeInfo`` objects, so any
drift in the agreed dataclass contract (fields renamed, removed, or made
required) breaks loudly here instead of silently passing against a fake.
"""

from __future__ import annotations

from datetime import datetime

import pytest

from stackunderflow.routes import worktrees as worktrees_route
from stackunderflow.services.worktrees import WorktreeInfo
from stackunderflow.store import db, schema

_USD = {"code": "USD", "symbol": "$", "rate_from_usd": 1.0, "warning": None}


def _info(**overrides) -> WorktreeInfo:
    """A real ``WorktreeInfo`` with plausible defaults, overridable per test."""
    base = {
        "path": "/repo/.claude/worktrees/agent-abc",
        "branch": "worktree-agent-abc",
        "head": "0123abcd",
        "parent_repo": "/repo",
        "parent_slug": "-repo",
        "dirty_count": 0,
        "unique_commits": 0,
        "age_days": 3.5,
        "verdict": "MERGED_SAFE_TO_PRUNE",
        "sessions": 2,
        "cost_usd": 1.25,
        "prune_commands": [
            "git -C /repo worktree remove .claude/worktrees/agent-abc",
            "git -C /repo branch -D worktree-agent-abc",
        ],
    }
    base.update(overrides)
    return WorktreeInfo(**base)


def _setup(tmp_path, monkeypatch, infos, *, currency=_USD, calls=None):
    """Fresh schema-applied tmp store + mocked service layer.

    ``calls`` (a list) records every ``project_root`` the route passes to
    ``list_worktrees``. ``currency=None`` leaves the live helper in place.
    """
    store_db = tmp_path / "store.db"
    conn = db.connect(store_db)
    schema.apply(conn)
    conn.close()
    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)
    monkeypatch.setattr("stackunderflow.deps.current_log_path", None)

    def fake_list(conn, project_root=None):
        if calls is not None:
            calls.append(project_root)
        return list(infos)

    monkeypatch.setattr(worktrees_route, "list_worktrees", fake_list)
    if currency is not None:
        monkeypatch.setattr(
            worktrees_route, "active_currency_payload", lambda: dict(currency)
        )


# ── GET /api/worktrees ────────────────────────────────────────────────────────


@pytest.mark.asyncio
async def test_worktrees_route_payload_shape(tmp_path, monkeypatch):
    wt = _info()
    _setup(tmp_path, monkeypatch, [wt])

    body = await worktrees_route.get_worktrees()

    assert set(body.keys()) == {"scope", "worktrees", "summary", "scanned_at", "currency"}
    assert body["scope"] == "store"
    assert body["worktrees"] == [wt.to_dict()]
    # Prune commands survive serialization as a list — they are the preview.
    assert body["worktrees"][0]["prune_commands"] == wt.prune_commands
    assert body["currency"]["code"] == "USD"


@pytest.mark.asyncio
async def test_worktrees_route_summary_math(tmp_path, monkeypatch):
    infos = [
        _info(path="/r/wt-a", verdict="ACTIVE", cost_usd=1.0),
        _info(path="/r/wt-b", verdict="MERGED_SAFE_TO_PRUNE", cost_usd=2.0),
        _info(path="/r/wt-c", verdict="MERGED_SAFE_TO_PRUNE", cost_usd=3.0),
        _info(path="/r/wt-d", verdict="HAS_UNIQUE_WORK", cost_usd=4.0),
    ]
    _setup(tmp_path, monkeypatch, infos)

    body = await worktrees_route.get_worktrees()

    assert body["summary"] == {
        "total": 4,
        "safe_to_prune": 2,
        "has_unique_work": 1,
        "active": 1,
        "attributed_cost_usd": pytest.approx(10.0),
    }


@pytest.mark.asyncio
async def test_worktrees_route_unknown_verdict_counts_in_total_only(tmp_path, monkeypatch):
    """A verdict outside the agreed enum is never silently tallied into a bucket."""
    _setup(tmp_path, monkeypatch, [_info(verdict="SOMETHING_NEW", cost_usd=5.0)])

    body = await worktrees_route.get_worktrees()

    assert body["summary"]["total"] == 1
    assert body["summary"]["active"] == 0
    assert body["summary"]["safe_to_prune"] == 0
    assert body["summary"]["has_unique_work"] == 0
    assert body["summary"]["attributed_cost_usd"] == pytest.approx(5.0)


@pytest.mark.asyncio
async def test_worktrees_route_empty_store_is_wellformed(tmp_path, monkeypatch):
    _setup(tmp_path, monkeypatch, [])

    body = await worktrees_route.get_worktrees()

    assert body["scope"] == "store"
    assert body["worktrees"] == []
    assert body["summary"] == {
        "total": 0,
        "safe_to_prune": 0,
        "has_unique_work": 0,
        "active": 0,
        "attributed_cost_usd": 0.0,
    }


@pytest.mark.asyncio
async def test_worktrees_route_explicit_log_path_wins(tmp_path, monkeypatch):
    calls: list = []
    _setup(tmp_path, monkeypatch, [], calls=calls)
    monkeypatch.setattr("stackunderflow.deps.current_log_path", "/logs/other")

    body = await worktrees_route.get_worktrees(log_path="/logs/demo")

    assert calls == ["/logs/demo"]
    assert body["scope"] == "/logs/demo"


@pytest.mark.asyncio
async def test_worktrees_route_falls_back_to_current_log_path(tmp_path, monkeypatch):
    calls: list = []
    _setup(tmp_path, monkeypatch, [], calls=calls)
    monkeypatch.setattr("stackunderflow.deps.current_log_path", "/logs/demo")

    body = await worktrees_route.get_worktrees()

    assert calls == ["/logs/demo"]
    assert body["scope"] == "/logs/demo"


@pytest.mark.asyncio
async def test_worktrees_route_whole_store_when_no_project(tmp_path, monkeypatch):
    """No param + no active project → project_root=None (scan all known roots).

    Calling the handler directly also exercises the Query-sentinel coercion:
    the raw ``Query(None)`` default must be treated as "no path".
    """
    calls: list = []
    _setup(tmp_path, monkeypatch, [], calls=calls)

    body = await worktrees_route.get_worktrees()

    assert calls == [None]
    assert body["scope"] == "store"


@pytest.mark.asyncio
async def test_worktrees_route_converts_currency(tmp_path, monkeypatch):
    """Force a 2x FX rate: every cost_usd field and the summary must scale."""
    infos = [
        _info(path="/r/wt-a", cost_usd=1.25),
        _info(path="/r/wt-b", cost_usd=2.75),
    ]
    _setup(
        tmp_path,
        monkeypatch,
        infos,
        currency={"code": "EUR", "symbol": "€", "rate_from_usd": 2.0, "warning": None},
    )

    body = await worktrees_route.get_worktrees()

    assert body["worktrees"][0]["cost_usd"] == pytest.approx(2.50)
    assert body["worktrees"][1]["cost_usd"] == pytest.approx(5.50)
    assert body["summary"]["attributed_cost_usd"] == pytest.approx(8.0)
    assert body["currency"]["code"] == "EUR"


@pytest.mark.asyncio
async def test_worktrees_route_scanned_at_is_iso_utc(tmp_path, monkeypatch):
    _setup(tmp_path, monkeypatch, [])

    body = await worktrees_route.get_worktrees()

    scanned = datetime.fromisoformat(body["scanned_at"])
    assert scanned.tzinfo is not None


# ── POST /api/worktrees/attribute ─────────────────────────────────────────────


@pytest.mark.asyncio
async def test_worktrees_attribute_returns_updated_count(tmp_path, monkeypatch):
    _setup(tmp_path, monkeypatch, [])
    monkeypatch.setattr(worktrees_route, "attribute_fragments", lambda conn: 3)

    body = await worktrees_route.post_attribute()

    assert body == {"updated": 3}


@pytest.mark.asyncio
async def test_worktrees_attribute_idempotent_second_run(tmp_path, monkeypatch):
    """Idempotency contract: once linked, a re-POST reports 0 rows updated."""
    _setup(tmp_path, monkeypatch, [])
    results = iter([2, 0])
    monkeypatch.setattr(
        worktrees_route, "attribute_fragments", lambda conn: next(results)
    )

    assert (await worktrees_route.post_attribute()) == {"updated": 2}
    assert (await worktrees_route.post_attribute()) == {"updated": 0}


# ── registration ──────────────────────────────────────────────────────────────


def test_worktrees_routes_registered_on_app():
    """Both endpoints are wired into the FastAPI app in server.py."""
    from stackunderflow.server import app
    from tests.conftest import app_route_paths

    paths = app_route_paths(app)
    assert "/api/worktrees" in paths
    assert "/api/worktrees/attribute" in paths
