"""Tests for the post-ingest auto-reindex hook.

After run_ingest finishes, the search/tag/qa services should be invoked
once per touched project. Each must be in its own try/except so a beta
service failure cannot break ingest. An opt-out setting must skip the
hook entirely.
"""
from __future__ import annotations

import sqlite3
from pathlib import Path

import pytest

import stackunderflow.deps as deps
from stackunderflow.adapters.base import Record, SessionRef
from stackunderflow.ingest import run_ingest
from stackunderflow.store import db, schema


class _StubAdapter:
    name = "stub"

    def __init__(self, refs, records_per_ref):
        self._refs = refs
        self._records = records_per_ref

    def enumerate(self):
        yield from self._refs

    def read(self, ref, *, since_offset=0):
        yield from self._records.get(ref.session_id, [])


def _rec(seq: int, content: str = "hello world") -> Record:
    # raw payload mirrors the Claude JSONL shape so the classifier can
    # surface message content end-to-end (matters for the FTS test).
    return Record(
        provider="stub", session_id="s1", seq=seq,
        timestamp="2026-01-01T00:00:00+00:00", role="user", model=None,
        input_tokens=0, output_tokens=0,
        cache_create_tokens=0, cache_read_tokens=0,
        content_text=content, tools=(), cwd=None,
        is_sidechain=False, uuid=f"u{seq}", parent_uuid=None,
        raw={
            "type": "user",
            "uuid": f"u{seq}",
            "sessionId": "s1",
            "timestamp": "2026-01-01T00:00:00+00:00",
            "message": {"role": "user", "content": content},
        },
    )


def _ref(tmp_path: Path, slug: str = "-a", mtime: float = 1.0, size: int = 100) -> SessionRef:
    fp = tmp_path / f"{slug}.jsonl"
    fp.write_bytes(b"x" * size)
    return SessionRef("stub", slug, "s1", fp, file_mtime=mtime, file_size=size)


@pytest.fixture
def conn(tmp_path: Path) -> sqlite3.Connection:
    c = db.connect(tmp_path / "store.db")
    schema.apply(c)
    yield c
    c.close()


class _RecordingService:
    """Captures index_project calls. ``mode`` mirrors the three services."""

    def __init__(self, mode: str = "with_project"):
        self.mode = mode
        self.calls: list[tuple] = []

    def index_project(self, *args):
        self.calls.append(args)


class _BoomService:
    """Always raises — exercises the per-service try/except."""

    def __init__(self):
        self.called = False

    def index_project(self, *args):
        self.called = True
        raise RuntimeError("boom")


@pytest.fixture
def reset_deps():
    """Snapshot/restore deps so tests don't leak service stubs."""
    saved = {
        "search_service": deps.search_service,
        "tag_service": deps.tag_service,
        "qa_service": deps.qa_service,
    }
    deps.search_service = None
    deps.tag_service = None
    deps.qa_service = None
    try:
        yield
    finally:
        for k, v in saved.items():
            setattr(deps, k, v)


def test_run_ingest_auto_reindexes_touched_project(conn, tmp_path, reset_deps):
    search = _RecordingService(mode="with_project")
    tag = _RecordingService(mode="messages_only")
    qa = _RecordingService(mode="with_project")
    deps.search_service = search
    deps.tag_service = tag
    deps.qa_service = qa

    ref = _ref(tmp_path)
    run_ingest(conn, [_StubAdapter([ref], {"s1": [_rec(0), _rec(1)]})])

    assert len(search.calls) == 1
    assert search.calls[0][0] == "-a"
    assert len(qa.calls) == 1
    assert qa.calls[0][0] == "-a"
    # Tags receives messages-only (no project name positional).
    assert len(tag.calls) == 1


def test_run_ingest_does_not_reindex_when_no_new_messages(conn, tmp_path, reset_deps):
    search = _RecordingService()
    deps.search_service = search

    ref = _ref(tmp_path)
    adapter = _StubAdapter([ref], {"s1": [_rec(0)]})
    run_ingest(conn, [adapter])  # first run — should index
    assert len(search.calls) == 1

    run_ingest(conn, [adapter])  # second run — file unchanged, no reindex
    assert len(search.calls) == 1


def test_failing_service_does_not_break_others(conn, tmp_path, reset_deps):
    search = _BoomService()
    tag = _RecordingService(mode="messages_only")
    qa = _RecordingService()
    deps.search_service = search
    deps.tag_service = tag
    deps.qa_service = qa

    ref = _ref(tmp_path)
    counts = run_ingest(conn, [_StubAdapter([ref], {"s1": [_rec(0)]})])

    assert search.called  # search blew up
    assert len(tag.calls) == 1  # tag still ran
    assert len(qa.calls) == 1  # qa still ran
    assert counts == {"stub": 1}  # ingest still reported success


def test_opt_out_skips_reindex(conn, tmp_path, reset_deps, monkeypatch):
    search = _RecordingService()
    deps.search_service = search

    monkeypatch.setenv("AUTO_REINDEX_ON_INGEST", "false")

    ref = _ref(tmp_path)
    run_ingest(conn, [_StubAdapter([ref], {"s1": [_rec(0)]})])

    assert search.calls == []


def test_search_service_actually_indexes_after_ingest(tmp_path, reset_deps, monkeypatch):
    """End-to-end: a fresh ingest should leave the FTS index queryable
    without a manual /api/search/reindex POST."""
    from stackunderflow.services.search_service import SearchService

    search_db = tmp_path / "search.db"
    monkeypatch.setattr(
        "stackunderflow.services.search_service.SEARCH_DB_PATH", search_db
    )
    deps.search_service = SearchService(db_path=search_db)

    store_db_path = tmp_path / "store.db"
    monkeypatch.setattr(deps, "store_path", store_db_path)
    conn = db.connect(store_db_path)
    schema.apply(conn)
    try:
        ref = _ref(tmp_path)
        run_ingest(conn, [_StubAdapter([ref], {"s1": [_rec(0, "alpha bravo charlie")]})])
    finally:
        conn.close()

    results = deps.search_service.search(query="bravo")
    assert results["total"] >= 1
    assert any("bravo" in r.get("content", "").lower() for r in results["results"])
