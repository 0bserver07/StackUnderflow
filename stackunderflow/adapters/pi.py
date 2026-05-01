"""Pi (and OMP) session adapter.

Pi and OMP are sibling CLIs that share an on-disk format but use
different roots:

- Pi: ``~/.pi/agent/sessions/``
- OMP: ``~/.omp/agent/sessions/``

We implement them as a **single** ``PiAdapter`` that scans both roots —
the diff between two adapters would be one constant — and the env-flag
``STACKUNDERFLOW_BETA_PI`` controls discovery for both. ``project_slug``
embeds the source root name (``pi`` or ``omp``) so cross-tool reports
can still split them apart.

JSONL events look like::

    {"type": "session", "id": "...", "timestamp": "...", "cwd": "..."}
    {"type": "message", "id": "...", "timestamp": "...",
     "message": {"role": "assistant",
                 "content": [{"type": "text", "text": "..."}],
                 "model": "gpt-5",
                 "responseId": "...",
                 "usage": {"input": ..., "output": ...,
                           "cacheRead": ..., "cacheWrite": ...}}}

Storage: byte-offset resume (spec §1.4) — same as Codex.

Spec §3 (multi-provider).
"""

from __future__ import annotations

import json
import logging
from collections.abc import Iterator
from pathlib import Path

from .base import Record, SessionRef

_log = logging.getLogger(__name__)

_DEFAULT_MODEL = "gpt-5"

# (root path, label) — label feeds project_slug so a downstream report
# can keep Pi and OMP distinct without re-deriving from the path.
_DEFAULT_ROOTS: tuple[tuple[Path, str], ...] = (
    (Path.home() / ".pi" / "agent" / "sessions", "pi"),
    (Path.home() / ".omp" / "agent" / "sessions", "omp"),
)


class PiAdapter:
    """Source adapter for Pi and OMP CLIs (shared format)."""

    name = "pi"

    def __init__(self, roots: list[tuple[Path, str]] | None = None) -> None:
        # Tests inject explicit roots; production scans both defaults.
        self._roots = list(roots) if roots is not None else list(_DEFAULT_ROOTS)

    # ── enumeration ───────────────────────────────────────────────────

    def enumerate(self) -> Iterator[SessionRef]:
        for root, label in self._roots:
            if not root.is_dir():
                continue
            for fp in sorted(root.glob("*.jsonl")):
                try:
                    stat = fp.stat()
                except OSError as exc:
                    _log.warning("Cannot stat Pi/OMP session %s: %s", fp, exc)
                    continue

                session_id, cwd = _peek_session_meta(fp)
                if not session_id:
                    session_id = fp.stem
                project_slug = _slug_for(cwd, label) if cwd else label

                yield SessionRef(
                    provider=self.name,
                    project_slug=project_slug,
                    session_id=session_id,
                    file_path=fp,
                    file_mtime=stat.st_mtime,
                    file_size=stat.st_size,
                    source_kind="file",
                    # Hint preserves the source label so downstream code
                    # can tell Pi sessions from OMP sessions without
                    # re-parsing the file path.
                    source_hint={"source": label},
                )

    # ── reading ───────────────────────────────────────────────────────

    def read(self, ref: SessionRef, *, since_offset: int = 0) -> Iterator[Record]:
        try:
            fh = ref.file_path.open("rb")
        except OSError as exc:
            _log.warning("Cannot read %s: %s", ref.file_path, exc)
            return

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

                if event.get("type") != "message":
                    continue

                message = event.get("message") or {}
                if not isinstance(message, dict):
                    continue
                if message.get("role") != "assistant":
                    continue
                usage = message.get("usage")
                if not isinstance(usage, dict):
                    continue

                model = (
                    str(message.get("model"))
                    if isinstance(message.get("model"), str)
                    and message.get("model")
                    else _DEFAULT_MODEL
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
                    cwd=event.get("cwd") or None,
                    is_sidechain=False,
                    uuid=str(event.get("id") or f"{ref.session_id}:{line_offset}"),
                    parent_uuid=None,
                    raw=event,
                )


# ── helpers ───────────────────────────────────────────────────────────


def _peek_session_meta(fp: Path) -> tuple[str, str]:
    """Return ``(session_id, cwd)`` from the first ``session`` event."""
    try:
        with fp.open("rb") as fh:
            first = fh.readline()
    except OSError:
        return "", ""
    stripped = first.strip()
    if not stripped:
        return "", ""
    try:
        obj = json.loads(stripped)
    except (json.JSONDecodeError, ValueError):
        return "", ""
    if obj.get("type") != "session":
        return "", ""
    return str(obj.get("id") or ""), str(obj.get("cwd") or "")


def _slug_for(project_path: str, label: str) -> str:
    """Project slug includes the source label so Pi vs OMP stays separate."""
    import os

    cleaned = (
        os.path.abspath(project_path)
        .rstrip(os.sep)
        .replace(os.sep, "-")
        .replace("_", "-")
    )
    return f"{label}{cleaned}"


def _normalize_usage(usage: dict) -> dict[str, int]:
    """Pi/OMP shape → canonical 4-key shape."""
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
    """Pluck tool names from ``toolCall`` / ``tool_use`` blocks."""
    if not isinstance(content, list):
        return ()
    tools: list[str] = []
    for blk in content:
        if not isinstance(blk, dict):
            continue
        if blk.get("type") in ("toolCall", "tool_use"):
            name = blk.get("name")
            if isinstance(name, str) and name:
                tools.append(name)
    return tuple(tools)
