"""Playback service — per-session (and per-project) tool-call timeline.

The dashboard's session view is a flat message list. This module turns a
session into a *sequence of state-changing events* — one row per tool call —
so the "Playback" tab can render a scrubbable timeline:

* "what did the agent actually do?" — just the tool calls, in order;
* "when did this break?" — play forward until a tool result reveals it;
* a shareable ``?session=ID&seq=42`` deep link to a single step.

This is **pure read-side work** over data already in the store:

* ``messages.raw_json`` — the Anthropic message envelope, which carries the
  structured ``message.content[]`` blocks (``type == "tool_use"`` for the
  call, ``type == "tool_result"`` for the result) plus the Claude Code
  ``toolUseResult`` summary on the result-bearing user message;
* ``messages.timestamp`` — used to order events and (paired with the
  result message's timestamp) to compute a rough per-tool duration;
* ``captured_events`` — if the spec-05 hooks table is present, its
  PostToolUse rows give an authoritative success/failure flag. Absent
  → ``success`` falls back to the ``is_error`` signal in the transcript,
  and is ``None`` when neither is available. **No schema migration** is
  added by this module; it degrades gracefully when ``captured_events``
  doesn't exist (the common case — hooks aren't installed).

Defensive parsing throughout: a malformed ``raw_json`` row never raises;
it just doesn't contribute events (or contributes one with
``summary="(unparseable)"`` when the envelope is recoverable but the
inner shape is wrong).

Public API
----------

* :class:`PlaybackEvent` — one tool call.
* :func:`session_playback` — ordered event stream for one session.
* :func:`project_timeline` — cross-session event stream for one project.
* :func:`playback_event_to_dict` — JSON serialiser used by the route.

See ``.notes/specs/10-playback-timeline.md`` for the design rationale and
the "v1 vs v2 (virtual-filesystem reconstruction)" scope split — this
module is v1 only.
"""

from __future__ import annotations

import json
import sqlite3
from dataclasses import asdict, dataclass
from datetime import datetime
from typing import Any

__all__ = [
    "PlaybackEvent",
    "session_playback",
    "project_timeline",
    "playback_event_to_dict",
    "summarize_tool_call",
]

# Cap on how much of the input/output text we keep in ``payload_excerpt``.
# Spec says "200-char excerpt"; keep it a named constant so the route and
# tests agree.
_EXCERPT_CHARS = 200

# Tools whose primary argument is a filesystem path. The first matching key
# (in order) is taken as ``target_path``.
_PATH_INPUT_KEYS = (
    "file_path",
    "filePath",
    "notebook_path",
    "notebookPath",
    "path",
)


# ── dataclass ────────────────────────────────────────────────────────────────


@dataclass(frozen=True, slots=True)
class PlaybackEvent:
    """One tool call within a session — the unit the scrubber steps through.

    ``seq`` is the 0-based index of this tool call within the *full* event
    stream for the session (or project, for :func:`project_timeline`). When
    a ``tool_filter`` is applied the returned list is a subset, but each
    event keeps its original ``seq`` — so a filtered view's positions still
    line up with the unfiltered timeline (gaps where other tools were).
    """

    seq: int
    ts: str
    message_id: int
    tool_name: str
    summary: str
    target_path: str | None
    byte_count: int | None
    success: bool | None
    duration_ms: int | None
    payload_excerpt: str
    # Which session this event belongs to. Always set; redundant for
    # :func:`session_playback` (every event shares it) but essential for
    # :func:`project_timeline`, which interleaves multiple sessions.
    session_id: str


# ── defensive JSON helpers ───────────────────────────────────────────────────


def _loads(blob: str | None) -> Any:
    if not blob:
        return None
    try:
        return json.loads(blob)
    except (json.JSONDecodeError, TypeError, ValueError):
        return None


def _envelope(raw_json: str | None) -> dict[str, Any]:
    """Top-level transcript object (``{type, message, timestamp, ...}``)."""
    obj = _loads(raw_json)
    return obj if isinstance(obj, dict) else {}


def _content_blocks(envelope: dict[str, Any]) -> list[Any]:
    msg = envelope.get("message")
    if not isinstance(msg, dict):
        return []
    body = msg.get("content")
    return body if isinstance(body, list) else []


