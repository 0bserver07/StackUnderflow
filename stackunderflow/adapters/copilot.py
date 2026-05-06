"""GitHub Copilot session adapter.

Reads conversation data the GitHub Copilot CLI / VS Code chat extension
writes in two distinct on-disk formats:

1. **Legacy** — ``~/.copilot/session-state/{sessionId}/events.jsonl``
   Each line is one event with ``type`` in:
       ``session.model_change`` — sets the model for subsequent turns
       ``user.message``        — content from the user
       ``assistant.message``   — content + token usage + tool calls
   The ``sessionId`` is the directory name. The optional companion file
   ``workspace.yaml`` (or ``workspace.json``) at the same level carries
   the project's ``cwd``.

2. **VS Code transcript** —
   ``~/Library/Application Support/Code/User/workspaceStorage/{hash}/GitHub.copilot-chat/transcripts/*.jsonl``
   First line is ``{ "type": "session.start", "data": { "producer": "copilot-agent", ... } }``;
   subsequent lines are ``user.message`` / ``assistant.message`` events
   in the same shape as legacy. The transcript filename (sans ``.jsonl``)
   is the ``sessionId``; the parent ``workspaceStorage/{hash}/`` provides
   the workspace UUID used for the project slug.

Records: one ``Record`` per ``assistant.message`` event with non-zero
output tokens (or estimated tokens when output is missing).

Token strategy:
  - When an event has explicit ``outputTokens > 0`` we use it as-is.
  - Otherwise we fall back to ``len(text) // 4`` for output tokens and
    stamp ``record.raw["cost_source"] = "estimated"`` so the cost layer
    knows to mark the row in the dashboard.
  - Input tokens always default to the most recently observed
    ``user.message`` text length / 4 unless the assistant event carries
    an explicit ``inputTokens`` count.

Model inference: the catalog notes that tool-call IDs leak the upstream
provider — ``toolu_bdrk_...`` ids come from Anthropic Bedrock and ``call_...``
ids come from OpenAI. When an assistant event has no explicit ``model`` but
*does* carry tool calls, we read the first id's prefix and synthesise a
canonical-shape string (``claude-auto`` or ``gpt-auto``) so the
``CopilotPricer`` can route to the right delegate. Without either signal we
fall back to ``copilot-auto`` (no rate available).

``source_kind="file"``; ``seq`` is the byte offset of the start of the line
that produced the record so resumable reads pick up where the previous
ingest left off.

macOS only for v1 — Linux / Windows path constants are present in the
module but not exercised by ``enumerate()`` (see spec §5).

Defensive sizing: JSONL transcripts / events files larger than
``MAX_SESSION_FILE_BYTES`` (128 MB; see
``stackunderflow/adapters/_streaming.py``) are **skipped with a logged
warning** rather than parsed. Smaller files stream line-by-line.
"""

from __future__ import annotations

import json
import logging
import os
import re
import sys
from collections.abc import Iterator
from datetime import UTC, datetime
from pathlib import Path
from typing import Any

from ._streaming import iter_jsonl_lines, stat_or_skip
from .base import Record, SessionRef

_log = logging.getLogger(__name__)


# ── path constants ────────────────────────────────────────────────────

# Legacy CLI: ~/.copilot/session-state/{sessionId}/events.jsonl
_LEGACY_ROOT = Path.home() / ".copilot" / "session-state"

# VS Code transcript: workspaceStorage/{hash}/GitHub.copilot-chat/transcripts/*.jsonl
_VSCODE_WORKSPACE_STORAGE_MACOS = (
    Path.home() / "Library" / "Application Support" / "Code" / "User" / "workspaceStorage"
)
# untested
_VSCODE_WORKSPACE_STORAGE_LINUX = (
    Path.home() / ".config" / "Code" / "User" / "workspaceStorage"
)
# untested
_VSCODE_WORKSPACE_STORAGE_WINDOWS = (
    Path(os.environ.get("APPDATA", "")) / "Code" / "User" / "workspaceStorage"
)

