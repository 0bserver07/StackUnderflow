"""Context-window replay — reconstruct what the model "saw" at a point in a
session (StackUnderflow issue #96, Spec 24).

Playback v2 (:mod:`stackunderflow.services.playback_fs`) reconstructs the
*filesystem* at time T. This module is the analog one layer up: the *context*
the model was working from. Given a session and a ``seq`` cutoff it returns the
ordered message sequence that had accumulated up to that point — role, a short
content preview, an estimated token footprint per message, the tool calls each
turn issued, and a **running token total** so a reader can watch the context
grow turn by turn.

MVP context semantics — READ THIS
=================================

"What the model saw at seq K" is defined here as **the session's own message
sequence, in ``seq`` order, for every message with ``seq <= K``**. That is a
deliberate, documented simplification:

* It is faithful for the common single-thread session — the transcript IS the
  context, in order, and this returns exactly that prefix.
* It does **not** model harness-specific context-window eviction: once a real
  session exceeds the model's context limit the harness compacts / drops older
  turns, so the *live* window at seq K may be a strict subset of "everything up
  to K". Reconstructing that requires the harness's compaction signals and the
  per-model context ceiling; both are out of scope for the MVP and are noted as
  a future refinement (see issue #96's ``warnings`` / ``context_max`` ideas).
* The per-message token figure is a ``chars/4`` estimate of that message's own
  text + tool-call payload — NOT the transcript's stored per-call
  ``input_tokens``. Stored ``input_tokens`` for an assistant turn already counts
  the *entire* prior context sent to the model, so summing it across turns would
  multiply-count the same history many times over. Estimating each message's
  own footprint and accumulating gives an honest, monotonically-growing "how big
  is the context getting" curve, which is what the running total is for. The
  estimate is approximate by construction (closed tokenizers are not public);
  treat it as a shape, not an invoice.

Contract
========

This surface is **advisory and read-only**. It never writes to the store and
**never raises** for data reasons: an unknown session, a session with zero
messages, or malformed ``raw_json`` all yield an empty-but-valid result (with a
``warnings`` note where useful), so a route/CLI can splice the output without a
try/except around every field.

Public API
----------
* :func:`build_context_timeline` — the full (uncut) reconstruction for a
  session. This is the heavy, cache-friendly unit: a route memoizes it per
  session and re-slices cheaply as the user scrubs.
* :func:`slice_context_timeline` — cut a full timeline to ``seq <= at_seq`` and
  recompute the totals. Pure and cheap.
* :func:`reconstruct_context` — the composed entry point
  (``session_id`` + ``at_seq`` → dict), used by the CLI and any direct caller.
* :func:`empty_context` — the canonical empty-but-valid shape (missing session,
  out-of-scope fence, ...), so every producer emits the identical dict.
"""

from __future__ import annotations

import json
import sqlite3
from typing import Any

from stackunderflow.services.playback import (
    _content_blocks,
    _envelope,
    summarize_tool_call,
)

__all__ = [
    "reconstruct_context",
    "build_context_timeline",
    "slice_context_timeline",
    "empty_context",
]

# Cap on the per-message content preview. Long enough to recognise the turn,
# short enough that a whole session's timeline stays a light payload.
_PREVIEW_CHARS = 240


# ── token estimation ─────────────────────────────────────────────────────────


def _estimate_tokens(text: str) -> int:
    """``chars/4`` token estimate for a chunk of text (0 for empty).

    Mirrors the ``chars/4`` heuristic the agent-output envelope and the
    discovery ranker use, so the numbers are comparable across surfaces. The
    ``+1`` on non-empty text avoids a zero-token turn that carried real content.
    """
    if not text:
        return 0
    return len(text) // 4 + 1


def _safe_json(value: Any) -> str:
    try:
        return json.dumps(value, default=str, separators=(",", ":"))
    except (TypeError, ValueError):
        return str(value)


# ── tool-call extraction ─────────────────────────────────────────────────────


def _tool_calls_from_envelope(env: dict[str, Any]) -> list[tuple[str, dict[str, Any]]]:
    """Pull ``(name, input)`` for every ``tool_use`` block in the raw envelope.

    ``raw_json`` is the authoritative source (it carries the full tool input);
    the derived ``tools_json`` column only holds tool *names*.
    """
    out: list[tuple[str, dict[str, Any]]] = []
    for blk in _content_blocks(env):
        if not isinstance(blk, dict) or blk.get("type") != "tool_use":
            continue
        name = blk.get("name")
        if not isinstance(name, str) or not name:
            continue
        tinput = blk.get("input")
        out.append((name, tinput if isinstance(tinput, dict) else {}))
    return out


def _tool_calls_from_tools_json(
    tools_json: str | None,
) -> list[tuple[str, dict[str, Any]]]:
    """Fallback tool-call list from the ``tools_json`` column.

    Handles both shapes the tree uses: the canonical array-of-strings
    (``["Edit", "Read"]``, names only) and the array-of-objects
    (``[{"name": "Edit", "input": {...}}]``) some fixtures / adapters carry.
    """
    if not tools_json:
        return []
    try:
        parsed = json.loads(tools_json)
    except (json.JSONDecodeError, TypeError, ValueError):
        return []
    if not isinstance(parsed, list):
        return []
    out: list[tuple[str, dict[str, Any]]] = []
    for entry in parsed:
        if isinstance(entry, str) and entry:
            out.append((entry, {}))
        elif isinstance(entry, dict):
            name = entry.get("name")
            if isinstance(name, str) and name:
                tinput = entry.get("input")
                out.append((name, tinput if isinstance(tinput, dict) else {}))
    return out


