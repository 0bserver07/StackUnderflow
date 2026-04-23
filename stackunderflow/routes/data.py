"""Data/stats/dashboard routes — store-backed, no pipeline or cache imports."""

from __future__ import annotations

import time
from pathlib import Path

from fastapi import APIRouter, HTTPException
from fastapi.responses import JSONResponse

import stackunderflow.deps as deps
from stackunderflow.adapters import registered
from stackunderflow.api.messages import get_messages_summary, get_paginated_messages
from stackunderflow.ingest import run_ingest
from stackunderflow.routes.cost import COST_KEYS
from stackunderflow.store import db, queries, schema

router = APIRouter()


# ── helpers ───────────────────────────────────────────────────────────────────

def _require_project() -> str:
    if not deps.current_log_path:
        raise HTTPException(status_code=400, detail="No project selected")
    return deps.current_log_path


def _get_project_id(conn, log_path: str) -> int:
    slug = Path(log_path).name
    row = queries.get_project(conn, slug=slug)
    if row is None:
        raise HTTPException(
            status_code=404,
            detail=f"Project '{slug}' not found in store — try /api/refresh first",
        )
    return row.id


def _reindex_services(log_path: str, messages: list[dict]) -> None:
    project_dir = Path(log_path).name
    for svc, name in [
        (deps.search_service, "search"),
        (deps.qa_service, "qa"),
        (deps.tag_service, "tags"),
    ]:
        if svc is None:
            continue
        try:
            if name == "tags":
                svc.index_project(messages)
            else:
                svc.index_project(project_dir, messages)
        except Exception as e:
            deps.logger.debug(f"{name} index update failed: {e}")


# ── routes ────────────────────────────────────────────────────────────────────

@router.get("/api/stats")
async def get_stats(timezone_offset: int = 0):
    """Get statistics for the current project."""
    log_path = _require_project()
    t0 = time.time()
    conn = db.connect(deps.store_path)
    try:
        project_id = _get_project_id(conn, log_path)
        _, stats = queries.get_project_stats(conn, project_id=project_id, tz_offset=timezone_offset)
    finally:
        conn.close()
    deps.logger.debug(f"stats [store] {(time.time()-t0)*1000:.1f}ms")
    return stats


@router.get("/api/dashboard-data")
async def get_dashboard_data(timezone_offset: int = 0):
    """Get optimized data for initial dashboard load."""
    log_path = _require_project()
    t0 = time.time()
    conn = db.connect(deps.store_path)
    try:
        project_id = _get_project_id(conn, log_path)
        messages, stats = queries.get_project_stats(
            conn, project_id=project_id, tz_offset=timezone_offset
        )
    finally:
        conn.close()

    first_page = get_paginated_messages(messages, page=1, per_page=50)
    # §A3: the heavy analytics sections moved to /api/cost-data. Strip them
    # from this payload so the initial dashboard load stays under 1 MB.
    lean_stats = {k: v for k, v in stats.items() if k not in COST_KEYS}
    deps.logger.debug(f"dashboard-data [store] {(time.time()-t0)*1000:.1f}ms")
    return {
        "statistics": lean_stats,
        "messages_page": first_page,
        "message_count": len(messages),
        "is_reindexing": deps.is_reindexing,
        "config": {
            "messages_initial_load": deps.config.get("messages_initial_load"),
            "max_date_range_days": deps.config.get("max_date_range_days"),
        },
    }


@router.get("/api/messages")
async def get_messages(limit: int | None = None, timezone_offset: int = 0):
    """Get messages for the current project."""
    log_path = _require_project()
    t0 = time.time()
    conn = db.connect(deps.store_path)
    try:
        project_id = _get_project_id(conn, log_path)
        messages = queries.get_project_messages(conn, project_id=project_id, limit=limit)
    finally:
        conn.close()
    deps.logger.debug(f"messages [store] {(time.time()-t0)*1000:.1f}ms")
    return messages


@router.get("/api/messages/summary")
async def get_messages_summary_endpoint():
    """Get summary statistics about messages without loading all data."""
    log_path = _require_project()
    conn = db.connect(deps.store_path)
    try:
        project_id = _get_project_id(conn, log_path)
        messages = queries.get_project_messages(conn, project_id=project_id)
    finally:
        conn.close()
    return get_messages_summary(messages)


@router.post("/api/refresh")
async def refresh_data(request: dict):
    """Refresh project data — runs an incremental ingest pass then returns status."""
    if not deps.current_log_path:
        return await refresh_all_projects(request)

    log_path = deps.current_log_path
    t0 = time.time()
    conn = db.connect(deps.store_path)
    try:
        schema.apply(conn)
        counts = run_ingest(conn, registered())
    finally:
        conn.close()

    slug = Path(log_path).name
    new_msgs = counts.get(slug, 0)

    if new_msgs:
        conn2 = db.connect(deps.store_path)
        try:
            row = queries.get_project(conn2, slug=slug)
            if row is not None:
                messages = queries.get_project_messages(conn2, project_id=row.id)
                deps.is_reindexing = True
                try:
                    _reindex_services(log_path, messages)
                finally:
                    deps.is_reindexing = False
        finally:
            conn2.close()

    ms = int((time.time() - t0) * 1000)
    return JSONResponse({
        "status": "success",
        "message": (
            "Files changed - data refreshed successfully"
            if new_msgs else "No changes detected - using cached data"
        ),
        "files_changed": new_msgs > 0,
        "message_count": new_msgs,
        "refresh_time_ms": ms,
    })


async def refresh_all_projects(request: dict):
    """Refresh all projects — runs an incremental ingest pass via the session store."""
    t0 = time.time()
    conn = db.connect(deps.store_path)
    try:
        schema.apply(conn)
        counts = run_ingest(conn, registered())
    finally:
        conn.close()

    total_new = sum(counts.values())
    ms = int((time.time() - t0) * 1000)
    return JSONResponse({
        "status": "success",
        "message": (
            f"Ingested {total_new} new records"
            if total_new else "No changes detected"
        ),
        "files_changed": total_new > 0,
        "refresh_time_ms": ms,
        "projects_refreshed": total_new,
        "total_projects": total_new,
    })
