"""HTTP route for the context-budget estimator.

Exposes ``GET /api/context-budget?project=<slug>`` returning the same
``ContextBudget`` shape the CLI emits in JSON mode.

If ``project`` is omitted, the global budget (``~/.claude`` only) is
returned. If the slug is unknown, the client gets a 404 rather than a
silent global fallback — that's a query mistake worth surfacing.
"""

from __future__ import annotations

from pathlib import Path

from fastapi import APIRouter, HTTPException
from fastapi.responses import JSONResponse

import stackunderflow.deps as deps
from stackunderflow.services.context_budget import (
    estimate_context_budget,
    estimate_global_budget,
)
from stackunderflow.store import db, queries, schema

router = APIRouter()


@router.get("/api/context-budget")
async def get_context_budget(project: str | None = None):
    """Return the estimated per-session context budget.

    ``project`` is a project slug (the same slug used by ``/api/projects``).
    Without it, the global budget is returned.
    """
    if project is None:
        return JSONResponse(estimate_global_budget().to_dict())

    conn = db.connect(deps.store_path)
    try:
        schema.apply(conn)
        row = queries.get_project(conn, slug=project)
    finally:
        conn.close()
    if row is None:
        raise HTTPException(status_code=404, detail=f"Unknown project slug: {project}")

    project_dir = Path(row.path) if row.path else None
    if project_dir is None or not project_dir.exists():
        # Project exists in the store but the on-disk path is gone.
        # Fall back to the global budget shape rather than raising —
        # the CLAUDE.md slice will simply be zero.
        budget = estimate_global_budget()
        return JSONResponse(budget.to_dict())

    return JSONResponse(estimate_context_budget(project_dir).to_dict())