def _tool_calls_for_row(raw_json: str | None, tools_json: str | None) -> list[tuple[str, dict[str, Any]]]:
    env = _envelope(raw_json)
    calls = _tool_calls_from_envelope(env)
    if calls:
        return calls
    return _tool_calls_from_tools_json(tools_json)


# ── preview formatting ───────────────────────────────────────────────────────


def _preview(content: str, tool_labels: list[str]) -> str:
    """A one-glance preview of the turn: its text, or a tool-activity stand-in.

    Assistant turns that are pure tool calls (and tool-result user turns) often
    have empty ``content_text``; surface the tool activity so the timeline isn't
    a column of blanks.
    """
    text = (content or "").strip()
    if not text and tool_labels:
        text = "[" + ", ".join(tool_labels) + "]"
    text = text.replace("\r\n", "\n")
    if len(text) <= _PREVIEW_CHARS:
        return text
    return text[: _PREVIEW_CHARS - 1] + "…"


# ── session resolution ───────────────────────────────────────────────────────


def _resolve_session(conn: sqlite3.Connection, session_id: str) -> tuple[int, str] | None:
    """``session_id`` → ``(session_fk, session_id)`` for the most-recent match.

    ``session_id`` is unique per project, not globally; take the most-recently
    active row, matching :func:`playback._resolve_session`. Advisory: any store
    error is swallowed into "unknown session" rather than propagated.
    """
    try:
        row = conn.execute(
            "SELECT id, session_id FROM sessions WHERE session_id = ? "
            "ORDER BY last_ts DESC NULLS LAST, id DESC LIMIT 1",
            (session_id,),
        ).fetchone()
    except sqlite3.Error:
        return None
    if row is None:
        return None
    return int(row["id"]), str(row["session_id"])


# ── public API ───────────────────────────────────────────────────────────────


def empty_context(
    session_id: str,
    *,
    at_seq: int | None = None,
    warnings: list[str] | None = None,
) -> dict[str, Any]:
    """The canonical empty-but-valid reconstruction.

    Every producer (missing session, out-of-scope fence, empty session) returns
    THIS exact shape so consumers can rely on the keys unconditionally.
    """
    return {
        "session_id": session_id,
        "at_seq": at_seq,
        "message_count": 0,
        "total_tokens": 0,
        "events": [],
        "warnings": list(warnings or []),
    }


def build_context_timeline(
    conn: sqlite3.Connection, *, session_id: str
) -> dict[str, Any]:
    """Full (uncut) context reconstruction for ``session_id``.

    Walks the session's messages in ``seq`` order and builds one event per
    message with a running token total. This is the cache-friendly unit — build
    it once, then :func:`slice_context_timeline` for any ``at_seq`` cheaply.

    Never raises: an unknown session or a store error yields
    :func:`empty_context` with a note.
    """
    resolved = _resolve_session(conn, session_id)
    if resolved is None:
        return empty_context(
            session_id, warnings=[f"session not found in store: {session_id}"]
        )
    session_fk, sid = resolved

    try:
        rows = conn.execute(
            "SELECT seq, role, content_text, tools_json, raw_json "
            "FROM messages WHERE session_fk = ? ORDER BY seq",
            (session_fk,),
        ).fetchall()
    except sqlite3.Error as exc:  # advisory — never surface a store error
        return empty_context(sid, warnings=[f"could not read messages: {exc}"])

    events: list[dict[str, Any]] = []
    cumulative = 0
    for r in rows:
        content = r["content_text"] or ""
        calls = _tool_calls_for_row(r["raw_json"], r["tools_json"])
        tool_labels = [summarize_tool_call(name, inp) for name, inp in calls]
        tool_payload = "".join(name + _safe_json(inp) for name, inp in calls)
        tokens = _estimate_tokens(content) + _estimate_tokens(tool_payload)
        cumulative += tokens
        events.append(
            {
                "seq": int(r["seq"]),
                "role": r["role"] or "",
                "content_preview": _preview(content, tool_labels),
                "tokens": tokens,
                "cumulative_tokens": cumulative,
                "tool_calls": tool_labels,
            }
        )

    return {
        "session_id": sid,
        "at_seq": None,
        "message_count": len(events),
        "total_tokens": cumulative,
        "events": events,
        "warnings": [],
    }


def slice_context_timeline(
    full: dict[str, Any], *, at_seq: int | None
) -> dict[str, Any]:
    """Cut a full timeline to messages with ``seq <= at_seq`` and retotal.

    ``at_seq is None`` returns the whole timeline (the "current context is the
    entire session" view). Because events are ``seq``-ordered and each carries
    its own prefix-sum ``cumulative_tokens``, the slice's ``total_tokens`` is
    just the last retained event's cumulative — no re-summation needed. Pure:
    the returned dict is fresh; event dicts are shared (callers that memoize the
    full timeline deep-copy it before slicing).
    """
    events = full.get("events") or []
    if at_seq is None:
        kept = list(events)
    else:
        kept = [e for e in events if int(e.get("seq", 0)) <= at_seq]
    total = kept[-1]["cumulative_tokens"] if kept else 0
    return {
        "session_id": full.get("session_id", ""),
        "at_seq": at_seq,
        "message_count": len(kept),
        "total_tokens": total,
        "events": kept,
        "warnings": list(full.get("warnings") or []),
    }


def reconstruct_context(
    conn: sqlite3.Connection, *, session_id: str, at_seq: int | None = None
) -> dict[str, Any]:
    """Reconstruct the context for ``session_id`` up to ``at_seq`` (inclusive).

    The composed entry point: build the full timeline, then slice. Direct
    callers (the CLI) use this; the route splits the two so it can cache the
    build and slice per scrub. See the module docstring for the MVP semantics.
    """
    full = build_context_timeline(conn, session_id=session_id)
    return slice_context_timeline(full, at_seq=at_seq)
