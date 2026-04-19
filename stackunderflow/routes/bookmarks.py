"""Bookmark routes."""

from typing import Any

from fastapi import APIRouter, HTTPException
from fastapi.responses import JSONResponse

import stackunderflow.deps as deps
from stackunderflow.store import db

router = APIRouter()


@router.get("/api/bookmarks")
async def list_bookmarks(tag: str | None = None, sort_by: str = "created_at"):
    """List all bookmarks with optional filtering"""
    if deps.bookmark_service is None:
        return JSONResponse(
            {"error": "Bookmark service is not available. It failed to initialize on startup."},
            status_code=503,
        )
    try:
        bookmarks = deps.bookmark_service.list_all(tag=tag, sort_by=sort_by)

        # Enrich with session metadata from store
        if bookmarks:
            session_ids = [b["session_id"] for b in bookmarks if b.get("session_id")]
            if session_ids:
                try:
                    conn = db.connect(deps.store_path)
                    try:
                        placeholders = ",".join("?" * len(session_ids))
                        rows = conn.execute(
                            f"SELECT session_id, first_ts, last_ts, message_count "
                            f"FROM sessions WHERE session_id IN ({placeholders})",
                            session_ids,
                        ).fetchall()
                        meta = {r["session_id"]: r for r in rows}
                    finally:
                        conn.close()
                    for bm in bookmarks:
                        sid = bm.get("session_id")
                        if sid and sid in meta:
                            bm["session_first_ts"] = meta[sid]["first_ts"]
                            bm["session_last_ts"] = meta[sid]["last_ts"]
                            bm["session_message_count"] = meta[sid]["message_count"]
                except Exception:
                    pass  # metadata enrichment is best-effort

        return JSONResponse({"bookmarks": bookmarks})
    except Exception as e:
        return JSONResponse({"error": f"Failed to list bookmarks: {str(e)}"}, status_code=500)


@router.post("/api/bookmarks")
async def add_bookmark(data: dict[str, Any]):
    """Add a new bookmark"""
    if deps.bookmark_service is None:
        return JSONResponse(
            {"error": "Bookmark service is not available. It failed to initialize on startup."},
            status_code=503,
        )
    try:
        session_id = data.get("session_id")
        if not session_id:
            raise HTTPException(status_code=400, detail="session_id is required")

        title = data.get("title", "Untitled bookmark")
        message_index = data.get("message_index")
        notes = data.get("notes", "")
        tags = data.get("tags", [])

        bookmark = deps.bookmark_service.add(
            session_id=session_id,
            title=title,
            message_index=message_index,
            notes=notes,
            tags=tags,
        )
        return JSONResponse(bookmark, status_code=201)
    except HTTPException:
        raise
    except Exception as e:
        return JSONResponse({"error": f"Failed to add bookmark: {str(e)}"}, status_code=500)


@router.delete("/api/bookmarks/{bookmark_id}")
async def remove_bookmark(bookmark_id: str):
    """Remove a bookmark by ID"""
    if deps.bookmark_service is None:
        return JSONResponse(
            {"error": "Bookmark service is not available. It failed to initialize on startup."},
            status_code=503,
        )
    try:
        removed = deps.bookmark_service.remove(bookmark_id)
        if not removed:
            raise HTTPException(status_code=404, detail="Bookmark not found")
        return JSONResponse({"status": "success", "message": "Bookmark removed"})
    except HTTPException:
        raise
    except Exception as e:
        return JSONResponse({"error": f"Failed to remove bookmark: {str(e)}"}, status_code=500)


@router.put("/api/bookmarks/{bookmark_id}")
async def update_bookmark(bookmark_id: str, data: dict[str, Any]):
    """Update a bookmark (notes, tags, title)"""
    if deps.bookmark_service is None:
        return JSONResponse(
            {"error": "Bookmark service is not available. It failed to initialize on startup."},
            status_code=503,
        )
    try:
        updated = deps.bookmark_service.update(
            bookmark_id=bookmark_id,
            title=data.get("title"),
            notes=data.get("notes"),
            tags=data.get("tags"),
        )
        if not updated:
            raise HTTPException(status_code=404, detail="Bookmark not found")
        return JSONResponse(updated)
    except HTTPException:
        raise
    except Exception as e:
        return JSONResponse({"error": f"Failed to update bookmark: {str(e)}"}, status_code=500)


@router.get("/api/bookmarks/session/{session_id}")
async def get_session_bookmarks(session_id: str):
    """Get all bookmarks for a specific session"""
    if deps.bookmark_service is None:
        return JSONResponse(
            {"error": "Bookmark service is not available. It failed to initialize on startup."},
            status_code=503,
        )
    try:
        bookmarks = deps.bookmark_service.get_by_session(session_id)
        return JSONResponse({"bookmarks": bookmarks})
    except Exception as e:
        return JSONResponse({"error": f"Failed to get session bookmarks: {str(e)}"}, status_code=500)


@router.post("/api/bookmarks/toggle")
async def toggle_bookmark(data: dict[str, Any]):
    """Toggle a bookmark for a session (add if not exists, remove if exists)"""
    if deps.bookmark_service is None:
        return JSONResponse(
            {"error": "Bookmark service is not available. It failed to initialize on startup."},
            status_code=503,
        )
    try:
        session_id = data.get("session_id")
        if not session_id:
            raise HTTPException(status_code=400, detail="session_id is required")

        title = data.get("title", "Untitled bookmark")
        message_index = data.get("message_index")

        result = deps.bookmark_service.toggle(session_id=session_id, title=title, message_index=message_index)
        return JSONResponse(result)
    except HTTPException:
        raise
    except Exception as e:
        return JSONResponse({"error": f"Failed to toggle bookmark: {str(e)}"}, status_code=500)