def _stringify_result_content(content: Any) -> str:
    """Normalise a ``tool_result`` block's ``content`` to plain text.

    Anthropic's wire shape is either a bare string or a list of
    ``{"type": "text", "text": "..."}`` (and occasionally image blocks,
    which we skip). Anything else degrades to ``str(content)``.
    """
    if content is None:
        return ""
    if isinstance(content, str):
        return content
    if isinstance(content, list):
        parts: list[str] = []
        for blk in content:
            if isinstance(blk, dict):
                if blk.get("type") == "text" and isinstance(blk.get("text"), str):
                    parts.append(blk["text"])
            elif isinstance(blk, str):
                parts.append(blk)
        return "\n".join(parts)
    if isinstance(content, dict):
        # e.g. ``{"stdout": "...", "stderr": "..."}`` — best-effort.
        if isinstance(content.get("text"), str):
            return content["text"]
        return json.dumps(content, default=str)
    return str(content)


# ── tool-result index ────────────────────────────────────────────────────────


@dataclass(frozen=True, slots=True)
class _ResultInfo:
    text: str
    is_error: bool | None
    ts: str | None
    message_id: int | None


def _index_results(rows: list[sqlite3.Row]) -> dict[str, _ResultInfo]:
    """Map every ``tool_use_id`` → its result (text + error flag + ts).

    Walks the user-role messages, pulling ``type == "tool_result"`` blocks
    out of ``message.content[]``. The Claude Code ``toolUseResult`` field on
    the same message is consulted only for the error flag, since the block's
    own ``content`` is the canonical text.
    """
    out: dict[str, _ResultInfo] = {}
    for r in rows:
        if r["role"] != "user":
            continue
        env = _envelope(r["raw_json"])
        tur = env.get("toolUseResult")
        tur_is_error: bool | None = None
        if isinstance(tur, dict):
            for k in ("is_error", "isError"):
                v = tur.get(k)
                if isinstance(v, bool):
                    tur_is_error = v
                    break
        for blk in _content_blocks(env):
            if not isinstance(blk, dict) or blk.get("type") != "tool_result":
                continue
            tuid = blk.get("tool_use_id")
            if not isinstance(tuid, str) or not tuid:
                continue
            is_error = blk.get("is_error")
            if not isinstance(is_error, bool):
                is_error = tur_is_error
            out[tuid] = _ResultInfo(
                text=_stringify_result_content(blk.get("content")),
                is_error=is_error,
                ts=r["timestamp"] if r["timestamp"] else None,
                message_id=int(r["id"]) if r["id"] is not None else None,
            )
    return out


# ── captured_events (spec 05) join — optional ────────────────────────────────


def _has_captured_events(conn: sqlite3.Connection) -> bool:
    row = conn.execute(
        "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'captured_events'"
    ).fetchone()
    return row is not None


# A PostToolUse hook fires shortly after the tool returns, so its event
# timestamp sits just *after* the assistant message that issued the call.
# Anchor a failure event to a tool-call message when the message's ts falls
# inside this window before the event.
_CAPTURED_ANCHOR_BACK_S = 90.0
_CAPTURED_ANCHOR_FWD_S = 2.0


def _captured_failure_message_ids(
    conn: sqlite3.Connection, *, session_id: str, assistant_rows: list[sqlite3.Row]
) -> dict[int, bool]:
    """Best-effort outcome overlay from the spec-05 ``captured_events`` table.

    Returns ``{messages.id: False}`` for tool-call messages that a
    ``event_kind in ('failure', 'correction')`` hook event lines up with
    (matched on ``session_id`` + timestamp proximity — the hook fires right
    after the tool returns). Only ever yields ``False``: hooks record
    failures/corrections, not positive confirmations, so a *missing* entry
    leaves ``success`` to the transcript signal (or ``None``).

    Until spec 05 lands the table doesn't exist and this returns ``{}`` —
    the documented "works without hooks installed" path. Any unexpected
    table shape → ``{}`` (never crash a playback request).
    """
    if not session_id or not _has_captured_events(conn):
        return {}
    try:
        rows = conn.execute(
            "SELECT ts FROM captured_events "
            "WHERE session_id = ? AND event_kind IN ('failure', 'correction')",
            (session_id,),
        ).fetchall()
    except sqlite3.Error:
        return {}
    failure_ts: list[datetime] = []
    for r in rows:
        dt = _parse_iso(r["ts"])
        if dt is not None:
            failure_ts.append(dt)
    if not failure_ts:
        return {}
    out: dict[int, bool] = {}
    for m in assistant_rows:
        if m["role"] != "assistant" or m["id"] is None:
            continue
        m_dt = _parse_iso(m["timestamp"])
        if m_dt is None:
            continue
        for f_dt in failure_ts:
            delta = (f_dt - m_dt).total_seconds()
            if -_CAPTURED_ANCHOR_FWD_S <= delta <= _CAPTURED_ANCHOR_BACK_S:
                out[int(m["id"])] = False
                break
    return out


