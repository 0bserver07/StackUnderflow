"""Q&A pair routes."""

from fastapi import APIRouter, HTTPException
from fastapi.responses import JSONResponse

import stackunderflow.deps as deps

router = APIRouter()


@router.get("/api/qa")
async def list_qa_pairs(
    project: str | None = None,
    date_from: str | None = None,
    date_to: str | None = None,
    search: str | None = None,
    resolution_status: str | None = None,
    page: int = 1,
    per_page: int = 20,
):
    """List extracted Q&A pairs with filtering and pagination.

    Args:
        project: Optional project name filter
        date_from: Optional start date (YYYY-MM-DD)
        date_to: Optional end date (YYYY-MM-DD)
        search: Optional search text within Q&A pairs
        resolution_status: Optional filter; one of 'resolved' | 'looped' | 'open'
        page: Page number (1-indexed)
        per_page: Results per page (max 100)
    """
    if deps.qa_service is None:
        return JSONResponse(
            {"error": "Q&A service is not available. It failed to initialize on startup."},
            status_code=503,
        )
    try:
        per_page = min(per_page, 100)
        results = deps.qa_service.list_qa(
            project=project,
            date_from=date_from,
            date_to=date_to,
            search=search,
            resolution_status=resolution_status,
            page=page,
            per_page=per_page,
        )
        return JSONResponse(results)
    except Exception as e:
        deps.logger.error(f"Q&A list error: {e}")
        return JSONResponse({"error": f"Failed to list Q&A pairs: {str(e)}"}, status_code=500)


@router.get("/api/qa/stats")
async def qa_stats():
    """Get statistics about the Q&A index."""
    if deps.qa_service is None:
        return JSONResponse(
            {"error": "Q&A service is not available. It failed to initialize on startup."},
            status_code=503,
        )
    try:
        stats = deps.qa_service.get_stats()
        return JSONResponse(stats)
    except Exception as e:
        return JSONResponse({"error": f"Failed to get Q&A stats: {str(e)}"}, status_code=500)


@router.get("/api/qa/{qa_id}")
async def get_qa_pair(qa_id: str):
    """Get a single Q&A pair with full context."""
    if deps.qa_service is None:
        return JSONResponse(
            {"error": "Q&A service is not available. It failed to initialize on startup."},
            status_code=503,
        )
    try:
        qa = deps.qa_service.get_qa_by_id(qa_id)
        if not qa:
            raise HTTPException(status_code=404, detail="Q&A pair not found")
        return JSONResponse(qa)
    except HTTPException:
        raise
    except Exception as e:
        return JSONResponse({"error": f"Failed to get Q&A pair: {str(e)}"}, status_code=500)


@router.post("/api/qa/reindex")
async def reindex_qa():
    """Rebuild the Q&A index by re-extracting pairs from all sessions."""
    if deps.qa_service is None:
        return JSONResponse(
            {"error": "Q&A service is not available. It failed to initialize on startup."},
            status_code=503,
        )
    import time

    start_time = time.time()
    try:
        result = deps.qa_service.reindex_all(deps.cache, deps.cache)
        elapsed_ms = (time.time() - start_time) * 1000
        result["elapsed_ms"] = round(elapsed_ms, 2)
        deps.logger.info(
            f"Q&A reindex completed: {result['projects_indexed']} projects, "
            f"{result['total_qa_indexed']} Q&A pairs in {elapsed_ms:.0f}ms"
        )
        return JSONResponse(result)
    except Exception as e:
        deps.logger.error(f"Q&A reindex error: {e}")
        return JSONResponse({"error": f"Q&A reindex failed: {str(e)}"}, status_code=500)
