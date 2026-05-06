"""Tests for ``POST /api/etl/backfill`` — the manual backfill kick-off route.

Locks:

* 202 Accepted with a well-formed ``{job_id, started_at}`` body on success.
* The ``force`` flag plumbs through to the orchestrator.
* 409 Conflict with ``{error, job_id}`` when a backfill is already
  running in this process.
* ``GET /api/etl/status`` reports the in-progress job in its
  ``current_job`` block while one is in flight.
* The background task releases the slot when it returns, so the next
  POST can succeed.
* Errors raised by the orchestrator are caught + the slot released
  (so a single failure can't poison the route forever).

The orchestrator itself is mocked in every test in this module — the
backfill body is covered by tests/stackunderflow/etl/test_backfill.py
and the e2e suite. These tests' job is to lock the route's response
shape and the concurrency guard.
"""

from __future__ import annotations

import pytest
from fastapi import FastAPI
from fastapi.testclient import TestClient

import stackunderflow.deps as deps
import stackunderflow.etl.backfill_jobs as backfill_jobs
import stackunderflow.routes.etl as etl_routes
from stackunderflow.routes.etl import router as etl_router
from stackunderflow.store import db, schema


# ── fixtures ────────────────────────────────────────────────────────────────


@pytest.fixture()
def app_client(tmp_path, monkeypatch):
    """Mount the etl router with a fresh schema-applied store + clean slot."""
    store_db = tmp_path / "store.db"
    conn = db.connect(store_db)
    schema.apply(conn)
    conn.close()

    monkeypatch.setattr(deps, "store_path", store_db)
    monkeypatch.setattr(deps, "watcher_handle", None, raising=False)
    backfill_jobs._reset_for_tests()

    app = FastAPI()
    app.include_router(etl_router)
    yield TestClient(app), store_db
    # Belt-and-braces — clear the slot so a teardown failure can't leak
    # state into the next test module.
    backfill_jobs._reset_for_tests()


@pytest.fixture()
def captured_backfill(monkeypatch):
    """Replace ``etl_routes.run_backfill`` with a recorder.

    Yields a list[dict] populated by each call. The replacement is a
    no-op against the connection — it just records ``force`` so tests
    can assert plumbing without actually exercising the orchestrator.
    """
    calls: list[dict] = []

    def _fake(_conn, *, force: bool = False) -> None:
        calls.append({"force": force})

    monkeypatch.setattr(etl_routes, "run_backfill", _fake)
    return calls


@pytest.fixture()
def failing_backfill(monkeypatch):
    """Replace ``run_backfill`` with one that raises.

    Used by the slot-release-on-error test to confirm that a poison
    backfill doesn't leak the slot.
    """
    calls: list[dict] = []

    def _fake(_conn, *, force: bool = False) -> None:
        calls.append({"force": force})
        raise RuntimeError("synthetic backfill failure")

    monkeypatch.setattr(etl_routes, "run_backfill", _fake)
    return calls


# ── 202 success path ─────────────────────────────────────────────────────────


