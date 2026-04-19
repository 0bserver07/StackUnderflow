"""Tag management routes."""

from fastapi import APIRouter, HTTPException
from fastapi.responses import JSONResponse

import stackunderflow.deps as deps
from stackunderflow.store import db, queries

router = APIRouter()


@router.get("/api/tags")
async def get_tag_cloud():
    """Get tag cloud (all tags with counts and metadata)"""
    if deps.tag_service is None:
        return JSONResponse(
            {"error": "Tag service is not available. It failed to initialize on startup."},
            status_code=503,
        )
    try:
        cloud = deps.tag_service.get_tag_cloud()
        return JSONResponse(cloud)
    except Exception as e:
        return JSONResponse({"error": f"Failed to get tag cloud: {str(e)}"}, status_code=500)


@router.get("/api/tags/session/{session_id}")
async def get_session_tags(session_id: str):
    """Get all tags for a specific session"""
    if deps.tag_service is None:
        return JSONResponse(
            {"error": "Tag service is not available. It failed to initialize on startup."},
            status_code=503,
        )
    try:
        tags = deps.tag_service.get_session_tags(session_id)
        return JSONResponse(tags)
    except Exception as e:
        return JSONResponse({"error": f"Failed to get session tags: {str(e)}"}, status_code=500)


@router.post("/api/tags/session/{session_id}")
async def add_manual_tag(session_id: str, data: dict[str, str]):
    """Add a manual tag to a session"""
    if deps.tag_service is None:
        return JSONResponse(
            {"error": "Tag service is not available. It failed to initialize on startup."},
            status_code=503,
        )
    try:
        tag = data.get("tag", "").strip()
        if not tag:
            raise HTTPException(status_code=400, detail="tag is required")

        result = deps.tag_service.add_manual_tag(session_id, tag)
        return JSONResponse(result)
    except HTTPException:
        raise
    except Exception as e:
        return JSONResponse({"error": f"Failed to add tag: {str(e)}"}, status_code=500)


@router.delete("/api/tags/session/{session_id}/{tag}")
async def remove_manual_tag(session_id: str, tag: str):
    """Remove a manual tag from a session"""
    if deps.tag_service is None:
        return JSONResponse(
            {"error": "Tag service is not available. It failed to initialize on startup."},
            status_code=503,
        )
    try:
        result = deps.tag_service.remove_manual_tag(session_id, tag)
        return JSONResponse(result)
    except Exception as e:
        return JSONResponse({"error": f"Failed to remove tag: {str(e)}"}, status_code=500)


@router.get("/api/tags/browse/{tag}")
async def browse_tag(tag: str):
    """Get all sessions with a given tag"""
    if deps.tag_service is None:
        return JSONResponse(
            {"error": "Tag service is not available. It failed to initialize on startup."},
            status_code=503,
        )
    try:
        sessions = deps.tag_service.get_sessions_by_tag(tag)
        return JSONResponse({"tag": tag, "sessions": sessions, "count": len(sessions)})
    except Exception as e:
        return JSONResponse({"error": f"Failed to browse tag: {str(e)}"}, status_code=500)


@router.post("/api/tags/reindex")
async def reindex_tags():
    """Rebuild auto-tags from all available project data"""
    if deps.tag_service is None:
        return JSONResponse(
            {"error": "Tag service is not available. It failed to initialize on startup."},
            status_code=503,
        )
    import time

    start_time = time.time()
    try:
        conn = db.connect(deps.store_path)
        try:
            project_rows = queries.list_projects(conn)
        finally:
            conn.close()
        projects = [{"dir_name": p.slug, "log_path": p.path or ""} for p in project_rows]
        result = deps.tag_service.reindex_all(None, None, projects=projects)
        elapsed_ms = (time.time() - start_time) * 1000
        result["elapsed_ms"] = round(elapsed_ms, 2)
        deps.logger.info(
            f"Tag reindex completed: {result['projects_indexed']} projects, "
            f"{result['total_sessions_tagged']} sessions in {elapsed_ms:.0f}ms"
        )
        return JSONResponse(result)
    except Exception as e:
        deps.logger.error(f"Tag reindex error: {e}")
        return JSONResponse({"error": f"Reindex failed: {str(e)}"}, status_code=500)