# ── summary / excerpt formatting ─────────────────────────────────────────────


def _short_path(path: str) -> str:
    """Trim an absolute path to its last two components for display.

    ``/Users/x/repo/routes/cost.py`` → ``routes/cost.py``; a path that's
    already short (``routes/cost.py``) is returned unchanged.
    """
    norm = path.replace("\\", "/").rstrip("/")
    parts = [p for p in norm.split("/") if p]
    if len(parts) <= 2:
        return "/".join(parts) if parts else path
    return "/".join(parts[-2:])


def _first_command_word(cmd: str) -> str:
    """First shell token of a Bash command, skipping common prefixes.

    ``cd /tmp && pytest -q`` → ``pytest``; ``pytest tests/`` → ``pytest``.
    A pure best-effort tokeniser — never raises.
    """
    text = cmd.strip()
    if not text:
        return ""
    # Skip leading ``cd ... &&`` / ``cd ... ;`` segments — they're plumbing,
    # not the operation the user cares about.
    for sep in ("&&", ";"):
        while True:
            head, _, rest = text.partition(sep)
            if rest and head.strip().split()[:1] == ["cd"]:
                text = rest.strip()
                continue
            break
    # Also skip a leading ``VAR=value`` env assignment or ``sudo``/``time``.
    tokens = text.split()
    while tokens and (
        tokens[0] in ("sudo", "time", "env", "nice", "nohup")
        or ("=" in tokens[0] and not tokens[0].startswith("-") and "/" not in tokens[0].split("=")[0])
    ):
        tokens = tokens[1:]
    return tokens[0] if tokens else text.split()[0]


def _input_path(tool_input: dict[str, Any]) -> str | None:
    for key in _PATH_INPUT_KEYS:
        v = tool_input.get(key)
        if isinstance(v, str) and v.strip():
            return v
    return None


def _mcp_label(tool_name: str) -> str:
    """``mcp__github__create_pr`` → ``github.create_pr`` (best effort)."""
    rest = tool_name[len("mcp__") :]
    bits = rest.split("__", 1)
    if len(bits) == 2:
        return f"{bits[0]}.{bits[1]}"
    return rest or tool_name


def summarize_tool_call(
    tool_name: str,
    tool_input: dict[str, Any] | None,
    tool_result_text: str | None = None,
) -> str:
    """One-line, human-readable label for a tool call.

    Table-driven over the tool names Claude Code emits; an unknown tool
    name falls back to ``"<Tool> <first path-ish arg>"`` so newly-added
    tools still read sensibly. ``mcp__server__tool`` collapses to
    ``server.tool``. Never raises — bad input yields ``"(unparseable)"``.
    """
    if not isinstance(tool_name, str) or not tool_name:
        return "(unparseable)"
    inp = tool_input if isinstance(tool_input, dict) else {}

    name = tool_name
    if name.startswith("mcp__"):
        return _mcp_label(name)

    handler = _SUMMARY_HANDLERS.get(name)
    if handler is not None:
        try:
            return handler(inp, tool_result_text or "")
        except Exception:  # pragma: no cover - defensive; never crash a row
            return name

    # Generic fallback: surface a path-ish argument if there is one.
    p = _input_path(inp)
    if p:
        return f"{name} {_short_path(p)}"
    for key in ("pattern", "query", "url", "command", "description"):
        v = inp.get(key)
        if isinstance(v, str) and v.strip():
            snippet = v.strip().splitlines()[0]
            return f"{name}: {snippet[:60]}"
    return name


def _sum_file_op(verb: str):
    def _h(inp: dict[str, Any], _res: str) -> str:
        p = _input_path(inp)
        return f"{verb} {_short_path(p)}" if p else verb
    return _h


