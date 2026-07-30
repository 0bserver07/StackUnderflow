"""``POST /api/refresh`` with a project selected — the branch that had none.

Two defects lived in this branch, both invisible because nothing covered it:

1. ``run_ingest`` returns PROVIDER-keyed counts, but the gate read
   ``counts.get(slug)``. That lookup has been structurally ``0`` since the
   function was written, so ``files_changed`` / ``message_count`` reported
   "no changes detected" after every successful ingest.

2. Behind that dead gate sat a second reindex pass. ``run_ingest`` already
   refreshes the search / tag / Q&A indexes for every touched slug
   (``auto_reindex_touched``, which also honours ``auto_reindex_on_ingest``).
   The second pass keyed on the same slug, so fixing (1) alone would have made
   it DELETE and rewrite the merged index ingest had just written — from a
   single ``get_project(slug)`` ``fetchone`` that picks whichever provider row
   the planner happens to return. It is gone; these tests keep it gone.
"""

from __future__ import annotations

import json
from pathlib import Path

import pytest

import stackunderflow.deps as deps
from stackunderflow.routes import data as data_route
from stackunderflow.store import db, schema


class _SpySearchService:
    def __init__(self) -> None:
        self.indexed: list[str] = []

    def index_project(self, project_name, messages):  # noqa: ARG002
        self.indexed.append(project_name)


@pytest.fixture()
def refresh_env(tmp_path, monkeypatch):
    store_db = tmp_path / "store.db"
    conn = db.connect(store_db)
    schema.apply(conn)
    conn.execute(
        "INSERT INTO projects (provider, slug, display_name, first_seen, last_modified) "
        "VALUES ('claude', '-refresh-me', '-refresh-me', 0.0, 0.0)",
    )
    conn.commit()
    conn.close()
    monkeypatch.setattr(deps, "store_path", store_db)
    monkeypatch.setattr(deps, "current_log_path", "/fake/-refresh-me")
    monkeypatch.setattr(data_route, "registered", lambda: [])
    spy = _SpySearchService()
    monkeypatch.setattr(deps, "search_service", spy)
    monkeypatch.setattr(deps, "qa_service", None)
    monkeypatch.setattr(deps, "tag_service", None)
    return spy


def _body(response) -> dict:
    return json.loads(response.body.decode("utf-8"))


@pytest.mark.asyncio
async def test_refresh_reports_provider_keyed_counts(refresh_env, monkeypatch) -> None:
    """Counts are provider-keyed; the response must sum them, not slug-index."""
    monkeypatch.setattr(
        data_route, "run_ingest", lambda conn, adapters: {"claude": 5, "codex": 2},
    )
    body = _body(await data_route.refresh_data({}))
    assert body["files_changed"] is True
    assert body["message_count"] == 7
    assert "Files changed" in body["message"]


@pytest.mark.asyncio
async def test_refresh_does_not_reindex_a_second_time(refresh_env, monkeypatch) -> None:
    """``run_ingest`` owns reindexing — the route must not repeat it."""
    monkeypatch.setattr(data_route, "run_ingest", lambda conn, adapters: {"claude": 5})
    await data_route.refresh_data({})
    assert refresh_env.indexed == [], (
        "the route re-indexed on top of auto_reindex_touched — that DELETEs and "
        "rewrites the merged index, and ignores auto_reindex_on_ingest=false"
    )


@pytest.mark.asyncio
async def test_refresh_reports_no_changes_on_an_empty_pass(refresh_env, monkeypatch) -> None:
    monkeypatch.setattr(data_route, "run_ingest", lambda conn, adapters: {})
    body = _body(await data_route.refresh_data({}))
    assert body["files_changed"] is False
    assert body["message_count"] == 0
    assert "No changes detected" in body["message"]
    assert refresh_env.indexed == []


@pytest.mark.asyncio
async def test_refresh_invalidates_the_selected_slugs_caches(refresh_env, monkeypatch) -> None:
    """The dashboard memo for this slug is dropped once the gate reports work."""
    monkeypatch.setattr(data_route, "run_ingest", lambda conn, adapters: {"claude": 1})
    slug = Path(deps.current_log_path).name
    dropped: list[str | None] = []
    monkeypatch.setattr(data_route, "invalidate_dashboard_cache", dropped.append)
    monkeypatch.setattr(data_route, "_invalidate_stats_cache", lambda s=None: None)
    await data_route.refresh_data({})
    assert dropped == [slug]
