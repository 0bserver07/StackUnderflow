"""Session / JSONL file browsing routes."""

import json
import logging
import os
from pathlib import Path

from fastapi import APIRouter, HTTPException
from fastapi.responses import JSONResponse

import stackunderflow.deps as deps
from stackunderflow.infra.costs import compute_cost

router = APIRouter()
_log = logging.getLogger(__name__)


# Get JSONL files for current project
@router.get("/api/jsonl-files")
async def get_jsonl_files(project: str | None = None):
    """Get list of JSONL files for a project with metadata"""
    log_path = deps.current_log_path

    # If specific project provided, use that
    if project:
        claude_base = Path.home() / ".claude" / "projects"
        constructed = (claude_base / project).resolve()
        if not str(constructed).startswith(str(claude_base.resolve()) + os.sep):
            raise HTTPException(status_code=400, detail="Invalid path")
        log_path = str(constructed)

    if not log_path:
        raise HTTPException(status_code=400, detail="No project selected")

    try:
        log_dir = Path(log_path)
        if not log_dir.exists():
            raise HTTPException(status_code=404, detail="Log directory not found")

        # Get all JSONL files with metadata
        files_with_metadata = []
        for f in log_dir.rglob("*.jsonl"):
            stat = f.stat()

            # Try to get first and last timestamps from file content
            first_timestamp = None
            last_timestamp = None
            try:
                with open(f) as file:
                    # Read first line
                    first_line = file.readline()
                    if first_line:
                        first_data = json.loads(first_line)
                        first_timestamp = first_data.get("timestamp")

                    # Read last line efficiently
                    # Seek to end and read backwards to find last complete line
                    file.seek(0, 2)  # Go to end of file
                    file_length = file.tell()

                    # Read last 4KB (should be enough for last line)
                    seek_pos = max(0, file_length - 4096)
                    file.seek(seek_pos)
                    last_chunk = file.read()

                    # Find last complete line
                    lines = last_chunk.strip().split("\n")
                    if lines:
                        last_line = lines[-1]
                        try:
                            last_data = json.loads(last_line)
                            last_timestamp = last_data.get("timestamp")
                        except (json.JSONDecodeError, ValueError):
                            # If last line is incomplete, try second to last
                            if len(lines) > 1:
                                try:
                                    last_data = json.loads(lines[-2])
                                    last_timestamp = last_data.get("timestamp")
                                except (json.JSONDecodeError, ValueError):
                                    pass
            except Exception as exc:
                _log.debug("Metadata extraction failed for %s: %s", f, exc)
                # fall through with whatever defaults we have

            # Convert timestamps to unix time if available
            created_time = stat.st_ctime
            modified_time = stat.st_mtime

            if first_timestamp:
                try:
                    from datetime import datetime

                    dt = datetime.fromisoformat(first_timestamp.replace("Z", "+00:00"))
                    created_time = dt.timestamp()
                except (ValueError, AttributeError):
                    pass

            if last_timestamp:
                try:
                    from datetime import datetime

                    dt = datetime.fromisoformat(last_timestamp.replace("Z", "+00:00"))
                    modified_time = dt.timestamp()
                except (ValueError, AttributeError):
                    pass

            # Quick scan for rich metadata
            msg_count = 0
            user_count = 0
            assistant_count = 0
            total_input_tok = 0
            total_output_tok = 0
            primary_model = None
            first_user_msg = None
            tool_count = 0
            try:
                import orjson as _oj
                raw_bytes = f.read_bytes()
                for raw_line in raw_bytes.split(b"\n"):
                    if not raw_line.strip():
                        continue
                    try:
                        obj = _oj.loads(raw_line)
                    except Exception:  # noqa: S112 -- per-line JSON parse in hot path; logging would be too noisy
                        continue
                    line_type = obj.get("type", "")
                    if line_type in ("user", "human"):
                        msg_count += 1
                        # distinguish real prompts from tool result returns
                        msg_body = obj.get("message", {})
                        c = msg_body.get("content") if isinstance(msg_body, dict) else None
                        if c is None:
                            c = obj.get("content")
                        is_tool_return = isinstance(c, list) and any(
                            isinstance(blk, dict) and blk.get("type") == "tool_result"
                            for blk in c
                        )
                        if not is_tool_return:
                            user_count += 1
                            if first_user_msg is None:
                                if isinstance(c, str) and c.strip():
                                    first_user_msg = c.strip()[:150]
                                elif isinstance(c, list):
                                    for blk in c:
                                        if isinstance(blk, str) and blk.strip():
                                            first_user_msg = blk.strip()[:150]
                                            break
                                        if isinstance(blk, dict) and blk.get("type") == "text":
                                            t = (blk.get("text", "") or "").strip()
                                            if t:
                                                first_user_msg = t[:150]
                                                break
                    elif line_type == "assistant":
                        assistant_count += 1
                        msg_count += 1
                        msg_body = obj.get("message", {})
                        if isinstance(msg_body, dict):
                            m = msg_body.get("model")
                            if m and m != "N/A" and not primary_model:
                                primary_model = m
                            usage = msg_body.get("usage", {})
                            if isinstance(usage, dict):
                                total_input_tok += usage.get("input_tokens", 0) or 0
                                total_output_tok += usage.get("output_tokens", 0) or 0
                            content = msg_body.get("content")
                            if isinstance(content, list):
                                tool_count += sum(
                                    1 for blk in content
                                    if isinstance(blk, dict) and blk.get("type") == "tool_use"
                                )
            except Exception as exc:
                _log.debug("Token/tool aggregation failed for %s: %s", f, exc)

            # Compute estimated cost from token counts + model
            estimated_cost = 0.0
            if primary_model and (total_input_tok or total_output_tok):
                cost_data = compute_cost(
                    {"input": total_input_tok, "output": total_output_tok},
                    primary_model,
                )
                estimated_cost = cost_data.get("total_cost", 0.0)

            # Relative path from log_dir for nested sub-agent files
            rel_path = str(f.relative_to(log_dir))
            is_subagent = f.name.startswith("agent-") or "subagents" in str(f)

            files_with_metadata.append({
                "name": f.name,
                "path": rel_path,
                "is_subagent": is_subagent,
                "created": created_time,
                "modified": modified_time,
                "size": stat.st_size,
                "messages": msg_count,
                "user_messages": user_count,
                "assistant_messages": assistant_count,
                "input_tokens": total_input_tok,
                "output_tokens": total_output_tok,
                "model": primary_model,
                "title": first_user_msg,
                "tool_calls": tool_count,
                "estimated_cost": round(estimated_cost, 4),
            })

        # Sort by creation time (earliest first)
        files_with_metadata.sort(key=lambda x: x["created"])

        return JSONResponse(files_with_metadata)
    except Exception as e:
        raise HTTPException(status_code=500, detail=f"Error reading log files: {str(e)}") from e


