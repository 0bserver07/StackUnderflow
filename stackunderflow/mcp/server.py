"""FastMCP server exposing `session_query` over stdio.

Discovers Claude-Code-format JSONL session logs across the standard
agent home directories (`~/.claude`, `~/.claude-opus`, `~/.claude-glm`,
…) and parses them through `stackunderflow.adapters.claude.ClaudeAdapter`
— no SQLite, no ingest pipeline, no schema lock-in.

Run with: `stackunderflow-mcp` (stdio transport).
"""

from __future__ import annotations

import logging
from collections.abc import Iterable
from pathlib import Path
from typing import Literal

from mcp.server.fastmcp import FastMCP

from stackunderflow.adapters.base import Record, SessionRef
from stackunderflow.adapters.claude import ClaudeAdapter

_log = logging.getLogger(__name__)

# Standard locations where Claude-Code-format JSONL logs live. Each
# directory is expected to contain `projects/<slug>/<session>.jsonl`.
DEFAULT_AGENT_ROOTS: tuple[str, ...] = (
    "~/.claude",
    "~/.claude-opus",
    "~/.claude-sonnet",
    "~/.claude-haiku",
    "~/.claude-glm",
)

# Tool-input keys we consider identifying enough to surface in summaries.
_TOOL_ARG_SUMMARY_KEYS = frozenset(
    {
        "file_path",
        "path",
        "command",
        "pattern",
        "query",
        "url",
        "notebook_path",
        "old_string",
        "new_string",
        "subagent_type",
    }
)

_MAX_ARG_VALUE_LEN = 200
_PREVIEW_LEN = 200

_adapter = ClaudeAdapter()


def _enumerate_claude_format(root: Path, agent_label: str) -> Iterable[SessionRef]:
    """Yield SessionRefs for every JSONL file under `root/projects/<slug>/`."""
    projects_dir = root / "projects"
    if not projects_dir.is_dir():
        return
    for project_dir in projects_dir.iterdir():
        if not project_dir.is_dir():
            continue
        for fp in sorted(project_dir.glob("*.jsonl")):
            try:
                stat = fp.stat()
            except OSError:
                continue
            yield SessionRef(
                provider=agent_label,
                project_slug=project_dir.name,
                session_id=fp.stem,
                file_path=fp,
                file_mtime=stat.st_mtime,
                file_size=stat.st_size,
            )


def discover_sessions(
    roots: Iterable[str | Path] = DEFAULT_AGENT_ROOTS,
) -> list[SessionRef]:
    """Discover all session files across the given agent root directories.

    Each root is expanded with `~` resolution; non-existent roots are
    silently skipped. The agent label on each `SessionRef` is derived
    from the root directory name (e.g. `~/.claude-opus` → `claude-opus`).
    """
    refs: list[SessionRef] = []
    for r in roots:
        root = Path(r).expanduser() if isinstance(r, str) else Path(r).expanduser()
        if not root.is_dir():
            continue
        agent_label = root.name.lstrip(".") or "unknown"
        refs.extend(_enumerate_claude_format(root, agent_label))
    return refs


def _summarize_tool_args(raw_input: dict) -> dict:
    """Pull only identifying keys, truncate long values."""
    if not isinstance(raw_input, dict):
        return {}
    out: dict = {}
    for k, v in raw_input.items():
        if k not in _TOOL_ARG_SUMMARY_KEYS:
            continue
        if isinstance(v, str) and len(v) > _MAX_ARG_VALUE_LEN:
            out[k] = v[:_MAX_ARG_VALUE_LEN] + "…"
        else:
            out[k] = v
    return out


def _extract_tool_calls(rec: Record) -> list[dict]:
    """Return [{name, args}, …] for each tool_use block in the record."""
    msg = rec.raw.get("message") if isinstance(rec.raw, dict) else None
    if not isinstance(msg, dict):
        return []
    body = msg.get("content")
    if not isinstance(body, list):
        return []
    calls: list[dict] = []
    for blk in body:
        if not (isinstance(blk, dict) and blk.get("type") == "tool_use"):
            continue
        name = blk.get("name") or ""
        if not name:
            continue
        calls.append({"name": name, "args": _summarize_tool_args(blk.get("input") or {})})
    return calls


