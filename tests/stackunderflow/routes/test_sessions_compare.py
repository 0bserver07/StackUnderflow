"""Tests for GET /api/sessions/compare — analytics-expansion spec §1.10."""
from __future__ import annotations

import pytest
from fastapi import HTTPException

from stackunderflow.routes.sessions import compare_sessions
from stackunderflow.store import db, schema


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


_SESSION_A = {
    "session_id": "sess-a",
    "started_at": "2026-02-01T00:00:00Z",
    "ended_at":   "2026-02-01T00:10:00Z",
    "duration_s": 600.0,
    "cost": 1.0,
    "tokens": {"input": 1000, "output": 500, "cache_creation": 0, "cache_read": 0},
    "messages": 10,
    "commands": 3,
    "errors": 0,
    "first_prompt_preview": "start of session A",
    "models_used": ["claude-sonnet-4-20250514"],
}

_SESSION_B = {
    "session_id": "sess-b",
    "started_at": "2026-02-02T00:00:00Z",
    "ended_at":   "2026-02-02T00:30:00Z",
    "duration_s": 1800.0,
    "cost": 3.0,
    "tokens": {"input": 4000, "output": 1500, "cache_creation": 0, "cache_read": 200},
    "messages": 25,
    "commands": 8,
    "errors": 2,
    "first_prompt_preview": "start of session B",
    "models_used": ["claude-sonnet-4-20250514"],
}


@pytest.mark.asyncio
async def test_compare_returns_a_b_and_diff(tmp_path, monkeypatch):
    store_db = tmp_path / "store.db"
    slug = "-test-compare-proj"
    _seed_project(store_db, slug)

    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)
    monkeypatch.setattr("stackunderflow.deps.current_log_path", f"/fake/{slug}")

    def fake_stats(conn, *, project_id, tz_offset=0):  # noqa: ARG001
        return [], {"session_costs": [_SESSION_A, _SESSION_B]}

    monkeypatch.setattr(
        "stackunderflow.routes.sessions.queries.get_project_stats",
        fake_stats,
    )

    resp = await compare_sessions(a="sess-a", b="sess-b")
    import json
    body = json.loads(resp.body)

    assert body["a"]["session_id"] == "sess-a"
    assert body["b"]["session_id"] == "sess-b"
    # diff = b - a
    assert body["diff"]["cost"] == pytest.approx(2.0)
    assert body["diff"]["commands"] == 5
    assert body["diff"]["errors"] == 2
    assert body["diff"]["duration_s"] == pytest.approx(1200.0)
    assert body["diff"]["tokens"]["input"] == 3000
    assert body["diff"]["tokens"]["cache_read"] == 200


@pytest.mark.asyncio
async def test_compare_404_when_session_missing(tmp_path, monkeypatch):
    store_db = tmp_path / "store.db"
    slug = "-test-missing-proj"
    _seed_project(store_db, slug)

    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)
    monkeypatch.setattr("stackunderflow.deps.current_log_path", f"/fake/{slug}")

    def fake_stats(conn, *, project_id, tz_offset=0):  # noqa: ARG001
        return [], {"session_costs": [_SESSION_A]}

    monkeypatch.setattr(
        "stackunderflow.routes.sessions.queries.get_project_stats",
        fake_stats,
    )

    with pytest.raises(HTTPException) as exc_info:
        await compare_sessions(a="sess-a", b="missing-session")
    assert exc_info.value.status_code == 404
    assert "missing-session" in exc_info.value.detail


@pytest.mark.asyncio
async def test_compare_400_when_no_project(monkeypatch):
    monkeypatch.setattr("stackunderflow.deps.current_log_path", None)
    with pytest.raises(HTTPException) as exc_info:
        await compare_sessions(a="x", b="y")
    assert exc_info.value.status_code == 400


@pytest.mark.asyncio
async def test_compare_uses_log_path_query_over_current(tmp_path, monkeypatch):
    """Explicit log_path query wins over deps.current_log_path."""
    store_db = tmp_path / "store.db"
    slug = "-explicit-proj"
    _seed_project(store_db, slug)

    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)
    # current_log_path points somewhere bogus; query should still succeed
    monkeypatch.setattr("stackunderflow.deps.current_log_path", "/not/real")

    captured: dict = {}

    def fake_stats(conn, *, project_id, tz_offset=0):  # noqa: ARG001
        captured["project_id"] = project_id
        return [], {"session_costs": [_SESSION_A, _SESSION_B]}

    monkeypatch.setattr(
        "stackunderflow.routes.sessions.queries.get_project_stats",
        fake_stats,
    )

    resp = await compare_sessions(a="sess-a", b="sess-b", log_path=f"/whatever/{slug}")
    assert resp.status_code == 200
    assert captured.get("project_id") is not None
