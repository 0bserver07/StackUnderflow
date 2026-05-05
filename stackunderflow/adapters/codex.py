"""OpenAI Codex session adapter.

Reads Codex CLI rollout files at ~/.codex/sessions/YYYY/MM/DD/
rollout-YYYY-MM-DDTHH-MM-SS-<uuid>.jsonl.

Each rollout is JSONL; the first line is a `session_meta` event that carries
the `id`, `cwd`, `originator` (must start with "codex"), `cli_version`, and
`model_provider`. Subsequent lines are `response_item` entries (messages and
function calls) and periodic `event_msg` token-count updates. This adapter
normalises those into the cross-source `Record` shape declared in
`stackunderflow/adapters/base.py`.

Token shape: this adapter emits the *raw* OpenAI shape — cached input
tokens are kept inside `input_tokens` and reasoning tokens stay separate
from `output_tokens`. The flattening to canonical shape (subtracting
cached, folding reasoning into output) lives in
`infra/providers/openai.py:OpenAIPricer.normalize_tokens()` so all OpenAI
providers share one normalization seam. See multi-provider spec §1.5 / §2.

Defensive sizing: JSONL rollouts larger than ``MAX_SESSION_FILE_BYTES``
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

    def watch_paths(self) -> list[Path]:
        """Return ``~/.codex/sessions`` (the rollout JSONL root) for the
        ETL watcher. The ``Wave 2C`` watcher in
        ``stackunderflow/etl/watcher.py`` filters non-existent roots, so
        a fresh machine without Codex installed silently contributes
        nothing.
        """
        return [self._root]

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
        # Buffer records emitted since the most recent token_count so we
        # can retroactively attach tokens to the last assistant record
        # in the turn before flushing in original order.
        buffer: list[Record] = []

        # ``iter_jsonl_lines`` enforces the 128 MB defensive cap and
        # streams line-by-line; rollouts above the cap are skipped with
        # a warning rather than parsed.
        for line_offset, raw_line in iter_jsonl_lines(
            ref.file_path, since_offset=since_offset,
        ):
            # `since_offset == 0` means "fresh read, yield everything".
            # Otherwise, the caller already saw the record at exactly
            # `since_offset`, so skip it.
            if since_offset > 0 and line_offset <= since_offset:
                continue
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
                # seq = byte offset where this line started. Aligns with
                # the Claude adapter so the storage-aware contract test
                # ("resume from seq=midpoint") works for both providers.
                record = self._record_from_response_item(
                    event, payload, ref=ref, seq=line_offset,
                )
                if record is not None:
                    buffer.append(record)
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
    """Return a new buffer where the most recent assistant Record carries
    the supplied per-turn token usage.

    The 4-slot ``Record`` shape (input / output / cache_create /
    cache_read) is fixed; we flatten OpenAI's raw shape into it here so
    every downstream consumer of ``Record.*_tokens`` (DB columns, cache
    stats, model-mix dashboards) sees the same convention as Anthropic
    records: ``input_tokens`` excludes cached, ``output_tokens`` is the
    fully-billable output (including reasoning), ``cache_create_tokens``
    is 0 (OpenAI doesn't bill writes), and ``cache_read_tokens`` carries
    cached input.

    The same flattening is also implemented as
    ``OpenAIPricer.normalize_tokens()`` so callers that *do* hand it raw
    OpenAI shape (a future API-level integration, or a re-test fixture)
    get the identical canonical numbers. Cost-equivalence with the
    pre-refactor adapter is verified by
    ``tests/stackunderflow/infra/providers/test_codex_cost_equivalence.py``.
    Records are frozen, so we rebuild the slot we want to update via
    dataclass-style replacement.
    """
    idx = _last_assistant_index(buffer)
    if idx is None:
        return buffer

    target = buffer[idx]
    canonical = _canonicalize_openai_usage(last_usage)
    updated = Record(
        provider=target.provider,
        session_id=target.session_id,
        seq=target.seq,
        timestamp=target.timestamp,
        role=target.role,
        model=target.model,
        input_tokens=canonical["input"],
        output_tokens=canonical["output"],
        cache_create_tokens=canonical["cache_creation"],
        cache_read_tokens=canonical["cache_read"],
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


def _canonicalize_openai_usage(raw: dict) -> dict[str, int]:
    """Single seam shared with ``OpenAIPricer.normalize_tokens``.

    Imported lazily so the adapter module remains free of provider-pricer
    dependencies at import time.
    """
    from stackunderflow.infra.providers.openai import OpenAIPricer
    return OpenAIPricer().normalize_tokens(raw)


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
