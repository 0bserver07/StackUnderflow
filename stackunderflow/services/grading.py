"""LLM-Graded Session Quality service.

Retrieves transcripts, static analysis findings, and queries a local Ollama
instance to grade session quality, saving results in the database.
"""

from __future__ import annotations

import json
import logging
import sqlite3
from datetime import UTC, datetime
from typing import Any

import httpx

from stackunderflow.services.static_analysis.runner import get_session_quality

logger = logging.getLogger(__name__)


def get_stored_grade(conn: sqlite3.Connection, session_id: str) -> dict[str, Any] | None:
    """Retrieve the stored grade for a session if it exists."""
    row = conn.execute(
        "SELECT overall_score, grades_json, rationale, suggestions_json, graded_at "
        "FROM session_quality_metrics WHERE session_id = ?",
        (session_id,),
    ).fetchone()
    if row is None:
        return None

    try:
        grades = json.loads(row["grades_json"])
    except Exception:
        grades = {}

    try:
        suggestions = json.loads(row["suggestions_json"])
    except Exception:
        suggestions = []

    return {
        "session_id": session_id,
        "overall_score": float(row["overall_score"]),
        "grades": grades,
        "rationale": str(row["rationale"]),
        "suggestions": suggestions,
        "graded_at": str(row["graded_at"]),
        # Only real (LLM) grades are ever persisted, so a stored row is "llm".
        "grade_source": "llm",
    }


def grade_session(
    conn: sqlite3.Connection,
    session_id: str,
    *,
    force: bool = False,
    ollama_url: str = "http://localhost:11434",
) -> dict[str, Any]:
    """Grade a session using a local Ollama model, caching the result.

    If cached result exists and `force` is False, return the cached result.
    """
    if not force:
        cached = get_stored_grade(conn, session_id)
        if cached is not None:
            return cached

    # 1. Retrieve transcript
    messages = conn.execute(
        "SELECT m.role, m.content_text "
        "FROM messages m "
        "JOIN sessions s ON s.id = m.session_fk "
        "WHERE s.session_id = ? "
        "ORDER BY m.seq",
        (session_id,),
    ).fetchall()

    transcript_parts = []
    for m in messages:
        role = m["role"].upper()
        content = m["content_text"]
        if content:
            # truncate message content to prevent context window explosion
            if len(content) > 4000:
                content = content[:4000] + "\n... [TRUNCATED] ..."
            transcript_parts.append(f"[{role}]: {content}")

    transcript_text = "\n\n".join(transcript_parts)
    if not transcript_text:
        transcript_text = "(Empty session transcript)"

    # 2. Retrieve static analysis deltas
    sa_quality = get_session_quality(conn, session_id)
    sa_metrics = sa_quality.summary.get("metrics", {})

    sa_parts = []
    for metric, info in sa_metrics.items():
        sa_parts.append(
            f"- {metric}: improved={info.get('improved', 0)}, "
            f"regressed={info.get('regressed', 0)}, "
            f"avg_delta={info.get('avg_delta')}"
        )
    static_analysis_text = "\n".join(sa_parts) if sa_parts else "(No static analysis deltas)"

    # 3. Discover model
    model_name = "qwen2.5-coder:7b"
    try:
        resp = httpx.get(f"{ollama_url}/api/tags", timeout=3.0)
        if resp.status_code == 200:
            models = resp.json().get("models", [])
            if models:
                model_name = models[0]["name"]
    except Exception as e:
        logger.debug("Failed to discover Ollama models, using fallback %r: %s", model_name, e)

    # 4. Formulate prompts
    system_prompt = (
        "You are an expert technical lead grading an AI coding assistant session.\n"
        "Analyze the transcript of the session, the static-analysis findings (if any), and grade the session.\n"
        "Your response MUST be a single, valid JSON object containing exactly these keys:\n"
        "{\n"
        '  "overall_score": <float 1.0 to 10.0>,\n'
        '  "grades": {\n'
        '    "goal_clarity": <float 1.0 to 10.0>,\n'
        '    "execution_efficiency": <float 1.0 to 10.0>,\n'
        '    "success": <float 1.0 to 10.0>\n'
        "  },\n"
        '  "rationale": "<brief explanation text>",\n'
        '  "suggestions": ["<suggestion 1>", "<suggestion 2>", ...]\n'
        "}"
    )

    user_prompt = (
        f"--- SESSION TRANSCRIPT ---\n"
        f"{transcript_text}\n\n"
        f"--- STATIC ANALYSIS DELTAS ---\n"
        f"{static_analysis_text}\n\n"
        f"Please grade this session and return the JSON assessment."
    )

    # 5. Query Ollama
    result_data = None
    try:
        payload = {
            "model": model_name,
            "messages": [
                {"role": "system", "content": system_prompt},
                {"role": "user", "content": user_prompt},
            ],
            "format": "json",
            "options": {
                "temperature": 0.2,
            },
            "stream": False,
        }
        resp = httpx.post(f"{ollama_url}/api/chat", json=payload, timeout=30.0)
        if resp.status_code == 200:
            content_str = resp.json().get("message", {}).get("content", "")
            result_data = json.loads(content_str)
        else:
            logger.warning("Ollama API chat returned HTTP %s", resp.status_code)
    except Exception as e:
        logger.warning("Ollama API chat call failed: %s", e)

    # A missing/malformed model response is a TRANSIENT fallback, not a real
    # grade — whether Ollama threw, returned non-200, or sent unparseable JSON.
    # We must NOT persist it: a lazy GET while Ollama is down would otherwise
    # write a fabricated 5.0 into session_quality_metrics that is
    # indistinguishable from a real grade and is then served from cache forever
    # (corrupting every aggregate over grades). Instead we return it clearly
    # flagged and uncached, so the next request recomputes it once Ollama is back.
    is_fallback = not isinstance(result_data, dict)
    if is_fallback:
        result_data = {
            "overall_score": 5.0,
            "grades": {
                "goal_clarity": 5.0,
                "execution_efficiency": 5.0,
                "success": 5.0,
            },
            "rationale": "Fallback grade: local Ollama instance was offline or failed to grade.",
            "suggestions": ["Ensure local Ollama service is running on port 11434."],
        }

    overall_score = float(result_data.get("overall_score", 5.0))
    grades = result_data.get("grades", {})
    if not isinstance(grades, dict):
        grades = {}
    grades.setdefault("goal_clarity", 5.0)
    grades.setdefault("execution_efficiency", 5.0)
    grades.setdefault("success", 5.0)

    rationale = str(result_data.get("rationale", "No rationale provided."))
    suggestions = result_data.get("suggestions", [])
    if not isinstance(suggestions, list):
        suggestions = [str(suggestions)] if suggestions else []

    graded_at = datetime.now(UTC).isoformat().replace("+00:00", "Z")
    grade_source = "fallback" if is_fallback else "llm"

    # 6. Persist ONLY real grades. Fallbacks are transient, so a later request
    # recomputes them instead of leaving fabricated data behind in the store.
    if not is_fallback:
        conn.execute(
            "INSERT OR REPLACE INTO session_quality_metrics "
            "(session_id, overall_score, grades_json, rationale, suggestions_json, graded_at) "
            "VALUES (?, ?, ?, ?, ?, ?)",
            (
                session_id,
                overall_score,
                json.dumps(grades),
                rationale,
                json.dumps(suggestions),
                graded_at,
            ),
        )
        conn.commit()

    return {
        "session_id": session_id,
        "overall_score": overall_score,
        "grades": grades,
        "rationale": rationale,
        "suggestions": suggestions,
        "graded_at": graded_at,
        "grade_source": grade_source,
    }