# Subpath inside each workspaceStorage/{hash}/ directory.
_COPILOT_CHAT_SUBDIR = Path("GitHub.copilot-chat") / "transcripts"


def _default_vscode_workspace_storage() -> Path:
    """Return the platform-appropriate workspaceStorage root."""
    if sys.platform == "darwin":
        return _VSCODE_WORKSPACE_STORAGE_MACOS
    if sys.platform.startswith("linux"):
        return _VSCODE_WORKSPACE_STORAGE_LINUX
    if sys.platform.startswith("win"):
        return _VSCODE_WORKSPACE_STORAGE_WINDOWS
    return _VSCODE_WORKSPACE_STORAGE_MACOS


# ── tool-call-id model inference ──────────────────────────────────────

# `toolu_bdrk_*` is Anthropic-Bedrock; `toolu_*` is the bare Anthropic
# tool-use id. Both indicate a Claude-family model.
_TOOLU_PREFIX_RE = re.compile(r"^toolu(?:_bdrk)?_", re.IGNORECASE)
# `call_*` is OpenAI's tool-call id shape.
_CALL_PREFIX_RE = re.compile(r"^call_", re.IGNORECASE)


def _infer_model_from_tool_calls(tool_calls: object) -> str | None:
    """Return ``"claude-auto"`` / ``"gpt-auto"`` from the first tool-call id.

    Returns ``None`` when the structure is missing or no recognisable
    prefix is present. The synthesised name is intentionally vendor-
    prefixed so ``CopilotPricer.canonicalize`` can route via the
    ``claude-`` / ``gpt-`` heuristic.
    """
    if not isinstance(tool_calls, list):
        return None
    for tc in tool_calls:
        if not isinstance(tc, dict):
            continue
        tc_id = tc.get("id") or tc.get("toolCallId")
        if not isinstance(tc_id, str) or not tc_id:
            continue
        if _TOOLU_PREFIX_RE.match(tc_id):
            return "claude-auto"
        if _CALL_PREFIX_RE.match(tc_id):
            return "gpt-auto"
    return None


# ── adapter ───────────────────────────────────────────────────────────