def _is_error_record(rec: Record) -> bool:
    """Heuristic: a tool_result block flagged is_error, or with error-like text."""
    msg = rec.raw.get("message") if isinstance(rec.raw, dict) else None
    if not isinstance(msg, dict):
        return False
    body = msg.get("content")
    if not isinstance(body, list):
        return False
    for blk in body:
        if not (isinstance(blk, dict) and blk.get("type") == "tool_result"):
            continue
        if blk.get("is_error"):
            return True
        content = blk.get("content")
        if isinstance(content, str) and _looks_like_error(content):
            return True
        if isinstance(content, list):
            for sub in content:
                if isinstance(sub, dict) and isinstance(sub.get("text"), str):
                    if _looks_like_error(sub["text"]):
                        return True
    return False


def _looks_like_error(text: str) -> bool:
    lowered = text.lower()
    return "error" in lowered or "exception" in lowered or "traceback" in lowered


def _summarize_record(rec: Record, ref: SessionRef) -> dict:
    """Build a JSON-serialisable summary of a Record."""
    preview = rec.content_text
    if len(preview) > _PREVIEW_LEN:
        preview = preview[:_PREVIEW_LEN] + "…"
    return {
        "agent": ref.provider,
        "project_slug": ref.project_slug,
        "session_id": rec.session_id,
        "timestamp": rec.timestamp,
        "role": rec.role,
        "model": rec.model,
        "tools": list(rec.tools),
        "tool_calls": _extract_tool_calls(rec),
        "content_preview": preview,
        "is_sidechain": rec.is_sidechain,
        "uuid": rec.uuid,
    }


def session_query_impl(
    session_id: str | None = None,
    limit: int = 20,
    kind: Literal["tool_calls", "errors", "all"] = "all",
    *,
    roots: Iterable[str | Path] = DEFAULT_AGENT_ROOTS,
) -> list[dict]:
    """Implementation — see `session_query` for the user-facing tool docs.

    Reads sessions in mtime-descending order so we hit recent activity
    first; stops after gathering ~4×limit candidates and then sorts by
    record timestamp. This avoids parsing every record on disk for the
    common "show me the last 20 events" query.
    """
    if limit <= 0:
        return []

    refs = discover_sessions(roots)
    if session_id is not None:
        refs = [r for r in refs if r.session_id == session_id]
    refs.sort(key=lambda r: r.file_mtime, reverse=True)

    target = max(limit * 4, limit)
    matches: list[dict] = []
    for ref in refs:
        try:
            for rec in _adapter.read(ref):
                if kind == "tool_calls" and not rec.tools:
                    continue
                if kind == "errors" and not _is_error_record(rec):
                    continue
                matches.append(_summarize_record(rec, ref))
        except Exception as exc:
            _log.warning("Failed to read %s: %s", ref.file_path, exc)
            continue
        if len(matches) >= target:
            break

    matches.sort(key=lambda m: m["timestamp"], reverse=True)
    return matches[:limit]


mcp = FastMCP("stackunderflow")


@mcp.tool()
def session_query(
    session_id: str | None = None,
    limit: int = 20,
    kind: Literal["tool_calls", "errors", "all"] = "all",
) -> list[dict]:
    """Return recent events from local coding-agent session logs.

    Scans Claude-Code-format JSONL files under `~/.claude*` directories
    (`~/.claude`, `~/.claude-opus`, `~/.claude-glm`, …) and returns a
    flat, timestamp-sorted list of events. Useful for asking the agent
    questions like "what tools did I run last hour?" or "find the last
    error I hit".

    Args:
        session_id: If set, only events from this session_id are returned.
        limit: Maximum events to return (default 20).
        kind: Filter — "tool_calls" returns only assistant records that
            invoked at least one tool; "errors" returns records whose
            tool_result blocks look like errors; "all" returns everything.

    Returns:
        List of dicts with: agent, project_slug, session_id, timestamp,
        role, model, tools, tool_calls (name + summarised args),
        content_preview, is_sidechain, uuid.
    """
    return session_query_impl(session_id=session_id, limit=limit, kind=kind)


def main() -> None:
    """Console-script entry point — run the server over stdio."""
    mcp.run()


if __name__ == "__main__":
    main()
