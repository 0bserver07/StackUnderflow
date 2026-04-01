"""Miscellaneous routes: pricing, share, related, curriculum, health, favicon, assets, ollama proxy."""

import os
from typing import Any

from fastapi import APIRouter, HTTPException, Request
from fastapi.responses import FileResponse, JSONResponse

import stackunderflow.deps as deps
from stackunderflow.pipeline import process as run_pipeline

router = APIRouter()


# ── Pricing ───────────────────────────────────────────────────────────────────

@router.get("/api/pricing")
async def get_pricing():
    """Get current model pricing"""
    if deps.pricing_service is None:
        return JSONResponse(
            {"error": "Pricing service is not available. It failed to initialize on startup."},
            status_code=503,
        )
    try:
        pricing_data = deps.pricing_service.get_pricing()

        return JSONResponse(
            {
                "pricing": pricing_data["pricing"],
                "source": pricing_data["source"],
                "timestamp": pricing_data["timestamp"],
                "is_stale": pricing_data.get("is_stale", False),
            }
        )
    except Exception as e:
        return JSONResponse({"error": f"Failed to get pricing: {str(e)}"}, status_code=500)


@router.post("/api/pricing/refresh")
async def refresh_pricing():
    """Force refresh pricing data"""
    if deps.pricing_service is None:
        return JSONResponse(
            {"error": "Pricing service is not available. It failed to initialize on startup."},
            status_code=503,
        )
    try:
        success = deps.pricing_service.force_refresh()

        if success:
            return JSONResponse({"status": "success", "message": "Pricing updated successfully"})
        else:
            return JSONResponse({"status": "error", "message": "Failed to fetch pricing from LiteLLM"}, status_code=500)
    except Exception as e:
        return JSONResponse({"error": f"Failed to refresh pricing: {str(e)}"}, status_code=500)


# ── Share ─────────────────────────────────────────────────────────────────────

@router.get("/api/share-enabled")
async def share_enabled():
    """Check if sharing is enabled"""
    return {"enabled": True}


@router.post("/api/share")
async def create_share_link(data: dict[str, Any], request: Request):
    """Create a shareable link for the dashboard"""
    try:
        from stackunderflow.share import ShareManager

        share_manager = ShareManager()

        # Extract request info for logging
        request_info = {
            "ip": request.client.host if request.client else None,
            "user_agent": request.headers.get("user-agent"),
        }

        # Create share link
        result = await share_manager.create_share_link(
            statistics=data.get("statistics", {}),
            charts_data=data.get("charts", {}),
            make_public=data.get("make_public", False),
            include_commands=data.get("include_commands", False),
            user_commands=data.get("user_commands", []),
            project_name=data.get("project_name"),
            request_info=request_info,
        )

        return result
    except Exception as e:
        import traceback

        traceback.print_exc()
        raise HTTPException(status_code=500, detail=str(e))


# ── Related sessions ──────────────────────────────────────────────────────────

@router.get("/api/related/{session_id}")
async def get_related_sessions(session_id: str, limit: int = 5):
    """Get sessions related to a given session based on shared tags, project, and tools.

    Args:
        session_id: The session ID to find related sessions for
        limit: Maximum number of results (default 5, max 20)

    Returns:
        JSON with related sessions list sorted by similarity score
    """
    if deps.related_service is None:
        return JSONResponse(
            {"error": "Related service is not available. It failed to initialize on startup."},
            status_code=503,
        )
    try:
        limit = min(max(1, limit), 20)

        # Get messages from cache if available (needed for tool/project info)
        messages = None
        if deps.current_log_path:
            memory_result = deps.cache.fetch(deps.current_log_path)
            if memory_result:
                messages, _ = memory_result
            else:
                cached_messages = deps.cache.load_messages(deps.current_log_path)
                if cached_messages:
                    messages = cached_messages

        related = deps.related_service.find_related(
            session_id=session_id,
            messages=messages,
            limit=limit,
        )

        return JSONResponse({
            "session_id": session_id,
            "related": related,
            "count": len(related),
        })
    except Exception as e:
        deps.logger.error(f"Error finding related sessions: {e}")
        return JSONResponse(
            {"error": f"Failed to find related sessions: {str(e)}"},
            status_code=500,
        )


# ── Curriculum ────────────────────────────────────────────────────────────────

@router.get("/api/curriculum")
async def get_curriculum(
    focus: str | None = None,
    difficulty: str | None = None,
    refresh: bool = False,
):
    """
    Generate personalized learning curriculum from error patterns and Q&A history.

    Uses Modal + Claude to analyze your Claude Code usage and create lessons.

    Args:
        focus: Optional focus area (e.g., "file operations", "bash commands")
        difficulty: "beginner", "intermediate", or "advanced"
        refresh: Force regeneration (ignore cache)
    """
    from stackunderflow.services.curriculum_service import get_curriculum_service

    if not deps.current_log_path or not os.path.exists(deps.current_log_path):
        raise HTTPException(status_code=400, detail="No project selected. Set a project first.")

    try:
        # Get stats (use cached if available)
        messages, stats = run_pipeline(deps.current_log_path)

        # Get Q&A pairs for the current project
        qa_pairs = []
        if deps.qa_service:
            try:
                from pathlib import Path as _Path
                project_dir = _Path(deps.current_log_path).name if deps.current_log_path else None
                qa_data = deps.qa_service.list_qa(project=project_dir, per_page=50)
                qa_pairs = qa_data.get("results", [])
            except Exception as e:
                deps.logger.warning(f"Failed to get Q&A pairs: {e}")

        # Generate curriculum
        curriculum_service = get_curriculum_service()
        curriculum = curriculum_service.generate_curriculum(
            stats=stats,
            qa_pairs=qa_pairs,
            focus_area=focus,
            difficulty=difficulty,
            use_cache=not refresh,
        )

        return JSONResponse(curriculum)

    except Exception as e:
        deps.logger.error(f"Curriculum generation failed: {e}")
        return JSONResponse(
            {"error": f"Failed to generate curriculum: {str(e)}"},
            status_code=500,
        )


