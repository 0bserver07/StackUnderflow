"""Kiro (kiroagent) session adapter.

Reads chat files Kiro writes under VS Code-style globalStorage:

- macOS: ``~/Library/Application Support/Kiro/User/globalStorage/kiro.kiroagent/``
- Windows: ``%APPDATA%\\Kiro\\User\\globalStorage\\kiro.kiroagent\\``  *(untested)*
- Linux: ``~/.config/Kiro/User/globalStorage/kiro.kiroagent/``  *(untested)*

Each ``.chat`` file is a single JSON document::

    {
      "executionId": "...",
      "actionId": "...",
      "chat": [
        {"role": "human" | "bot" | "tool", "content": "..."}
      ],
      "metadata": {
        "modelId": "claude.3.5.sonnet",
        "startTime": "...",
        "endTime": "...",
        "workflowId": "..."
      }
    }

**Quirks**:

- **Tokens are estimated** from content length / 4 (Kiro doesn't record
  per-call usage). We mark every emitted Record with
  ``raw["cost_source"] = "estimated"`` so the cost layer can decide to
  show or down-weight these numbers.
- Model ids come dot-separated (``claude.3.5.sonnet``); we normalise
  them to the dash-separated form (``claude-3-5-sonnet``) so the
  Anthropic pricer's family heuristic matches.
- One Record per execution (the entire chat is rolled up into a single
  assistant turn). Resumable reads (``since_offset``) are by event index
  — Kiro files are small and aren't streamed.

Spec §3 (multi-provider).

Defensive sizing: ``.chat`` files larger than ``MAX_SESSION_FILE_BYTES``
(128 MB; see ``stackunderflow/adapters/_streaming.py``) are **skipped
with a logged warning** rather than parsed. Single-document JSON cannot
stream, so the cap is the only safety net."""

from __future__ import annotations

import json
import logging
import os
import sys
from collections.abc import Iterator
from pathlib import Path

from ._streaming import stat_or_skip
from .base import Record, SessionRef

_log = logging.getLogger(__name__)


def _kiro_global_storage() -> Path:
    """Return the platform-appropriate Kiro ``globalStorage`` root.

    Same platform-branch shape as the VS Code adapters; ``APPDATA`` is
    read at call time so tests can monkeypatch it. Real-box validation on
    Windows/Linux is still pending (catalog §3).
    """
    if sys.platform.startswith("win"):
        return Path(os.environ.get("APPDATA", "")) / "Kiro" / "User" / "globalStorage" / "kiro.kiroagent"
    if sys.platform.startswith("linux"):
        return Path.home() / ".config" / "Kiro" / "User" / "globalStorage" / "kiro.kiroagent"
    return Path.home() / "Library" / "Application Support" / "Kiro" / "User" / "globalStorage" / "kiro.kiroagent"


_DEFAULT_MODEL = "kiro-auto"


