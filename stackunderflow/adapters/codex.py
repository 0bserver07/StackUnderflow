"""OpenAI Codex session adapter.

Reads Codex CLI rollout files at ~/.codex/sessions/YYYY/MM/DD/
rollout-YYYY-MM-DDTHH-MM-SS-<uuid>.jsonl.

Each rollout is JSONL; the first line is a `session_meta` event that carries
the `id`, `cwd`, `originator` (must start with "codex"), `cli_version`, and
`model_provider`. Subsequent lines are `response_item` entries (messages and
function calls) and periodic `event_msg` token-count updates. This adapter
normalises those into the cross-source `Record` shape declared in
`stackunderflow/adapters/base.py`.

Token accounting quirk: OpenAI embeds cached-input tokens *inside*
`input_tokens`; we subtract them so the normalised `Record.input_tokens`
counts only fresh (uncached) input, matching the Anthropic convention.
Reasoning tokens are billed as output, so they are bundled into
`Record.output_tokens`.
"""

from __future__ import annotations

import json
import logging
import os
from collections.abc import Iterator
from pathlib import Path

from .base import Record, SessionRef

_log = logging.getLogger(__name__)

# Codex tool name -> canonical cross-source tool label. Unknown names pass
# through untouched so new Codex tools remain visible until we classify them.
_TOOL_NAME_MAP = {
    "exec_command": "Bash",
    "read_file": "Read",
    "write_file": "Edit",
    "apply_diff": "Edit",
    "apply_patch": "Edit",
    "spawn_agent": "Agent",
    "close_agent": "Agent",
    "wait_agent": "Agent",
    "read_dir": "Glob",
}

# Files bigger than this trigger a warning but are still parsed.
_LARGE_FILE_BYTES = 64 * 1024 * 1024