def _sum_bash(inp: dict[str, Any], _res: str) -> str:
    cmd = inp.get("command")
    if not isinstance(cmd, str) or not cmd.strip():
        return "Bash"
    return f"Bash: {_first_command_word(cmd)}"


def _sum_glob(inp: dict[str, Any], _res: str) -> str:
    pat = inp.get("pattern")
    base = f"Glob {pat}" if isinstance(pat, str) and pat else "Glob"
    p = inp.get("path")
    if isinstance(p, str) and p.strip():
        return f"{base} in {_short_path(p)}"
    return base


def _sum_grep(inp: dict[str, Any], _res: str) -> str:
    pat = inp.get("pattern")
    return f"Grep {pat}" if isinstance(pat, str) and pat else "Grep"


def _sum_ls(inp: dict[str, Any], _res: str) -> str:
    p = inp.get("path")
    return f"LS {_short_path(p)}" if isinstance(p, str) and p else "LS"


def _sum_task(inp: dict[str, Any], _res: str) -> str:
    desc = inp.get("description")
    if isinstance(desc, str) and desc.strip():
        return f"Task: {desc.strip()[:60]}"
    sub = inp.get("subagent_type")
    if isinstance(sub, str) and sub.strip():
        return f"Task: {sub.strip()}"
    return "Task"


def _sum_web_fetch(inp: dict[str, Any], _res: str) -> str:
    url = inp.get("url")
    return f"WebFetch {url}" if isinstance(url, str) and url else "WebFetch"


def _sum_web_search(inp: dict[str, Any], _res: str) -> str:
    q = inp.get("query")
    return f"WebSearch: {q[:60]}" if isinstance(q, str) and q else "WebSearch"


def _sum_todo(inp: dict[str, Any], _res: str) -> str:
    todos = inp.get("todos")
    n = len(todos) if isinstance(todos, list) else 0
    return f"TodoWrite ({n} todo{'s' if n != 1 else ''})"


def _sum_skill(inp: dict[str, Any], _res: str) -> str:
    s = inp.get("skill") or inp.get("command")
    return f"Skill: {s}" if isinstance(s, str) and s else "Skill"


def _sum_notebook_edit(inp: dict[str, Any], _res: str) -> str:
    p = inp.get("notebook_path") or inp.get("notebookPath") or inp.get("file_path")
    return f"NotebookEdit {_short_path(p)}" if isinstance(p, str) and p else "NotebookEdit"


_SUMMARY_HANDLERS: dict[str, Any] = {
    "Read": _sum_file_op("Read"),
    "Write": _sum_file_op("Write"),
    "Edit": _sum_file_op("Edit"),
    "MultiEdit": _sum_file_op("MultiEdit"),
    "NotebookRead": _sum_file_op("NotebookRead"),
    "NotebookEdit": _sum_notebook_edit,
    "Bash": _sum_bash,
    "BashOutput": lambda inp, _r: "BashOutput",
    "KillBash": lambda inp, _r: "KillBash",
    "KillShell": lambda inp, _r: "KillShell",
    "Glob": _sum_glob,
    "Grep": _sum_grep,
    "LS": _sum_ls,
    "Task": _sum_task,
    "Agent": _sum_task,
    "WebFetch": _sum_web_fetch,
    "WebSearch": _sum_web_search,
    "TodoWrite": _sum_todo,
    "Skill": _sum_skill,
    "ToolSearch": lambda inp, _r: f"ToolSearch: {inp.get('query', '')[:60]}".rstrip(": "),
    "ExitPlanMode": lambda inp, _r: "ExitPlanMode",
    "EnterPlanMode": lambda inp, _r: "EnterPlanMode",
    "AskUserQuestion": lambda inp, _r: "AskUserQuestion",
    "TaskCreate": lambda inp, _r: f"TaskCreate: {inp.get('description', '')[:60]}".rstrip(": "),
    "TaskUpdate": lambda inp, _r: "TaskUpdate",
    "TaskGet": lambda inp, _r: "TaskGet",
    "TaskList": lambda inp, _r: "TaskList",
    "SendMessage": lambda inp, _r: f"SendMessage → {inp.get('to', '')}".rstrip("→ "),
}

