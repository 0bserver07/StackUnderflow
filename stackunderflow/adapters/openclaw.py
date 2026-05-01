"""OpenClaw (and rebrand-cousins) session adapter.

OpenClaw ships under several names. This adapter checks each candidate
base directory in order and reads from whichever one(s) exist:

- ``~/.openclaw/agents/``
- ``~/.clawdbot/agents/``
- ``~/.moltbot/agents/``
- ``~/.moldbot/agents/``

Within each base, the layout is::

    {base}/{agent}/sessions/{sessionId}.jsonl

JSONL events look like::

    {"type": "session", "id": "...", "timestamp": "..."}
    {"type": "model_change", "data": {"model": "claude-..."}, "timestamp": "..."}
    {"type": "message", "id": "...", "timestamp": "...",
     "message": {"role": "assistant",
                 "content": [{"type": "text", "text": "..."}],
                 "model": "claude-3-5-sonnet",
                 "provider": "anthropic",
                 "usage": {"input": 100, "output": 50,
                           "cacheRead": 10, "cacheWrite": 5,
                           "cost": 0.0012}}}

Model resolution: prefer ``message.model`` per record; otherwise use the
most recent ``model_change`` event seen so far in the file. Falls back
to ``"openclaw-unknown"``.

Storage: byte-offset resume (spec §1.4) — same as Codex/Claude. ``seq``
is the byte offset where each JSONL line started.

Spec §3 (multi-provider).
"""

from __future__ import annotations

import json
import logging
import os
from collections.abc import Iterator
from pathlib import Path

from .base import Record, SessionRef

_log = logging.getLogger(__name__)

# Order matters: enumerate() walks each in turn so first-found wins for
# cross-listed agents. Unlikely to collide in practice (the rebrands
# don't share agent ids), but the deterministic order means tests can
# rely on it.
_CANDIDATE_BASES = (
    "~/.openclaw/agents",
    "~/.clawdbot/agents",
    "~/.moltbot/agents",
    "~/.moldbot/agents",
)

_DEFAULT_MODEL = "openclaw-unknown"


class OpenClawAdapter:
    """Source adapter for OpenClaw and its rebranded forks."""

    name = "openclaw"

    def __init__(self, base_dirs: list[Path] | None = None) -> None:
        # Tests inject explicit base dirs; production uses the candidate
        # list expanded against $HOME.
        if base_dirs is not None:
            self._bases = list(base_dirs)
        else:
            self._bases = [Path(os.path.expanduser(p)) for p in _CANDIDATE_BASES]

    # ── enumeration ───────────────────────────────────────────────────

    def enumerate(self) -> Iterator[SessionRef]:
        for base in self._bases:
            if not base.is_dir():
                continue
            for agent_dir in sorted(p for p in base.iterdir() if p.is_dir()):
                sessions_dir = agent_dir / "sessions"
                if not sessions_dir.is_dir():
                    continue
                for fp in sorted(sessions_dir.glob("*.jsonl")):
                    try:
                        stat = fp.stat()
                    except OSError as exc:
                        _log.warning(
                            "Cannot stat OpenClaw session %s: %s", fp, exc,
                        )
                        continue

                    session_id = _peek_session_id(fp) or fp.stem

                    yield SessionRef(
                        provider=self.name,
                        project_slug=agent_dir.name or "openclaw",
                        session_id=session_id,
                        file_path=fp,
                        file_mtime=stat.st_mtime,
                        file_size=stat.st_size,
                        source_kind="file",
                        source_hint=None,
                    )

    # ── reading ───────────────────────────────────────────────────────

    def read(self, ref: SessionRef, *, since_offset: int = 0) -> Iterator[Record]:
        try:
            fh = ref.file_path.open("rb")
        except OSError as exc:
            _log.warning("Cannot read %s: %s", ref.file_path, exc)
            return

        # Most-recent ``model_change`` seen so far. We always start by
        # scanning from the head to capture model_change events that may
        # precede ``since_offset`` — otherwise a resumed read could lose
        # the model context for records past the resume floor.
        current_model: str | None = _scan_for_model(ref.file_path, since_offset)

        with fh:
            fh.seek(since_offset)
            offset = since_offset
            for raw_line in fh:
                line_offset = offset
                offset += len(raw_line)
                if since_offset > 0 and line_offset <= since_offset:
                    continue
                stripped = raw_line.strip()
                if not stripped:
                    continue
                try:
                    event = json.loads(stripped)
                except (json.JSONDecodeError, ValueError) as exc:
                    _log.debug(
                        "Skipping malformed JSON line in %s: %s",
                        ref.file_path, exc,
                    )
                    continue

                etype = event.get("type")

                if etype == "model_change":
                    new_model = _model_from_model_change(event)
                    if new_model:
                        current_model = new_model
                    continue

                if etype != "message":
                    continue

                message = event.get("message") or {}
                if not isinstance(message, dict):
                    continue
                role = message.get("role")
                if role != "assistant":
                    # Spec says one Record per assistant message *with
                    # usage*; user/system messages don't drive cost.
                    continue
                usage = message.get("usage")
                if not isinstance(usage, dict):
                    continue

                model = (
                    str(message.get("model"))
                    if isinstance(message.get("model"), str)
                    and message.get("model")
                    else current_model or _DEFAULT_MODEL
                )

                tokens = _normalize_usage(usage)
                content = message.get("content")

                yield Record(
                    provider=self.name,
                    session_id=ref.session_id,
                    seq=line_offset,
                    timestamp=str(event.get("timestamp") or ""),
                    role="assistant",
                    model=model,
                    input_tokens=tokens["input"],
                    output_tokens=tokens["output"],
                    cache_create_tokens=tokens["cache_creation"],
                    cache_read_tokens=tokens["cache_read"],
                    content_text=_message_text(content),
                    tools=_tools_from_content(content),
                    cwd=None,
                    is_sidechain=False,
                    uuid=str(event.get("id") or f"{ref.session_id}:{line_offset}"),
                    parent_uuid=None,
                    raw=event,
                )


