"""Hermes session adapter.

Hermes writes JSONL conversation logs into its sessions directory under `~/.hermes/sessions/`.

Within each base, the layout is::

    ~/.hermes/sessions/{sessionId}.jsonl
    or recursively in nested project subdirectories.

JSONL events look like::

    {"type": "session", "id": "...", "timestamp": "..."}
    {"type": "model_change", "data": {"model": "claude-..."}, "timestamp": "..."}
    {"type": "message", "id": "...", "timestamp": "...",
     "message": {"role": "assistant",
                 "content": [{"type": "text", "text": "..."}],
                 "model": "claude-3-5-sonnet",
                 "provider": "anthropic",
                 "usage": {"input": 100, "output": 50,
                           "cacheRead": 10, "cacheWrite": 5}}}
"""

from __future__ import annotations

import json
import logging
import os
from collections.abc import Iterator
from pathlib import Path

from ._streaming import iter_jsonl_lines, stat_or_skip
from .base import Record, SessionRef

_log = logging.getLogger(__name__)

_DEFAULT_ROOT = "~/.hermes/sessions"
_DEFAULT_MODEL = "hermes-unknown"


class HermesAdapter:
    """Source adapter for Hermes agent."""

    name = "hermes"

    def __init__(self, roots: list[Path] | None = None) -> None:
        if roots is not None:
            self._roots = list(roots)
        else:
            self._roots = [Path(os.path.expanduser(_DEFAULT_ROOT))]

    # ── enumeration ───────────────────────────────────────────────────

    def enumerate(self) -> Iterator[SessionRef]:
        for root in self._roots:
            if not root.is_dir():
                continue
            # Search recursively using glob("**/*.jsonl") to cover any nested project subdirs
            for fp in sorted(root.glob("**/*.jsonl")):
                try:
                    stat = fp.stat()
                except OSError as exc:
                    _log.warning("Cannot stat Hermes session %s: %s", fp, exc)
                    continue

                session_id = _peek_session_id(fp) or fp.stem

                yield SessionRef(
                    provider=self.name,
                    project_slug=fp.parent.name if fp.parent != root else "hermes",
                    session_id=session_id,
                    file_path=fp,
                    file_mtime=stat.st_mtime,
                    file_size=stat.st_size,
                    source_kind="file",
                    source_hint=None,
                )

    def watch_paths(self) -> list[Path]:
        return self._roots

    # ── reading ───────────────────────────────────────────────────────

    def read(self, ref: SessionRef, *, since_offset: int = 0) -> Iterator[Record]:
        if stat_or_skip(ref.file_path) is None:
            return

        current_model: str | None = _scan_for_model(ref.file_path, since_offset)

        for line_offset, raw_line in iter_jsonl_lines(
            ref.file_path, since_offset=since_offset,
        ):
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
                cwd=event.get("cwd") or None,
                is_sidechain=False,
                uuid=str(event.get("id") or f"{ref.session_id}:{line_offset}"),
                parent_uuid=None,
                raw=event,
            )


# ── helpers ───────────────────────────────────────────────────────────


def _peek_session_id(fp: Path) -> str:
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
    return {
        "input": _safe_int(usage.get("input")),
        "output": _safe_int(usage.get("output")),
        "cache_creation": _safe_int(usage.get("cacheWrite")),
        "cache_read": _safe_int(usage.get("cacheRead")),
    }


def _safe_int(val: object) -> int:
    if isinstance(val, (int, float)):
        return max(int(val), 0)
    if isinstance(val, str):
        try:
            return max(int(val), 0)
        except ValueError:
            return 0
    if isinstance(val, bytes | bytearray):
        try:
            return max(int(val.decode("utf-8", errors="replace")), 0)
        except ValueError:
            return 0
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
