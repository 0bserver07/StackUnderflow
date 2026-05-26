"""Wave 3A — ``/api/projects?include_stats=true`` reads from ``project_mart``."""

from __future__ import annotations

import json
import time

import pytest

from stackunderflow.routes.projects import get_projects
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
        " total_cache_read, total_cache_create, total_cost_usd) "
        "VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        (
            project_id, provider, slug, slug,
            kw.get("first_ts"), kw.get("last_ts"),
            kw.get("total_messages", 0), kw.get("total_sessions", 0),
            kw.get("total_input_tokens", 0), kw.get("total_output_tokens", 0),
            kw.get("total_cache_read", 0), kw.get("total_cache_create", 0),
            kw.get("total_cost_usd", 0.0),
        ),
    )


@pytest.mark.asyncio
async def test_projects_uses_project_mart_when_populated(tmp_path, monkeypatch):
    store_db = tmp_path / "store.db"
    conn = _connect(store_db)
    pid = _insert_project(conn, provider="claude", slug="alpha")
    _insert_project_mart(
        conn, project_id=pid, provider="claude", slug="alpha",
        total_input_tokens=12345, total_output_tokens=6789,
        total_cache_read=200, total_cache_create=100, total_cost_usd=2.5,
        first_ts="2026-04-01T00:00:00Z", last_ts="2026-04-30T00:00:00Z",
    )
    conn.commit()
    conn.close()
    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)
    response = await get_projects(include_stats=True)
    body = json.loads(response.body.decode("utf-8"))
    proj = body["projects"][0]
    stats = proj["stats"]
    assert stats["total_input_tokens"] == 12345
    assert stats["total_output_tokens"] == 6789
    assert stats["total_cost"] == pytest.approx(2.5)
    assert stats["first_message_date"] == "2026-04-01T00:00:00Z"


@pytest.mark.asyncio
async def test_projects_falls_back_when_mart_empty(tmp_path, monkeypatch):
    store_db = tmp_path / "store.db"
    conn = _connect(store_db)
    _insert_project(conn, provider="claude", slug="alpha")
    conn.commit()
    conn.close()
    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)
    response = await get_projects(include_stats=True)
    body = json.loads(response.body.decode("utf-8"))
    assert body["projects"][0]["stats"]["total_input_tokens"] == 0


@pytest.mark.asyncio
async def test_projects_route_under_100ms_with_100k_mart_rows(tmp_path, monkeypatch):
    store_db = tmp_path / "store.db"
    conn = _connect(store_db)
    pids = []
    for i in range(100):
        pid = _insert_project(conn, provider="claude", slug=f"proj-{i:03d}", last_modified=float(i))
        _insert_project_mart(conn, project_id=pid, provider="claude", slug=f"proj-{i:03d}",
            total_input_tokens=1000, total_cost_usd=0.1)
        pids.append(pid)
    rows = []
    for pid in pids:
        for d in range(1000):
            rows.append((f"2024-01-{(d % 28) + 1:02d}", pid, "claude", "claude-sonnet-4-5",
                         "standard", 1, 1, 0, 0, 1, 1, 0.001))
    conn.executemany(
        "INSERT OR IGNORE INTO daily_mart "
        "(day, project_id, provider, model, speed, input_tokens, output_tokens, "
        " cache_read, cache_create, message_count, session_count, cost_usd) "
        "VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)", rows)
    conn.commit()
    conn.close()
    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)
    await get_projects(include_stats=True)
    t0 = time.perf_counter()
    response = await get_projects(include_stats=True)
    elapsed_ms = (time.perf_counter() - t0) * 1000
    body = json.loads(response.body.decode("utf-8"))
    assert len(body["projects"]) == 100
    assert elapsed_ms < 100, f"slow: {elapsed_ms:.1f}ms"


@pytest.mark.asyncio
async def test_set_project_by_dir_bypasses_fs_if_in_db(tmp_path, monkeypatch):
    import stackunderflow.deps as deps
    from stackunderflow.routes.projects import set_project_by_dir

    store_db = tmp_path / "store.db"
    conn = _connect(store_db)
    _insert_project(conn, provider="antigravity", slug="test-antigravity-proj")
    conn.commit()
    conn.close()

    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)
    monkeypatch.setattr("stackunderflow.deps.current_project_path", None)
    monkeypatch.setattr("stackunderflow.deps.current_log_path", None)

    response = await set_project_by_dir({"dir_name": "test-antigravity-proj"})
    body = json.loads(response.body.decode("utf-8"))

    assert body["status"] == "success"
    assert deps.current_project_path == "test-antigravity-proj"
    assert deps.current_log_path is not None
    assert "test-antigravity-proj" in deps.current_log_path

