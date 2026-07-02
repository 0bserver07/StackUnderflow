"""Tests for ``GET /api/patterns`` — the cross-session coding-health route."""

from __future__ import annotations

import json
from datetime import UTC, datetime, timedelta

import pytest
from fastapi import HTTPException

from stackunderflow.routes import patterns as patterns_route
from stackunderflow.store import db, schema


def _iso(days_ago: float, minutes: int = 0) -> str:
    return (
        datetime.now(UTC) - timedelta(days=days_ago) + timedelta(minutes=minutes)
    ).isoformat()


def _seed(store_db, *, slug="demo"):
    """One project with a recurring Edit failure on /repo/auth.py in 2 of 3
    sessions (touches in the mart, errors in messages)."""
    conn = db.connect(store_db)
    schema.apply(conn)
    conn.execute(
        "INSERT INTO projects (provider, slug, display_name, first_seen, last_modified) "
        "VALUES ('claude', ?, ?, 0, 0)",
        (slug, slug),
    )
    pid = conn.execute("SELECT id FROM projects WHERE slug = ?", (slug,)).fetchone()["id"]

    for i in (1, 2, 3):
        sid_txt = f"{slug}-s{i}"
        conn.execute(
            "INSERT INTO sessions (project_id, session_id, message_count) VALUES (?, ?, 0)",
            (pid, sid_txt),
        )
        sfk = conn.execute(
            "SELECT id FROM sessions WHERE project_id = ? AND session_id = ?",
            (pid, sid_txt),
        ).fetchone()["id"]
        conn.execute(
            "INSERT INTO message_tool_mart "
            "(message_id, project_id, session_id, ts, day, tool_name, file_path, byte_count, call_index) "
            "VALUES (?, ?, ?, ?, ?, 'Edit', '/repo/auth.py', NULL, 0)",
            (9000 + i, pid, sid_txt, _iso(5, i), _iso(5, i)[:10]),
        )
        if i <= 2:  # failing sessions
            tu = f"tu-{slug}-{i}"
            calls = [{"id": tu, "name": "Edit", "input": {"file_path": "/repo/auth.py"}}]
            raw_call = {
                "type": "assistant",
                "message": {
                    "role": "assistant",
                    "content": [
                        {"type": "tool_use", "id": tu, "name": "Edit",
                         "input": {"file_path": "/repo/auth.py"}}
                    ],
                },
            }
            conn.execute(
                "INSERT INTO messages (session_fk, seq, timestamp, role, model, "
                " input_tokens, output_tokens, cache_create_tokens, cache_read_tokens, "
                " content_text, tools_json, raw_json, is_sidechain, uuid, parent_uuid, speed) "
                "VALUES (?, 1, ?, 'assistant', '', 0, 0, 0, 0, '', ?, ?, 0, '', NULL, 'standard')",
                (sfk, _iso(5, i), json.dumps(calls), json.dumps(raw_call)),
            )
            raw_err = {
                "type": "user",
                "message": {
                    "role": "user",
                    "content": [
                        {"type": "tool_result", "tool_use_id": tu, "is_error": True,
                         "content": "String to replace not found in /repo/auth.py."}
                    ],
                },
            }
            conn.execute(
                "INSERT INTO messages (session_fk, seq, timestamp, role, model, "
                " input_tokens, output_tokens, cache_create_tokens, cache_read_tokens, "
                " content_text, tools_json, raw_json, is_sidechain, uuid, parent_uuid, speed) "
                "VALUES (?, 2, ?, 'user', '', 0, 0, 0, 0, ?, '[]', ?, 0, '', NULL, 'standard')",
                (sfk, _iso(5, i + 1),
                 "String to replace not found in /repo/auth.py.", json.dumps(raw_err)),
            )
    conn.commit()
    conn.close()