class CodexAdapter:
    """Source adapter for OpenAI Codex CLI rollout files."""

    name = "codex"

    def __init__(self, sessions_root: Path | None = None) -> None:
        self._root = sessions_root or (Path.home() / ".codex" / "sessions")

    # ── enumeration ───────────────────────────────────────────────────

    def enumerate(self) -> Iterator[SessionRef]:
        root = self._root
        if not root.is_dir():
            return

        for fp in sorted(root.glob("*/*/*/rollout-*.jsonl")):
            try:
                meta = self._read_session_meta(fp)
            except OSError as exc:
                _log.warning("Cannot open Codex rollout %s: %s", fp, exc)
                continue
            if meta is None:
                continue

            payload = meta["payload"]
            # Originator check is case-insensitive: shipping Codex builds use
            # values like "codex-tui", "codex_cli_rs", and "Codex Desktop".
            # Legacy rollouts (pre-session_meta wrapper) carry no originator,
            # but their location under ~/.codex/sessions/ is enough signal.
            originator = str(payload.get("originator") or "")
            if originator and not originator.lower().startswith("codex"):
                continue

            session_id = str(payload.get("id") or "")
            if not session_id:
                continue

            cwd = payload.get("cwd") or ""
            project_slug = _slug_for(cwd) if cwd else f"codex-{session_id}"

            stat = fp.stat()
            if stat.st_size > _LARGE_FILE_BYTES:
                _log.warning(
                    "Codex rollout %s is %d bytes (>%d); reading anyway",
                    fp, stat.st_size, _LARGE_FILE_BYTES,
                )

            yield SessionRef(
                provider=self.name,
                project_slug=project_slug,
                session_id=session_id,
                file_path=fp,
                file_mtime=stat.st_mtime,
                file_size=stat.st_size,
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
            seq = 0
            # Buffer records emitted since the most recent token_count so we
            # can retroactively attach tokens to the last assistant record
            # in the turn before flushing in original order.
            buffer: list[Record] = []

            for raw_line in fh:
                stripped = raw_line.strip()
                if not stripped:
                    continue
                try:
                    event = json.loads(stripped)
                except (json.JSONDecodeError, ValueError) as exc:
                    _log.debug("Skipping malformed JSON line in %s: %s", ref.file_path, exc)
                    continue

                etype = event.get("type")
                payload = event.get("payload") or {}

                if etype == "response_item":
                    record = self._record_from_response_item(
                        event, payload, ref=ref, seq=seq,
                    )
                    if record is not None:
                        buffer.append(record)
                        seq += 1
                    continue

                if etype == "event_msg" and payload.get("type") == "token_count":
                    info = payload.get("info")
                    if isinstance(info, dict):
                        last = info.get("last_token_usage")
                        if isinstance(last, dict):
                            buffer = _attach_tokens_to_last_assistant(buffer, last)
                    # Flush the completed turn regardless of whether we had
                    # usable token info.
                    yield from buffer
                    buffer = []
                    continue

                # Other event_msg types (task_started, task_complete, error,
                # user_message, etc.) and turn_context events are ignored in
                # Phase 1. session_meta was already consumed during enumerate.

            # End of file: flush any records that never saw a token_count.
            yield from buffer

    # ── internals ─────────────────────────────────────────────────────

    def _read_session_meta(self, fp: Path) -> dict | None:
        """Return the first-line session_meta event (normalised to the modern
        wrapper shape: `{type, timestamp, payload: {...}}`) or None.

        Pre-0.20 Codex rollouts omit the wrapper and inline session metadata
        directly on the root object (`{id, timestamp, instructions, git}`).
        We coerce those into the wrapper shape so downstream enumerate() can
        treat both formats uniformly.
        """
        with fp.open("rb") as fh:
            first_line = fh.readline()
        stripped = first_line.strip()
        if not stripped:
            return None
        try:
            obj = json.loads(stripped)
        except (json.JSONDecodeError, ValueError):
            return None

        if obj.get("type") == "session_meta":
            if not isinstance(obj.get("payload"), dict):
                return None
            return obj

        # Legacy inline shape: accept if it at least carries an `id`.
        if isinstance(obj.get("id"), str):
            return {
                "type": "session_meta",
                "timestamp": obj.get("timestamp", ""),
                "payload": obj,
            }
        return None

    def _record_from_response_item(
        self,
        event: dict,
        payload: dict,
        *,
        ref: SessionRef,
        seq: int,
    ) -> Record | None:
        kind = payload.get("type")
        timestamp = str(event.get("timestamp") or "")

        if kind == "message":
            role = payload.get("role")
            if role not in ("user", "assistant"):
                # Codex also emits "developer" / "system" pseudo-turns for
                # framework messages; skip them to match Claude's conversational
                # filtering.
                return None
            return Record(
                provider=self.name,
                session_id=ref.session_id,
                seq=seq,
                timestamp=timestamp,
                role=role,
                model=None,
                input_tokens=0,
                output_tokens=0,
                cache_create_tokens=0,
                cache_read_tokens=0,
                content_text=_message_text(payload.get("content")),
                tools=(),
                cwd=None,
                is_sidechain=False,
                uuid=f"{ref.session_id}:{seq}",
                parent_uuid=None,
                raw=event,
            )

        if kind == "function_call":
            raw_name = str(payload.get("name") or "")
            if raw_name in ("spawn_agent", "wait_agent", "close_agent"):
                _log.debug(
                    "Codex sub-agent call %s in %s (not expanded in Phase 1)",
                    raw_name, ref.file_path,
                )
            tool_label = _TOOL_NAME_MAP.get(raw_name, raw_name)
            return Record(
                provider=self.name,
                session_id=ref.session_id,
                seq=seq,
                timestamp=timestamp,
                role="assistant",
                model=None,
                input_tokens=0,
                output_tokens=0,
                cache_create_tokens=0,
                cache_read_tokens=0,
                content_text="",
                tools=(tool_label,) if tool_label else (),
                cwd=None,
                is_sidechain=False,
                uuid=f"{ref.session_id}:{seq}",
                parent_uuid=None,
                raw=event,
            )

        return None


# ── helpers ───────────────────────────────────────────────────────────


def _slug_for(project_path: str) -> str:
    """Claude-compatible slug: absolute path, trailing sep stripped, `/` -> `-`,
    leading `-` prepended. Keeps a single project under both adapters aligned."""
    return (
        os.path.abspath(project_path)
        .rstrip(os.sep)
        .replace(os.sep, "-")
        .replace("_", "-")
    )


def _message_text(content: object) -> str:
    """Concatenate every `.text` field across content blocks."""
    if isinstance(content, str):
        return content
    if not isinstance(content, list):
        return ""
    pieces: list[str] = []
    for blk in content:
        if isinstance(blk, dict):
            text = blk.get("text")
            if isinstance(text, str) and text:
                pieces.append(text)
        elif isinstance(blk, str):
            pieces.append(blk)
    return "\n".join(pieces)


def _attach_tokens_to_last_assistant(
    buffer: list[Record],
    last_usage: dict,
) -> list[Record]:
    """Return a new buffer where the most recent assistant Record carries the
    supplied per-turn token usage. Records are frozen, so we rebuild the one
    we want to update via dataclass-style replacement."""
    idx = _last_assistant_index(buffer)
    if idx is None:
        return buffer

    raw_input = int(last_usage.get("input_tokens", 0) or 0)
    cached = int(last_usage.get("cached_input_tokens", 0) or 0)
    raw_output = int(last_usage.get("output_tokens", 0) or 0)
    reasoning = int(last_usage.get("reasoning_output_tokens", 0) or 0)

    target = buffer[idx]
    updated = Record(
        provider=target.provider,
        session_id=target.session_id,
        seq=target.seq,
        timestamp=target.timestamp,
        role=target.role,
        model=target.model,
        input_tokens=max(raw_input - cached, 0),
        output_tokens=raw_output + reasoning,
        cache_create_tokens=0,  # OpenAI does not bill prompt-cache writes.
        cache_read_tokens=cached,
        content_text=target.content_text,
        tools=target.tools,
        cwd=target.cwd,
        is_sidechain=target.is_sidechain,
        uuid=target.uuid,
        parent_uuid=target.parent_uuid,
        raw=target.raw,
    )
    new_buf = list(buffer)
    new_buf[idx] = updated
    return new_buf


def _last_assistant_index(buffer: list[Record]) -> int | None:
    for i in range(len(buffer) - 1, -1, -1):
        rec = buffer[i]
        # Attach tokens to the assistant *message* (text turn), not to a bare
        # function_call record. Text records are the ones with content_text
        # or without a single tool entry; prefer text-bearing records.
        if rec.role == "assistant" and not rec.tools:
            return i
    # Fallback: any assistant record (e.g., turns that were tool-only).
    for i in range(len(buffer) - 1, -1, -1):
        if buffer[i].role == "assistant":
            return i
    return None
