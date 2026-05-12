"""message_tool_mart — one row per (message, tool_name, call_index).

The first **per-message-grain** mart. The seven marts before it
(daily, session, project, provider_day, model_day, tool, command) all
roll usage up to an aggregate key — ``(day, project)``, ``(session)``,
``(day, project, tool_name)``. This one keeps the row-per-tool-call
detail that ``reports/optimize.py``'s detectors need (which files were
read, how big the Bash output was, how many Reads vs Edits a session
ran) without re-parsing ``messages.raw_json`` on every request — a scan
that fans out across every monthly partition since v008 turned
``messages`` into a UNION-ALL view.

Sourcing
========
Watermark on ``usage_events.id`` (``messages`` is a view post-v008 and
can't be watermarked directly). For each event in the window we JOIN
back to its source ``messages`` row, parse ``raw_json`` for ``tool_use``
blocks, and emit one mart row per call:

* ``tool_name`` — the block's ``name`` (``"Read"`` / ``"Bash"`` / ...).
* ``file_path`` — ``input.file_path`` / ``input.path`` /
  ``input.notebook_path``; for ``Task`` the ``input.subagent_type`` (so
  the ghost-agent detector reads invoked agents straight off the mart).
  ``NULL`` when none apply (e.g. ``Bash`` with no path arg).
* ``byte_count`` — for write-family tools, the size of the payload we
  wrote (``Write``→``content``, ``Edit``→``new_string``,
  ``MultiEdit``→Σ ``new_string``, ``NotebookEdit``→``new_source``); for
  output-producing tools (``Bash``, ``Read``, ``Grep``, ...) the size
  of the tool *result*, pulled from the immediately-following message's
  ``tool_result`` block matched on ``tool_use_id``. ``NULL`` when we
  can't size it.
* ``call_index`` — 0-based, **per tool name within the message**. A
  message that calls Read, Edit, Read produces Read#0, Edit#0, Read#1.
  ``UNIQUE(message_id, tool_name, call_index)`` is the dedup key the
  ``INSERT OR IGNORE`` relies on.

Pattern selection
=================
Per-entity, append-via-``INSERT OR IGNORE``. Each event yields a fixed
set of ``(message_id, tool_name, call_index)`` rows, so re-running a
window (partial-failure recovery) is a no-op for already-built rows.
The watermark advances to ``max(usage_events.id)`` every refresh, even
when the window's events carry no parseable tool calls — same contract
as ``tool_mart`` / ``command_mart``.

Staleness caveat: if a message is re-parsed with a *different* tool
shape (a Cursor-v3-style reparenting), the old rows survive (``INSERT
OR IGNORE`` only adds). ``rebuild_from_scratch`` (the backfill
``--force`` path) clears the table first, so a full rebuild self-heals.
Acceptable for v1 — see spec open question 3.

Cost shape of ``refresh()``
===========================
Per event: one ``messages`` row JOIN + one correlated lookup for the
*next* message (its ``raw_json`` holds the ``tool_result`` blocks we
size Bash/Read output from). On the incremental path the window is a
handful of events, so this is a handful of indexed lookups. On
``rebuild_from_scratch`` it's O(events) correlated lookups against the
``messages`` view — each branch of the UNION-ALL uses its
``(session_fk, seq)`` index — which is a one-time backfill cost, same
order as ``command_mart``'s per-event prompt walk.
"""

from __future__ import annotations

import json
import sqlite3
from dataclasses import dataclass
from typing import Any

from .base import MartBuilder

# Tools whose ``file_path`` slot is the subagent being spawned, not a path.
_TASK_TOOLS = frozenset({"Task"})

# Input keys that carry a filesystem path, in priority order.
_FILE_PATH_KEYS = ("file_path", "path", "notebook_path")


@dataclass(frozen=True)
class ToolCall:
    """One parsed ``tool_use`` block, normalised to the mart's columns."""

    tool_name: str
    file_path: str | None
    byte_count: int | None
    call_index: int


