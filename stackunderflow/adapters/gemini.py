"""Gemini CLI session adapter.

Reads Gemini CLI session files at::

    ~/.gemini/tmp/{project}/chats/session-*.json
    ~/.gemini/tmp/{project}/chats/session-*.jsonl

Two on-disk formats coexist (codeburn-catalog §7):

* **CLI ≤0.38 (single JSON)** — one top-level JSON object:
  ``{ sessionId, startTime, projectHash?, lastUpdated?, kind?, messages: [...] }``.
  We parse the whole file once and yield one ``Record`` per entry in
  ``messages``.
* **CLI ≥0.39 (JSONL)** — one metadata line followed by one message
  line per entry. We parse line-by-line.

Storage / resumption note (spec §3.2):

This adapter is a **hybrid** — it reads files (so ``source_kind="file"``)
but ``seq`` is the *index in the messages array* for the single-JSON
variant (0, 1, 2, …) and the byte offset of the line start for the
JSONL variant. ``read(ref, since_offset=N)`` therefore means "skip
records at-or-before seq N", not "seek to byte N", for the single-JSON
case. ``test_read_since_offset_is_storage_aware`` only checks
monotonic ``seq`` and that resume yields strictly fewer records past a
midpoint — that contract holds for both variants. This same
non-byte-offset ``seq`` pattern is documented on the Cline adapter.

Message shape (codeburn-catalog §7)::

    {
      id, timestamp,
      type: 'user' | 'gemini' | 'info',
      content: string | [{ text }],
      tokens: { input, output, cached, thoughts, tool, total },
      model,
      toolCalls: [{ id, name, args }],
      thoughts
    }

Token normalization (canonical 4-key shape):

* ``input_tokens`` = ``tokens.input - tokens.cached`` (cached is a
  subset of input; canonical input is the fresh portion only — same
  convention as OpenAI / Qwen).
* ``output_tokens`` = ``tokens.output + tokens.thoughts`` (reasoning
  rolls into the billable output column).
* ``cache_read_tokens`` = ``tokens.cached``.
* ``cache_create_tokens`` = ``0`` (Gemini does not surface cache
  writes).

macOS-only path constants in v1; Windows / Linux are documented at the
constant definitions but ``# untested`` per spec §5.
"""

from __future__ import annotations

import json
import logging
from collections.abc import Iterator
from pathlib import Path

from .base import Record, SessionRef

_log = logging.getLogger(__name__)

# Files bigger than this trigger a warning but are still parsed.
_LARGE_FILE_BYTES = 64 * 1024 * 1024

# Gemini tool name -> canonical cross-source label. Unknown names pass
# through. The bundled Gemini tools are roughly the same set as Qwen.
_TOOL_NAME_MAP = {
    "shell": "Bash",
    "execute": "Bash",
    "run_shell_command": "Bash",
    "read_file": "Read",
    "edit_file": "Edit",
    "write_file": "Edit",
    "replace": "Edit",
    "list_directory": "Glob",
    "glob": "Glob",
    "grep": "Grep",
    "search_file_content": "Grep",
}

_DEFAULT_MODEL = "gemini-auto"

_GEMINI_ROOT_MACOS = Path.home() / ".gemini" / "tmp"
# _GEMINI_ROOT_WINDOWS  # untested
# _GEMINI_ROOT_LINUX    # untested