class KiroAdapter:
    """Source adapter for the Kiro agent extension."""

    name = "kiro"

    def __init__(self, storage_root: Path | None = None) -> None:
        self._root = storage_root or _kiro_global_storage()

    # ── enumeration ───────────────────────────────────────────────────

    def source_roots(self) -> list[Path]:
        """Roots ``backup create`` copies — the same data
        ``enumerate()`` reads. Self-declared here (like ``name``),
        never listed centrally.
        """
        return [self._root]

    def enumerate(self) -> Iterator[SessionRef]:
        root = self._root
        if not root.is_dir():
            return

        # Kiro stores ``.chat`` files at the storage root and possibly
        # under nested directories (workspace-scoped subtrees). Walk the
        # tree and yield every ``*.chat`` we find.
        for fp in sorted(root.rglob("*.chat")):
            try:
                stat = fp.stat()
            except OSError as exc:
                _log.warning("Cannot stat Kiro chat %s: %s", fp, exc)
                continue

            session_id = fp.stem  # default; refined from metadata in read()
            workflow_id, project_slug = _peek_metadata(fp)
            if workflow_id:
                session_id = workflow_id

            yield SessionRef(
                provider=self.name,
                project_slug=project_slug or "kiro",
                session_id=session_id,
                file_path=fp,
                file_mtime=stat.st_mtime,
                file_size=stat.st_size,
                source_kind="file",
                source_hint=None,
            )

    # ── reading ───────────────────────────────────────────────────────

    def read(self, ref: SessionRef, *, since_offset: int = 0) -> Iterator[Record]:
        # Single-document JSON: must call ``json.load`` on the whole
        # file, so the only safety net is the defensive 128 MB cap.
        # Above the cap we yield nothing — same convention as the JSONL
        # adapters.
        if stat_or_skip(ref.file_path) is None:
            return
        try:
            with ref.file_path.open("rb") as fh:
                data = json.load(fh)
        except (OSError, json.JSONDecodeError, ValueError) as exc:
            _log.warning("Cannot read Kiro chat %s: %s", ref.file_path, exc)
            return
        if not isinstance(data, dict):
            return

        meta = data.get("metadata") or {}
        if not isinstance(meta, dict):
            meta = {}

        raw_model = meta.get("modelId")
        model = _normalize_model(raw_model) if isinstance(raw_model, str) else _DEFAULT_MODEL

        timestamp = str(meta.get("startTime") or meta.get("endTime") or "")

        chat = data.get("chat") or []
        if not isinstance(chat, list):
            chat = []

        # Kiro rolls up an entire execution into one logical assistant
        # turn. We compute a single seq for that turn (event index 0)
        # and emit one Record. ``since_offset >= 0`` skips it so the
        # resume contract still holds (a "midpoint" past the only record
        # yields nothing).
        if since_offset > 0:
            return

        human_text, bot_text = _join_chat(chat)
        # Token estimate: content_chars // 4.
        input_tokens = max(len(human_text) // 4, 0)
        output_tokens = max(len(bot_text) // 4, 0)

        # Mark estimated so the cost layer can flag / discount.
        raw_payload = dict(data)
        raw_payload["cost_source"] = "estimated"

        yield Record(
            provider=self.name,
            session_id=ref.session_id,
            seq=0,
            timestamp=timestamp,
            role="assistant",
            model=model,
            input_tokens=input_tokens,
            output_tokens=output_tokens,
            cache_create_tokens=0,
            cache_read_tokens=0,
            content_text=bot_text,
            tools=_extract_tools(bot_text),
            cwd=None,
            is_sidechain=False,
            uuid=str(data.get("executionId") or f"{ref.session_id}:0"),
            parent_uuid=None,
            raw=raw_payload,
        )


# ── helpers ───────────────────────────────────────────────────────────


def _peek_metadata(fp: Path) -> tuple[str, str]:
    """Return ``(workflow_id, project_slug)`` from the file's metadata block.

    Both default to empty when the file is unreadable or malformed.
    Project slug currently comes from the parent directory name as a
    proxy for "workspace" — Kiro's workspace-hash → directory mapping is
    not exposed in v1.
    """
    try:
        with fp.open("rb") as fh:
            obj = json.load(fh)
    except (OSError, json.JSONDecodeError, ValueError):
        return "", ""
    if not isinstance(obj, dict):
        return "", ""
    meta = obj.get("metadata") or {}
    workflow_id = meta.get("workflowId") if isinstance(meta, dict) else None
    return (
        str(workflow_id or ""),
        fp.parent.name or "kiro",
    )


def _normalize_model(model_id: str) -> str:
    """``claude.3.5.sonnet`` → ``claude-3-5-sonnet``.

    Dots become dashes; everything else passes through. Empty input
    falls back to ``kiro-auto``.
    """
    if not model_id:
        return _DEFAULT_MODEL
    return model_id.replace(".", "-").strip() or _DEFAULT_MODEL


def _join_chat(chat: list) -> tuple[str, str]:
    """Concatenate human-side and bot-side messages separately."""
    human_pieces: list[str] = []
    bot_pieces: list[str] = []
    for entry in chat:
        if not isinstance(entry, dict):
            continue
        role = entry.get("role")
        content = entry.get("content")
        text = content if isinstance(content, str) else ""
        if role == "human":
            human_pieces.append(text)
        elif role == "bot":
            bot_pieces.append(text)
        # ``tool`` role is ignored for token estimation; tool *names*
        # are extracted from bot text via _extract_tools.
    return "\n".join(human_pieces), "\n".join(bot_pieces)


def _extract_tools(bot_text: str) -> tuple[str, ...]:
    """Pull tool names from ``<tool_use><name>X</name>`` markers.

    Kiro embeds tool invocations as XML-style fragments inside bot text.
    We do a permissive split rather than a full XML parse — invalid/partial
    fragments yield zero tool names rather than raising.
    """
    if not bot_text:
        return ()
    out: list[str] = []
    cursor = 0
    while True:
        start = bot_text.find("<tool_use>", cursor)
        if start < 0:
            break
        nopen = bot_text.find("<name>", start)
        if nopen < 0:
            break
        nclose = bot_text.find("</name>", nopen)
        if nclose < 0:
            break
        name = bot_text[nopen + len("<name>") : nclose].strip()
        if name:
            out.append(name)
        cursor = nclose + len("</name>")
    return tuple(out)
