"""Grok (xAI ``grok`` CLI) session adapter — BETA.

Reads the grok CLI's per-project session transcripts at::

    ~/.grok/sessions/<url-encoded-cwd>/<session-uuid>/chat_history.jsonl

The sessions root is a portable dotfile (``~/.grok/sessions``) — no
platform branch, unlike the VS Code-style adapters. Each *project* is a
subdirectory whose name is the URL-encoded absolute working directory,
e.g. ``%2FUsers%2Fme%2Fproj`` → ``/Users/me/proj`` (decoded with
``urllib.parse.unquote``). We map that decoded cwd to the **same**
``project_slug`` Claude Code uses for its ``~/.claude/projects/`` dirs
(every non-alphanumeric char → ``-``; see
``stackunderflow.adapters.claude_teams.slug_for_path``) so a repo's Grok
and Claude sessions line up under one project. Verified against the real
``~/.claude/projects/-Users-yadkonrad--claude`` (the leading ``.`` of
``.claude`` becomes a dash — proving the catch-all transform, not the
separators-only one).

Each *session* is a UUIDv7 directory; its name is the ``session_id``. The
transcript is ``chat_history.jsonl`` — one JSON object per line. Sibling
files (``events.jsonl``, ``updates.jsonl``, ``summary.json``,
``system_prompt.txt``, ``prompt_context.json``, …) are ignored in v1.

Record shape (top-level ``type`` is the discriminator — ``kind`` / ``role``
that appear in the catalog are absent in practice)::

    {"type": "system",      "content": "<str>"}
    {"type": "user",        "content": [{"type": "text", "text": "..."}], "synthetic_reason"?}
    {"type": "reasoning",   "encrypted_content": "<str>", "summary": [...], "id", "status"}
    {"type": "assistant",   "content": "<str>", "tool_calls": [{"id", "name", "arguments"}],
                            "model_id": "grok-build", "model_fingerprint"}
    {"type": "tool_result", "content": "<str>", "tool_call_id": "..."}

**Quirks**

- **No token usage anywhere.** Neither ``chat_history.jsonl`` nor the
  sibling ``events.jsonl`` / ``summary.json`` record token counts, so
  tokens are *estimated* from content length / 4 (same convention as the
  Kiro adapter). Every Record carries ``raw["cost_source"] = "estimated"``
  so the cost layer can flag / down-weight the numbers.
- **Encrypted reasoning.** ``reasoning`` records store the chain-of-thought
  in ``encrypted_content`` (encrypted at rest) and carry **no** ``content``
  field. We do not attempt decryption — the text is treated as empty /
  unavailable, so an encrypted reasoning turn estimates to 0 tokens
  rather than crashing.
- **No per-message timestamp.** Records carry no time field; we derive a
  stable ISO 8601 stamp from the session dir's UUIDv7 (its first 48 bits
  are a unix-ms creation time) and fall back to the transcript's mtime
  when the id is not a parseable UUIDv7.
- ``model_id`` is ``"grok-build"`` (the only model the v0.2.x CLI ships)
  and appears on ``assistant`` records only; billable model turns default
  to ``grok-build``.

``seq`` is the byte offset of each line start — same convention as the
Codex / Claude / Qwen JSONL adapters — so ``read(ref, since_offset=N)``
resumes via ``fh.seek(N)`` and the storage-aware contract test holds.

Defensive sizing: ``chat_history.jsonl`` files larger than
``MAX_SESSION_FILE_BYTES`` (128 MB; see
``stackunderflow/adapters/_streaming.py``) are **skipped with a logged
warning** rather than parsed. Smaller files stream line-by-line.

Spec §3 (multi-provider). Beta — off by default; set
``STACKUNDERFLOW_BETA_GROK=1`` to enable.
"""

from __future__ import annotations

import json
import logging
import urllib.parse
import uuid
from collections.abc import Iterator
from datetime import UTC, datetime
from pathlib import Path

from ._streaming import iter_jsonl_lines
from .base import Record, SessionRef
from .claude_teams import slug_for_path

_log = logging.getLogger(__name__)

_TRANSCRIPT_NAME = "chat_history.jsonl"
_DEFAULT_MODEL = "grok-build"

# ``type`` → canonical role. Types not in this map are non-conversational
# (the ``system`` prompt) or unknown and are skipped at ``read()`` time.
# ``reasoning`` keeps its own role so the normalizer can treat it as a
# billable assistant-side turn (mirrors how the Kiro normalizer accepts
# both ``assistant`` and ``bot``). ``tool_result`` / ``backend_tool_call``
# are emitted as non-billable ``tool`` rows for transcript fidelity; the
# normalizer skips them.
_ROLE_BY_TYPE = {
    "user": "user",
    "assistant": "assistant",
    "reasoning": "reasoning",
    "tool_result": "tool",
    "backend_tool_call": "tool",
}

