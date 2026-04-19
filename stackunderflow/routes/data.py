"""Data/stats/dashboard routes."""

import time
from pathlib import Path

from fastapi import APIRouter, HTTPException
from fastapi.responses import JSONResponse

import stackunderflow.deps as deps
from stackunderflow.api.messages import get_messages_summary, get_paginated_messages
from stackunderflow.ingest import run_ingest
from stackunderflow.pipeline import process as run_pipeline
from stackunderflow.pipeline.aggregator import recompute_tz_stats

router = APIRouter()


# ── helpers ──────────────────────────────────────────────────────────────────

def _require_project() -> str:
    """Return current_log_path or raise 400."""
    if not deps.current_log_path:
        raise HTTPException(status_code=400, detail="No project selected")
    return deps.current_log_path


def _fetch_or_process(
    log_path: str,
    *,
    tz_offset: int = 0,
) -> tuple[list[dict], dict, str]:
    """Return (messages, stats, source) from cache or pipeline.

    source is 'memory', 'disk', or 'pipeline'.
    """
    # L1 — memory
    hit = deps.cache.fetch(log_path)
    if hit:
        return hit[0], hit[1], "memory"

    # L2 — disk
    cached_stats = deps.cache.load_stats(log_path)
    cached_msgs = deps.cache.load_messages(log_path)
    if cached_stats and cached_msgs and not deps.cache.has_disk_changes(log_path):
        deps.cache.store(log_path, cached_msgs, cached_stats)
        return cached_msgs, cached_stats, "disk"

    # L3 — process from JSONL
    messages, stats = run_pipeline(log_path, tz_offset=tz_offset)
    deps.cache.persist_stats(log_path, stats)
    deps.cache.persist_messages(log_path, messages)
    deps.cache.store(log_path, messages, stats)
    return messages, stats, "pipeline"


def _ensure_disk_cache(log_path: str, messages: list[dict], stats: dict) -> None:
    """Persist to disk if not already cached."""
    if not deps.cache.load_stats(log_path):
        deps.cache.persist_stats(log_path, stats)
        deps.cache.persist_messages(log_path, messages)


def _reindex_services(log_path: str, messages: list[dict]) -> None:
    """Update search/QA/tag indexes for a project."""
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


# ── routes ───────────────────────────────────────────────────────────────────

@router.get("/api/stats")
async def get_stats(timezone_offset: int = 0):
    """Get statistics for the current project."""
    log_path = _require_project()
    t0 = time.time()

    messages, stats, source = _fetch_or_process(log_path, tz_offset=timezone_offset)
    _ensure_disk_cache(log_path, messages, stats)

    deps.logger.debug(f"stats [{source}] {(time.time()-t0)*1000:.1f}ms")
    return stats


@router.get("/api/dashboard-data")
async def get_dashboard_data(timezone_offset: int = 0):
    """Get optimized data for initial dashboard load."""
    log_path = _require_project()
    t0 = time.time()

    messages, stats, source = _fetch_or_process(log_path, tz_offset=timezone_offset)

    # Recompute tz-sensitive stats if served from cache with a non-zero offset
    if timezone_offset != 0 and source != "pipeline":
        tz_patch = recompute_tz_stats(messages, timezone_offset)
        stats = {**stats, **tz_patch}

    first_page = get_paginated_messages(messages, page=1, per_page=50)

    deps.logger.debug(f"dashboard-data [{source}] {(time.time()-t0)*1000:.1f}ms")

    return {
        "statistics": stats,
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

    messages, _, source = _fetch_or_process(log_path, tz_offset=timezone_offset)

    deps.logger.debug(f"messages [{source}] {(time.time()-t0)*1000:.1f}ms")

    if limit and limit < len(messages):
        return messages[:limit]
    return messages


@router.get("/api/messages/summary")
async def get_messages_summary_endpoint():
    """Get summary statistics about messages without loading all data."""
    log_path = _require_project()
    messages, _, _ = _fetch_or_process(log_path)
    return get_messages_summary(messages)


@router.post("/api/refresh")
async def refresh_data(request: dict):
    """Refresh project data — runs an incremental ingest pass."""
    if not deps.current_log_path:
        return await refresh_all_projects(request)

    log_path = deps.current_log_path
    tz_offset = request.get("timezone_offset", 0)
    t0 = time.time()

    if not deps.cache.has_disk_changes(log_path):
        ms = (time.time() - t0) * 1000
        return JSONResponse({
            "status": "success",
            "message": "No changes detected - using cached data",
            "files_changed": False,
            "refresh_time_ms": ms,
        })

    # Files changed — invalidate and reprocess
    try:
        deps.cache.drop(log_path)
        deps.cache.invalidate_disk(log_path)

        messages, stats = run_pipeline(log_path, tz_offset=tz_offset)
        deps.cache.persist_stats(log_path, stats)
        deps.cache.persist_messages(log_path, messages)
        deps.cache.store(log_path, messages, stats)

        deps.is_reindexing = True
        try:
            _reindex_services(log_path, messages)
        finally:
            deps.is_reindexing = False

        ms = (time.time() - t0) * 1000
        return JSONResponse({
            "status": "success",
            "message": "Files changed - data refreshed successfully",
            "files_changed": True,
            "message_count": len(messages),
            "refresh_time_ms": ms,
        })
    except Exception as e:
        raise HTTPException(status_code=500, detail=f"Error refreshing data: {str(e)}") from e


async def refresh_all_projects(request: dict):
    """Refresh all projects — runs an incremental ingest pass via the session store."""
    from stackunderflow.adapters import registered
    from stackunderflow.store import db, schema

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


@router.get("/api/cache/status")
async def get_cache_status():
    """Get cache statistics."""
    memory_stats = deps.cache.metrics()
    project_info = deps.cache.slot_info(deps.current_log_path) if deps.current_log_path else None
    return JSONResponse({
        "cache": memory_stats,
        "current_project": project_info,
        "current_log_path": deps.current_log_path,
    })
