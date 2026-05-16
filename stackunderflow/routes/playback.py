"""Playback routes — per-session (and per-project) tool-call timeline.

Two read-only endpoints, both under ``/api/playback``:

* ``GET /api/playback/{session_id}``
  Ordered tool-call event stream for one session. Query params:
  ``tool_filter=Edit,Write`` (comma-separated, exact tool names),
  ``limit=1000``, ``include_payload=1`` (default on — set ``0`` for a
  lighter payload without the 200-char excerpts).
  Response: ``{"session_id", "events": [PlaybackEvent...], "total", "truncated"}``.
  404 when the session id isn't in the store.

* ``GET /api/playback/project/{project_slug}``
  Cross-session timeline for a whole project. Query params:
  ``since=7d`` (relative ``Nd|Nh|Nm`` or an ISO-8601 instant),
  ``tool_filter=Edit``, ``limit=5000``, ``include_payload=1`` (default
  *off* here — a project-wide stream is large).
  Response: ``{"project_slug", "events": [...], "total", "truncated"}``.
  404 when the slug isn't in the store.

This surface is **pure read-side** over ``messages.raw_json`` (+ the
optional spec-05 ``captured_events`` table for an authoritative
success/failure flag) — no schema migration. See
``.notes/specs/10-playback-timeline.md`` for the v1/v2 scope split
(v1 = event stream only; v2 = virtual-filesystem reconstruction, later).
"""

from __future__ import annotations

import re
import sqlite3
from datetime import UTC, datetime, timedelta

from fastapi import APIRouter, HTTPException, Query
from fastapi.responses import JSONResponse

import stackunderflow.deps as deps
from stackunderflow.services import playback as playback_service
from stackunderflow.services import playback_fs as playback_fs_service
from stackunderflow.services import risk as risk_service
from stackunderflow.store import db, queries, schema

router = APIRouter()

_SINCE_RELATIVE = re.compile(r"^\s*(\d+)\s*([dhm])\s*$", re.IGNORECASE)
_UNIT_SECONDS = {"d": 86_400, "h": 3_600, "m": 60}


def _parse_tool_filter(raw: str | None) -> list[str] | None:
    """``"Edit,Write"`` → ``["Edit", "Write"]``; blank / ``None`` → ``None``."""
    if not raw:
        return None
    parts = [p.strip() for p in raw.split(",")]
    cleaned = [p for p in parts if p]
    return cleaned or None


def _parse_since(raw: str | None) -> str | None:
    """Translate a ``since`` query value to an ISO-8601 lower bound.

    Accepts a relative spec (``7d``, ``24h``, ``90m``) — resolved against
    ``now`` (UTC) — or a literal ISO timestamp, which is passed straight
    through. Anything unrecognised is treated as a literal (the SQL
    comparison just won't match, which is harmless). ``None`` / blank →
    ``None`` (no lower bound).
    """
    if not raw or not raw.strip():
        return None
    m = _SINCE_RELATIVE.match(raw)
    if m:
        amount = int(m.group(1))
        secs = amount * _UNIT_SECONDS[m.group(2).lower()]
        return (datetime.now(UTC) - timedelta(seconds=secs)).isoformat()
    return raw.strip()


def _parse_paths_param(raw: str | None) -> list[str] | None:
    """``"src/a.py,src/b.py"`` → ``["src/a.py", "src/b.py"]``."""
    if not raw:
        return None
    parts = [p.strip() for p in raw.split(",")]
    cleaned = [p for p in parts if p]
    return cleaned or None


