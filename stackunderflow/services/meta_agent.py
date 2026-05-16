"""Meta-agent — tool-call dispatcher for the right-side sidebar.

The meta-agent gives the user's local LLM access to the same store the
dashboard reads. The route in ``stackunderflow/routes/meta_agent.py``
calls Ollama's ``/api/chat`` with the tool catalogue defined here; when
the model emits a ``tool_calls`` array we dispatch each call to a
backend executor, append the JSON-safe result as a ``role: "tool"``
message, and call Ollama again. The loop is bounded by
``MAX_TOOL_HOPS`` so a misbehaving model can't drive a runaway chain.

Why this module exists
----------------------
The discovery / playback / cost services are already shaped for read-
only access (they take a ``conn`` and return dataclasses + dicts). The
meta-agent doesn't re-implement them; it adapts their return types to
the small flat-JSON shape that the LLM can read inside its context
window. Every tool result is capped at ``_RESULT_CHAR_BUDGET`` bytes of
text so a model with a 4k context doesn't blow up on a single
``get_session_playback`` response.

Privacy
-------
Nothing here talks to a remote LLM. The route only proxies to the local
Ollama instance at ``localhost:11434`` (same as
``routes/misc.ollama_proxy``); all tool executors read from the local
SQLite store at ``~/.stackunderflow/store.db``. The user's data never
leaves the machine.
"""

from __future__ import annotations

import json
import sqlite3
import time
from collections.abc import Callable
from dataclasses import dataclass
from datetime import UTC, datetime
from typing import Any

from stackunderflow.reports.aggregate import build_report
from stackunderflow.reports.scope import parse_period
from stackunderflow.services import discovery, playback_fs

__all__ = [
    "TOOL_CATALOG",
    "ToolResult",
    "execute_tool",
    "MAX_TOOL_HOPS",
]


# ── budget ──────────────────────────────────────────────────────────────────
#
# Cap the textual size of every tool result so a single noisy tool can't
# blow the LLM context window. ``_RESULT_CHAR_BUDGET`` is the inclusive
# upper bound on ``json.dumps(result)`` — we slice payloads from the tail
# end (snippets / file content / events) when over budget rather than
# refusing to return anything.
_RESULT_CHAR_BUDGET = 4_000

# How many tool-call hops we allow in one user turn before the loop
# stops calling Ollama again. 5 covers "search → drill in → summarise"
# style chains without enabling pathological loops.
MAX_TOOL_HOPS = 5


# ── result type ─────────────────────────────────────────────────────────────


@dataclass(frozen=True)
class ToolResult:
    """Outcome of one tool execution. Always JSON-serialisable.

    * ``ok`` — true when the executor returned without raising. False
      results still carry ``data`` (an ``{"error": ...}`` dict) so the
      LLM can see the failure and adjust.
    * ``data`` — the JSON-safe payload that will be stringified into the
      ``role: "tool"`` message.
    * ``duration_ms`` — wall time the executor spent. Surfaced to the
      frontend so the tool-call surface can show a timing badge.
    """

    name: str
    ok: bool
    data: dict[str, Any]
    duration_ms: int

    def to_dict(self) -> dict[str, Any]:
        return {
            "name": self.name,
            "ok": self.ok,
            "data": self.data,
            "duration_ms": self.duration_ms,
        }


# ── tool catalog (JSON schema) ─────────────────────────────────────────────
#
# Ollama's tool-call format follows the OpenAI ``function``-call shape:
# each tool entry is ``{"type": "function", "function": {name, description,
# parameters: <jsonschema>}}``. Models that advertise the ``tools``
# capability (qwen2.5-coder, llama3.2, firefunction-v2, others) honour
# this contract; models without it ignore the array and we fall back to
# plain chat — the frontend renders a "tools unavailable" pill above the
# input.

