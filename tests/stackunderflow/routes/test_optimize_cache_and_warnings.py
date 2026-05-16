"""Tests for the /api/optimize response cache + mart-backfill warnings.

Covers:
* Identical args hit the in-process cache (heavy work runs once).
* ``?force=true`` bypasses the cache for that call.
* Store mtime change invalidates the entry naturally.
* When ``message_tool_mart`` is empty the response carries a
  ``warnings[].code == "mart_empty"`` hint.
"""

from __future__ import annotations

import os
import time

import pytest

from stackunderflow.routes import optimize as optimize_route
from stackunderflow.store import db, schema


def _seed_minimal_store(store_db):
    """A store with one project — enough to drive the optimize detectors."""
    conn = db.connect(store_db)
    schema.apply(conn)
    conn.execute(
        "INSERT INTO projects (provider, slug, display_name, first_seen, last_modified) "
        "VALUES ('claude', 'demo-proj', 'demo-proj', 0, 0)"
    )
    conn.commit()
    conn.close()


def _patch_route(monkeypatch, store_db):
    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)
    optimize_route.invalidate_optimize_cache()


# ── cache behaviour ─────────────────────────────────────────────────────────


@pytest.mark.asyncio
async def test_cache_hit_skips_heavy_work(tmp_path, monkeypatch):
    store_db = tmp_path / "store.db"
    _seed_minimal_store(store_db)
    _patch_route(monkeypatch, store_db)

    calls: list[int] = []

    def _fake_patterns(conn, **kw):  # noqa: ARG001
        calls.append(1)
        return []

    def _fake_waste(conn, **kw):  # noqa: ARG001
        return []

    monkeypatch.setattr(
        "stackunderflow.routes.optimize.find_patterns", _fake_patterns,
    )
    monkeypatch.setattr(
        "stackunderflow.routes.optimize.find_waste", _fake_waste,
    )

    cold = await optimize_route.get_optimize_report()
    hot1 = await optimize_route.get_optimize_report()
    hot2 = await optimize_route.get_optimize_report()

    assert len(calls) == 1, "expected heavy work to run exactly once"
    assert cold["cache"] == "miss"
    assert hot1["cache"] == "hit"
    assert hot2["cache"] == "hit"
    # Hot response carries the same payload minus the cache marker.
    cold_copy = dict(cold)
    cold_copy.pop("cache")
    hot_copy = dict(hot1)
    hot_copy.pop("cache")
    assert hot_copy == cold_copy


@pytest.mark.asyncio
async def test_force_bypasses_cache(tmp_path, monkeypatch):
    store_db = tmp_path / "store.db"
    _seed_minimal_store(store_db)
    _patch_route(monkeypatch, store_db)

    calls: list[int] = []

    def _fake_patterns(conn, **kw):  # noqa: ARG001
        calls.append(1)
        return []

    def _fake_waste(conn, **kw):  # noqa: ARG001
        return []

    monkeypatch.setattr(
        "stackunderflow.routes.optimize.find_patterns", _fake_patterns,
    )
    monkeypatch.setattr(
        "stackunderflow.routes.optimize.find_waste", _fake_waste,
    )

    await optimize_route.get_optimize_report()
    await optimize_route.get_optimize_report(force=True)
    await optimize_route.get_optimize_report(force=True)

    # Two forced calls + the initial cold one = 3 invocations.
    assert len(calls) == 3


@pytest.mark.asyncio
async def test_distinct_args_keep_separate_cache_entries(tmp_path, monkeypatch):
    store_db = tmp_path / "store.db"
    _seed_minimal_store(store_db)
    _patch_route(monkeypatch, store_db)

    calls: list[int] = []

    def _fake_patterns(conn, **kw):  # noqa: ARG001
        calls.append(1)
        return []

    def _fake_waste(conn, **kw):  # noqa: ARG001
        return []

    monkeypatch.setattr(
        "stackunderflow.routes.optimize.find_patterns", _fake_patterns,
    )
    monkeypatch.setattr(
        "stackunderflow.routes.optimize.find_waste", _fake_waste,
    )

    await optimize_route.get_optimize_report(period="30days")
    await optimize_route.get_optimize_report(period="7days")
    # Different period == different key, so 2 cold misses.
    assert len(calls) == 2

    # Repeating each → both hit the cache.
    await optimize_route.get_optimize_report(period="30days")
    await optimize_route.get_optimize_report(period="7days")
    assert len(calls) == 2


@pytest.mark.asyncio
async def test_store_mtime_change_invalidates_entry(tmp_path, monkeypatch):
    store_db = tmp_path / "store.db"
    _seed_minimal_store(store_db)
    _patch_route(monkeypatch, store_db)

    calls: list[int] = []

    def _fake_patterns(conn, **kw):  # noqa: ARG001
        calls.append(1)
        return []

    def _fake_waste(conn, **kw):  # noqa: ARG001
        return []

    monkeypatch.setattr(
        "stackunderflow.routes.optimize.find_patterns", _fake_patterns,
    )
    monkeypatch.setattr(
        "stackunderflow.routes.optimize.find_waste", _fake_waste,
    )

    await optimize_route.get_optimize_report()
    await optimize_route.get_optimize_report()  # hit
    assert len(calls) == 1

    # Bump mtime artificially — the file-based signature changes and the
    # cache entry no longer matches.
    new_mtime = time.time() + 60
    os.utime(store_db, (new_mtime, new_mtime))
    await optimize_route.get_optimize_report()
    assert len(calls) == 2, "mtime change should have invalidated the cache"


# ── warnings: mart_empty hint ───────────────────────────────────────────────


@pytest.mark.asyncio
async def test_mart_empty_emits_warning(tmp_path, monkeypatch):
    store_db = tmp_path / "store.db"
    _seed_minimal_store(store_db)
    _patch_route(monkeypatch, store_db)

    monkeypatch.setattr(
        "stackunderflow.routes.optimize.find_patterns", lambda conn, **kw: [],
    )
    monkeypatch.setattr(
        "stackunderflow.routes.optimize.find_waste", lambda conn, **kw: [],
    )

    payload = await optimize_route.get_optimize_report()

    # Fresh store has no message_tool_mart rows → mart_empty warning fires.
    codes = {w["code"] for w in payload["warnings"]}
    assert "mart_empty" in codes


@pytest.mark.asyncio
async def test_warnings_empty_when_mart_populated(tmp_path, monkeypatch):
    store_db = tmp_path / "store.db"
    _seed_minimal_store(store_db)
    _patch_route(monkeypatch, store_db)

    # Insert one message_tool_mart row to trip mart_has_message_tool_rows().
    conn = db.connect(store_db)
    conn.execute(
        "INSERT INTO message_tool_mart "
        "(message_id, project_id, session_id, ts, day, tool_name, "
        " file_path, byte_count, call_index) "
        "VALUES (1, 1, 's1', '2026-04-01T00:00:00Z', '2026-04-01', "
        " 'Read', NULL, NULL, 0)"
    )
    conn.commit()
    conn.close()

    monkeypatch.setattr(
        "stackunderflow.routes.optimize.find_patterns", lambda conn, **kw: [],
    )
    monkeypatch.setattr(
        "stackunderflow.routes.optimize.find_waste", lambda conn, **kw: [],
    )

    payload = await optimize_route.get_optimize_report()
    codes = {w["code"] for w in payload["warnings"]}
    assert "mart_empty" not in codes
