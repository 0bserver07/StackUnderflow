"""The blocking ``routes/data.py`` handlers must not run on the event loop.

``get_stats`` / ``get_dashboard_data`` / ``get_messages`` /
``get_messages_summary_endpoint`` are wall-to-wall blocking: sqlite reads and,
on a mart miss, the full aggregator pipeline (seconds). Declared ``async def``
they held the single event loop for that entire time, so one dashboard request
stalled every other request in the process — SSE included.

They are plain ``def`` now, which is the signal starlette uses to dispatch a
handler to its threadpool. These tests pin both halves: the structural
property (not coroutine functions) and the observable one (the handler body
runs on a different thread from the loop).

The loop thread is captured by an ``async`` probe route on the same app and
the same ``TestClient`` — one portal, one event loop, one thread — so the
comparison is against the real loop thread rather than the test's.
"""

from __future__ import annotations

import inspect
import threading

import pytest
from fastapi import FastAPI
from fastapi.testclient import TestClient

from stackunderflow.routes import data as data_route
from stackunderflow.store import db, schema


@pytest.fixture()
def client(tmp_path, monkeypatch):
    store_db = tmp_path / "store.db"
    conn = db.connect(store_db)
    schema.apply(conn)
    conn.execute(
        "INSERT INTO projects (provider, slug, display_name, first_seen, last_modified) "
        "VALUES ('claude', '-loop-probe', '-loop-probe', 0.0, 0.0)",
    )
    conn.commit()
    conn.close()
    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)
    monkeypatch.setattr("stackunderflow.deps.current_log_path", "/fake/-loop-probe")

    app = FastAPI()
    app.include_router(data_route.router)

    @app.get("/probe/loop-thread")
    async def _probe() -> dict:
        # An `async def` route body always runs on the event-loop thread.
        return {"tid": threading.get_ident()}

    with TestClient(app) as c:
        yield c


def _loop_tid(client: TestClient) -> int:
    return int(client.get("/probe/loop-thread").json()["tid"])


def test_blocking_handlers_are_not_coroutine_functions() -> None:
    """The mechanism itself — starlette threadpools sync endpoints only."""
    for fn in (
        data_route.get_stats,
        data_route.get_dashboard_data,
        data_route.get_messages,
        data_route.get_messages_summary_endpoint,
    ):
        assert not inspect.iscoroutinefunction(fn), f"{fn.__name__} is still async def"
    # The two refresh entry points stay async — they await each other.
    assert inspect.iscoroutinefunction(data_route.refresh_data)
    assert inspect.iscoroutinefunction(data_route.refresh_all_projects)


def test_get_stats_body_runs_off_the_event_loop(client, monkeypatch) -> None:
    captured: dict[str, int] = {}

    def spy(conn, *, project_ids, slug, tz_offset=0, keys=None):  # noqa: ARG001
        captured["tid"] = threading.get_ident()
        return {"overview": {}}

    monkeypatch.setattr(data_route, "_project_stats_cached", spy)
    loop_tid = _loop_tid(client)
    assert client.get("/api/stats").status_code == 200
    assert captured.get("tid") is not None, "handler body never ran"
    assert captured["tid"] != loop_tid, "get_stats ran on the event-loop thread"


def test_messages_summary_body_runs_off_the_event_loop(client, monkeypatch) -> None:
    captured: dict[str, int] = {}
    real = data_route._get_project_ids

    def spy(conn, log_path):
        captured["tid"] = threading.get_ident()
        return real(conn, log_path)

    monkeypatch.setattr(data_route, "_get_project_ids", spy)
    loop_tid = _loop_tid(client)
    assert client.get("/api/messages/summary").status_code == 200
    assert captured.get("tid") is not None, "handler body never ran"
    assert captured["tid"] != loop_tid, "summary handler ran on the event-loop thread"


def test_refresh_all_projects_pushes_its_ingest_off_the_event_loop(client, monkeypatch) -> None:
    """``refresh_all_projects`` keeps its ``async`` signature but must not run
    ``run_ingest`` inline — it walks every adapter's files and writes sqlite."""
    captured: dict[str, int] = {}

    def spy(conn, adapters):  # noqa: ARG001
        captured["tid"] = threading.get_ident()
        return {}

    monkeypatch.setattr(data_route, "run_ingest", spy)
    monkeypatch.setattr(data_route, "registered", lambda: [])
    monkeypatch.setattr("stackunderflow.deps.current_log_path", None)
    loop_tid = _loop_tid(client)
    assert client.post("/api/refresh", json={}).status_code == 200
    assert captured.get("tid") is not None, "ingest never ran"
    assert captured["tid"] != loop_tid, "run_ingest ran on the event-loop thread"