class TestSuccessPath:
    def test_returns_202_with_job_id_and_started_at(self, app_client, captured_backfill):
        client, _ = app_client
        r = client.post("/api/etl/backfill", json={"force": False})
        assert r.status_code == 202
        body = r.json()
        assert set(body.keys()) == {"job_id", "started_at"}
        assert isinstance(body["job_id"], str) and body["job_id"]
        # uuid4().hex is 32 hex chars — sanity-check that something
        # uuid-shaped came back rather than e.g. "None" string.
        assert len(body["job_id"]) == 32
        assert all(c in "0123456789abcdef" for c in body["job_id"])
        # ISO 8601 timestamp with the canonical "+00:00" suffix.
        assert isinstance(body["started_at"], str) and "T" in body["started_at"]
        assert body["started_at"].endswith("+00:00") or body["started_at"].endswith("Z")
        # TestClient runs BackgroundTasks synchronously after the
        # response, so the recorder fires before .post() returns.
        assert captured_backfill == [{"force": False}]

    def test_force_flag_plumbs_through_to_orchestrator(self, app_client, captured_backfill):
        client, _ = app_client
        r = client.post("/api/etl/backfill", json={"force": True})
        assert r.status_code == 202
        assert captured_backfill == [{"force": True}]

    def test_force_defaults_to_false_when_body_omits_it(self, app_client, captured_backfill):
        client, _ = app_client
        r = client.post("/api/etl/backfill", json={})
        assert r.status_code == 202
        assert captured_backfill == [{"force": False}]

    def test_no_body_is_treated_as_force_false(self, app_client, captured_backfill):
        client, _ = app_client
        # No JSON body at all — handler defaults to {"force": false}.
        r = client.post("/api/etl/backfill")
        assert r.status_code == 202
        assert captured_backfill == [{"force": False}]

    def test_two_sequential_posts_both_succeed_when_first_finishes(
        self, app_client, captured_backfill,
    ):
        """Background task releases the slot, so the second POST is accepted."""
        client, _ = app_client
        r1 = client.post("/api/etl/backfill", json={"force": False})
        assert r1.status_code == 202
        # First job's background task already ran (TestClient is sync).
        assert backfill_jobs.get_current_job() is None
        r2 = client.post("/api/etl/backfill", json={"force": True})
        assert r2.status_code == 202
        assert r1.json()["job_id"] != r2.json()["job_id"]
        assert captured_backfill == [{"force": False}, {"force": True}]


# ── 409 concurrency guard ────────────────────────────────────────────────────


class TestConcurrencyGuard:
    def test_409_when_backfill_already_running(self, app_client, captured_backfill):
        client, _ = app_client
        # Pre-claim the slot so the route sees an existing job.
        existing = backfill_jobs.start_job(force=False)
        try:
            r = client.post("/api/etl/backfill", json={"force": False})
            assert r.status_code == 409
            body = r.json()
            assert body == {
                "error": "backfill_in_progress",
                "job_id": existing["job_id"],
            }
            # The orchestrator must not have been invoked for the
            # rejected POST.
            assert captured_backfill == []
        finally:
            backfill_jobs._reset_for_tests()

    def test_409_response_does_not_schedule_a_background_task(
        self, app_client, captured_backfill,
    ):
        client, _ = app_client
        backfill_jobs.start_job(force=True)
        try:
            r = client.post("/api/etl/backfill", json={"force": True})
            assert r.status_code == 409
            # Even though TestClient runs background tasks synchronously,
            # the rejected request must not have queued one.
            assert captured_backfill == []
        finally:
            backfill_jobs._reset_for_tests()


# ── status surface integration ──────────────────────────────────────────────


class TestStatusReflectsInProgress:
    def test_idle_status_reports_no_current_job(self, app_client):
        client, _ = app_client
        body = client.get("/api/etl/status").json()
        assert body["current_job"] is None

    def test_status_reflects_in_progress_job(self, app_client):
        client, _ = app_client
        existing = backfill_jobs.start_job(force=True)
        try:
            r = client.get("/api/etl/status")
            assert r.status_code == 200
            body = r.json()
            assert body["current_job"] is not None
            assert body["current_job"]["job_id"] == existing["job_id"]
            assert body["current_job"]["force"] is True
            assert body["current_job"]["status"] == "running"
            # ``started_at`` round-trips as a string, not a parsed dt.
            assert isinstance(body["current_job"]["started_at"], str)
        finally:
            backfill_jobs._reset_for_tests()


# ── error recovery ──────────────────────────────────────────────────────────


class TestErrorRecovery:
    def test_orchestrator_failure_releases_the_slot(self, app_client, failing_backfill):
        """A poison backfill must not leak the slot — the next POST succeeds."""
        client, _ = app_client
        r1 = client.post("/api/etl/backfill", json={"force": False})
        # The route still returns 202 — the failure happens inside the
        # background task after the response has been flushed.
        assert r1.status_code == 202
        # TestClient runs the background task synchronously, so by the
        # time .post() returns the failure has already cleared the slot.
        assert backfill_jobs.get_current_job() is None
        # Confirm the recorder was hit — we want to be sure the failing
        # path executed.
        assert failing_backfill == [{"force": False}]
        # And a follow-up POST succeeds (slot is free).
        r2 = client.post("/api/etl/backfill", json={"force": False})
        assert r2.status_code == 202