@router.get("/api/playback/{session_id}/fs")
async def get_session_fs_snapshot(
    session_id: str,
    at: str = Query(..., description="ISO-8601 / RFC-3339 cutoff timestamp"),
    paths: str | None = Query(None, description="Comma-separated file paths to restrict"),
    include_content: bool = Query(True),
) -> JSONResponse:
    """Reconstruct file contents for ``session_id`` at time ``at``.

    Replays the session's Read / Write / Edit / MultiEdit / NotebookEdit
    tool calls (in order, up to and including ``at``) and returns the
    state of each touched file. ``paths`` restricts to specific files;
    ``include_content=false`` returns metadata only (sizes + which
    operations were applied) without the file bodies.

    Each ``files[<path>]`` entry also carries a ``risk`` block when the
    file has been reverted at least once in the past — see
    :func:`stackunderflow.services.risk.file_risk_summary`. Files with
    no failure-mode history have no ``risk`` key (the badge is rendered
    only when the count is non-zero, so the metadata fetch stays small).

    * 404 — session not in store.
    * 422 — ``at`` couldn't be parsed.
    * 200 — ``files`` may be empty when the session exists but issued no
      file-touching tool calls before ``at``.
    """
    conn = db.connect(deps.store_path)
    try:
        schema.apply(conn)
        try:
            snapshot = playback_fs_service.reconstruct_fs_at(
                conn,
                session_id,
                at=at,
                paths=_parse_paths_param(paths),
                include_content=include_content,
            )
        except playback_fs_service.UnknownSession as e:
            raise HTTPException(status_code=404, detail=str(e)) from e
        except playback_fs_service.FsReconstructionError as e:
            raise HTTPException(status_code=422, detail=str(e)) from e
        # Per-file risk overlay. Cheap on a typical snapshot (≤ tens of
        # files) and read-only against the same connection. Skip the
        # call entirely when there are no files (keeps the trivial
        # path free of needless work).
        files = snapshot.get("files") or {}
        for path in files:
            try:
                summary = risk_service.file_risk_summary(conn, path)
            except (ValueError, sqlite3.DatabaseError):
                # Best-effort: a malformed path or a flaky read should
                # not fail the snapshot endpoint.
                continue
            if summary["reverted"] > 0 or summary["failed"] > 0:
                files[path]["risk"] = {
                    "reverted_count": summary["reverted"],
                    "failed_count": summary["failed"],
                    "worked_count": summary["worked"],
                    "total_sessions": summary["total_sessions"],
                }
    finally:
        conn.close()
    return JSONResponse(snapshot)


@router.get("/api/playback/{session_id}")
async def get_session_playback(
    session_id: str,
    tool_filter: str | None = Query(None),
    limit: int = Query(1000, ge=1, le=10_000),
    include_payload: bool = Query(True),
) -> JSONResponse:
    """Ordered tool-call event stream for ``session_id``.

    404 when the session can't be found in the store. 200 with an empty
    ``events`` list when the session exists but issued no tool calls — so
    the dashboard can tell "wrong session" from "nothing to play back".
    """
    conn = db.connect(deps.store_path)
    try:
        # Idempotent + cheap on a current store; protects against a
        # fresh-install request before the lifespan migration runs.
        schema.apply(conn)
        page = playback_service.session_playback_page(
            conn,
            session_id,
            tool_filter=_parse_tool_filter(tool_filter),
            limit=limit,
            include_payload=include_payload,
        )
    finally:
        conn.close()

    if page is None:
        raise HTTPException(
            status_code=404, detail=f"Session not found in store: {session_id}"
        )
    events, truncated = page
    return JSONResponse(
        {
            "session_id": session_id,
            "events": [playback_service.playback_event_to_dict(e) for e in events],
            "total": len(events),
            "truncated": truncated,
        }
    )


@router.get("/api/playback/project/{project_slug}")
async def get_project_timeline(
    project_slug: str,
    since: str | None = Query(None),
    tool_filter: str | None = Query(None),
    limit: int = Query(5000, ge=1, le=20_000),
    include_payload: bool = Query(False),
) -> JSONResponse:
    """Cross-session tool-call timeline for ``project_slug``.

    404 when the slug isn't in the store. ``since`` defaults to "no lower
    bound"; pass ``7d`` (or an ISO instant) to scope to recent activity —
    on a busy project the unbounded stream can be very large, which is why
    ``include_payload`` defaults to *off* on this endpoint.
    """
    conn = db.connect(deps.store_path)
    try:
        schema.apply(conn)
        project = queries.get_project(conn, slug=project_slug)
        if project is None:
            raise HTTPException(
                status_code=404, detail=f"Project not found in store: {project_slug}"
            )
        events, truncated = playback_service.project_timeline_page(
            conn,
            project.id,
            since=_parse_since(since),
            tool_filter=_parse_tool_filter(tool_filter),
            limit=limit,
            include_payload=include_payload,
        )
    finally:
        conn.close()

    return JSONResponse(
        {
            "project_slug": project_slug,
            "events": [playback_service.playback_event_to_dict(e) for e in events],
            "total": len(events),
            "truncated": truncated,
        }
    )


# Re-exported for tests and documentation.
__all__ = ["router"]
