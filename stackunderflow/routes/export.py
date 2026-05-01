"""Dashboard-facing export route.

Mirrors the ``stackunderflow export`` CLI command so the React UI can
download the same CSV / JSON via a button click. The actual rollup +
rendering lives in ``stackunderflow.reports.export.run_export`` —
both the CLI and this route call it, so output stays in lockstep.
"""

from __future__ import annotations

from fastapi import APIRouter, HTTPException, Query
from fastapi.responses import Response

import stackunderflow.deps as deps
from stackunderflow.reports.export import run_export
from stackunderflow.store import db, schema

router = APIRouter()

_VALID_FORMATS = {"csv", "json"}
_VALID_PERIODS = {"today", "week", "month", "all"}

# Module-level singleton Query objects so the function signature stays
# free of B008 (mutable default produced by a function call).
_FMT_Q     = Query(..., description="Output format: csv or json.")
_PERIOD_Q  = Query(
    None,
    description=(
        "Window: today, week, month, all. "
        "Omit for multi-period rollup (today + 7d + 30d)."
    ),
)
_PROV_Q    = Query(None, description="Filter by provider.")
_PROJECT_Q = Query(
    default=None,
    description="Include only this project slug. Repeatable.",
)
_EXCLUDE_Q = Query(
    default=None,
    description="Exclude this project slug. Repeatable.",
)


@router.get("/api/export")
async def export_endpoint(
    format: str = _FMT_Q,  # noqa: A002 — matches CLI flag name
    period: str | None = _PERIOD_Q,
    provider: str | None = _PROV_Q,
    project: list[str] | None = _PROJECT_Q,
    exclude: list[str] | None = _EXCLUDE_Q,
):
    """Stream an export file as a download attachment.

    Returns ``text/csv`` or ``application/json`` with a
    ``Content-Disposition: attachment`` header so the browser saves
    the file rather than rendering it.
    """
    if format not in _VALID_FORMATS:
        raise HTTPException(
            status_code=400,
            detail=f"format must be one of {sorted(_VALID_FORMATS)}",
        )
    if period is not None and period not in _VALID_PERIODS:
        raise HTTPException(
            status_code=400,
            detail=f"period must be one of {sorted(_VALID_PERIODS)} or omitted",
        )

    conn = db.connect(deps.store_path)
    try:
        schema.apply(conn)
        try:
            text, content_type, filename = run_export(
                conn,
                fmt=format,
                period=period,
                provider=provider,
                include=list(project) if project else None,
                exclude=list(exclude) if exclude else None,
            )
        except ValueError as e:
            raise HTTPException(status_code=400, detail=str(e)) from e
    finally:
        conn.close()

    return Response(
        content=text,
        media_type=content_type,
        headers={
            "Content-Disposition": f'attachment; filename="{filename}"',
            # Help the React fetch/blob path know the right name without
            # parsing Content-Disposition (some libs don't surface it).
            "X-Suggested-Filename": filename,
        },
    )