@pytest.mark.asyncio
async def test_patterns_route_returns_mined_report(tmp_path, monkeypatch):
    store_db = tmp_path / "store.db"
    _seed(store_db)
    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)
    monkeypatch.setattr("stackunderflow.deps.current_log_path", None)

    body = await patterns_route.get_patterns(project="demo")

    assert body["project"] == "demo"
    assert body["since"] == "90d"
    report = body["report"]
    assert report["window"]["days"] == 90
    assert report["sources"]["message_tool_mart"] is True

    entry = report["file_risk"][0]
    assert entry["path"] == "/repo/auth.py"
    assert entry["touch_session_count"] == 3
    assert entry["failure_session_count"] == 2
    assert entry["failure_rate"] == round(2 / 3, 4)  # report rounds rates to 4dp

    sig = report["error_signatures"][0]
    assert sig["category"] == "Content Not Found"
    assert sig["session_count"] == 2


@pytest.mark.asyncio
async def test_patterns_route_default_scope_uses_current_log_path(tmp_path, monkeypatch):
    store_db = tmp_path / "store.db"
    _seed(store_db, slug="demo")
    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)
    # Active dashboard project — its basename is the slug.
    monkeypatch.setattr("stackunderflow.deps.current_log_path", "/logs/demo")

    body = await patterns_route.get_patterns()
    assert body["project"] == "demo"
    assert body["report"]["file_risk"][0]["path"] == "/repo/auth.py"


@pytest.mark.asyncio
async def test_patterns_route_unknown_slug_is_empty_not_500(tmp_path, monkeypatch):
    store_db = tmp_path / "store.db"
    _seed(store_db)
    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)
    monkeypatch.setattr("stackunderflow.deps.current_log_path", None)

    body = await patterns_route.get_patterns(project="does-not-exist")
    assert body["project"] == "does-not-exist"
    assert body["report"]["file_risk"] == []
    assert body["report"]["totals"]["session_count"] == 0


@pytest.mark.asyncio
async def test_patterns_route_whole_store_when_no_project(tmp_path, monkeypatch):
    store_db = tmp_path / "store.db"
    _seed(store_db)
    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)
    monkeypatch.setattr("stackunderflow.deps.current_log_path", None)

    body = await patterns_route.get_patterns()
    assert body["project"] is None
    assert body["report"]["file_risk"][0]["path"] == "/repo/auth.py"


@pytest.mark.asyncio
async def test_patterns_route_since_window_is_applied(tmp_path, monkeypatch):
    store_db = tmp_path / "store.db"
    _seed(store_db)  # data sits ~5 days back
    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)
    monkeypatch.setattr("stackunderflow.deps.current_log_path", None)

    wide = await patterns_route.get_patterns(project="demo", since="30d")
    narrow = await patterns_route.get_patterns(project="demo", since="2d")

    assert wide["since"] == "30d"
    assert wide["report"]["window"]["days"] == 30
    assert wide["report"]["file_risk"]
    assert narrow["since"] == "2d"
    assert narrow["report"]["file_risk"] == []
    assert narrow["report"]["totals"]["error_count"] == 0


@pytest.mark.asyncio
async def test_patterns_route_rejects_bad_since(tmp_path, monkeypatch):
    monkeypatch.setattr("stackunderflow.deps.store_path", tmp_path / "store.db")
    monkeypatch.setattr("stackunderflow.deps.current_log_path", None)
    for bad in ("banana", "0d", "9999d", "-3d", "30", "d", "30days"):
        with pytest.raises(HTTPException) as exc:
            await patterns_route.get_patterns(since=bad)
        assert exc.value.status_code == 400


@pytest.mark.asyncio
async def test_patterns_route_empty_store_is_wellformed(tmp_path, monkeypatch):
    store_db = tmp_path / "store.db"
    conn = db.connect(store_db)
    schema.apply(conn)
    conn.close()
    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)
    monkeypatch.setattr("stackunderflow.deps.current_log_path", None)

    body = await patterns_route.get_patterns()
    assert body["report"]["file_risk"] == []
    assert body["report"]["error_signatures"] == []
    assert body["report"]["command_clusters"] == []
    assert body["report"]["totals"]["error_count"] == 0


def test_patterns_route_registered_on_app():
    """The route is wired into the FastAPI app in server.py."""
    from stackunderflow.server import app
    from tests.conftest import app_route_paths

    assert "/api/patterns" in app_route_paths(app)
