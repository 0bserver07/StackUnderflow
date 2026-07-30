"""Tests for the in-process /api/dashboard-data memo cache.

Covers:
* hot hit returns the same payload as the cold miss
* a new session in the store changes the signature → cache invalidates
* /api/refresh + invalidate_dashboard_cache() drops the entry
"""
from __future__ import annotations

import pytest

from stackunderflow.routes import data as data_route
from stackunderflow.store import db, schema


def _seed_project(store_db, slug: str) -> int:
    conn = db.connect(store_db)
    schema.apply(conn)
    cur = conn.execute(
        "INSERT INTO projects (provider, slug, display_name, first_seen, last_modified) "
        "VALUES (?, ?, ?, ?, ?)",
        ("claude", slug, slug, 0.0, 0.0),
    )
    project_id = cur.lastrowid
    conn.commit()
    conn.close()
    assert project_id is not None
    return project_id


def _add_session(store_db, project_id: int, *, session_id: str, last_ts: str, n: int) -> None:
    conn = db.connect(store_db)
    conn.execute(
        "INSERT INTO sessions (project_id, session_id, first_ts, last_ts, message_count) "
        "VALUES (?, ?, ?, ?, ?)",
        (project_id, session_id, last_ts, last_ts, n),
    )
    conn.commit()
    conn.close()


def _fake_get_project_stats(call_log: list[int]):
    """Returns a stub that records call count and yields a deterministic payload."""

    def _impl(conn, *, project_id, tz_offset=0):  # noqa: ARG001
        call_log.append(1)
        return (
            [{"id": "m1", "role": "user", "content": "hi"}],
            {
                "overview": {"project_name": "demo"},
                "tools": {},
                "sessions": {"count": 1},
                "user_interactions": {"command_details": [{"x": 1}], "summary": "ok"},
            },
        )

    return _impl


@pytest.mark.asyncio
async def test_hot_hit_returns_same_payload_as_cold_miss(tmp_path, monkeypatch):
    store_db = tmp_path / "store.db"
    slug = "-cache-proj"
    project_id = _seed_project(store_db, slug)
    _add_session(store_db, project_id, session_id="s1", last_ts="2026-04-25T00:00:00Z", n=3)

    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)
    monkeypatch.setattr("stackunderflow.deps.current_log_path", f"/fake/{slug}")
    data_route.invalidate_dashboard_cache()

    calls: list[int] = []
    monkeypatch.setattr(
        "stackunderflow.routes.data.queries.get_project_stats",
        _fake_get_project_stats(calls),
    )

    cold = data_route.get_dashboard_data()
    hot1 = data_route.get_dashboard_data()
    hot2 = data_route.get_dashboard_data()

    # heavy work ran exactly once across the three calls
    assert len(calls) == 1, f"expected 1 cold miss, got {len(calls)}"
    # cached response is identical to the cold one
    assert hot1 == cold
    assert hot2 == cold
    # command_details was stripped by the §D1 lean-payload rule
    assert "command_details" not in hot1["statistics"]["user_interactions"]


@pytest.mark.asyncio
async def test_new_session_invalidates_cache(tmp_path, monkeypatch):
    store_db = tmp_path / "store.db"
    slug = "-invalidate-proj"
    project_id = _seed_project(store_db, slug)
    _add_session(store_db, project_id, session_id="s1", last_ts="2026-04-25T00:00:00Z", n=3)

    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)
    monkeypatch.setattr("stackunderflow.deps.current_log_path", f"/fake/{slug}")
    data_route.invalidate_dashboard_cache()

    calls: list[int] = []
    monkeypatch.setattr(
        "stackunderflow.routes.data.queries.get_project_stats",
        _fake_get_project_stats(calls),
    )

    data_route.get_dashboard_data()
    assert len(calls) == 1

    # add a brand new session — signature changes, cache must miss
    _add_session(store_db, project_id, session_id="s2", last_ts="2026-04-26T00:00:00Z", n=2)
    data_route.get_dashboard_data()
    assert len(calls) == 2, "new session should have invalidated the cache"

    # subsequent call hits the cache again
    data_route.get_dashboard_data()
    assert len(calls) == 2


@pytest.mark.asyncio
async def test_more_messages_in_existing_session_invalidates(tmp_path, monkeypatch):
    """Same session, but message_count or last_ts changes — must invalidate."""
    store_db = tmp_path / "store.db"
    slug = "-grow-proj"
    project_id = _seed_project(store_db, slug)
    _add_session(store_db, project_id, session_id="s1", last_ts="2026-04-25T00:00:00Z", n=3)

    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)
    monkeypatch.setattr("stackunderflow.deps.current_log_path", f"/fake/{slug}")
    data_route.invalidate_dashboard_cache()

    calls: list[int] = []
    monkeypatch.setattr(
        "stackunderflow.routes.data.queries.get_project_stats",
        _fake_get_project_stats(calls),
    )

    data_route.get_dashboard_data()
    assert len(calls) == 1

    # bump message_count — same key, but signature differs
    conn = db.connect(store_db)
    conn.execute(
        "UPDATE sessions SET message_count = ?, last_ts = ? WHERE session_id = ?",
        (10, "2026-04-25T01:00:00Z", "s1"),
    )
    conn.commit()
    conn.close()

    data_route.get_dashboard_data()
    assert len(calls) == 2, "growing the existing session must invalidate"


@pytest.mark.asyncio
async def test_explicit_invalidate_drops_entry(tmp_path, monkeypatch):
    store_db = tmp_path / "store.db"
    slug = "-explicit-proj"
    project_id = _seed_project(store_db, slug)
    _add_session(store_db, project_id, session_id="s1", last_ts="2026-04-25T00:00:00Z", n=3)

    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)
    monkeypatch.setattr("stackunderflow.deps.current_log_path", f"/fake/{slug}")
    data_route.invalidate_dashboard_cache()

    calls: list[int] = []
    monkeypatch.setattr(
        "stackunderflow.routes.data.queries.get_project_stats",
        _fake_get_project_stats(calls),
    )

    data_route.get_dashboard_data()
    data_route.get_dashboard_data()
    assert len(calls) == 1

    data_route.invalidate_dashboard_cache(slug)
    data_route.get_dashboard_data()
    assert len(calls) == 2

    data_route.invalidate_dashboard_cache()  # full clear
    data_route.get_dashboard_data()
    assert len(calls) == 3


@pytest.mark.asyncio
async def test_tz_offset_is_part_of_cache_key(tmp_path, monkeypatch):
    """Different tz_offset must miss separately — aggregator output depends on it."""
    store_db = tmp_path / "store.db"
    slug = "-tz-proj"
    project_id = _seed_project(store_db, slug)
    _add_session(store_db, project_id, session_id="s1", last_ts="2026-04-25T00:00:00Z", n=1)

    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)
    monkeypatch.setattr("stackunderflow.deps.current_log_path", f"/fake/{slug}")
    data_route.invalidate_dashboard_cache()

    calls: list[int] = []
    monkeypatch.setattr(
        "stackunderflow.routes.data.queries.get_project_stats",
        _fake_get_project_stats(calls),
    )

    data_route.get_dashboard_data(timezone_offset=0)
    data_route.get_dashboard_data(timezone_offset=300)
    data_route.get_dashboard_data(timezone_offset=0)
    data_route.get_dashboard_data(timezone_offset=300)
    assert len(calls) == 2, "each tz_offset should miss exactly once"
