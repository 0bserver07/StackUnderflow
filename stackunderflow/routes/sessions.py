"""Session / JSONL file browsing routes (backed by session store)."""

import json
from datetime import datetime
from pathlib import Path

from fastapi import APIRouter, HTTPException
from fastapi.responses import JSONResponse

import stackunderflow.deps as deps
from stackunderflow.infra.costs import compute_cost
from stackunderflow.store import db, queries

router = APIRouter()


def _iso_to_ts(iso: str | None) -> float:
    if not iso:
        return 0.0
    try:
        return datetime.fromisoformat(iso.replace("Z", "+00:00")).timestamp()
    except (ValueError, AttributeError):
        return 0.0


def _duration_minutes(first: str | None, last: str | None) -> float | None:
    if not first or not last:
        return None
    try:
        start = datetime.fromisoformat(first.replace("Z", "+00:00"))
        end = datetime.fromisoformat(last.replace("Z", "+00:00"))
        return (end - start).total_seconds() / 60
    except (ValueError, AttributeError):
        return None


@router.get("/api/jsonl-files")
async def get_jsonl_files(project: str | None = None):
    """Get list of JSONL files for a project with metadata"""
    log_path = deps.current_log_path

    if project:
        slug = project
    elif log_path:
        slug = Path(log_path).name
    else:
        raise HTTPException(status_code=400, detail="No project selected")

    try:
        conn = db.connect(deps.store_path)
        try:
            project_row = queries.get_project(conn, slug=slug)
            if project_row is None:
                return JSONResponse([])

            sessions = queries.list_sessions(conn, project_id=project_row.id)

            files = []
            for session in sessions:
                stats = queries.get_session_stats(conn, session_fk=session.id)

                first_msg_row = conn.execute(
                    "SELECT content_text FROM messages "
                    "WHERE session_fk = ? AND role = 'user' "
                    "  AND content_text IS NOT NULL AND content_text != '' "
                    "ORDER BY seq LIMIT 1",
                    (session.id,),
                ).fetchone()
                title = first_msg_row["content_text"][:150] if first_msg_row else None

                estimated_cost = 0.0
                if stats["model"] and (stats["input_tokens"] or stats["output_tokens"]):
                    cost_data = compute_cost(
                        {"input": stats["input_tokens"], "output": stats["output_tokens"]},
                        stats["model"],
                    )
                    estimated_cost = cost_data.get("total_cost", 0.0)

                files.append({
                    "name": f"{session.session_id}.jsonl",
                    "path": f"{session.session_id}.jsonl",
                    "is_subagent": session.session_id.startswith("agent-"),
                    "created": _iso_to_ts(session.first_ts),
                    "modified": _iso_to_ts(session.last_ts),
                    "size": 0,
                    "messages": session.message_count,
                    "user_messages": stats["user_messages"],
                    "assistant_messages": stats["assistant_messages"],
                    "input_tokens": stats["input_tokens"],
                    "output_tokens": stats["output_tokens"],
                    "model": stats["model"],
                    "title": title,
                    "tool_calls": stats["tool_calls"],
                    "estimated_cost": round(estimated_cost, 4),
                })
        finally:
            conn.close()

        files.sort(key=lambda x: x["created"])
        return JSONResponse(files)
    except HTTPException:
        raise
    except Exception as e:
        raise HTTPException(status_code=500, detail=f"Error reading log files: {str(e)}") from e


@router.get("/api/sessions/compare")
async def compare_sessions(a: str, b: str, log_path: str | None = None):
    """Compare two sessions — returns cost/token/duration diffs per spec §1.10.

    Reuses ``session_costs`` from the standard dashboard payload so both
    sides share the exact same cost-attribution logic as the Cost tab.
    """
    path = log_path or deps.current_log_path
    if not path:
        raise HTTPException(status_code=400, detail="No project selected or log_path provided")

    slug = Path(path).name
    try:
        conn = db.connect(deps.store_path)
        try:
            project_row = queries.get_project(conn, slug=slug)
            if project_row is None:
                raise HTTPException(status_code=404, detail=f"Project '{slug}' not found in store")
            _, stats = queries.get_project_stats(conn, project_id=project_row.id)
        finally:
            conn.close()
    except HTTPException:
        raise
    except Exception as e:
        raise HTTPException(status_code=500, detail=f"Failed to load stats: {e}") from e

    session_costs = stats.get("session_costs", []) or []
    by_id = {s["session_id"]: s for s in session_costs}
    sa = by_id.get(a)
    sb = by_id.get(b)
    if sa is None or sb is None:
        missing = [sid for sid, hit in ((a, sa), (b, sb)) if hit is None]
        raise HTTPException(
            status_code=404,
            detail=f"Session(s) not found: {', '.join(missing)}",
        )

    keys = set(sa.get("tokens", {})) | set(sb.get("tokens", {}))
    diff = {
        "cost":       sb["cost"] - sa["cost"],
        "tokens":     {k: sb["tokens"].get(k, 0) - sa["tokens"].get(k, 0) for k in keys},
        "commands":   sb["commands"] - sa["commands"],
        "errors":     sb["errors"] - sa["errors"],
        "duration_s": sb["duration_s"] - sa["duration_s"],
    }
    return JSONResponse({"a": sa, "b": sb, "diff": diff})


@router.get("/api/jsonl-content")
async def get_jsonl_content(file: str, project: str | None = None):
    """Get content of a specific JSONL file"""
    log_path = deps.current_log_path

    if project:
        slug = project
    elif log_path:
        slug = Path(log_path).name
    else:
        raise HTTPException(status_code=400, detail="No project selected")

    session_id = Path(file).stem
    if not session_id:
        raise HTTPException(status_code=400, detail="Invalid file parameter")

    try:
        conn = db.connect(deps.store_path)
        try:
            project_row = queries.get_project(conn, slug=slug)
            if project_row is None:
                raise HTTPException(status_code=404, detail="Project not found in store")

            session_row = conn.execute(
                "SELECT id FROM sessions WHERE project_id = ? AND session_id = ?",
                (project_row.id, session_id),
            ).fetchone()
            if session_row is None:
                raise HTTPException(status_code=404, detail="File not found")

            messages = queries.get_session_messages(conn, session_fk=session_row["id"])
        finally:
            conn.close()

        lines = []
        user_count = 0
        assistant_count = 0
        cwd = None

        for i, msg in enumerate(messages):
            try:
                raw = json.loads(msg.raw_json)
            except (json.JSONDecodeError, TypeError):
                raw = {"error": "parse error", "line_number": i + 1}
            lines.append(raw)
            if i == 0:
                cwd = raw.get("cwd", "")
            if msg.role == "user":
                user_count += 1
            elif msg.role == "assistant":
                assistant_count += 1

        first_ts = messages[0].timestamp if messages else None
        last_ts = messages[-1].timestamp if messages else None

        return JSONResponse({
            "lines": lines,
            "total_lines": len(lines),
            "user_count": user_count,
            "assistant_count": assistant_count,
            "metadata": {
                "session_id": session_id,
                "file_size": 0,
                "created": _iso_to_ts(first_ts),
                "modified": _iso_to_ts(last_ts),
                "first_timestamp": first_ts,
                "last_timestamp": last_ts,
                "duration_minutes": _duration_minutes(first_ts, last_ts),
                "cwd": cwd,
            },
        })
    except HTTPException:
        raise
    except Exception as e:
        raise HTTPException(status_code=500, detail=f"Error reading file: {str(e)}") from e