TOOL_CATALOG: list[dict[str, Any]] = [
    {
        "type": "function",
        "function": {
            "name": "search_past_decisions",
            "description": (
                "Search the user's StackUnderflow store for past sessions whose "
                "messages mention a free-form query string. Returns a ranked list "
                "of matching sessions with project / cost / a short content "
                "excerpt. Use this for 'have I dealt with X before?' style "
                "questions."
            ),
            "parameters": {
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Free-form text. Empty returns no matches.",
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Max sessions to return (1..20). Default 5.",
                        "minimum": 1,
                        "maximum": 20,
                    },
                    "project": {
                        "type": "string",
                        "description": (
                            "Optional project slug filter (matches "
                            "``projects.slug``)."
                        ),
                    },
                    "since": {
                        "type": "string",
                        "description": (
                            "Optional cutoff. ``\"7d\"`` / ``\"30d\"`` / "
                            "``\"24h\"`` or an ISO timestamp."
                        ),
                    },
                },
                "required": ["query"],
            },
        },
    },
    {
        "type": "function",
        "function": {
            "name": "find_sessions_in_path",
            "description": (
                "List the user's StackUnderflow sessions whose project filesystem "
                "path is ``path`` or any ancestor of it. Useful for 'show me what "
                "happened in this repo' / 'recent activity in this directory' "
                "questions."
            ),
            "parameters": {
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": (
                            "Absolute or tilde-prefixed filesystem path (e.g. "
                            "``~/dev/myproj``)."
                        ),
                    },
                    "since": {
                        "type": "string",
                        "description": (
                            "Optional cutoff (``\"30d\"`` / ``\"24h\"`` / ISO). "
                            "Default: no cutoff."
                        ),
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Max sessions returned. Default 5.",
                        "minimum": 1,
                        "maximum": 20,
                    },
                },
                "required": ["path"],
            },
        },
    },
    {
        "type": "function",
        "function": {
            "name": "find_sessions_touching_file",
            "description": (
                "List sessions where ``file`` shows up as a tool argument (Read / "
                "Edit / Write) or in free-form message text. Use this for 'who "
                "touched X' / 'when did we last edit Y' questions."
            ),
            "parameters": {
                "type": "object",
                "properties": {
                    "file": {
                        "type": "string",
                        "description": "Path to the file (absolute or relative).",
                    },
                    "mode": {
                        "type": "string",
                        "description": (
                            "``\"read\"`` (only Read-tool hits), ``\"write\"`` "
                            "(Edit/Write/MultiEdit/NotebookEdit hits), or "
                            "``\"any\"`` (default)."
                        ),
                        "enum": ["read", "write", "any"],
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Max sessions returned. Default 5.",
                        "minimum": 1,
                        "maximum": 20,
                    },
                },
                "required": ["file"],
            },
        },
    },
    {
        "type": "function",
        "function": {
            "name": "get_project_summary",
            "description": (
                "Return a flat summary for one project: session count, message "
                "count, lifetime cost in USD, first / last activity. Use for "
                "'what's the state of this project?' / 'how big is X?'."
            ),
            "parameters": {
                "type": "object",
                "properties": {
                    "slug": {
                        "type": "string",
                        "description": (
                            "Project slug (e.g. ``\"my-project\"``). When omitted, "
                            "summarises the current project context if one is "
                            "available; otherwise returns an error."
                        ),
                    },
                },
                "required": [],
            },
        },
    },
    {
        "type": "function",
        "function": {
            "name": "get_cost_summary",
            "description": (
                "Cross-project cost rollup over a fixed period. Returns "
                "``total_cost`` USD plus per-project breakdown. Use for "
                "'what did I spend this month?' / 'top spenders'."
            ),
            "parameters": {
                "type": "object",
                "properties": {
                    "period": {
                        "type": "string",
                        "description": (
                            "One of: ``\"today\"``, ``\"7days\"``, ``\"30days\"``, "
                            "``\"month\"`` (default), ``\"all\"``."
                        ),
                        "enum": ["today", "7days", "30days", "month", "all"],
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Top-N projects to include. Default 10.",
                        "minimum": 1,
                        "maximum": 25,
                    },
                },
                "required": [],
            },
        },
    },
    {
        "type": "function",
        "function": {
            "name": "get_session_playback",
            "description": (
                "Reconstruct what the AI agent did to the filesystem in session "
                "``session_id`` up to time ``at``. Returns a list of touched files "
                "with metadata (no file bodies — those would blow the budget). "
                "Use for 'what did the agent change in session X?'."
            ),
            "parameters": {
                "type": "object",
                "properties": {
                    "session_id": {
                        "type": "string",
                        "description": "Session UUID (matches ``sessions.session_id``).",
                    },
                    "at": {
                        "type": "string",
                        "description": (
                            "ISO timestamp cutoff. Default: ``null`` means "
                            "end-of-session (no cutoff applied)."
                        ),
                    },
                },
                "required": ["session_id"],
            },
        },
    },
    {
        "type": "function",
        "function": {
            "name": "recommend_mode",
            "description": (
                "Recommend the cheapest model that fits a task, based on the "
                "user's own past sessions. Pattern-matches the prompt's "
                "intent + token-band + language hints against past similar "
                "sessions and returns the model whose similar history had the "
                "lowest median cost. Returns confidence=0.0 when there isn't "
                "enough historical data (no opinion). Use this for 'this task "
                "fits a Sonnet, you used Opus' routing nudges."
            ),
            "parameters": {
                "type": "object",
                "properties": {
                    "prompt": {
                        "type": "string",
                        "description": (
                            "The task prompt to score. Required, non-empty."
                        ),
                    },
                    "current_model": {
                        "type": "string",
                        "description": (
                            "The model the caller would otherwise route to. "
                            "Drives the cost_delta_usd field."
                        ),
                    },
                },
                "required": ["prompt"],
            },
        },
    },
    {
        "type": "function",
        "function": {
            "name": "list_recent_sessions",
            "description": (
                "Return the most recently active sessions across the store. Use "
                "this for 'what did I work on lately?' style questions."
            ),
            "parameters": {
                "type": "object",
                "properties": {
                    "project": {
                        "type": "string",
                        "description": "Optional project-slug filter.",
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Max sessions returned. Default 10.",
                        "minimum": 1,
                        "maximum": 25,
                    },
                },
                "required": [],
            },
        },
    },
    {
        "type": "function",
        "function": {
            "name": "recommend_skills",
            "description": (
                "List repeated workflow patterns the user could turn into "
                "auto-generated Claude Code skills. Mines the local store for "
                "patterns appearing in ``threshold``+ distinct sessions within "
                "``window_days`` and filters out anything they already have a "
                "skill for. Read-only — each row carries an ``accept_command`` "
                "the user can paste to install. Use for 'what should I "
                "automate?' / 'any skill suggestions for this project?'."
            ),
            "parameters": {
                "type": "object",
                "properties": {
                    "project": {
                        "type": "string",
                        "description": (
                            "Project slug to scope to. When omitted, the "
                            "current project context is used if available; "
                            "otherwise the call returns an error."
                        ),
                    },
                    "threshold": {
                        "type": "integer",
                        "description": (
                            "Minimum distinct sessions a pattern must clear. "
                            "Default 5."
                        ),
                        "minimum": 1,
                        "maximum": 50,
                    },
                    "window_days": {
                        "type": "integer",
                        "description": "Lookback window in days. Default 30.",
                        "minimum": 1,
                        "maximum": 365,
                    },
                },
                "required": [],
            },
        },
    },
]