# Roles whose visible content is model output we estimate tokens for.
_BILLABLE_ROLES = ("assistant", "reasoning")

# Grok tool name -> canonical cross-source tool label. Mirrors the small
# set the Qwen / Codex adapters map; unknown names pass through untouched
# so new Grok tools stay visible in the dashboard.
_TOOL_NAME_MAP = {
    "run_terminal_command": "Bash",
    "shell": "Bash",
    "exec_command": "Bash",
    "read_file": "Read",
    "list_dir": "Glob",
    "glob": "Glob",
    "grep": "Grep",
    "search": "Grep",
    "edit_file": "Edit",
    "write_file": "Edit",
    "create_file": "Edit",
}


def _grok_sessions_root() -> Path:
    """Return the grok CLI sessions root (``~/.grok/sessions``).

    A portable dotfile — no platform branch. Overridable via the
    ``GrokAdapter(sessions_root=...)`` constructor so tests can point at
    synthetic fixtures without monkeypatching ``Path.home()``.
    """
    return Path.home() / ".grok" / "sessions"


class GrokAdapter:
    """Source adapter for the xAI ``grok`` CLI (BETA)."""

    name = "grok"

    def __init__(self, sessions_root: Path | None = None) -> None:
        # ``sessions_root`` is overridable so tests point at synthetic
        # fixtures — same shape as Kiro's ``storage_root`` / Qwen's
        # ``projects_root``.
        self._root = sessions_root if sessions_root is not None else _grok_sessions_root()

    def watch_paths(self) -> list[Path]:
        """Watch the sessions root; a missing root is a clean no-op upstream."""
        return [self._root]

    # ── enumeration ───────────────────────────────────────────────────

    def enumerate(self) -> Iterator[SessionRef]:
        root = self._root
        if not root.is_dir():
            # Not installed / never used — clean no-op rather than raise.
            return

        for project_dir in sorted(p for p in root.iterdir() if p.is_dir()):
            project_slug = _project_slug(project_dir.name)
            for session_dir in sorted(p for p in project_dir.iterdir() if p.is_dir()):
                fp = session_dir / _TRANSCRIPT_NAME
                if not fp.is_file():
                    continue
                try:
                    stat = fp.stat()
                except OSError as exc:
                    _log.warning("Cannot stat Grok transcript %s: %s", fp, exc)
                    continue

                yield SessionRef(
                    provider=self.name,
                    project_slug=project_slug,
                    # The session UUID dir name is the session id.
                    session_id=session_dir.name,
                    file_path=fp,
                    file_mtime=stat.st_mtime,
                    file_size=stat.st_size,
                    source_kind="file",
                    source_hint=None,
                )

    # ── reading ───────────────────────────────────────────────────────

    def read(self, ref: SessionRef, *, since_offset: int = 0) -> Iterator[Record]:
        # ``iter_jsonl_lines`` enforces the 128 MB defensive cap (yields
        # nothing for oversize files) and streams line-by-line.
        timestamp = _session_timestamp(ref)
        for line_offset, raw_line in iter_jsonl_lines(
            ref.file_path,
            since_offset=since_offset,
        ):
            # ``since_offset == 0`` means "fresh read, yield everything".
            # Otherwise the caller already saw the record at exactly
            # ``since_offset`` so skip it.
            if since_offset > 0 and line_offset <= since_offset:
                continue
            stripped = raw_line.strip()
            if not stripped:
                continue
            try:
                obj = json.loads(stripped)
            except (json.JSONDecodeError, ValueError) as exc:
                _log.debug(
                    "Skipping malformed Grok JSON line in %s: %s",
                    ref.file_path,
                    exc,
                )
                continue
            if not isinstance(obj, dict):
                continue

            record = self._record_from_obj(
                obj,
                ref=ref,
                seq=line_offset,
                timestamp=timestamp,
            )
            if record is not None:
                yield record

    # ── internals ─────────────────────────────────────────────────────

    def _record_from_obj(
        self,
        obj: dict,
        *,
        ref: SessionRef,
        seq: int,
        timestamp: str,
    ) -> Record | None:
        rtype = obj.get("type")
        role = _ROLE_BY_TYPE.get(rtype) if isinstance(rtype, str) else None
        if role is None:
            # ``system`` prompt / unknown type — not a conversational turn.
            return None

        # ``reasoning`` carries the chain-of-thought in ``encrypted_content``
        # (no ``content`` field); we don't decrypt, so the text is empty.
        text = _content_text(obj)
        tools = _tools_from(obj)

        # No token usage in the source: estimate output from the visible
        # content (chars / 4) for model turns. Encrypted reasoning has no
        # readable text → 0 tokens. ``user`` / ``tool`` rows carry no usage
        # (only model turns are billed — matches Claude / Qwen).
        if role in _BILLABLE_ROLES:
            output_tokens = max(len(text) // 4, 0)
            model = _model_from(obj)
        else:
            output_tokens = 0
            model = None

        raw_payload = dict(obj)
        # Mark estimated so the cost layer knows it's not authoritative.
        raw_payload["cost_source"] = "estimated"

        return Record(
            provider=self.name,
            session_id=ref.session_id,
            seq=seq,
            timestamp=timestamp,
            role=role,
            model=model,
            input_tokens=0,
            output_tokens=output_tokens,
            cache_create_tokens=0,
            cache_read_tokens=0,
            content_text=text,
            tools=tools,
            cwd=None,
            is_sidechain=False,
            uuid=str(obj.get("id") or f"{ref.session_id}:{seq}"),
            parent_uuid=None,
            raw=raw_payload,
        )


# ── helpers ───────────────────────────────────────────────────────────


def _project_slug(encoded_dir_name: str) -> str:
    """``%2FUsers%2Fme%2Fproj`` → Claude-style slug for ``/Users/me/proj``.

    Decode the URL-encoded cwd, then run it through the **same** transform
    Claude Code uses to name its ``~/.claude/projects/`` dirs
    (``slug_for_path``: every non-alphanumeric char → ``-``) so a repo's
    Grok sessions line up with that repo's Claude sessions.
    """
    decoded = urllib.parse.unquote(encoded_dir_name)
    return slug_for_path(decoded)


def _content_text(obj: dict) -> str:
    """Concatenate the readable text from a record's ``content``.

    ``assistant`` / ``system`` / ``tool_result`` content is a plain string;
    ``user`` content is a list of ``{type, text}`` parts. ``reasoning``
    records carry only ``encrypted_content`` (no ``content``) — we don't
    decrypt, so they resolve to an empty string.
    """
    content = obj.get("content")
    if isinstance(content, str):
        return content
    if not isinstance(content, list):
        return ""
    pieces: list[str] = []
    for part in content:
        if isinstance(part, dict):
            text = part.get("text")
            if isinstance(text, str) and text:
                pieces.append(text)
        elif isinstance(part, str):
            pieces.append(part)
    return "\n".join(pieces)


def _tools_from(obj: dict) -> tuple[str, ...]:
    """Extract tool names from an assistant record's ``tool_calls``.

    Grok puts the tool name at the top level of each call
    (``{"id", "name", "arguments"}``). Unknown names pass through unmapped.
    """
    calls = obj.get("tool_calls")
    if not isinstance(calls, list):
        return ()
    names: list[str] = []
    for call in calls:
        if not isinstance(call, dict):
            continue
        name = call.get("name")
        if not isinstance(name, str) or not name:
            continue
        names.append(_TOOL_NAME_MAP.get(name, name))
    return tuple(names)


def _model_from(obj: dict) -> str:
    """Return the record's ``model_id``, defaulting to ``grok-build``.

    Only ``assistant`` records carry ``model_id``; ``reasoning`` turns
    don't, but they're still grok-build model output, so we default.
    """
    raw = obj.get("model_id")
    if isinstance(raw, str) and raw:
        return raw
    return _DEFAULT_MODEL


def _session_timestamp(ref: SessionRef) -> str:
    """ISO 8601 stamp for every record in a session.

    Grok records carry no per-message timestamp. Derive one from the
    session dir's UUIDv7 (its first 48 bits are a unix-ms creation time);
    fall back to the transcript's mtime when the id isn't a UUIDv7.
    """
    ms = _uuidv7_unix_ms(ref.session_id)
    if ms is not None:
        try:
            return datetime.fromtimestamp(ms / 1000, tz=UTC).isoformat()
        except (OverflowError, OSError, ValueError):
            pass
    return datetime.fromtimestamp(ref.file_mtime, tz=UTC).isoformat()


def _uuidv7_unix_ms(session_id: str) -> int | None:
    """Extract the unix-ms timestamp from a UUIDv7 string (top 48 bits).

    Returns ``None`` when ``session_id`` isn't a version-7 UUID.
    """
    try:
        parsed = uuid.UUID(session_id)
    except (ValueError, AttributeError, TypeError):
        return None
    if parsed.version != 7:
        return None
    return (parsed.int >> 80) & 0xFFFF_FFFF_FFFF
