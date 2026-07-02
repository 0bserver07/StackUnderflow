"""Qwen Code session adapter.

Reads Qwen CLI session JSONL files at::

    $QWEN_DATA_DIR/projects/{project}/chats/*.jsonl
    ~/.qwen/projects/{project}/chats/*.jsonl  (default)

Each entry is one JSON object on its own line carrying the shape::

    {
      uuid, sessionId, timestamp,
      type: 'user' | 'assistant',
      model?,
      message: {
        role,
        parts: [{ text, thought?, functionCall: { name, args } }]
      },
      usageMetadata: {
        promptTokenCount, candidatesTokenCount,
        thoughtsTokenCount, cachedContentTokenCount
      }
    }

One ``Record`` is yielded per assistant entry that carries
``usageMetadata``; user entries also produce records (with zero tokens)
for conversation accounting. ``seq`` is the byte offset of the line
start — same convention as the Codex / Claude JSONL adapters, so
``read(ref, since_offset=N)`` resumes by ``fh.seek(N)`` and the
storage-aware contract test holds.

Token normalization (canonical 4-key shape):

* ``input_tokens`` = ``promptTokenCount - cachedContentTokenCount``
  (cached counts inside ``promptTokenCount``; canonical input is fresh
  input only — same convention as OpenAI / Anthropic).
* ``output_tokens`` = ``candidatesTokenCount + thoughtsTokenCount``
  (reasoning ("thoughts") rolls into the billable output column —
  matches the convention used for OpenAI reasoning tokens).
* ``cache_read_tokens`` = ``cachedContentTokenCount``.
* ``cache_create_tokens`` = ``0`` (Qwen does not surface cache writes).

macOS-only path constants in v1; Windows / Linux are documented at the
constant definitions but ``# untested`` per spec §5.

Defensive sizing: JSONL chats larger than ``MAX_SESSION_FILE_BYTES``
(128 MB; see ``stackunderflow/adapters/_streaming.py``) are **skipped
with a logged warning** rather than parsed. Smaller files stream
line-by-line.
"""

from __future__ import annotations

import json
import logging
import os
from collections.abc import Iterator
from pathlib import Path

from ._streaming import iter_jsonl_lines
from .base import Record, SessionRef

_log = logging.getLogger(__name__)

# Files bigger than this trigger a warning but are still parsed. Same
# threshold as the Codex adapter — keeps surprise-large files visible
# in the logs without aborting the read.
_LARGE_FILE_BYTES = 64 * 1024 * 1024

# Qwen tool name -> canonical cross-source tool label. Mirror the small
# set Codex maps; unknown names pass through untouched.
_TOOL_NAME_MAP = {
    "shell": "Bash",
    "execute": "Bash",
    "exec_command": "Bash",
    "run_command": "Bash",
    "read_file": "Read",
    "edit_file": "Edit",
    "write_file": "Edit",
    "apply_diff": "Edit",
    "list_directory": "Glob",
    "glob": "Glob",
    "grep": "Grep",
    "search": "Grep",
}

_DEFAULT_MODEL = "qwen-auto"


def _qwen_root() -> Path:
    """Resolve the Qwen projects root, honouring ``$QWEN_DATA_DIR``.

    Codeburn `qwen-parser.ts` checks ``QWEN_DATA_DIR`` first; that
    behaviour matters for users with sandboxed installs. We mirror it
    exactly so a single test can swap the env var instead of needing a
    constructor override.
    """
    env = os.environ.get("QWEN_DATA_DIR")
    if env:
        return Path(env) / "projects"
    return Path.home() / ".qwen" / "projects"