def tool_names() -> list[str]:
    """Names of every tool the catalogue exposes."""
    return [t["function"]["name"] for t in TOOL_CATALOG]


# ── executors ──────────────────────────────────────────────────────────────


def _truncate(value: dict[str, Any]) -> dict[str, Any]:
    """Slice payloads when ``json.dumps(value)`` exceeds the budget.

    Strategy: serialise once to measure; if over the budget, trim the
    longest list-typed value at the top level repeatedly until we fit.
    Strings get a hard right-side cut. The result keeps the same keys
    so the LLM can still see the shape.
    """
    encoded = json.dumps(value, default=str)
    if len(encoded) <= _RESULT_CHAR_BUDGET:
        return value

    out = dict(value)
    # First trim list payloads, longest-first, halving each pass.
    while True:
        encoded = json.dumps(out, default=str)
        if len(encoded) <= _RESULT_CHAR_BUDGET:
            return out
        lists = [(k, v) for k, v in out.items() if isinstance(v, list) and len(v) > 1]
        if not lists:
            break
        lists.sort(key=lambda kv: len(json.dumps(kv[1], default=str)), reverse=True)
        biggest_key, biggest_val = lists[0]
        out[biggest_key] = biggest_val[: max(1, len(biggest_val) // 2)]
        out["_truncated"] = True

    # Final fallback: stringify and slice. Keeps the contract simple
    # even if the model emits a tool call whose result is a giant scalar.
    text = json.dumps(out, default=str)
    if len(text) > _RESULT_CHAR_BUDGET:
        out = {
            "_truncated": True,
            "_text": text[: _RESULT_CHAR_BUDGET - 64] + "...[truncated]",
        }
    return out


def _exec_search_past_decisions(conn: sqlite3.Connection, args: dict[str, Any]) -> dict[str, Any]:
    query = str(args.get("query") or "")
    if not query.strip():
        return {"error": "query is required"}
    limit = int(args.get("limit") or 5)
    project = args.get("project")
    since = args.get("since")
    matches = discovery.search_past_decisions(
        conn,
        query,
        project=str(project) if project else None,
        since=str(since) if since else None,
        limit=max(1, min(20, limit)),
    )
    return {
        "query": query,
        "count": len(matches),
        "sessions": [m.to_dict() for m in matches],
    }


def _exec_find_sessions_in_path(conn: sqlite3.Connection, args: dict[str, Any]) -> dict[str, Any]:
    path = str(args.get("path") or "")
    if not path.strip():
        return {"error": "path is required"}
    limit = int(args.get("limit") or 5)
    since = args.get("since")
    matches = discovery.find_sessions_in_path(
        conn,
        path,
        since=str(since) if since else None,
        limit=max(1, min(20, limit)),
    )
    return {
        "path": path,
        "count": len(matches),
        "sessions": [m.to_dict() for m in matches],
    }


def _exec_find_sessions_touching_file(
    conn: sqlite3.Connection, args: dict[str, Any]
) -> dict[str, Any]:
    file = str(args.get("file") or "")
    if not file.strip():
        return {"error": "file is required"}
    mode = str(args.get("mode") or "any").lower()
    if mode not in {"any", "read", "write"}:
        return {"error": f"mode must be one of read|write|any (got {mode!r})"}
    limit = int(args.get("limit") or 5)
    matches = discovery.find_sessions_touching_file(
        conn,
        file,
        mode=mode,
        limit=max(1, min(20, limit)),
    )
    return {
        "file": file,
        "mode": mode,
        "count": len(matches),
        "sessions": [m.to_dict() for m in matches],
    }


def _exec_get_project_summary(
    conn: sqlite3.Connection, args: dict[str, Any], *, current_slug: str | None = None
) -> dict[str, Any]:
    slug = args.get("slug") or current_slug
    if not slug:
        return {
            "error": (
                "slug is required (no current project context to fall back on)"
            )
        }
    slug = str(slug)
    row = conn.execute(
        "SELECT id, provider, slug, display_name, path, first_seen, last_modified "
        "FROM projects WHERE slug = ? LIMIT 1",
        (slug,),
    ).fetchone()
    if row is None:
        return {"error": f"project not found: {slug}"}
    project_id = int(row["id"] if isinstance(row, sqlite3.Row) else row[0])

    # Sessions + messages + cost — single rollup using the same shape the
    # /api/projects route already builds. We avoid the full pipeline so
    # the tool stays cheap on large projects.
    rollup = conn.execute(
        "SELECT COUNT(DISTINCT s.id) AS sessions, "
        "       COUNT(m.id)          AS messages, "
        "       COALESCE(SUM(m.input_tokens), 0)  AS input_tokens, "
        "       COALESCE(SUM(m.output_tokens), 0) AS output_tokens, "
        "       MIN(m.timestamp)     AS first_ts, "
        "       MAX(m.timestamp)     AS last_ts, "
        "       MAX(CASE WHEN m.model IS NOT NULL AND m.model != '' "
        "                THEN m.model END) AS model "
        "FROM sessions s LEFT JOIN messages m ON m.session_fk = s.id "
        "WHERE s.project_id = ?",
        (project_id,),
    ).fetchone()

    # Cost — guard the import to keep the tool cheap when pricing isn't
    # available (fresh installs without the pricing service).
    cost_usd = 0.0
    if rollup and rollup["model"]:
        try:
            from stackunderflow.infra.costs import compute_cost

            cost_usd = float(
                compute_cost(
                    {
                        "input": int(rollup["input_tokens"] or 0),
                        "output": int(rollup["output_tokens"] or 0),
                    },
                    rollup["model"],
                ).get("total_cost", 0.0)
            )
        except Exception:
            cost_usd = 0.0

    return {
        "slug": slug,
        "provider": row["provider"] if isinstance(row, sqlite3.Row) else row[1],
        "display_name": row["display_name"] if isinstance(row, sqlite3.Row) else row[3],
        "path": row["path"] if isinstance(row, sqlite3.Row) else row[4],
        "sessions": int(rollup["sessions"] or 0) if rollup else 0,
        "messages": int(rollup["messages"] or 0) if rollup else 0,
        "cost_usd": round(cost_usd, 4),
        "first_message_ts": rollup["first_ts"] if rollup else None,
        "last_message_ts": rollup["last_ts"] if rollup else None,
    }


def _exec_get_cost_summary(conn: sqlite3.Connection, args: dict[str, Any]) -> dict[str, Any]:
    period = str(args.get("period") or "month")
    try:
        scope = parse_period(period)
    except ValueError as e:
        return {"error": str(e)}
    limit = int(args.get("limit") or 10)
    report = build_report(conn, scope=scope, include=None, exclude=None)
    top = (report.get("by_project") or [])[: max(1, min(25, limit))]
    return {
        "period": period,
        "label": scope.label,
        "since": scope.since,
        "until": scope.until,
        "total_cost_usd": round(float(report.get("total_cost") or 0.0), 4),
        "total_messages": int(report.get("total_messages") or 0),
        "total_sessions": int(report.get("total_sessions") or 0),
        "top_projects": [
            {
                "slug": p.get("name"),
                "cost_usd": round(float(p.get("cost") or 0.0), 4),
                "messages": int(p.get("messages") or 0),
                "sessions": int(p.get("sessions") or 0),
            }
            for p in top
        ],
    }


def _exec_get_session_playback(
    conn: sqlite3.Connection, args: dict[str, Any]
) -> dict[str, Any]:
    session_id = str(args.get("session_id") or "")
    if not session_id.strip():
        return {"error": "session_id is required"}
    at = args.get("at")
    # If no ``at``, use the session's last_ts so the snapshot is the
    # end-of-session state. Avoids forcing the model to invent a timestamp.
    if not at:
        row = conn.execute(
            "SELECT last_ts FROM sessions WHERE session_id = ? LIMIT 1",
            (session_id,),
        ).fetchone()
        if row is None:
            return {"error": f"session not found: {session_id}"}
        at = row["last_ts"] if isinstance(row, sqlite3.Row) else row[0]
        if not at:
            # No messages on this session — return empty snapshot.
            return {
                "session_id": session_id,
                "at": None,
                "files": [],
                "warnings": ["session has no messages"],
            }

    try:
        snapshot = playback_fs.reconstruct_fs_at(
            conn,
            session_id,
            at=str(at),
            include_content=False,  # never inline file bodies into the LLM context
        )
    except playback_fs.UnknownSession:
        return {"error": f"session not found: {session_id}"}
    except playback_fs.FsReconstructionError as e:
        return {"error": str(e)}
    return {
        "session_id": snapshot.get("session_id"),
        "snapshot_ts": snapshot.get("snapshot_ts"),
        "file_count": len(snapshot.get("files") or []),
        "files": [
            {
                "path": f.get("path"),
                "byte_count": f.get("byte_count"),
                "last_modified_ts": f.get("last_modified_ts"),
                "operations_applied": f.get("operations_applied"),
                "reconstruction_complete": f.get("reconstruction_complete"),
            }
            for f in (snapshot.get("files") or [])
        ],
        "warnings": snapshot.get("warnings") or [],
    }


def _exec_recommend_mode(
    conn: sqlite3.Connection, args: dict[str, Any]
) -> dict[str, Any]:
    from stackunderflow.services import mode_recommender

    prompt = str(args.get("prompt") or "")
    if not prompt.strip():
        return {"error": "prompt is required"}
    current_model = args.get("current_model")
    return mode_recommender.recommend(
        conn, prompt,
        current_model=str(current_model) if current_model else None,
    )


def _exec_list_recent_sessions(
    conn: sqlite3.Connection, args: dict[str, Any]
) -> dict[str, Any]:
    project = args.get("project")
    limit = int(args.get("limit") or 10)
    limit = max(1, min(25, limit))

    where_parts: list[str] = []
    params: list[Any] = []
    if project:
        where_parts.append("p.slug = ?")
        params.append(str(project))
    where_sql = (" WHERE " + " AND ".join(where_parts)) if where_parts else ""

    rows = conn.execute(
        "SELECT s.session_id AS session_id, "
        "       p.slug AS slug, "
        "       p.provider AS provider, "
        "       s.first_ts AS first_ts, "
        "       s.last_ts AS last_ts, "
        "       s.message_count AS message_count "
        "FROM sessions s "
        "JOIN projects p ON p.id = s.project_id"
        + where_sql
        + " ORDER BY COALESCE(s.last_ts, '') DESC LIMIT ?",
        [*params, limit],
    ).fetchall()
    return {
        "count": len(rows),
        "sessions": [
            {
                "session_id": (r["session_id"] if isinstance(r, sqlite3.Row) else r[0]),
                "slug": (r["slug"] if isinstance(r, sqlite3.Row) else r[1]),
                "provider": (r["provider"] if isinstance(r, sqlite3.Row) else r[2]),
                "first_ts": (r["first_ts"] if isinstance(r, sqlite3.Row) else r[3]),
                "last_ts": (r["last_ts"] if isinstance(r, sqlite3.Row) else r[4]),
                "message_count": int(
                    (r["message_count"] if isinstance(r, sqlite3.Row) else r[5]) or 0
                ),
            }
            for r in rows
        ],
    }


def _exec_recommend_skills(
    conn: sqlite3.Connection, args: dict[str, Any], *, current_slug: str | None = None
) -> dict[str, Any]:
    """Surface skill recommendations for the active project.

    The slug resolution mirrors ``_exec_get_project_summary`` — explicit
    ``project`` arg wins, otherwise we fall back to the route-supplied
    ``current_slug``. Without either we return an error so the model
    knows to ask the user which project to scan.
    """
    from stackunderflow.services import skill_recommender

    project = args.get("project") or current_slug
    if not project:
        return {
            "error": (
                "project is required (no current project context). Pass "
                "the project slug explicitly."
            )
        }
    threshold = int(args.get("threshold") or 5)
    window_days = int(args.get("window_days") or 30)
    try:
        result = skill_recommender.recommend_skills(
            conn,
            project=str(project),
            threshold=max(1, min(50, threshold)),
            window_days=max(1, min(365, window_days)),
        )
    except ValueError as exc:
        return {"error": str(exc)}
    payload = result.to_dict()
    # The full skill body can be large; the LLM only needs the headline
    # fields to surface a recommendation. Drop the template body before
    # truncation; the user can fetch the full text via the CLI.
    payload["recommendations"] = [
        {k: v for k, v in r.items() if k != "suggested_skill_template"}
        for r in payload["recommendations"]
    ]
    return payload


# Dispatcher table — name → (conn, args) callable. Keeping this flat
# (instead of dynamic getattr) makes the surface explicit: a new tool
# has to be added in three places: catalogue, dispatcher, and tests.
_EXECUTORS: dict[str, Callable[..., dict[str, Any]]] = {
    "search_past_decisions": _exec_search_past_decisions,
    "find_sessions_in_path": _exec_find_sessions_in_path,
    "find_sessions_touching_file": _exec_find_sessions_touching_file,
    "get_project_summary": _exec_get_project_summary,
    "get_cost_summary": _exec_get_cost_summary,
    "get_session_playback": _exec_get_session_playback,
    "list_recent_sessions": _exec_list_recent_sessions,
    "recommend_skills": _exec_recommend_skills,
    "recommend_mode": _exec_recommend_mode,
}


def execute_tool(
    conn: sqlite3.Connection,
    name: str,
    args: dict[str, Any],
    *,
    current_slug: str | None = None,
) -> ToolResult:
    """Dispatch one tool call. Always returns a JSON-safe ``ToolResult``.

    Unknown names return ``ok=False`` with an explanatory ``error`` —
    the LLM sees the failure and can either pick a real tool name or
    apologise. We never raise out of this function; the streaming wire
    format depends on a tool-call always being followed by a result
    event with the same matching ``id``.
    """
    started = time.monotonic()
    try:
        if name not in _EXECUTORS:
            data: dict[str, Any] = {
                "error": (
                    f"unknown tool: {name!r}. Known tools: {sorted(_EXECUTORS)}"
                )
            }
            return ToolResult(
                name=name,
                ok=False,
                data=data,
                duration_ms=int((time.monotonic() - started) * 1000),
            )

        # Normalise the args: Ollama can send strings or dicts depending
        # on the model. We coerce to dict-with-string-keys.
        if isinstance(args, str):
            try:
                args = json.loads(args)
            except json.JSONDecodeError:
                args = {}
        if not isinstance(args, dict):
            args = {}

        executor = _EXECUTORS[name]
        if name in ("get_project_summary", "recommend_skills"):
            payload = executor(conn, args, current_slug=current_slug)
        else:
            payload = executor(conn, args)
        if not isinstance(payload, dict):
            payload = {"result": payload}
        payload = _truncate(payload)
        ok = "error" not in payload
        return ToolResult(
            name=name,
            ok=ok,
            data=payload,
            duration_ms=int((time.monotonic() - started) * 1000),
        )
    except Exception as exc:  # noqa: BLE001 — never raise from a tool dispatch
        return ToolResult(
            name=name,
            ok=False,
            data={"error": f"{type(exc).__name__}: {exc}"},
            duration_ms=int((time.monotonic() - started) * 1000),
        )


# ── helpers for the route ──────────────────────────────────────────────────


def now_iso() -> str:
    """UTC ISO-8601 used to stamp streaming wire events."""
    return datetime.now(UTC).isoformat()
