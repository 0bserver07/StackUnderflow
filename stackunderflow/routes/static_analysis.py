"""Static-analysis routes — Spec 21 (issue #93).

Read-only endpoints under ``/api/static-analysis``:

* ``GET /api/static-analysis/session/{session_id}``
  Return the persisted findings + summary for ``session_id``. 200 with
  empty ``findings`` when the session exists but hasn't been analyzed
  yet (the dashboard can prompt the user to run ``stackunderflow
  analyze session <id>``); 200 with a populated payload otherwise. We
  *don't* analyse on demand here — that's a CLI / backfill operation
  (analyzers fork shell subprocesses, not what an HTTP handler should
  do without explicit user action).

The runner / writer side lives in
:mod:`stackunderflow.services.static_analysis`.
"""

from __future__ import annotations

from fastapi import APIRouter
from fastapi.responses import JSONResponse

import stackunderflow.deps as deps
from stackunderflow.services import static_analysis
from stackunderflow.services.static_analysis.runner import quality_to_dict
from stackunderflow.store import db, schema

router = APIRouter()


@router.get("/api/static-analysis/session/{session_id}")
async def get_session_static_analysis(session_id: str) -> JSONResponse:
    """Persisted static-analysis findings for ``session_id``.

    200 with empty ``findings`` when the session has no rows in the
    table (either never analyzed, or analyzed but produced no metrics
    — the consumer can disambiguate by checking ``findings == []``
    against ``summary.metrics`` being empty).
    """
    conn = db.connect(deps.store_path)
    try:
        # Idempotent + cheap; protects against a fresh-install request
        # firing before the lifespan migration runs.
        schema.apply(conn)
        quality = static_analysis.get_session_quality(conn, session_id)
    finally:
        conn.close()
    return JSONResponse(quality_to_dict(quality))


__all__ = ["router"]
