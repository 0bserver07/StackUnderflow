"""Tests for the process-local backfill job slots.

Covers the contract documented in :mod:`stackunderflow.etl.backfill_jobs`:

* ``start_job`` / ``complete_job`` cycle a single slot atomically.
* ``complete_job`` with the wrong ``job_id`` is a no-op for both the
  current slot **and** the last-job slot — a stale callback must not
  pollute either.
* Failed completions retain the error string + ``status="failed"`` in
  the last-job slot so the dashboard banner can render it.
* Successful completions retain ``status="complete"`` (no ``error``
  key) in the last-job slot for the same TTL window.
* The TTL is enforced lazily on read: after
  :data:`LAST_JOB_TTL_SECONDS` elapses, ``get_last_job`` returns
  ``None`` and clears the slot.
"""

from __future__ import annotations

from collections.abc import Iterator
from datetime import UTC, datetime, timedelta

import pytest

import stackunderflow.etl.backfill_jobs as backfill_jobs
from stackunderflow.etl.backfill_jobs import (
    LAST_JOB_TTL_SECONDS,
    BackfillInProgressError,
    complete_job,
    get_current_job,
    get_last_job,
    start_job,
)


@pytest.fixture(autouse=True)
def _reset_slots() -> Iterator[None]:
    """Clear both slots around every test so module-level state is
    isolated across cases (and across this module + sibling modules)."""
    backfill_jobs._reset_for_tests()
    yield
    backfill_jobs._reset_for_tests()


# ── start_job / get_current_job ─────────────────────────────────────────────


class TestStartJob:
    def test_returns_a_running_job_dict(self) -> None:
        job = start_job(force=False)
        assert set(job.keys()) == {"job_id", "started_at", "force", "status"}
        assert isinstance(job["job_id"], str) and len(job["job_id"]) == 32
        assert job["force"] is False
        assert job["status"] == "running"
        assert isinstance(job["started_at"], str) and "T" in job["started_at"]

    def test_force_flag_round_trips(self) -> None:
        assert start_job(force=True)["force"] is True

    def test_concurrent_start_raises_with_existing_job(self) -> None:
        first = start_job(force=False)
        with pytest.raises(BackfillInProgressError) as exc_info:
            start_job(force=True)
        assert exc_info.value.current_job["job_id"] == first["job_id"]

    def test_get_current_job_returns_a_copy(self) -> None:
        job = start_job(force=False)
        snap = get_current_job()
        assert snap is not None
        assert snap == job
        # Mutating the returned dict must not corrupt the slot.
        snap["status"] = "tampered"
        assert get_current_job()["status"] == "running"  # type: ignore[index]


# ── complete_job → last-job slot ────────────────────────────────────────────


class TestCompleteJobSuccess:
    def test_clears_current_slot(self) -> None:
        job = start_job(force=False)
        complete_job(job["job_id"])
        assert get_current_job() is None

    def test_records_complete_status_on_last_job(self) -> None:
        job = start_job(force=False)
        complete_job(job["job_id"], status="complete")
        last = get_last_job()
        assert last is not None
        assert last["job_id"] == job["job_id"]
        assert last["status"] == "complete"
        assert last["force"] is False
        assert last["started_at"] == job["started_at"]
        assert isinstance(last["completed_at"], str) and "T" in last["completed_at"]
        # Successful completions don't carry an error key — consumers
        # branch on its presence.
        assert "error" not in last

    def test_get_last_job_returns_a_copy(self) -> None:
        job = start_job(force=True)
        complete_job(job["job_id"], status="complete")
        snap = get_last_job()
        assert snap is not None
        snap["status"] = "tampered"
        # Underlying slot still reads the canonical value.
        assert get_last_job()["status"] == "complete"  # type: ignore[index]


class TestCompleteJobFailure:
    def test_records_failed_status_with_error_message(self) -> None:
        job = start_job(force=False)
        complete_job(job["job_id"], status="failed", error="connection refused")
        last = get_last_job()
        assert last is not None
        assert last["status"] == "failed"
        assert last["error"] == "connection refused"
        assert last["job_id"] == job["job_id"]
        assert last["force"] is False

    def test_failed_run_clears_current_slot(self) -> None:
        job = start_job(force=True)
        complete_job(job["job_id"], status="failed", error="boom")
        assert get_current_job() is None

    def test_error_is_none_when_caller_omits_it(self) -> None:
        # Callers should pass an error message on failure but the slot
        # tolerates a missing one rather than crashing.
        job = start_job(force=False)
        complete_job(job["job_id"], status="failed")
        last = get_last_job()
        assert last is not None
        assert last["status"] == "failed"
        assert last["error"] is None