class CopilotAdapter:
    """Source adapter for GitHub Copilot session JSONL.

    Both the legacy ``~/.copilot/session-state/`` layout and the newer
    VS Code transcript layout are enumerated by a single adapter — they
    share the same per-line event shape, so one ``read()`` implementation
    handles both.
    """

    name = "copilot"

    def __init__(
        self,
        *,
        legacy_root: Path | None = None,
        vscode_workspace_storage: Path | None = None,
    ) -> None:
        self._legacy_root = legacy_root or _LEGACY_ROOT
        self._vscode_root = (
            vscode_workspace_storage or _default_vscode_workspace_storage()
        )

    # ── enumeration ───────────────────────────────────────────────────

    def enumerate(self) -> Iterator[SessionRef]:
        yield from self._enumerate_legacy()
        yield from self._enumerate_vscode_transcripts()

    def _enumerate_legacy(self) -> Iterator[SessionRef]:
        root = self._legacy_root
        if not root.is_dir():
            return
        for session_dir in sorted(p for p in root.iterdir() if p.is_dir()):
            events = session_dir / "events.jsonl"
            if not events.is_file():
                continue
            try:
                stat = events.stat()
            except OSError as exc:
                _log.warning("Cannot stat Copilot session %s: %s", events, exc)
                continue
            project_slug = _legacy_project_slug(session_dir)
            yield SessionRef(
                provider=self.name,
                project_slug=project_slug,
                session_id=session_dir.name,
                file_path=events,
                file_mtime=stat.st_mtime,
                file_size=stat.st_size,
                source_kind="file",
                source_hint={"format": "legacy"},
            )

    def _enumerate_vscode_transcripts(self) -> Iterator[SessionRef]:
        root = self._vscode_root
        if not root.is_dir():
            return
        for workspace_dir in sorted(p for p in root.iterdir() if p.is_dir()):
            transcripts_dir = workspace_dir / _COPILOT_CHAT_SUBDIR
            if not transcripts_dir.is_dir():
                continue
            workspace_hash = workspace_dir.name
            for jsonl in sorted(transcripts_dir.glob("*.jsonl")):
                try:
                    stat = jsonl.stat()
                except OSError as exc:
                    _log.warning("Cannot stat Copilot transcript %s: %s", jsonl, exc)
                    continue
                yield SessionRef(
                    provider=self.name,
                    # Workspace UUID is the only project signal we have for
                    # the VS Code format — fall through to "copilot" if the
                    # hash dir name is empty (defensive; shouldn't happen).
                    project_slug=f"copilot-vscode/{workspace_hash}" or "copilot",
                    session_id=jsonl.stem,
                    file_path=jsonl,
                    file_mtime=stat.st_mtime,
                    file_size=stat.st_size,
                    source_kind="file",
                    source_hint={
                        "format": "vscode-transcript",
                        "workspace_hash": workspace_hash,
                    },
                )

    # ── reading ───────────────────────────────────────────────────────

    def read(
        self, ref: SessionRef, *, since_offset: int = 0
    ) -> Iterator[Record]:
        """Stream records out of one ``events.jsonl`` / transcript file.

        ``seq`` is the byte offset of the start of the originating line —
        same convention as the Claude / Codex JSONL adapters — so a caller
        can pass ``since_offset=last_seq`` to resume past the last record
        ingested.
        """
        path = ref.file_path
        if not path.is_file():
            _log.warning("Copilot session file missing at read time: %s", path)
            return
        # Defensive 128 MB cap. ``iter_jsonl_lines`` would re-stat below,
        # but checking up-front keeps the behaviour explicit alongside
        # the ``is_file`` guard.
        if stat_or_skip(path) is None:
            return

        # Track the "current model" from session.model_change events and the
        # most recent user message so assistant events can attach both to
        # their record.
        current_model: str | None = None
        last_user_text: str = ""

        for line_offset, line in iter_jsonl_lines(path, since_offset=since_offset):
            if since_offset > 0 and line_offset <= since_offset:
                # Caller already saw the record at exactly ``since_offset``;
                # skip duplicates. Matches the convention used by every
                # other JSONL adapter in this package.
                continue
            if not line.strip():
                continue
            event = _safe_loads_line(line, path=path)
            if event is None:
                continue
            etype = event.get("type")

            if etype == "session.model_change":
                # Update the rolling model. Don't yield a record.
                candidate = _extract_model(event)
                if candidate:
                    current_model = candidate
                continue

            if etype == "session.start":
                # Transcript header — capture model if present, but
                # don't emit a record.
                candidate = _extract_model(event)
                if candidate:
                    current_model = candidate
                continue

            if etype == "user.message":
                text = _extract_text(event)
                if text:
                    last_user_text = text
                continue

            if etype != "assistant.message":
                continue

            text = _extract_text(event)
            out_tokens, out_estimated = _output_tokens_for(event, text)
            in_tokens, in_estimated = _input_tokens_for(
                event, last_user_text=last_user_text
            )

            # codeburn says "records: one per assistant.message with
            # outputTokens > 0". We extend that: if the event has no
            # explicit count, we estimate from text length and only
            # skip when both the explicit value AND the estimate are
            # zero (purely empty assistant turn).
            if out_tokens <= 0:
                continue

            tool_calls_field = event.get("toolCalls")
            if not isinstance(tool_calls_field, list):
                data_envelope = event.get("data")
                if isinstance(data_envelope, dict):
                    tool_calls_field = data_envelope.get("toolCalls")
            # Priority order:
            #   1. ``model`` field explicitly on this event (most specific).
            #   2. Rolling ``current_model`` from a previous
            #      ``session.model_change`` / ``session.start`` — this is
            #      the session's *declared* model, which is more reliable
            #      than a tool-call-id heuristic.
            #   3. Tool-call-id prefix inference (``toolu_...`` →
            #      Anthropic family, ``call_...`` → OpenAI). Last-resort
            #      heuristic; only kicks in when the session never
            #      declared a model at all.
            #   4. ``copilot-auto`` literal as a final default.
            #
            # Earlier code put tool-call-id inference *above* current_model.
            # That dropped a fully-qualified id like
            # ``claude-sonnet-4-5-20250929`` (declared in model_change)
            # back to the family-only ``claude-auto`` whenever the next
            # turn happened to call a tool, losing model granularity in
            # the marts. The drift was caught by Wave 5's beta-normalizer
            # validation — see ``docs/beta-normalizer-drift.md``.
            model = (
                _extract_model(event)
                or current_model
                or _infer_model_from_tool_calls(tool_calls_field)
                or "copilot-auto"
            )
            # Bind the inference into rolling state so subsequent
            # turns without their own model field stay coherent.
            current_model = model

            timestamp = _extract_timestamp(event)
            raw_payload: dict[str, Any] = dict(event)
            if out_estimated or in_estimated:
                raw_payload["cost_source"] = "estimated"

            yield Record(
                provider=self.name,
                session_id=ref.session_id,
                seq=line_offset,
                timestamp=timestamp,
                role="assistant",
                model=model,
                input_tokens=in_tokens,
                output_tokens=out_tokens,
                cache_create_tokens=0,
                cache_read_tokens=0,
                content_text=text,
                tools=_extract_tool_names(event),
                cwd=None,
                is_sidechain=False,
                uuid=f"{ref.session_id}:{line_offset}",
                parent_uuid=None,
                raw=raw_payload,
            )