class QwenAdapter:
    """Source adapter for the Qwen Code CLI."""

    name = "qwen"

    def __init__(self, projects_root: Path | None = None) -> None:
        # ``projects_root`` is overridable so tests can point at synthetic
        # fixtures without monkey-patching ``Path.home()`` or the env.
        self._root = projects_root if projects_root is not None else _qwen_root()

    # ── enumeration ───────────────────────────────────────────────────

    def enumerate(self) -> Iterator[SessionRef]:
        root = self._root
        if not root.is_dir():
            # Not installed / never used — clean no-op rather than raise.
            return

        for project_dir in sorted(p for p in root.iterdir() if p.is_dir()):
            chats_dir = project_dir / "chats"
            if not chats_dir.is_dir():
                continue
            for fp in sorted(chats_dir.glob("*.jsonl")):
                try:
                    stat = fp.stat()
                except OSError as exc:
                    _log.warning("Cannot stat Qwen chat %s: %s", fp, exc)
                    continue

                if stat.st_size > _LARGE_FILE_BYTES:
                    _log.warning(
                        "Qwen chat %s is %d bytes (>%d); reading anyway",
                        fp, stat.st_size, _LARGE_FILE_BYTES,
                    )

                yield SessionRef(
                    provider=self.name,
                    project_slug=project_dir.name,
                    # ``session_id`` is finalised in ``read()`` from the
                    # first entry's ``sessionId`` field — we use the
                    # filename stem here so the registry/store has a
                    # deterministic id even before the file is opened.
                    session_id=fp.stem,
                    file_path=fp,
                    file_mtime=stat.st_mtime,
                    file_size=stat.st_size,
                    source_kind="file",
                    source_hint=None,
                )

    # ── reading ───────────────────────────────────────────────────────

    def read(self, ref: SessionRef, *, since_offset: int = 0) -> Iterator[Record]:
        # ``iter_jsonl_lines`` enforces the 128 MB defensive cap and
        # streams line-by-line; oversized chats are skipped with a
        # warning rather than parsed.
        for line_offset, raw_line in iter_jsonl_lines(
            ref.file_path, since_offset=since_offset,
        ):
            # ``since_offset == 0`` means "fresh read, yield
            # everything". Otherwise the caller already saw the
            # record at exactly ``since_offset`` so skip it.
            if since_offset > 0 and line_offset <= since_offset:
                continue
            stripped = raw_line.strip()
            if not stripped:
                continue
            try:
                entry = json.loads(stripped)
            except (json.JSONDecodeError, ValueError) as exc:
                _log.debug(
                    "Skipping malformed Qwen JSON line in %s: %s",
                    ref.file_path, exc,
                )
                continue
            if not isinstance(entry, dict):
                continue

            record = self._record_from_entry(entry, ref=ref, seq=line_offset)
            if record is not None:
                yield record

    # ── internals ─────────────────────────────────────────────────────

    def _record_from_entry(
        self,
        entry: dict,
        *,
        ref: SessionRef,
        seq: int,
    ) -> Record | None:
        etype = entry.get("type")
        if etype not in ("user", "assistant"):
            return None
        role = etype  # 'user' / 'assistant'

        message = entry.get("message") if isinstance(entry.get("message"), dict) else {}
        parts = message.get("parts") if isinstance(message, dict) else None
        text = _text_from_parts(parts)
        tools = _tools_from_parts(parts)

        usage = entry.get("usageMetadata")
        tokens = _normalize_usage(usage)

        session_id = str(entry.get("sessionId") or ref.session_id)
        raw_model = entry.get("model")
        if not isinstance(raw_model, str) or not raw_model:
            # A non-string model (dict / list / number) would poison the
            # Record contract and crash the store write downstream.
            raw_model = None
        model = raw_model or (_DEFAULT_MODEL if role == "assistant" else None)
        timestamp = str(entry.get("timestamp") or "")
        uuid = str(entry.get("uuid") or f"{session_id}:{seq}")

        return Record(
            provider=self.name,
            session_id=session_id,
            seq=seq,
            timestamp=timestamp,
            role=role,
            model=model,
            input_tokens=tokens["input"],
            output_tokens=tokens["output"],
            cache_create_tokens=tokens["cache_creation"],
            cache_read_tokens=tokens["cache_read"],
            content_text=text,
            tools=tools,
            cwd=None,
            is_sidechain=False,
            uuid=uuid,
            parent_uuid=None,
            raw=entry,
        )


# ── helpers ───────────────────────────────────────────────────────────


def _text_from_parts(parts: object) -> str:
    """Concatenate every ``.text`` field across content parts.

    Parts marked ``thought=True`` are reasoning traces — we still
    include their text in ``content_text`` for downstream search /
    classification, mirroring how the Claude adapter keeps
    ``thinking`` blocks visible.
    """
    if not isinstance(parts, list):
        return ""
    pieces: list[str] = []
    for part in parts:
        if isinstance(part, dict):
            text = part.get("text")
            if isinstance(text, str) and text:
                pieces.append(text)
        elif isinstance(part, str):
            pieces.append(part)
    return "\n".join(pieces)


def _tools_from_parts(parts: object) -> tuple[str, ...]:
    """Extract tool names from ``functionCall`` blocks in parts.

    Unknown names pass through unmapped so new Qwen tools remain visible
    in the dashboard — same convention as the Codex adapter.
    """
    if not isinstance(parts, list):
        return ()
    names: list[str] = []
    for part in parts:
        if not isinstance(part, dict):
            continue
        fc = part.get("functionCall")
        if not isinstance(fc, dict):
            continue
        name = fc.get("name")
        if not isinstance(name, str) or not name:
            continue
        names.append(_TOOL_NAME_MAP.get(name, name))
    return tuple(names)


def _normalize_usage(usage: object) -> dict[str, int]:
    """Flatten Qwen ``usageMetadata`` into the canonical 4-key shape.

    See the module docstring for the full rule. Missing / malformed
    fields default to 0 so a partial entry still produces a valid
    Record.
    """
    if not isinstance(usage, dict):
        return {"input": 0, "output": 0, "cache_creation": 0, "cache_read": 0}

    prompt = _safe_int(usage.get("promptTokenCount"))
    candidates = _safe_int(usage.get("candidatesTokenCount"))
    thoughts = _safe_int(usage.get("thoughtsTokenCount"))
    cached = _safe_int(usage.get("cachedContentTokenCount"))

    return {
        "input": max(prompt - cached, 0),
        "output": candidates + thoughts,
        "cache_creation": 0,
        "cache_read": cached,
    }


def _safe_int(value: object) -> int:
    """Coerce ``value`` to a non-negative int, defaulting to 0 on failure."""
    if value is None or value == "":
        return 0
    try:
        out = int(value)
    except (TypeError, ValueError, OverflowError):
        # OverflowError: JSON like ``1e999`` parses to float('inf').
        return 0
    return max(out, 0)