class TestCompleteJobWrongId:
    def test_no_op_on_empty_slot(self) -> None:
        # No current job; complete_job should be a silent no-op rather
        # than raising or polluting the last-job slot.
        complete_job("does-not-exist")
        assert get_current_job() is None
        assert get_last_job() is None

    def test_no_op_when_running_id_does_not_match(self) -> None:
        running = start_job(force=False)
        complete_job("a-totally-different-id", status="failed", error="ignored")
        # Running slot survives — a stale callback didn't claim it.
        current = get_current_job()
        assert current is not None
        assert current["job_id"] == running["job_id"]
        # Last-job slot stays empty — a stale callback can't fabricate
        # a "last completed run" for a job that never existed.
        assert get_last_job() is None

    def test_after_real_completion_a_stale_callback_is_no_op(self) -> None:
        first = start_job(force=False)
        complete_job(first["job_id"], status="complete")
        last_after = get_last_job()
        # Now a stale callback for a completely different id arrives —
        # it must not overwrite the genuine last-job entry.
        complete_job("stale-id", status="failed", error="should be ignored")
        assert get_last_job() == last_after


# ── TTL semantics ──────────────────────────────────────────────────────────


class TestLastJobTtl:
    def test_returns_job_inside_window(self, monkeypatch: pytest.MonkeyPatch) -> None:
        # Freeze "now" at start of the test, then complete the job.
        t0 = datetime(2026, 5, 6, 12, 0, 0, tzinfo=UTC)
        monkeypatch.setattr(backfill_jobs, "_now", lambda: t0)
        job = start_job(force=False)
        complete_job(job["job_id"], status="failed", error="x")

        # Advance clock by half the TTL — still within the window.
        monkeypatch.setattr(
            backfill_jobs,
            "_now",
            lambda: t0 + timedelta(seconds=LAST_JOB_TTL_SECONDS / 2),
        )
        assert get_last_job() is not None

    def test_returns_none_after_ttl_elapses(self, monkeypatch: pytest.MonkeyPatch) -> None:
        t0 = datetime(2026, 5, 6, 12, 0, 0, tzinfo=UTC)
        monkeypatch.setattr(backfill_jobs, "_now", lambda: t0)
        job = start_job(force=False)
        complete_job(job["job_id"], status="failed", error="x")

        # Advance clock just past the TTL boundary.
        monkeypatch.setattr(
            backfill_jobs,
            "_now",
            lambda: t0 + timedelta(seconds=LAST_JOB_TTL_SECONDS + 1),
        )
        assert get_last_job() is None

    def test_expired_read_clears_slot_for_subsequent_reads(
        self, monkeypatch: pytest.MonkeyPatch,
    ) -> None:
        # Once a TTL-expired read is observed, the slot is wiped — we
        # don't want the slot accumulating dicts forever.
        t0 = datetime(2026, 5, 6, 12, 0, 0, tzinfo=UTC)
        monkeypatch.setattr(backfill_jobs, "_now", lambda: t0)
        job = start_job(force=False)
        complete_job(job["job_id"], status="complete")

        monkeypatch.setattr(
            backfill_jobs,
            "_now",
            lambda: t0 + timedelta(seconds=LAST_JOB_TTL_SECONDS + 5),
        )
        assert get_last_job() is None
        # Even after we move the clock back, the slot stays cleared.
        monkeypatch.setattr(backfill_jobs, "_now", lambda: t0)
        assert get_last_job() is None

    def test_successful_then_failed_overwrites_slot(self) -> None:
        # New job's outcome supersedes the previous one within the
        # window — the slot is "most recent", not "first observed".
        a = start_job(force=False)
        complete_job(a["job_id"], status="complete")
        b = start_job(force=False)
        complete_job(b["job_id"], status="failed", error="b broke")
        last = get_last_job()
        assert last is not None
        assert last["job_id"] == b["job_id"]
        assert last["status"] == "failed"
        assert last["error"] == "b broke"


# ── start_job after a completion ───────────────────────────────────────────


class TestSlotCycleAfterCompletion:
    def test_can_start_new_job_after_completion(self) -> None:
        first = start_job(force=False)
        complete_job(first["job_id"], status="complete")
        # Slot is free → new job is accepted.
        second = start_job(force=True)
        assert second["job_id"] != first["job_id"]
        # Last-job slot still reflects the previous run.
        last = get_last_job()
        assert last is not None
        assert last["job_id"] == first["job_id"]

    def test_can_start_new_job_after_failure(self) -> None:
        first = start_job(force=False)
        complete_job(first["job_id"], status="failed", error="boom")
        second = start_job(force=False)
        assert second["job_id"] != first["job_id"]
        # The failure is preserved in last-job until either TTL or a
        # subsequent completion overwrites it.
        last = get_last_job()
        assert last is not None
        assert last["job_id"] == first["job_id"]
        assert last["status"] == "failed"
