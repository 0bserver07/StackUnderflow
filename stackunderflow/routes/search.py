"""Full-text search routes."""

from fastapi import APIRouter
from fastapi.responses import JSONResponse

import stackunderflow.deps as deps
from stackunderflow.store import db, queries

router = APIRouter()


@router.get("/api/search")
async def search_messages(
    q: str = "",
    project: str | None = None,
    date_from: str | None = None,
    date_to: str | None = None,
    model: str | None = None,
    role: str | None = None,
    page: int = 1,
    per_page: int = 20,
):
    """Full-text search across all indexed Claude Code sessions.

    Args:
        q: Search query text
        project: Optional project name filter
        date_from: Optional start date (YYYY-MM-DD)
        date_to: Optional end date (YYYY-MM-DD)
        model: Optional model name filter
        role: Optional role filter (user, assistant, etc.)
        page: Page number (1-indexed)
        per_page: Results per page (max 100)
    """
    if deps.search_service is None:
        return JSONResponse(
            {"error": "Search service is not available. It failed to initialize on startup."},
            status_code=503,
        )
    try:
        per_page = min(per_page, 100)
        results = deps.search_service.search(
            query=q,
            project=project,
            date_from=date_from,
            date_to=date_to,
            model=model,
            role=role,
            page=page,
            per_page=per_page,
        )
        return JSONResponse(results)
    except Exception as e:
        deps.logger.error(f"Search error: {e}")
        return JSONResponse({"error": f"Search failed: {str(e)}"}, status_code=500)


@router.post("/api/search/reindex")
async def reindex_search():
    """Rebuild the full-text search index from all available project data."""
    if deps.search_service is None:
        return JSONResponse(
            {"error": "Search service is not available. It failed to initialize on startup."},
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
        result = deps.search_service.reindex_all(None, None, projects=projects)
        elapsed_ms = (time.time() - start_time) * 1000
        result["elapsed_ms"] = round(elapsed_ms, 2)
        deps.logger.info(
            f"Search reindex completed: {result['projects_indexed']} projects, "
            f"{result['total_messages_indexed']} messages in {elapsed_ms:.0f}ms"
        )
        return JSONResponse(result)
    except Exception as e:
        deps.logger.error(f"Reindex error: {e}")
        return JSONResponse({"error": f"Reindex failed: {str(e)}"}, status_code=500)


@router.get("/api/search/stats")
async def search_index_stats():
    """Get statistics about the search index."""
    if deps.search_service is None:
        return JSONResponse(
            {"error": "Search service is not available. It failed to initialize on startup."},
            status_code=503,
        )
    try:
        stats = deps.search_service.get_index_stats()
        indexed_projects = deps.search_service.get_indexed_projects()
        stats["indexed_projects"] = indexed_projects
        return JSONResponse(stats)
    except Exception as e:
        return JSONResponse({"error": f"Failed to get search stats: {str(e)}"}, status_code=500)