# ``byte_count`` semantics: prefer the size of the *result* text (Read/Bash
# output, Grep matches, ...). For write-style tools that may not have a
# result yet, fall back to the size of what was written.
_WRITE_CONTENT_KEYS = ("content", "new_string", "new_str")
_WRITE_TOOLS = frozenset({"Write", "Edit", "MultiEdit", "NotebookEdit"})


def _byte_count(tool_name: str, tool_input: dict[str, Any], result_text: str | None) -> int | None:
    if result_text:
        return len(result_text.encode("utf-8", errors="replace"))
    if tool_name in _WRITE_TOOLS:
        for key in _WRITE_CONTENT_KEYS:
            v = tool_input.get(key)
            if isinstance(v, str):
                return len(v.encode("utf-8", errors="replace"))
        # MultiEdit: sum the new_string of each edit.
        edits = tool_input.get("edits")
        if isinstance(edits, list):
            total = 0
            seen = False
            for e in edits:
                if isinstance(e, dict) and isinstance(e.get("new_string"), str):
                    total += len(e["new_string"].encode("utf-8", errors="replace"))
                    seen = True
            if seen:
                return total
    return None


def _compact_input(tool_name: str, tool_input: dict[str, Any]) -> str:
    """A readable one-liner of the call's salient inputs (for the excerpt)."""
    if tool_name == "Bash":
        cmd = tool_input.get("command")
        return cmd.strip() if isinstance(cmd, str) else ""
    if tool_name in ("Edit", "MultiEdit"):
        old = tool_input.get("old_string")
        new = tool_input.get("new_string")
        if isinstance(old, str) or isinstance(new, str):
            return f"- {(old or '')[:80]!r}\n+ {(new or '')[:80]!r}"
    if tool_name == "Write":
        c = tool_input.get("content")
        if isinstance(c, str):
            return c
    p = _input_path(tool_input)
    if p:
        return p
    # Last resort: compact JSON of the input (capped).
    try:
        return json.dumps(tool_input, default=str)[: _EXCERPT_CHARS * 2]
    except (TypeError, ValueError):
        return ""


def _payload_excerpt(tool_name: str, tool_input: dict[str, Any], result_text: str | None) -> str:
    left = _compact_input(tool_name, tool_input).strip()
    right = (result_text or "").strip()
    if left and right:
        blended = f"{left}\n⇒ {right}"
    else:
        blended = left or right
    blended = blended.replace("\r\n", "\n")
    if len(blended) <= _EXCERPT_CHARS:
        return blended
    return blended[: _EXCERPT_CHARS - 1] + "…"


# ── timestamps ───────────────────────────────────────────────────────────────


def _parse_iso(ts: str | None) -> datetime | None:
    if not ts or not isinstance(ts, str):
        return None
    text = ts.strip()
    if not text:
        return None
    # ``...Z`` → ``...+00:00`` for ``fromisoformat`` (pre-3.11 strictness).
    if text.endswith("Z"):
        text = text[:-1] + "+00:00"
    try:
        return datetime.fromisoformat(text)
    except ValueError:
        return None


def _duration_ms(call_ts: str | None, result_ts: str | None) -> int | None:
    a = _parse_iso(call_ts)
    b = _parse_iso(result_ts)
    if a is None or b is None:
        return None
    try:
        delta_ms = int((b - a).total_seconds() * 1000)
    except (OverflowError, ValueError):
        return None
    return delta_ms if delta_ms >= 0 else None


# ── core builder ─────────────────────────────────────────────────────────────