@router.get("/api/curriculum/exercise/{error_category}")
async def get_exercise_for_error(error_category: str):
    """
    Generate a focused exercise for a specific error type.

    Args:
        error_category: The error type (e.g., "File Not Found", "Syntax Error")
    """
    from stackunderflow.services.curriculum_service import get_curriculum_service

    if not deps.current_log_path or not os.path.exists(deps.current_log_path):
        raise HTTPException(status_code=400, detail="No project selected. Set a project first.")

    try:
        # Get error examples from stats
        messages, stats = run_pipeline(deps.current_log_path)

        errors = stats.get("errors", {})
        by_category = errors.get("by_category", errors)

        error_data = by_category.get(error_category, {})
        examples = error_data.get("examples", []) if isinstance(error_data, dict) else []

        # Generate exercise
        curriculum_service = get_curriculum_service()
        exercise = curriculum_service.generate_exercise_for_error(
            error_category=error_category,
            error_examples=examples,
        )

        return JSONResponse(exercise)

    except Exception as e:
        deps.logger.error(f"Exercise generation failed: {e}")
        return JSONResponse(
            {"error": f"Failed to generate exercise: {str(e)}"},
            status_code=500,
        )


@router.get("/api/curriculum/status")
async def get_curriculum_status():
    """Check if Modal curriculum generation is available."""
    from stackunderflow.services.curriculum_service import get_curriculum_service

    service = get_curriculum_service()
    modal_available = service._check_modal_available()

    return JSONResponse({
        "modal_available": modal_available,
        "mode": "modal" if modal_available else "local",
        "deploy_command": None,
    })


# ── Health ────────────────────────────────────────────────────────────────────

@router.get("/api/health")
async def health_check():
    """Health check endpoint"""
    services = {
        "search": deps.search_service is not None,
        "tags": deps.tag_service is not None,
        "qa": deps.qa_service is not None,
        "bookmarks": deps.bookmark_service is not None,
        "pricing": deps.pricing_service is not None,
        "social": deps.social_service is not None,
    }
    return {"status": "ok", "services": services}


# ── Favicon ───────────────────────────────────────────────────────────────────

@router.get("/favicon.ico")
async def favicon():
    """Return empty favicon to prevent 404 errors"""
    return (
        FileResponse(os.path.join(deps.BASE_DIR, "static", "favicon.ico"), media_type="image/x-icon")
        if os.path.exists(os.path.join(deps.BASE_DIR, "static", "favicon.ico"))
        else JSONResponse(content={}, status_code=204)
    )


# ── React assets ──────────────────────────────────────────────────────────────

@router.get("/assets/{full_path:path}")
async def serve_react_assets(full_path: str):
    """Serve built React assets"""
    from pathlib import Path as _Path

    assets_dir = _Path(deps.BASE_DIR, "static", "react", "assets").resolve()
    file_path = _Path(deps.BASE_DIR, "static", "react", "assets", full_path).resolve()
    if not str(file_path).startswith(str(assets_dir) + os.sep):
        return JSONResponse(content={"error": "Invalid path"}, status_code=400)
    if file_path.exists() and file_path.is_file():
        return FileResponse(str(file_path))
    return JSONResponse(content={"error": "Asset not found"}, status_code=404)


# ── Ollama proxy ──────────────────────────────────────────────────────────────

@router.api_route("/ollama-api/{path:path}", methods=["GET", "POST", "PUT", "DELETE"])
async def ollama_proxy(path: str, request: Request):
    """Proxy requests to local Ollama instance"""
    import httpx

    ollama_url = f"http://localhost:11434/api/{path}"
    try:
        body = await request.body()
        async with httpx.AsyncClient(timeout=120.0) as client:
            response = await client.request(
                method=request.method,
                url=ollama_url,
                content=body,
                headers={k: v for k, v in request.headers.items() if k.lower() not in ('host', 'content-length')},
            )
            # For streaming responses
            if response.headers.get("transfer-encoding") == "chunked":
                from starlette.responses import StreamingResponse

                async def stream():
                    async for chunk in response.aiter_bytes():
                        yield chunk

                return StreamingResponse(
                    stream(),
                    status_code=response.status_code,
                    headers=dict(response.headers),
                )
            ct = response.headers.get("content-type", "")
            body = response.json() if ct.startswith("application/json") else {}
            return JSONResponse(content=body, status_code=response.status_code)
    except Exception:
        return JSONResponse(content={"error": "Ollama not available"}, status_code=502)