# ── helpers ───────────────────────────────────────────────────────────


def _safe_loads_line(line: bytes, *, path: Path) -> dict | None:
    """Parse one JSONL line into a dict; tolerate malformed entries."""
    try:
        obj = json.loads(line)
    except (json.JSONDecodeError, ValueError) as exc:
        _log.warning("Malformed JSON line in %s: %s", path, exc)
        return None
    return obj if isinstance(obj, dict) else None


def _extract_text(event: dict) -> str:
    """Pull message text out of either a flat ``content`` or a ``data`` envelope."""
    # Newer transcript format wraps the payload in ``data``.
    data = event.get("data")
    if isinstance(data, dict):
        candidate = data.get("content") or data.get("text")
        if isinstance(candidate, str):
            return candidate
        if isinstance(candidate, list):
            return _flatten_content_blocks(candidate)
    # Legacy / flat shape.
    candidate = event.get("content") or event.get("text") or event.get("message")
    if isinstance(candidate, str):
        return candidate
    if isinstance(candidate, list):
        return _flatten_content_blocks(candidate)
    if isinstance(candidate, dict):
        nested = candidate.get("content") or candidate.get("text")
        if isinstance(nested, str):
            return nested
    return ""


def _flatten_content_blocks(blocks: list) -> str:
    pieces: list[str] = []
    for blk in blocks:
        if isinstance(blk, dict):
            t = blk.get("text") or blk.get("content")
            if isinstance(t, str) and t:
                pieces.append(t)
        elif isinstance(blk, str):
            pieces.append(blk)
    return "\n".join(pieces)


def _extract_model(event: dict) -> str | None:
    """Return the explicit model id on this event, if any."""
    for key in ("model", "modelName", "modelId"):
        v = event.get(key)
        if isinstance(v, str) and v:
            return v
    data = event.get("data")
    if isinstance(data, dict):
        for key in ("model", "modelName", "modelId"):
            v = data.get(key)
            if isinstance(v, str) and v:
                return v
    return None


