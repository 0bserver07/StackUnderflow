"""ETL pipeline routes — Wave 4C status surface + the backfill kick-off.

Two endpoints share the same prefix so the dashboard can poll one and
mutate the other against the same logical surface:

* ``GET  /api/etl/status``     — health snapshot (assembler-backed)
* ``POST /api/etl/backfill``   — schedule a background backfill run

The status route is a thin shell over
:func:`stackunderflow.etl.status.assemble_status`; holding the SQL in
the assembler keeps the CLI ``stackunderflow etl status`` command and
this route in lockstep.

The backfill route wraps :func:`stackunderflow.etl.backfill.backfill`
in a FastAPI :class:`BackgroundTasks` task so the HTTP response returns
immediately while the actual rebuild runs in the worker thread. A
process-local lock (see :mod:`stackunderflow.etl.backfill_jobs`)
ensures only one backfill runs at a time — a second POST while a job
is in flight returns ``409 Conflict`` with the existing ``job_id`` so
the dashboard can surface "already running" without guessing.

Performance contract for status: <50ms end-to-end against a 200K-event
store on the maintainer's machine. The backfill route returns in <5ms;
the actual work happens off the request thread.
"""

from __future__ import annotations

import logging
from typing import Any

from fastapi import APIRouter, BackgroundTasks
from fastapi.responses import JSONResponse

import stackunderflow.deps as deps
from stackunderflow.etl.backfill import backfill as run_backfill
from stackunderflow.etl.backfill_jobs import (
    BackfillInProgressError,
    complete_job,
    start_job,
)
from stackunderflow.etl.status import assemble_status
from stackunderflow.store import db, schema

router = APIRouter()
_log = logging.getLogger(__name__)


@router.get("/api/etl/status")
async def get_etl_status() -> dict[str, Any]:
    """Return a live snapshot of the ETL pipeline.

    Response shape (see :func:`stackunderflow.etl.status.assemble_status`)::

        {
          "watcher": {"enabled": bool, "running": bool|"unknown", ...},
          "marts": {"daily": {"watermark": int, "row_count": int, ...}, ...},
          "events": {"total": int, "max_id": int,
                       "by_provider": {...}, "by_cost_source": {...}},
          "lag_seconds": int,
          "health": "live"|"syncing"|"stale"|"error",
          "current_job": {"job_id": str, "started_at": str,
                            "force": bool, "status": str} | None,
          "last_job": {"job_id": str, "started_at": str,
                        "completed_at": str, "force": bool,
                        "status": "complete"|"failed",
                        "error": str | None} | None
        }
    """
    conn = db.connect(deps.store_path)
    try:
        # ``schema.apply`` is idempotent and cheap on an already-current
        # store (single ``PRAGMA user_version`` read); it guarantees
        # the etl tables exist on a fresh-install machine where the
        # server hasn't yet booted to install them.
        schema.apply(conn)
        return assemble_status(conn)
    finally:
        conn.close()


@router.post("/api/etl/backfill")
async def post_etl_backfill(
    background_tasks: BackgroundTasks,
    body: dict | None = None,
) -> JSONResponse:
    """Schedule a background backfill run.

    Body (all fields optional)::

        {"force": bool}     # default false; full rebuild when true

    Returns ``202 Accepted`` with ``{"job_id", "started_at"}`` once the
    background task has been queued. The actual orchestrator runs after
    this response goes out — poll ``/api/etl/status`` for the
    ``current_job`` block to track progress.

    Returns ``409 Conflict`` with ``{"error": "backfill_in_progress",
    "job_id": "..."}`` if another backfill is already running in this
    process. Concurrency is process-local (threading.Lock + a
    module-level slot) — see
    :mod:`stackunderflow.etl.backfill_jobs` for the rationale.
    """
    force = bool((body or {}).get("force", False))

    try:
        job = start_job(force=force)
    except BackfillInProgressError as exc:
        # Surface the running job's id so the UI can render
        # "Backfill <abc12345> already running" without re-fetching
        # /api/etl/status. Status code 409 is the canonical "current
        # state is incompatible with this request" answer.
        return JSONResponse(
            {
                "error": "backfill_in_progress",
                "job_id": exc.current_job["job_id"],
            },
            status_code=409,
        )

    # Schedule the actual work to run after the response goes out.
    # FastAPI's BackgroundTasks runs synchronously after the response is
    # flushed, in the same worker thread — fine for our use case
    # because the route handler is async and returns immediately, and
    # the watcher cycle / other route handlers run in their own
    # threads.
    background_tasks.add_task(_run_backfill_in_background, job["job_id"], force)

    return JSONResponse(
        {"job_id": job["job_id"], "started_at": job["started_at"]},
        status_code=202,
    )


def _run_backfill_in_background(job_id: str, force: bool) -> None:
    """Worker entry point for the FastAPI BackgroundTask.

    Owns the connection lifecycle and always releases the job slot when
    done — even if the orchestrator raises. Errors are logged; the
    route has already returned 202 so we can't surface them to the
    caller, but the next ``/api/etl/status`` will reflect the cleared
    slot and the operator will see the traceback in the server log.
    """
    err: BaseException | None = None
    conn = db.connect(deps.store_path)
    try:
        # Schema is already applied on a running server, but a freshly
        # booted process might not have it yet. ``apply`` is idempotent.
        schema.apply(conn)
        run_backfill(conn, force=force)
    except Exception as exc:  # noqa: BLE001 — background task; log and clean up.
        err = exc
        _log.exception("backfill: background job %s failed", job_id)
    finally:
        try:
            conn.close()
        finally:
            # Use the canonical ``failed`` status so consumers (status
            # assembler, dashboard banner) have a single string to
            # branch on. ``error`` (the previous value) wasn't read by
            # anything but stayed inconsistent with the spec; the slot
            # had no ``last_job`` retention before this change so the
            # value never reached a consumer.
            complete_job(
                job_id,
                status="failed" if err else "complete",
                error=str(err) if err else None,
            )