class GeminiAdapter:
    """Source adapter for the Google Gemini CLI."""

    name = "gemini"

    def __init__(self, projects_root: Path | None = None) -> None:
        # ``projects_root`` is overridable so tests can point at a
        # synthetic ``tmp_path / 'tmp'`` tree without monkey-patching.
        self._root = projects_root if projects_root is not None else _GEMINI_ROOT_MACOS

    # ── enumeration ───────────────────────────────────────────────────

    def enumerate(self) -> Iterator[SessionRef]:
        root = self._root
        if not root.is_dir():
            return

        for project_dir in sorted(p for p in root.iterdir() if p.is_dir()):
            chats_dir = project_dir / "chats"
            if not chats_dir.is_dir():
                continue
            # Both ``session-*.json`` and ``session-*.jsonl`` may live
            # in the same directory; format is decided per-file in
            # ``read()`` from the file extension.
            files = sorted(
                list(chats_dir.glob("session-*.json"))
                + list(chats_dir.glob("session-*.jsonl"))
            )
            for fp in files:
                try:
                    stat = fp.stat()
                except OSError as exc:
                    _log.warning("Cannot stat Gemini chat %s: %s", fp, exc)
                    continue

                if stat.st_size > _LARGE_FILE_BYTES:
                    _log.warning(
                        "Gemini chat %s is %d bytes (>%d); reading anyway",
                        fp, stat.st_size, _LARGE_FILE_BYTES,
                    )

                yield SessionRef(
                    provider=self.name,
                    project_slug=project_dir.name,
                    session_id=fp.stem,  # finalised in ``read()`` from sessionId
                    file_path=fp,
                    file_mtime=stat.st_mtime,
                    file_size=stat.st_size,
                    source_kind="file",
                    source_hint={"format": _format_for(fp)},
                )

    # ── reading ───────────────────────────────────────────────────────

    def read(self, ref: SessionRef, *, since_offset: int = 0) -> Iterator[Record]:
        fmt = _format_for(ref.file_path)
        if fmt == "jsonl":
            yield from self._read_jsonl(ref, since_offset=since_offset)
        else:
            yield from self._read_single_json(ref, since_offset=since_offset)

    def _read_single_json(
        self, ref: SessionRef, *, since_offset: int
    ) -> Iterator[Record]:
        try:
            raw = ref.file_path.read_bytes()
        except OSError as exc:
            _log.warning("Cannot read Gemini chat %s: %s", ref.file_path, exc)
            return
        try:
            doc = json.loads(raw)
        except (json.JSONDecodeError, ValueError) as exc:
            _log.warning("Malformed Gemini JSON in %s: %s", ref.file_path, exc)
            return
        if not isinstance(doc, dict):
            return

        session_id = str(doc.get("sessionId") or ref.session_id)
        messages = doc.get("messages")
        if not isinstance(messages, list):
            return

        for idx, msg in enumerate(messages):
            # ``seq`` is the message index for single-JSON files;
            # ``since_offset`` is therefore "the highest index already
            # seen". Yield strictly past it — same semantics as Cline.
            if since_offset > 0 and idx <= since_offset:
                continue
            if not isinstance(msg, dict):
                continue
            record = self._record_from_message(
                msg, ref=ref, seq=idx, session_id=session_id,
            )
            if record is not None:
                yield record

    def _read_jsonl(self, ref: SessionRef, *, since_offset: int) -> Iterator[Record]:
        try:
            fh = ref.file_path.open("rb")
        except OSError as exc:
            _log.warning("Cannot read Gemini chat %s: %s", ref.file_path, exc)
            return

        with fh:
            fh.seek(since_offset)
            offset = since_offset
            session_id = ref.session_id

            for raw_line in fh:
                line_offset = offset
                offset += len(raw_line)
                if since_offset > 0 and line_offset <= since_offset:
                    continue
                stripped = raw_line.strip()
                if not stripped:
                    continue
                try:
                    entry = json.loads(stripped)
                except (json.JSONDecodeError, ValueError) as exc:
                    _log.debug(
                        "Skipping malformed Gemini JSONL line in %s: %s",
                        ref.file_path, exc,
                    )
                    continue
                if not isinstance(entry, dict):
                    continue

                # Metadata line in the ≥0.39 format carries
                # ``sessionId`` but no message ``type``. Capture the id
                # and skip — it's not a record, but it does refine the
                # session id we attach to subsequent records.
                etype = entry.get("type")
                if etype not in ("user", "gemini", "info"):
                    sid = entry.get("sessionId")
                    if isinstance(sid, str) and sid:
                        session_id = sid
                    continue

                record = self._record_from_message(
                    entry, ref=ref, seq=line_offset, session_id=session_id,
                )
                if record is not None:
                    yield record

    # ── internals ─────────────────────────────────────────────────────

    def _record_from_message(
        self,
        msg: dict,
        *,
        ref: SessionRef,
        seq: int,
        session_id: str,
    ) -> Record | None:
        mtype = msg.get("type")
        if mtype == "user":
            role = "user"
        elif mtype == "gemini":
            role = "assistant"
        else:
            # 'info' entries are framework chrome (model_change,
            # session_start, etc.) — not conversational records. Skip
            # to match the Claude adapter's filter on summary entries.
            return None

        text = _text_from_content(msg.get("content"))
        tools = _tools_from_message(msg)

        tokens = _normalize_tokens(msg.get("tokens"))

        timestamp = str(msg.get("timestamp") or "")
        model = msg.get("model") or (_DEFAULT_MODEL if role == "assistant" else None)
        uuid = str(msg.get("id") or f"{session_id}:{seq}")

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
            raw=msg,
        )


# ── helpers ───────────────────────────────────────────────────────────


def _format_for(path: Path) -> str:
    """Decide the on-disk format for ``path``.

    Extension is the cheap, reliable signal (.json → single doc;
    .jsonl → line-delimited). We don't sniff the contents — the
    Gemini CLI writes a stable extension per format.
    """
    if path.suffix.lower() == ".jsonl":
        return "jsonl"
    return "single_json"


def _text_from_content(content: object) -> str:
    """Flatten Gemini's ``content`` (string or list of ``{text}`` blocks)
    into one string."""
    if isinstance(content, str):
        return content
    if isinstance(content, list):
        pieces: list[str] = []
        for blk in content:
            if isinstance(blk, dict):
                t = blk.get("text")
                if isinstance(t, str):
                    pieces.append(t)
            elif isinstance(blk, str):
                pieces.append(blk)
        return "\n".join(pieces)
    return ""


def _tools_from_message(msg: dict) -> tuple[str, ...]:
    """Extract tool names from ``toolCalls``.

    Unknown names pass through so new Gemini tools remain visible —
    same convention as the Codex / Qwen adapters.
    """
    calls = msg.get("toolCalls")
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


def _normalize_tokens(tokens: object) -> dict[str, int]:
    """Flatten Gemini's ``tokens`` block into the canonical 4-key shape.

    See the module docstring for the rule. Missing / malformed fields
    default to 0 so a partial message still produces a valid Record.
    """
    if not isinstance(tokens, dict):
        return {"input": 0, "output": 0, "cache_creation": 0, "cache_read": 0}

    raw_in = _safe_int(tokens.get("input"))
    raw_out = _safe_int(tokens.get("output"))
    cached = _safe_int(tokens.get("cached"))
    thoughts = _safe_int(tokens.get("thoughts"))

    return {
        "input": max(raw_in - cached, 0),
        "output": raw_out + thoughts,
        "cache_creation": 0,
        "cache_read": cached,
    }


def _safe_int(value: object) -> int:
    if value is None or value == "":
        return 0
    try:
        out = int(value)
    except (TypeError, ValueError):
        return 0
    return max(out, 0)
