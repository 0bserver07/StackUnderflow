"""Miscellaneous routes: pricing, health, favicon, assets, ollama proxy."""

import os

from fastapi import APIRouter, Request
from fastapi.responses import FileResponse, JSONResponse

import stackunderflow.deps as deps

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