def _build_events(
    rows: list[sqlite3.Row],
    *,
    session_id_for: dict[int, str] | str,
    tool_filter: set[str] | None,
    limit: int,
    include_payload: bool,
    captured_success: dict[int, bool],
) -> tuple[list[PlaybackEvent], bool]:
    """Turn a seq/ts-ordered message list into a (possibly filtered) event
    stream. Returns ``(events, truncated)`` where ``truncated`` is ``True``
    when ``limit`` capped the output.

    ``session_id_for`` is either a single session-id string (every row
    belongs to it) or a ``{session_fk: session_id}`` map for the
    cross-session path.
    """
    results = _index_results(rows)
    events: list[PlaybackEvent] = []
    global_idx = 0  # 0-based index over *all* tool calls (pre-filter)
    truncated = False

    def _sid(row: sqlite3.Row) -> str:
        if isinstance(session_id_for, str):
            return session_id_for
        return session_id_for.get(int(row["session_fk"]), "")

    for r in rows:
        if r["role"] != "assistant":
            continue
        env = _envelope(r["raw_json"])
        blocks = _content_blocks(env)
        for blk in blocks:
            if not isinstance(blk, dict) or blk.get("type") != "tool_use":
                continue
            tname = blk.get("name")
            tinput = blk.get("input")
            tuid = blk.get("id")
            this_idx = global_idx
            global_idx += 1

            if not isinstance(tname, str) or not tname:
                # Recoverable envelope, bad inner shape — emit a marker so
                # the timeline length still matches the message stream.
                if tool_filter is not None:
                    continue
                if len(events) >= limit:
                    truncated = True
                    break
                events.append(
                    PlaybackEvent(
                        seq=this_idx,
                        ts=str(r["timestamp"] or ""),
                        message_id=int(r["id"]) if r["id"] is not None else 0,
                        tool_name="?",
                        summary="(unparseable)",
                        target_path=None,
                        byte_count=None,
                        success=None,
                        duration_ms=None,
                        payload_excerpt="",
                        session_id=_sid(r),
                    )
                )
                continue

            if tool_filter is not None and tname not in tool_filter:
                continue
            if len(events) >= limit:
                truncated = True
                break

            tinput = tinput if isinstance(tinput, dict) else {}
            res = results.get(tuid) if isinstance(tuid, str) else None
            result_text = res.text if res is not None else None

            # success: captured_events (authoritative) > transcript is_error.
            mid = int(r["id"]) if r["id"] is not None else 0
            success: bool | None
            if mid in captured_success:
                success = captured_success[mid]
            elif res is not None and res.is_error is not None:
                success = not res.is_error
            else:
                success = None

            events.append(
                PlaybackEvent(
                    seq=this_idx,
                    ts=str(r["timestamp"] or ""),
                    message_id=mid,
                    tool_name=tname,
                    summary=summarize_tool_call(tname, tinput, result_text),
                    target_path=_input_path(tinput),
                    byte_count=_byte_count(tname, tinput, result_text),
                    success=success,
                    duration_ms=_duration_ms(r["timestamp"], res.ts if res else None),
                    payload_excerpt=(
                        _payload_excerpt(tname, tinput, result_text) if include_payload else ""
                    ),
                    session_id=_sid(r),
                )
            )
        else:
            continue
        # Inner loop hit the limit → stop the outer loop too.
        break

    return events, truncated


# ── session-id resolution ────────────────────────────────────────────────────


def _resolve_session(conn: sqlite3.Connection, session_id: str) -> tuple[int, str] | None:
    """Resolve a ``session_id`` string to ``(session_fk, session_id)``.

    ``session_id`` is unique per project, not globally, so a value could
    in principle exist in two projects. We take the most-recently-active
    match — the dashboard scrubber wants "the" session, and the recent one
    is the overwhelmingly likely intent.
    """
    row = conn.execute(
        "SELECT id, session_id FROM sessions WHERE session_id = ? "
        "ORDER BY last_ts DESC NULLS LAST, id DESC LIMIT 1",
        (session_id,),
    ).fetchone()
    if row is None:
        return None
    return int(row["id"]), str(row["session_id"])


# ── public API ───────────────────────────────────────────────────────────────


def session_playback(
    conn: sqlite3.Connection,
    session_id: str,
    *,
    tool_filter: list[str] | None = None,
    limit: int = 1000,
    include_payload: bool = True,
) -> list[PlaybackEvent]:
    """Ordered tool-call event stream for one session.

    Events are ordered by message ``seq`` (which is file/wire order, i.e.
    chronological). ``tool_filter`` restricts to a subset of tool names
    (exact match) while preserving each event's ``seq`` from the full
    stream. ``limit`` caps the returned list; use :func:`session_playback_page`
    semantics via the route when you need the "truncated" signal.

    An unknown / missing ``session_id`` yields ``[]`` (the route is
    responsible for the 404 — it checks existence separately so this
    function can keep the spec's plain-``list`` return type).
    """
    resolved = _resolve_session(conn, session_id)
    if resolved is None:
        return []
    session_fk, sid = resolved
    events, _ = _session_events(
        conn,
        session_fk=session_fk,
        session_id=sid,
        tool_filter=_norm_filter(tool_filter),
        limit=max(0, int(limit)),
        include_payload=include_payload,
    )
    return events