class MessageToolMartBuilder(MartBuilder):
    """Per-(message, tool_name, call_index) detail rows for usage events."""

    name = "message_tool"

    def refresh(self, conn: sqlite3.Connection, since_event_id: int) -> int:
        max_id = _max_event_id(conn)
        if max_id <= since_event_id:
            return since_event_id

        rows = _fetch_window(conn, since_event_id=since_event_id, max_id=max_id)

        records: list[tuple[Any, ...]] = []
        for r in rows:
            result_sizes = _result_sizes(r["next_raw_json"])
            for tc in _parse_tool_calls(r["raw_json"], result_sizes=result_sizes):
                records.append(
                    (
                        r["message_id"], int(r["project_id"]), r["session_id"],
                        r["ts"], r["day"],
                        tc.tool_name, tc.file_path, tc.byte_count, tc.call_index,
                    )
                )

        if records:
            conn.executemany(
                """
                INSERT OR IGNORE INTO message_tool_mart (
                    message_id, project_id, session_id, ts, day,
                    tool_name, file_path, byte_count, call_index
                ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
                """,
                records,
            )

        return max_id

    def rebuild_from_scratch(self, conn: sqlite3.Connection) -> None:
        conn.execute("DELETE FROM message_tool_mart")
        self.refresh(conn, since_event_id=0)


# ── helpers ────────────────────────────────────────────────────────────


def _max_event_id(conn: sqlite3.Connection) -> int:
    row = conn.execute("SELECT MAX(id) AS m FROM usage_events").fetchone()
    if row is None:
        return 0
    val = row["m"] if hasattr(row, "keys") else row[0]
    return int(val) if val is not None else 0


def _fetch_window(
    conn: sqlite3.Connection, *, since_event_id: int, max_id: int,
) -> list[dict[str, Any]]:
    """Return joined event + source-message rows in ``(since, max]``.

    ``next_raw_json`` is the ``raw_json`` of the message that
    immediately follows the source message in the same conversation
    (by ``seq``) — almost always the ``tool_result`` turn, from which
    we size Bash/Read output. ``LEFT JOIN`` defends against an event
    whose source message was deleted (the FK on ``usage_events`` was
    dropped in v008); such events parse to no tool calls and contribute
    nothing.
    """
    sql = """
        SELECT e.source_message_fk AS message_id,
               e.project_id        AS project_id,
               e.session_id        AS session_id,
               e.ts                AS ts,
               e.day               AS day,
               m.raw_json          AS raw_json,
               (
                   SELECT m2.raw_json
                     FROM messages m2
                    WHERE m2.session_fk = m.session_fk
                      AND m2.seq > m.seq
                    ORDER BY m2.seq
                    LIMIT 1
               )                   AS next_raw_json
          FROM usage_events e
          LEFT JOIN messages m ON m.id = e.source_message_fk
         WHERE e.id > ? AND e.id <= ?
         ORDER BY e.id
    """
    return [dict(r) for r in conn.execute(sql, (since_event_id, max_id)).fetchall()]


def _parse_tool_calls(
    raw_json: str | None,
    *,
    result_sizes: dict[str, int] | None = None,
) -> list[ToolCall]:
    """Parse an assistant message's ``raw_json`` into ``ToolCall`` rows.

    Pure function — given the JSON text and an optional
    ``{tool_use_id: result_byte_size}`` map (from the following
    message), it returns the mart rows for that message. Defensive
    against every shape that isn't a Claude-style ``message.content[]``
    list of blocks: malformed JSON, missing keys, non-dict blocks, and
    blocks without a string ``name`` all yield no rows (the same
    coverage the legacy ``optimize._tool_calls_with_input`` parse has).
    """
    sizes = result_sizes or {}
    blocks = _tool_use_blocks(raw_json)
    if not blocks:
        return []

    per_tool: dict[str, int] = {}
    out: list[ToolCall] = []
    for blk in blocks:
        name = blk.get("name")
        if not isinstance(name, str) or not name:
            continue
        inp = blk.get("input")
        if not isinstance(inp, dict):
            inp = {}
        tool_use_id = blk.get("id")
        result_size = (
            sizes.get(tool_use_id) if isinstance(tool_use_id, str) else None
        )
        idx = per_tool.get(name, 0)
        per_tool[name] = idx + 1
        out.append(
            ToolCall(
                tool_name=name,
                file_path=_extract_file_path(name, inp),
                byte_count=_extract_byte_count(name, inp, result_size),
                call_index=idx,
            )
        )
    return out


