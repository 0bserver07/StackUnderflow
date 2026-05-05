"""Optimize / waste-detection routes.

Surfaces both the legacy looped-Q&A waste view and the structural
pattern findings (CLAUDE.md bloat, unused MCP, ghost agents, junk
reads, cache thrash, oversized bash output, exploration-only sessions).

GET ``/api/optimize?period=30days`` returns:
    {
        "scope": "last 30 days",
        "waste": [...],          # legacy find_waste()
        "patterns": [Finding,...]
    }
"""

from __future__ import annotations

from typing import Annotated

from fastapi import APIRouter, HTTPException, Query

import stackunderflow.deps as deps
from stackunderflow.reports.optimize import find_patterns, find_waste
from stackunderflow.reports.scope import parse_period
from stackunderflow.store import db, schema

router = APIRouter()


_VALID_PERIODS = {"today", "7days", "30days", "month", "all"}


@router.get("/api/optimize")
async def get_optimize_report(
    period: str = "30days",
    project: Annotated[list[str] | None, Query()] = None,
    exclude: Annotated[list[str] | None, Query()] = None,
):
    """Run waste + structural-pattern detection over *period*.

    Args:
        period: ``today | 7days | 30days | month | all``.
        project: Optional repeated query param to narrow project scope.
        exclude: Optional repeated query param to drop projects.
    """
    if period not in _VALID_PERIODS:
        raise HTTPException(
            status_code=400,
            detail=f"Unknown period '{period}'. Valid: {', '.join(sorted(_VALID_PERIODS))}",
        )

    scope = parse_period(period)

    conn = db.connect(deps.store_path)
    try:
        schema.apply(conn)
        waste = find_waste(
            conn,
            scope=scope,
            include=project,
            exclude=exclude,
        )
        patterns = find_patterns(
            conn,
            scope=scope,
            project_filter=project,
        )
    finally:
        conn.close()

    return {
        "scope": scope.label,
        "waste": waste,
        "patterns": [p.to_dict() for p in patterns],
    }
