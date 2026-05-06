"""Process-local concurrency guard for HTTP-triggered backfill runs.

A backfill rebuilds events + marts from scratch and can take minutes on
large stores. We don't want two HTTP-triggered runs racing each other,
so we serialize them via a process-local lock + a single-job-slot
"current job" pointer.

The HTTP route is the only entry point that needs guarding — the CLI
is single-threaded and the watcher runs on its own already-serialized
cycle, so a process-local lock (rather than a DB-side lock) is enough.
A DB-side lock would also persist across crashes; we want the next
``stackunderflow start`` to recover cleanly without manual cleanup.

Used by:
- :mod:`stackunderflow.routes.etl` — POST /api/etl/backfill claims the
  slot before scheduling the BackgroundTask, and releases it from the
  background worker's ``finally`` clause.
- :mod:`stackunderflow.etl.status` — surfaces the current job in the
  ``/api/etl/status`` payload so the dashboard badge can show a
  "backfilling" state without polling the route again.
"""

from __future__ import annotations

import threading
from datetime import UTC, datetime
from typing import Any
from uuid import uuid4


class BackfillInProgressError(Exception):
    """Raised by :func:`start_job` when a backfill is already running.

    The currently-running job dict is attached as ``current_job`` so the
    HTTP route can render its job_id in the 409 response without
    consulting :func:`get_current_job` again (which would race with a
    concurrent ``complete_job``).
    """

    def __init__(self, current_job: dict[str, Any]) -> None:
        super().__init__(f"Backfill already in progress: {current_job['job_id']}")
        self.current_job = current_job


# Module-level mutable state — guarded by ``_lock``. All callers must
# acquire the lock before reading or writing ``_current_job``.
_lock = threading.Lock()
_current_job: dict[str, Any] | None = None


def _now_iso() -> str:
    """ISO 8601 UTC timestamp with the trailing ``+00:00``.

    Matches the format produced by :func:`stackunderflow.etl.watermark`
    so dashboard timestamps line up across surfaces.
    """
    return datetime.now(UTC).isoformat()


def start_job(*, force: bool) -> dict[str, Any]:
    """Atomically claim the single backfill slot.

    Returns a *copy* of the new job dict on success. Raises
    :class:`BackfillInProgressError` (with the existing job attached) if a
    job is already running in this process.

    The returned dict has shape::

        {"job_id": str, "started_at": str (ISO8601 UTC),
         "force": bool, "status": "running"}
    """
    global _current_job
    with _lock:
        if _current_job is not None:
            raise BackfillInProgressError(dict(_current_job))
        job = {
            "job_id": uuid4().hex,
            "started_at": _now_iso(),
            "force": bool(force),
            "status": "running",
        }
        _current_job = job
        return dict(job)


def complete_job(
    job_id: str,
    *,
    status: str = "complete",
    error: str | None = None,  # noqa: ARG001 — reserved for future status payload
) -> None:
    """Release the slot. Idempotent.

    Safe to call from a background worker's ``finally`` block without
    first checking whether the job is the current one — a no-op if the
    slot is empty or has been re-claimed by a different ``job_id``.
    """
    global _current_job
    with _lock:
        if _current_job is None:
            return
        if _current_job.get("job_id") != job_id:
            # Some other job claimed the slot already — don't stomp it.
            return
        _current_job = None


def get_current_job() -> dict[str, Any] | None:
    """Return a *copy* of the currently-running job, or ``None``.

    Returns a copy rather than the live dict so callers can't mutate
    the slot's state without going through :func:`complete_job`.
    """
    with _lock:
        return dict(_current_job) if _current_job is not None else None


def _reset_for_tests() -> None:
    """Test-only: clear the slot so back-to-back tests don't collide.

    Underscore-prefixed because it is *not* part of the public surface;
    production callers must go through :func:`complete_job`.
    """
    global _current_job
    with _lock:
        _current_job = None


__all__ = [
    "BackfillInProgressError",
    "complete_job",
    "get_current_job",
    "start_job",
]