def _tool_use_blocks(raw_json: str | None) -> list[dict[str, Any]]:
    """Return the ``tool_use`` blocks from ``message.content[]``, or ``[]``."""
    if not raw_json:
        return []
    try:
        obj = json.loads(raw_json)
    except (json.JSONDecodeError, TypeError):
        return []
    if not isinstance(obj, dict):
        return []
    msg = obj.get("message")
    if not isinstance(msg, dict):
        return []
    content = msg.get("content")
    if not isinstance(content, list):
        return []
    return [
        b for b in content
        if isinstance(b, dict) and b.get("type") == "tool_use"
    ]


def _extract_file_path(tool_name: str, inp: dict[str, Any]) -> str | None:
    """Pick the filesystem path (or subagent name for ``Task``) from ``input``."""
    if tool_name in _TASK_TOOLS:
        for key in ("subagent_type", "agent"):
            v = inp.get(key)
            if isinstance(v, str) and v:
                return v
        return None
    for key in _FILE_PATH_KEYS:
        v = inp.get(key)
        if isinstance(v, str) and v:
            return v
    return None


def _extract_byte_count(
    tool_name: str, inp: dict[str, Any], result_size: int | None,
) -> int | None:
    """Compute ``byte_count`` for one call.

    Write-family tools: the size of the text payload in ``input``
    (``MultiEdit`` sums the ``new_string`` of every edit it carries).
    Everything else: the size of the matched tool result (``None`` when
    we couldn't pair one — e.g. the following turn wasn't a
    ``tool_result``, or this provider's ``raw_json`` doesn't carry one).
    """
    if tool_name == "Write":
        return _byte_len(inp.get("content"))
    if tool_name == "Edit":
        return _byte_len(inp.get("new_string"))
    if tool_name == "NotebookEdit":
        return _byte_len(inp.get("new_source"))
    if tool_name == "MultiEdit":
        edits = inp.get("edits")
        if not isinstance(edits, list):
            return None
        total = 0
        seen = False
        for e in edits:
            if not isinstance(e, dict):
                continue
            n = _byte_len(e.get("new_string"))
            if n is not None:
                total += n
                seen = True
        return total if seen else None
    return result_size


def _byte_len(value: Any) -> int | None:
    """UTF-8 byte length of a string value, or ``None`` if it isn't one."""
    if not isinstance(value, str):
        return None
    return len(value.encode("utf-8"))


def _result_sizes(next_raw_json: str | None) -> dict[str, int]:
    """``{tool_use_id: result_byte_size}`` from the following message's raw_json.

    Reads ``message.content[]`` for ``tool_result`` blocks and measures
    each block's rendered text content. Empty when ``next_raw_json`` is
    missing/malformed or carries no ``tool_result`` blocks — in which
    case output-producing calls in the preceding message get a ``NULL``
    ``byte_count``.
    """
    if not next_raw_json:
        return {}
    try:
        obj = json.loads(next_raw_json)
    except (json.JSONDecodeError, TypeError):
        return {}
    if not isinstance(obj, dict):
        return {}
    msg = obj.get("message")
    if not isinstance(msg, dict):
        return {}
    content = msg.get("content")
    if not isinstance(content, list):
        return {}
    out: dict[str, int] = {}
    for blk in content:
        if not isinstance(blk, dict) or blk.get("type") != "tool_result":
            continue
        tool_use_id = blk.get("tool_use_id")
        if not isinstance(tool_use_id, str) or not tool_use_id:
            continue
        out[tool_use_id] = len(_render_result_content(blk.get("content")).encode("utf-8"))
    return out


def _render_result_content(content: Any) -> str:
    """Flatten a ``tool_result.content`` (string, or list of text blocks) to text."""
    if isinstance(content, str):
        return content
    if isinstance(content, list):
        parts: list[str] = []
        for p in content:
            if isinstance(p, dict):
                t = p.get("text")
                if isinstance(t, str):
                    parts.append(t)
        return "".join(parts)
    return ""
