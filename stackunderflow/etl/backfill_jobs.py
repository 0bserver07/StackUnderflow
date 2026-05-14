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
- :mod:`stackunderflow.etl.status` — surfaces the current job + the
  most recently completed job in the ``/api/etl/status`` payload so
  the dashboard badge can show "backfilling" / "just completed" /
  "just failed" states without polling the route again.

Recently completed jobs (success or failure) are retained in the
:func:`get_last_job` slot for :data:`LAST_JOB_TTL_SECONDS` so the
dashboard has a chance to render the outcome — particularly the error
message on a failed run, which would otherwise be lost when
:func:`complete_job` clears the slot.
"""

from __future__ import annotations

import threading
from datetime import UTC, datetime
from typing import Any
from uuid import uuid4

# How long a finished job stays in the ``last_job`` slot before
# :func:`get_last_job` reports it as expired. Picked at 30 seconds so a
# dashboard polling on the standard 10s cadence (or 2s while a job is
# active) gets at least 3 chances to see the outcome before it's
# garbage-collected — long enough to render a banner, short enough that
# stale failures don't haunt the UI for minutes after the operator has
# moved on.
LAST_JOB_TTL_SECONDS: float = 30.0


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
# acquire the lock before reading or writing ``_current_job`` /
# ``_last_job``.
_lock = threading.Lock()
_current_job: dict[str, Any] | None = None
_last_job: dict[str, Any] | None = None


def _now() -> datetime:
    """Current UTC ``datetime`` — broken out so tests can monkeypatch."""
    return datetime.now(UTC)


def _now_iso() -> str:
    """ISO 8601 UTC timestamp with the trailing ``+00:00``.

    Matches the format produced by :func:`stackunderflow.etl.watermark`
    so dashboard timestamps line up across surfaces.
    """
    return _now().isoformat()


def _parse_iso(ts: str) -> datetime | None:
    """Parse an ISO 8601 timestamp produced by :func:`_now_iso`.

    Returns ``None`` on parse failure rather than raising — the slot's
    ``completed_at`` is informational and a malformed timestamp must
    not crash status assembly. Accepts both the canonical ``+00:00``
    and a trailing ``Z`` form so external callers who store a custom
    timestamp don't trip the parser.
    """
    if not ts:
        return None
    try:
        normalized = ts.replace("Z", "+00:00") if ts.endswith("Z") else ts
        parsed = datetime.fromisoformat(normalized)
        if parsed.tzinfo is None:
            parsed = parsed.replace(tzinfo=UTC)
        return parsed
    except (ValueError, TypeError):
        return None


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
    error: str | None = None,
) -> None:
    """Release the slot and record the outcome in the last-job slot.

    Idempotent. Safe to call from a background worker's ``finally``
    block without first checking whether the job is the current one — a
    no-op if the slot is empty or has been re-claimed by a different
    ``job_id``. In both no-op cases the last-job slot is **not**
    polluted: only the legitimate completion of the currently-running
    job updates it.

    Parameters
    ----------
    job_id:
        Must match the currently-running slot's ``job_id``; otherwise
        the call is a no-op.
    status:
        Outcome enum. Canonical values are ``"complete"`` (success) and
        ``"failed"`` (orchestrator raised). Any other string is
        accepted and stored verbatim — consumers should treat unknown
        values as "not success".
    error:
        Stringified error message when ``status="failed"``. Stored on
        the last-job slot so the dashboard can render it. Ignored when
        ``status="complete"`` (callers shouldn't pass it, but it's
        clamped to ``None`` defensively here).
    """
    global _current_job, _last_job
    with _lock:
        if _current_job is None:
            return
        if _current_job.get("job_id") != job_id:
            # Some other job claimed the slot already — don't stomp it
            # and don't pollute the last-job slot with a half-baked
            # entry that doesn't correspond to a real run.
            return
        finished = dict(_current_job)
        finished["status"] = status
        finished["completed_at"] = _now_iso()
        # Only retain ``error`` on actual failure paths; on success
        # paths the field is omitted from the slot so consumers can
        # branch on its presence.
        if status == "failed":
            finished["error"] = error
        _last_job = finished
        _current_job = None


def get_current_job() -> dict[str, Any] | None:
    """Return a *copy* of the currently-running job, or ``None``.

    Returns a copy rather than the live dict so callers can't mutate
    the slot's state without going through :func:`complete_job`.
    """
    with _lock:
        return dict(_current_job) if _current_job is not None else None


def get_last_job() -> dict[str, Any] | None:
    """Return a *copy* of the most recently completed job, or ``None``.

    Returns ``None`` if the slot is empty or if more than
    :data:`LAST_JOB_TTL_SECONDS` have elapsed since the job's
    ``completed_at`` timestamp. The TTL is checked on read (not via a
    background sweeper) so the slot stays cheap — every read is one
    parse + one subtraction.

    The returned dict has shape::

        {"job_id": str, "started_at": str, "completed_at": str,
         "force": bool, "status": "complete" | "failed",
         "error": str | None  # only present when status == "failed"
        }
    """
    global _last_job
    with _lock:
        if _last_job is None:
            return None
        completed_at = _parse_iso(str(_last_job.get("completed_at", "")))
        if completed_at is None:
            # Malformed timestamp — drop the slot rather than serve
            # stale data forever. Treating this as "expired" matches
            # the behavior a TTL check would produce on any non-zero
            # threshold.
            _last_job = None
            return None
        elapsed = (_now() - completed_at).total_seconds()
        if elapsed > LAST_JOB_TTL_SECONDS:
            # Lazy expiry — clear the slot so subsequent reads stay
            # fast and the slot doesn't accumulate stale dicts across
            # the lifetime of the process.
            _last_job = None
            return None
        return dict(_last_job)


def _reset_for_tests() -> None:
    """Test-only: clear both slots so back-to-back tests don't collide.

    Underscore-prefixed because it is *not* part of the public surface;
    production callers must go through :func:`complete_job`.
    """
    global _current_job, _last_job
    with _lock:
        _current_job = None
        _last_job = None


__all__ = [
    "BackfillInProgressError",
    "LAST_JOB_TTL_SECONDS",
    "complete_job",
    "get_current_job",
    "get_last_job",
    "start_job",
]