# Get JSONL file content
@router.get("/api/jsonl-content")
async def get_jsonl_content(file: str, project: str | None = None):
    """Get content of a specific JSONL file"""
    log_path = deps.current_log_path

    # If specific project provided, use that
    if project:
        claude_base = Path.home() / ".claude" / "projects"
        constructed = (claude_base / project).resolve()
        if not str(constructed).startswith(str(claude_base.resolve()) + os.sep):
            raise HTTPException(status_code=400, detail="Invalid path")
        log_path = str(constructed)

    if not log_path:
        raise HTTPException(status_code=400, detail="No project selected")

    try:
        log_dir_path = Path(log_path)
        file_path = (log_dir_path / file).resolve()
        if not str(file_path).startswith(str(log_dir_path.resolve()) + os.sep):
            raise HTTPException(status_code=400, detail="Invalid path")
        if not file_path.exists() or not file_path.suffix == ".jsonl":
            raise HTTPException(status_code=404, detail="File not found")

        # Get file metadata
        stat = file_path.stat()
        file_size = stat.st_size
        created_time = stat.st_ctime
        modified_time = stat.st_mtime

        lines = []
        user_count = 0
        assistant_count = 0
        first_timestamp = None
        last_timestamp = None
        session_id = None
        cwd = None

        with open(file_path) as f:
            for line_num, line in enumerate(f, 1):
                try:
                    data = json.loads(line)
                    lines.append(data)

                    # Extract metadata
                    if line_num == 1:
                        # Use filename as session ID since files can contain multiple sessions
                        session_id = file_path.stem
                        cwd = data.get("cwd", "")

                    # Track timestamps
                    if data.get("timestamp"):
                        if not first_timestamp:
                            first_timestamp = data["timestamp"]
                        last_timestamp = data["timestamp"]

                    # Count types
                    if data.get("type") == "user":
                        user_count += 1
                    elif data.get("type") == "assistant":
                        assistant_count += 1
                except json.JSONDecodeError:
                    # Include malformed lines for debugging
                    lines.append({"error": "JSON decode error", "line_number": line_num, "raw": line[:200]})

        # Calculate duration if we have timestamps
        duration_minutes = None
        if first_timestamp and last_timestamp:
            try:
                from datetime import datetime

                start = datetime.fromisoformat(first_timestamp.replace("Z", "+00:00"))
                end = datetime.fromisoformat(last_timestamp.replace("Z", "+00:00"))
                duration_minutes = (end - start).total_seconds() / 60

                # Use actual timestamps from content instead of file metadata
                created_time = start.timestamp()
                modified_time = end.timestamp()
            except (ValueError, AttributeError, KeyError):
                pass

        return JSONResponse(
            {
                "lines": lines,
                "total_lines": len(lines),
                "user_count": user_count,
                "assistant_count": assistant_count,
                "metadata": {
                    "session_id": session_id,
                    "file_size": file_size,
                    "created": created_time,
                    "modified": modified_time,
                    "first_timestamp": first_timestamp,
                    "last_timestamp": last_timestamp,
                    "duration_minutes": duration_minutes,
                    "cwd": cwd,
                },
            }
        )

    except Exception as e:
        raise HTTPException(status_code=500, detail=f"Error reading file: {str(e)}") from e
