"""HTTP routes for session quality grading."""

from __future__ import annotations

from fastapi import APIRouter, HTTPException

import stackunderflow.deps as deps
from stackunderflow.services.grading import get_stored_grade, grade_session
from stackunderflow.store import db

router = APIRouter()


@router.get("/api/static-analysis/session/{session_id}/quality")
async def get_quality(session_id: str):
    """Retrieve session quality metrics, performing lazy grading if missing."""
    conn = db.connect(deps.store_path)
    try:
        # Check if session exists first so we raise a proper 404
        sess = conn.execute("SELECT id FROM sessions WHERE session_id = ?", (session_id,)).fetchone()
        if sess is None:
            raise HTTPException(status_code=404, detail=f"Session {session_id} not found")

        grade = get_stored_grade(conn, session_id)
        if grade is None:
            # Trigger lazy grading
            grade = grade_session(conn, session_id, force=False)
        return grade
    finally:
        conn.close()


@router.post("/api/static-analysis/session/{session_id}/grade")
async def post_grade(session_id: str):
    """Force re-grading of the session and return the fresh quality metrics."""
    conn = db.connect(deps.store_path)
    try:
        # Check if session exists first so we raise a proper 404
        sess = conn.execute("SELECT id FROM sessions WHERE session_id = ?", (session_id,)).fetchone()
        if sess is None:
            raise HTTPException(status_code=404, detail=f"Session {session_id} not found")

        grade = grade_session(conn, session_id, force=True)
        return grade
    finally:
        conn.close()