# ── helpers ───────────────────────────────────────────────────────────


def _peek_session_id(fp: Path) -> str:
    """Return the ``session_start`` event's id, or empty on failure."""
    try:
        with fp.open("rb") as fh:
            first = fh.readline()
    except OSError:
        return ""
    stripped = first.strip()
    if not stripped:
        return ""
    try:
        obj = json.loads(stripped)
    except (json.JSONDecodeError, ValueError):
        return ""
    if obj.get("type") != "session":
        return ""
    return str(obj.get("id") or "")


def _scan_for_model(fp: Path, until_offset: int) -> str | None:
    """Walk the file and return the most recent ``model_change`` model id.

    Stops scanning at ``until_offset`` (exclusive) so resume reads still
    see model context that was established before the resume point.
    Returns None when no ``model_change`` precedes the offset.
    """
    if until_offset <= 0:
        return None
    current: str | None = None
    try:
        with fp.open("rb") as fh:
            offset = 0
            for raw in fh:
                line_offset = offset
                offset += len(raw)
                if line_offset >= until_offset:
                    break
                stripped = raw.strip()
                if not stripped:
                    continue
                try:
                    obj = json.loads(stripped)
                except (json.JSONDecodeError, ValueError):
                    continue
                if obj.get("type") == "model_change":
                    candidate = _model_from_model_change(obj)
                    if candidate:
                        current = candidate
    except OSError:
        return None
    return current


def _model_from_model_change(event: dict) -> str:
    data = event.get("data")
    if isinstance(data, dict):
        m = data.get("model")
        if isinstance(m, str) and m:
            return m
    m = event.get("model")
    if isinstance(m, str) and m:
        return m
    return ""


def _normalize_usage(usage: dict) -> dict[str, int]:
    """Map OpenClaw's usage shape to canonical 4-key shape."""
    return {
        "input": _safe_int(usage.get("input")),
        "output": _safe_int(usage.get("output")),
        "cache_creation": _safe_int(usage.get("cacheWrite")),
        "cache_read": _safe_int(usage.get("cacheRead")),
    }


def _safe_int(val: object) -> int:
    try:
        return max(int(val or 0), 0)
    except (TypeError, ValueError):
        return 0


def _message_text(content: object) -> str:
    if isinstance(content, str):
        return content
    if not isinstance(content, list):
        return ""
    pieces: list[str] = []
    for blk in content:
        if isinstance(blk, dict):
            t = blk.get("text")
            if isinstance(t, str) and t:
                pieces.append(t)
        elif isinstance(blk, str):
            pieces.append(blk)
    return "\n".join(pieces)


def _tools_from_content(content: object) -> tuple[str, ...]:
    """Pluck tool names from ``tool_use`` / ``toolCall`` blocks."""
    if not isinstance(content, list):
        return ()
    tools: list[str] = []
    for blk in content:
        if not isinstance(blk, dict):
            continue
        if blk.get("type") in ("tool_use", "toolCall"):
            name = blk.get("name")
            if isinstance(name, str) and name:
                tools.append(name)
    return tuple(tools)
