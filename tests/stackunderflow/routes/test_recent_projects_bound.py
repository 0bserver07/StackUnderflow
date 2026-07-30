"""PROJ-8 — ``GET /api/recent-projects`` bounds its read in SQL.

The route only ever returns the newest ``RECENT_PROJECTS_LIMIT`` rows, but it
used to read every project row out of the store, build a dict for each, and
then slice. The bound now goes into the query; because ``list_projects``
already orders by ``last_modified DESC``, the payload is identical.
"""

from __future__ import annotations

import json

import pytest

from stackunderflow.routes.projects import RECENT_PROJECTS_LIMIT, get_recent_projects
from stackunderflow.store import db, queries, schema


def _seed(store_db, count):
    conn = db.connect(store_db)
    schema.apply(conn)
    for i in range(count):
        conn.execute(
            "INSERT INTO projects (provider, slug, display_name, path, first_seen, last_modified) "
            "VALUES ('claude', ?, ?, ?, 0.0, ?)",
            (f"-p{i:03d}", f"-p{i:03d}", f"/repos/p{i:03d}", float(i)),
        )
    conn.commit()
    conn.close()


@pytest.mark.asyncio
async def test_recent_projects_bounds_the_query_not_the_python_list(tmp_path, monkeypatch):
    store_db = tmp_path / "store.db"
    _seed(store_db, RECENT_PROJECTS_LIMIT + 12)
    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)

    seen: list[int | None] = []
    real_list_projects = queries.list_projects

    def spy(conn, *, limit=None):
        seen.append(limit)
        return real_list_projects(conn, limit=limit)

    monkeypatch.setattr("stackunderflow.routes.projects.queries.list_projects", spy)
    response = await get_recent_projects()
    body = json.loads(response.body.decode("utf-8"))

    assert seen == [RECENT_PROJECTS_LIMIT]
    assert len(body["projects"]) == RECENT_PROJECTS_LIMIT


@pytest.mark.asyncio
async def test_recent_projects_payload_is_newest_first_and_unchanged(tmp_path, monkeypatch):
    """Same rows, same order, same fields as the old read-all-then-slice."""
    store_db = tmp_path / "store.db"
    total = RECENT_PROJECTS_LIMIT + 5
    _seed(store_db, total)
    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)

    response = await get_recent_projects()
    body = json.loads(response.body.decode("utf-8"))

    expected = [f"-p{i:03d}" for i in range(total - 1, total - 1 - RECENT_PROJECTS_LIMIT, -1)]
    assert [p["dir_name"] for p in body["projects"]] == expected
    first = body["projects"][0]
    assert first["log_path"] == f"/repos/p{total - 1:03d}"
    assert first["last_modified"] == float(total - 1)
    assert first["file_count"] == 0


@pytest.mark.asyncio
async def test_recent_projects_under_the_limit_returns_everything(tmp_path, monkeypatch):
    store_db = tmp_path / "store.db"
    _seed(store_db, 3)
    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)

    response = await get_recent_projects()
    body = json.loads(response.body.decode("utf-8"))
    assert [p["dir_name"] for p in body["projects"]] == ["-p002", "-p001", "-p000"]
