"""Droid (Factory) session adapter.

Reads sessions the Droid CLI writes under ``$FACTORY_DIR`` (or ``~/.factory/``
by default). Each project gets its own hash subdirectory containing one or
more JSONL session files plus a companion ``.settings.json`` that carries
the session-level token usage.

On-disk layout::

    {factoryDir}/sessions/{projectHash}/{sessionId}.jsonl
    {factoryDir}/sessions/{projectHash}/{sessionId}.settings.json

JSONL lines look like::

    {"type": "session_start", "id": "...", "timestamp": "...", "cwd": "..."}
    {"type": "message", "id": "...", "timestamp": "...",
     "message": {"role": "assistant", "content": [...]}}

Settings shape::

    {"model": "claude-3-5-sonnet",
     "tokenUsage": {"inputTokens": ..., "outputTokens": ...,
                    "cacheCreationTokens": ..., "cacheReadTokens": ...,
                    "thinkingTokens": ...}}

**Quirk (token distribution)**: Droid only tracks token usage at the
session level — there is no per-message usage. We take the simpler path:
distribute the session totals **evenly** across detected assistant
messages. With N assistant messages, each gets ``total // N`` and the
last one absorbs the remainder so the sum still matches. If there are
zero assistant messages we drop the totals on the floor — pricing a
record that doesn't exist would be invented data. See
``_distribute_session_tokens`` for the math.

Storage / resume note: same byte-offset semantics as Codex/Claude (spec
§3.2). ``seq`` is the byte offset of the JSONL line start. Since token
distribution depends on the *full* assistant message count, partial
reads via ``since_offset`` skip records but the *distribution* is
recomputed from the full file each call — pricing stays stable across
resumes.

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


def _factory_root() -> Path:
    """Return the configured Factory base dir.

    Honors ``$FACTORY_DIR`` exactly (Droid's own convention); falls back
    to ``~/.factory/`` when unset or empty.
    """
    env = os.environ.get("FACTORY_DIR", "").strip()
    if env:
        return Path(env).expanduser()
    return Path.home() / ".factory"


class DroidAdapter:
    """Source adapter for Droid (Factory) CLI sessions."""

    name = "droid"

    def __init__(self, sessions_root: Path | None = None) -> None:
        # ``sessions_root`` is the ``sessions/`` subdir directly — tests
        # and callers can override without monkey-patching the env var.
        self._root = sessions_root or (_factory_root() / "sessions")

    # ── enumeration ───────────────────────────────────────────────────

    def enumerate(self) -> Iterator[SessionRef]:
        root = self._root
        if not root.is_dir():
            # Not installed / never used — clean no-op rather than raise.
            return

        for project_dir in sorted(p for p in root.iterdir() if p.is_dir()):
            for fp in sorted(project_dir.glob("*.jsonl")):
                try:
                    stat = fp.stat()
                except OSError as exc:
                    _log.warning("Cannot stat Droid session %s: %s", fp, exc)
                    continue

                session_id, cwd = _read_session_meta(fp)
                if not session_id:
                    session_id = fp.stem
                project_slug = _slug_for(cwd) if cwd else project_dir.name

                yield SessionRef(
                    provider=self.name,
                    project_slug=project_slug,
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

        # Side-car settings file. ``foo.jsonl`` -> ``foo.settings.json``.
        settings_path = ref.file_path.with_suffix(".settings.json")
        model, totals = _load_settings(settings_path)

        # Pre-pass: count assistant messages to compute per-record share.
        # Cheap (one pass over JSONL) and gives stable distribution that
        # doesn't depend on resume offsets.
        assistant_count = _count_assistant_messages(ref.file_path)
        per_record = _distribute_session_tokens(totals, assistant_count)

        # Track which assistant message we're emitting so the *last* one
        # gets the leftover remainder (keeps sum == totals).
        assistant_idx = 0

        with fh:
            fh.seek(since_offset)
            offset = since_offset
            for raw_line in fh:
                line_offset = offset
                offset += len(raw_line)
                if since_offset > 0 and line_offset <= since_offset:
                    # Caller already saw this record; still need to count
                    # assistant messages we skip so the *remaining* records
                    # get the right slice of the distributed totals.
                    if _line_is_assistant_message(raw_line):
                        assistant_idx += 1
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

                if etype == "session_start":
                    # Already consumed for SessionRef; nothing to emit.
                    continue

                if etype != "message":
                    continue

                message = event.get("message") or {}
                role = message.get("role")
                if role not in ("user", "assistant"):
                    continue

                tokens = (
                    per_record[assistant_idx]
                    if role == "assistant"
                    and assistant_idx < len(per_record)
                    else _ZERO_TOKENS
                )
                if role == "assistant":
                    assistant_idx += 1

                timestamp = str(event.get("timestamp") or "")
                cwd = event.get("cwd") or None
                content = message.get("content")

                yield Record(
                    provider=self.name,
                    session_id=ref.session_id,
                    seq=line_offset,
                    timestamp=timestamp,
                    role=role,
                    model=model,
                    input_tokens=tokens["input"],
                    output_tokens=tokens["output"],
                    cache_create_tokens=tokens["cache_creation"],
                    cache_read_tokens=tokens["cache_read"],
                    content_text=_message_text(content),
                    tools=_tools_from_content(content),
                    cwd=cwd,
                    is_sidechain=False,
                    uuid=str(event.get("id") or f"{ref.session_id}:{line_offset}"),
                    parent_uuid=None,
                    raw=event,
                )


# ── helpers ───────────────────────────────────────────────────────────


_ZERO_TOKENS: dict[str, int] = {
    "input": 0, "output": 0, "cache_creation": 0, "cache_read": 0,
}


def _slug_for(project_path: str) -> str:
    """Claude-compatible slug for cross-adapter project alignment."""
    return (
        os.path.abspath(project_path)
        .rstrip(os.sep)
        .replace(os.sep, "-")
        .replace("_", "-")
    )


def _read_session_meta(fp: Path) -> tuple[str, str]:
    """Return ``(session_id, cwd)`` from the first line, both possibly empty."""
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
    if obj.get("type") != "session_start":
        return "", ""
    return str(obj.get("id") or ""), str(obj.get("cwd") or "")


def _load_settings(path: Path) -> tuple[str | None, dict[str, int]]:
    """Read the ``.settings.json`` companion. Missing file → empty totals.

    Returns ``(model, totals_dict)`` where totals carries canonical 4-key
    shape (``input``/``output``/``cache_creation``/``cache_read``).
    Anthropic-style ``cacheCreationTokens`` and ``cacheReadTokens`` map
    directly. Thinking tokens (Anthropic extended-thinking) fold into
    ``output`` so cost matches Anthropic billing.
    """
    if not path.is_file():
        return None, dict(_ZERO_TOKENS)
    try:
        with path.open("rb") as fh:
            obj = json.load(fh)
    except (OSError, json.JSONDecodeError, ValueError) as exc:
        _log.warning("Cannot read Droid settings %s: %s", path, exc)
        return None, dict(_ZERO_TOKENS)
    if not isinstance(obj, dict):
        return None, dict(_ZERO_TOKENS)

    model = obj.get("model")
    model = str(model) if isinstance(model, str) and model else None

    usage = obj.get("tokenUsage") or {}
    if not isinstance(usage, dict):
        usage = {}
    inp = _safe_int(usage.get("inputTokens"))
    out = _safe_int(usage.get("outputTokens"))
    cw = _safe_int(usage.get("cacheCreationTokens"))
    cr = _safe_int(usage.get("cacheReadTokens"))
    thinking = _safe_int(usage.get("thinkingTokens"))
    return model, {
        "input": inp,
        "output": out + thinking,
        "cache_creation": cw,
        "cache_read": cr,
    }


def _safe_int(val: object) -> int:
    try:
        return max(int(val or 0), 0)
    except (TypeError, ValueError):
        return 0


def _count_assistant_messages(fp: Path) -> int:
    """One pass to count assistant messages; used for token distribution."""
    n = 0
    try:
        with fp.open("rb") as fh:
            for line in fh:
                if _line_is_assistant_message(line):
                    n += 1
    except OSError:
        return 0
    return n


def _line_is_assistant_message(raw: bytes) -> bool:
    stripped = raw.strip()
    if not stripped:
        return False
    try:
        obj = json.loads(stripped)
    except (json.JSONDecodeError, ValueError):
        return False
    if obj.get("type") != "message":
        return False
    message = obj.get("message")
    if not isinstance(message, dict):
        return False
    return message.get("role") == "assistant"


def _distribute_session_tokens(
    totals: dict[str, int], n_assistant: int
) -> list[dict[str, int]]:
    """Spread session totals evenly across N assistant messages.

    Even split with the remainder absorbed by the last record so the sum
    still equals the totals (no rounding drift). With ``n_assistant == 0``
    we return an empty list — pricing a record that doesn't exist would
    be invented data.
    """
    if n_assistant <= 0:
        return []
    out: list[dict[str, int]] = []
    keys = ("input", "output", "cache_creation", "cache_read")
    bases = {k: totals.get(k, 0) // n_assistant for k in keys}
    rems = {k: totals.get(k, 0) - bases[k] * n_assistant for k in keys}
    for i in range(n_assistant):
        rec = dict(bases)
        if i == n_assistant - 1:
            for k in keys:
                rec[k] += rems[k]
        out.append(rec)
    return out


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
    """Pluck tool names from ``tool_use`` content blocks."""
    if not isinstance(content, list):
        return ()
    tools: list[str] = []
    for blk in content:
        if not isinstance(blk, dict):
            continue
        if blk.get("type") == "tool_use":
            name = blk.get("name")
            if isinstance(name, str) and name:
                tools.append(name)
    return tuple(tools)
