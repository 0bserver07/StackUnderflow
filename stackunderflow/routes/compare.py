"""Model-compare route — feeds the dashboard's Compare tab.

``GET /api/compare`` returns one row per model the user touched in the
chosen window, with every metric the side-by-side card needs (sessions,
calls, one-shot %, retry rate, cache hit rate, $/call, $/session,
total cost, total tokens). The CLI ``stackunderflow compare`` shares
the exact same implementation in ``services/compare.py`` so the two
surfaces stay in lockstep by construction.
"""

from __future__ import annotations

from typing import Any

from fastapi import APIRouter, HTTPException, Query

import stackunderflow.deps as deps
from stackunderflow.services.compare import build_compare_payload
from stackunderflow.store import db, schema

router = APIRouter()


_VALID_PERIODS = ("today", "week", "month", "all")

# Module-level singleton Query objects so the function signature stays
# free of B008 (mutable default produced by a function call).
_PERIOD_Q = Query("month", description="Window: today | week | month | all")
_PROJECT_Q = Query(None, description="Project slug filter (repeatable)")
_PROVIDER_Q = Query(None, description="Provider filter (e.g. claude, codex)")


@router.get("/api/compare")
async def get_compare(
    period: str = _PERIOD_Q,
    project: list[str] | None = _PROJECT_Q,
    provider: str | None = _PROVIDER_Q,
) -> dict[str, Any]:
    """Return per-model comparison metrics over ``period``.

    Mirrors the CLI's flag surface: ``period`` accepts the same four
    aliases, ``project=<slug>`` can repeat, and ``provider`` filters by
    adapter id. Returns 400 on an unknown period rather than silently
    falling back so the frontend surfaces typos.
    """
    if period not in _VALID_PERIODS:
        raise HTTPException(
            status_code=400,
            detail=f"Unknown period '{period}'. Valid: {', '.join(_VALID_PERIODS)}",
        )

    # When the route is invoked directly (tests, not via FastAPI's DI), the
    # ``Query(None)`` default leaks through as a Query sentinel — coerce
    # anything that isn't a real list into None so the service sees what it
    # expects.
    project_filter = list(project) if isinstance(project, list) else None
    provider_filter = provider if isinstance(provider, str) else None

    conn = db.connect(deps.store_path)
    try:
        schema.apply(conn)
        payload = build_compare_payload(
            conn,
            period=period,
            project_filter=project_filter,
            provider_filter=provider_filter,
        )
    finally:
        conn.close()
    return payload