def session_playback_page(
    conn: sqlite3.Connection,
    session_id: str,
    *,
    tool_filter: list[str] | None = None,
    limit: int = 1000,
    include_payload: bool = True,
) -> tuple[list[PlaybackEvent], bool] | None:
    """Like :func:`session_playback` but returns ``(events, truncated)``.

    ``None`` when the session can't be found — lets the route distinguish
    "wrong session id" (404) from "session with no tool calls" (200,
    empty list).
    """
    resolved = _resolve_session(conn, session_id)
    if resolved is None:
        return None
    session_fk, sid = resolved
    return _session_events(
        conn,
        session_fk=session_fk,
        session_id=sid,
        tool_filter=_norm_filter(tool_filter),
        limit=max(0, int(limit)),
        include_payload=include_payload,
    )


def _session_events(
    conn: sqlite3.Connection,
    *,
    session_fk: int,
    session_id: str,
    tool_filter: set[str] | None,
    limit: int,
    include_payload: bool,
) -> tuple[list[PlaybackEvent], bool]:
    rows = conn.execute(
        "SELECT id, session_fk, seq, timestamp, role, raw_json "
        "FROM messages WHERE session_fk = ? ORDER BY seq",
        (session_fk,),
    ).fetchall()
    captured = _captured_failure_message_ids(
        conn, session_id=session_id, assistant_rows=rows
    )
    return _build_events(
        rows,
        session_id_for=session_id,
        tool_filter=tool_filter,
        limit=limit,
        include_payload=include_payload,
        captured_success=captured,
    )


def project_timeline(
    conn: sqlite3.Connection,
    project_id: int,
    *,
    since: str | None = None,
    tool_filter: list[str] | None = None,
    limit: int = 5000,
    include_payload: bool = False,
) -> list[PlaybackEvent]:
    """Cross-session tool-call timeline for a whole project.

    Events from every session in the project, interleaved in chronological
    order. ``since`` is an ISO-8601 lower bound on the message timestamp
    (the route translates ``7d`` → an ISO instant before calling).
    ``include_payload`` defaults to ``False`` here: a project-wide stream
    can be large, so the heavy ``payload_excerpt`` is opt-in.

    Returns ``[]`` for an unknown project id — the route checks project
    existence separately for the 404.
    """
    events, _ = project_timeline_page(
        conn,
        project_id,
        since=since,
        tool_filter=tool_filter,
        limit=limit,
        include_payload=include_payload,
    )
    return events


def project_timeline_page(
    conn: sqlite3.Connection,
    project_id: int,
    *,
    since: str | None = None,
    tool_filter: list[str] | None = None,
    limit: int = 5000,
    include_payload: bool = False,
) -> tuple[list[PlaybackEvent], bool]:
    """Like :func:`project_timeline` but returns ``(events, truncated)``."""
    sid_by_fk: dict[int, str] = {
        int(r["id"]): str(r["session_id"])
        for r in conn.execute(
            "SELECT id, session_id FROM sessions WHERE project_id = ?", (project_id,)
        ).fetchall()
    }
    if not sid_by_fk:
        return [], False

    params: list[Any] = [project_id]
    sql = (
        "SELECT m.id, m.session_fk, m.seq, m.timestamp, m.role, m.raw_json "
        "FROM messages m JOIN sessions s ON s.id = m.session_fk "
        "WHERE s.project_id = ?"
    )
    if since:
        sql += " AND m.timestamp >= ?"
        params.append(since)
    sql += " ORDER BY m.timestamp, m.session_fk, m.seq"
    rows = conn.execute(sql, params).fetchall()
    return _build_events(
        rows,
        session_id_for=sid_by_fk,
        tool_filter=_norm_filter(tool_filter),
        limit=max(0, int(limit)),
        include_payload=include_payload,
        captured_success={},  # project-wide captured-events join: not worth it for v1
    )


def _norm_filter(tool_filter: list[str] | None) -> set[str] | None:
    if not tool_filter:
        return None
    cleaned = {t.strip() for t in tool_filter if isinstance(t, str) and t.strip()}
    return cleaned or None


# ── serialisation ────────────────────────────────────────────────────────────


def playback_event_to_dict(e: PlaybackEvent) -> dict[str, Any]:
    return asdict(e)