def _output_tokens_for(event: dict, text: str) -> tuple[int, bool]:
    """Return ``(output_tokens, estimated)`` for this assistant event."""
    explicit = _coerce_int(event.get("outputTokens"))
    if explicit > 0:
        return explicit, False
    data = event.get("data")
    if isinstance(data, dict):
        explicit = _coerce_int(data.get("outputTokens"))
        if explicit > 0:
            return explicit, False
    # Fall back to length / 4. If text is empty the caller will still see
    # zero and skip the record (that's the correct behaviour for an empty
    # assistant turn — these are filtered out).
    return max(len(text) // 4, 0), True


def _input_tokens_for(event: dict, *, last_user_text: str) -> tuple[int, bool]:
    """Return ``(input_tokens, estimated)`` for this assistant event.

    Prefer an explicit ``inputTokens`` on the event itself; otherwise
    estimate from the most recent ``user.message`` text. The estimated
    flag is what bubbles up to ``raw["cost_source"]``.
    """
    explicit = _coerce_int(event.get("inputTokens"))
    if explicit > 0:
        return explicit, False
    data = event.get("data")
    if isinstance(data, dict):
        explicit = _coerce_int(data.get("inputTokens"))
        if explicit > 0:
            return explicit, False
    return max(len(last_user_text) // 4, 0), True


def _coerce_int(v: object) -> int:
    if v is None:
        return 0
    try:
        return max(int(v), 0)
    except (TypeError, ValueError):
        return 0


def _extract_tool_names(event: dict) -> tuple[str, ...]:
    """Best-effort tool-name list off the assistant event."""
    raw = event.get("toolCalls")
    if not isinstance(raw, list):
        data = event.get("data")
        raw = data.get("toolCalls") if isinstance(data, dict) else None
    if not isinstance(raw, list):
        return ()
    names: list[str] = []
    for tc in raw:
        if not isinstance(tc, dict):
            continue
        n = tc.get("name") or tc.get("toolName")
        if isinstance(n, str) and n:
            names.append(n)
    return tuple(names)


def _extract_timestamp(event: dict) -> str:
    """Return an ISO 8601 UTC timestamp; fall back to "now" on miss."""
    for key in ("timestamp", "ts", "createdAt"):
        v = event.get(key)
        iso = _coerce_iso(v)
        if iso:
            return iso
    data = event.get("data")
    if isinstance(data, dict):
        for key in ("timestamp", "ts", "createdAt"):
            iso = _coerce_iso(data.get(key))
            if iso:
                return iso
    # Last resort — record the read time. Adapters are expected to emit
    # parseable ISO strings (contract test).
    return datetime.now(tz=UTC).isoformat()


def _coerce_iso(v: object) -> str | None:
    if v is None or v == "":
        return None
    if isinstance(v, (int, float)):
        try:
            # Heuristic: values > 10^12 are ms-epoch.
            if v > 1e12:
                return datetime.fromtimestamp(float(v) / 1000.0, tz=UTC).isoformat()
            return datetime.fromtimestamp(float(v), tz=UTC).isoformat()
        except (OverflowError, OSError, ValueError):
            return None
    if isinstance(v, str):
        s = v.strip()
        if not s:
            return None
        try:
            dt = datetime.fromisoformat(s.replace("Z", "+00:00"))
            if dt.tzinfo is None:
                dt = dt.replace(tzinfo=UTC)
            return dt.isoformat()
        except ValueError:
            return None
    return None


def _legacy_project_slug(session_dir: Path) -> str:
    """Read ``workspace.yaml`` / ``workspace.json`` for a cwd; fall back to ``"copilot"``.

    YAML support is intentionally rudimentary — we only look for a
    top-level ``cwd:`` line so we don't pull in PyYAML for one field.
    """
    for name in ("workspace.json", "workspace.yaml", "workspace.yml"):
        candidate = session_dir / name
        if not candidate.is_file():
            continue
        try:
            text = candidate.read_text(encoding="utf-8", errors="replace")
        except OSError:
            continue
        if name.endswith(".json"):
            try:
                obj = json.loads(text)
            except (json.JSONDecodeError, ValueError):
                continue
            if isinstance(obj, dict):
                cwd = obj.get("cwd")
                if isinstance(cwd, str) and cwd:
                    return _slugify_cwd(cwd)
        else:
            for line in text.splitlines():
                stripped = line.strip()
                if stripped.startswith("cwd:"):
                    cwd = stripped[len("cwd:") :].strip().strip("\"'")
                    if cwd:
                        return _slugify_cwd(cwd)
    # No workspace file — every legacy session lives under one logical
    # project.
    return "copilot"


def _slugify_cwd(cwd: str) -> str:
    """Match the Claude-style ``-Users-foo-bar`` slug shape."""
    return cwd.replace("/", "-").strip("-") or "copilot"
