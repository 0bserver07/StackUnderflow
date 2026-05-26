"""Cursor Agent (transcript files + optional SQLite metadata) session adapter.

Reads agent transcripts that the Cursor Agent CLI writes under
``~/.cursor/projects/{project}/agent-transcripts/``. Two transcript
formats are supported:

- **Legacy text format**: one ``.txt`` file per session, with marker
  lines: ``user:``, ``A:``, ``[Thinking]``, ``[Tool call]``,
  ``[Tool result]``. Group runs of user / assistant text into turns and
  emit one Record per assistant turn.

- **Composer 2 JSONL format**: ``agent-transcripts/{uuid}/*.jsonl`` with
  one JSON object per line: ``{ role, message: { content: [{ type,
  text?, name? }] } }``. Emit one Record per assistant message.

Format auto-detection is by extension: ``.jsonl`` → JSONL; anything else
treated as text.

A separate SQLite attribution DB at
``~/.cursor/ai-tracking/ai-code-tracking.db`` (table
``conversation_summaries`` with columns ``conversationId, model, updatedAt``,
keyed by ``conversationId``) is consulted opportunistically for the
``model`` field. When the DB is missing, the table is missing, or there's
no row for a given session, model is set to ``"cursor-agent"`` and we keep
going.

Tokens are estimated from character length (``len(text) // 4``) — Cursor
Agent doesn't surface explicit counts. Every Record gets stamped
``record.raw["cost_source"] = "estimated"`` so the cost layer / UI can
flag them as approximations.

``source_kind="file"`` (the transcript files are the source of truth; the
SQLite DB is just a metadata enrichment lookup). ``seq`` is the byte
offset of the start of the relevant transcript line / record so resume
works across both formats.

macOS only for v1; Linux paths are present (``~/.cursor/projects/`` is
the same) but untested.
"""

from __future__ import annotations

import hashlib
import json
import logging
import re
import sqlite3
from collections.abc import Iterator
from datetime import UTC, datetime
from pathlib import Path

from .base import Record, SessionRef

_log = logging.getLogger(__name__)


_DEFAULT_PROJECTS_ROOT = Path.home() / ".cursor" / "projects"
_DEFAULT_TRACKING_DB = (
    Path.home() / ".cursor" / "ai-tracking" / "ai-code-tracking.db"
)

# UUID-shaped filename / dirname matcher for session id extraction.
_UUID_RE = re.compile(
    r"^[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-"
    r"[0-9a-fA-F]{4}-[0-9a-fA-F]{12}$"
)

# Project-name prettifier. Strip leading absolute-path prefix tokens
# (e.g. ``-Users-yad-foo``) and trailing ISO-ish timestamps. The default
# is conservative — anything left that looks like a path separator gets
# normalised to a hyphen.
_TIMESTAMP_TAIL_RE = re.compile(r"[-_]?\d{4}-?\d{2}-?\d{2}[Tt _]?\d{2}.*$")
_LEADING_PATH_RE = re.compile(r"^[-_/]+")


class CursorAgentAdapter:
    """Source adapter for Cursor Agent transcripts (text + JSONL hybrid)."""

    name = "cursor-agent"

    def __init__(
        self,
        projects_root: Path | None = None,
        tracking_db: Path | None = None,
    ) -> None:
        # Both paths are overridable so tests can point at synthetic
        # fixtures without touching the user's home directory.
        self._projects_root = (
            Path(projects_root) if projects_root else _DEFAULT_PROJECTS_ROOT
        )
        self._tracking_db = (
            Path(tracking_db) if tracking_db else _DEFAULT_TRACKING_DB
        )

    # ── enumeration ───────────────────────────────────────────────────

    def enumerate(self) -> Iterator[SessionRef]:
        root = self._projects_root
        if not root.is_dir():
            return

        for project_dir in sorted(p for p in root.iterdir() if p.is_dir()):
            transcripts_dir = project_dir / "agent-transcripts"
            if not transcripts_dir.is_dir():
                continue

            project_slug = _prettify_project_name(project_dir.name)

            # Legacy text transcripts: flat .txt files in agent-transcripts/.
            for fp in sorted(transcripts_dir.glob("*.txt")):
                ref = self._build_ref(fp, project_slug)
                if ref is not None:
                    yield ref

            # Composer 2 JSONL transcripts: one subdirectory per session,
            # holding one or more .jsonl files. Codeburn warns there can
            # be multiple .jsonl files per session subdir; we yield one
            # SessionRef per .jsonl file with the subdir UUID as session id.
            for sub in sorted(p for p in transcripts_dir.iterdir() if p.is_dir()):
                for fp in sorted(sub.glob("*.jsonl")):
                    ref = self._build_ref(fp, project_slug, session_dir=sub)
                    if ref is not None:
                        yield ref

    def _build_ref(
        self,
        fp: Path,
        project_slug: str,
        *,
        session_dir: Path | None = None,
    ) -> SessionRef | None:
        try:
            stat = fp.stat()
        except OSError as exc:
            _log.warning("Cannot stat Cursor Agent transcript %s: %s", fp, exc)
            return None

        session_id = _session_id_for(fp, session_dir=session_dir)
        kind = "jsonl" if fp.suffix.lower() == ".jsonl" else "text"

        return SessionRef(
            provider=self.name,
            project_slug=project_slug,
            session_id=session_id,
            file_path=fp,
            file_mtime=stat.st_mtime,
            file_size=stat.st_size,
            source_kind="file",
            source_hint={"format": kind},
        )

    # ── reading ───────────────────────────────────────────────────────

    def read(
        self, ref: SessionRef, *, since_offset: int = 0
    ) -> Iterator[Record]:
        path = ref.file_path
        if not path.is_file():
            _log.warning("Cursor Agent transcript missing at read time: %s", path)
            return

        # Look up model from the attribution DB once per session — saves
        # opening the DB per record.
        model = self._lookup_model(ref.session_id) or "cursor-agent"

        kind = (ref.source_hint or {}).get("format")
        if kind == "jsonl":
            yield from _read_jsonl(
                path,
                ref=ref,
                model=model,
                provider=self.name,
                since_offset=since_offset,
            )
        else:
            yield from _read_text(
                path,
                ref=ref,
                model=model,
                provider=self.name,
                since_offset=since_offset,
            )

    # ── attribution DB ────────────────────────────────────────────────

    def _lookup_model(self, session_id: str) -> str | None:
        """Return the model recorded for ``session_id`` or None.

        The attribution DB is optional — if it's missing or the table /
        columns don't match, log and continue.
        """
        db_path = self._tracking_db
        if not db_path.is_file():
            return None
        try:
            conn = sqlite3.connect(f"file:{db_path}?mode=ro", uri=True)
        except sqlite3.Error as exc:
            _log.warning("Cannot open Cursor Agent tracking DB %s: %s", db_path, exc)
            return None

        try:
            # Codeburn keys conversation_summaries by conversationId.
            cur = conn.execute(
                "SELECT model FROM conversation_summaries "
                "WHERE conversationId = ? LIMIT 1",
                (session_id,),
            )
            row = cur.fetchone()
        except sqlite3.Error as exc:
            _log.debug("conversation_summaries lookup failed: %s", exc)
            return None
        finally:
            conn.close()

        if not row:
            return None
        model = row[0]
        return str(model) if model else None


# ── format readers ────────────────────────────────────────────────────


def _read_jsonl(
    path: Path,
    *,
    ref: SessionRef,
    model: str,
    provider: str,
    since_offset: int,
) -> Iterator[Record]:
    """Read Composer 2 JSONL transcripts, one Record per assistant message."""
    last_user_text = ""
    try:
        with path.open("rb") as fh:
            offset = 0
            while True:
                line = fh.readline()
                if not line:
                    break
                line_offset = offset
                offset += len(line)

                if since_offset and line_offset <= since_offset:
                    # Still update last_user_text so the resume's first
                    # assistant turn attaches the right user prompt.
                    parsed = _safe_jsonl_loads(line)
                    if parsed is not None and parsed.get("role") == "user":
                        last_user_text = _jsonl_message_text(parsed) or last_user_text
                    continue

                parsed = _safe_jsonl_loads(line)
                if parsed is None:
                    continue
                role = parsed.get("role")
                if not isinstance(role, str):
                    continue

                text = _jsonl_message_text(parsed)
                if role == "user":
                    last_user_text = text or last_user_text
                    continue
                if role != "assistant":
                    continue

                tools = _jsonl_message_tools(parsed)
                content_text = text or last_user_text
                input_estimate = max(len(last_user_text) // 4, 0)
                output_estimate = max(len(text) // 4, 0)

                raw_payload = dict(parsed)
                raw_payload["cost_source"] = "estimated"

                yield Record(
                    provider=provider,
                    session_id=ref.session_id,
                    seq=line_offset,
                    timestamp=datetime.now(tz=UTC).isoformat(),
                    role="assistant",
                    model=model,
                    input_tokens=input_estimate,
                    output_tokens=output_estimate,
                    cache_create_tokens=0,
                    cache_read_tokens=0,
                    content_text=content_text,
                    tools=tuple(tools),
                    cwd=None,
                    is_sidechain=False,
                    uuid=f"{ref.session_id}:{line_offset}",
                    parent_uuid=None,
                    raw=raw_payload,
                )
    except OSError as exc:
        _log.warning("Cursor Agent JSONL read failed on %s: %s", path, exc)


def _read_text(
    path: Path,
    *,
    ref: SessionRef,
    model: str,
    provider: str,
    since_offset: int,
) -> Iterator[Record]:
    """Read legacy text transcripts.

    Markers:
      - ``user:``           — start of a user turn
      - ``A:``              — start of an assistant turn
      - ``[Thinking]``      — marks a thinking block (we treat as
        assistant content for token estimation)
      - ``[Tool call]``     — assistant tool invocation; tool name follows
      - ``[Tool result]``   — tool output; informational

    A turn continues until the next marker. Each assistant turn yields
    one Record at the byte offset of its opening marker line.
    """
    try:
        raw = path.read_bytes()
    except OSError as exc:
        _log.warning("Cursor Agent text read failed on %s: %s", path, exc)
        return

    # Use line-by-line streaming so seq == byte offset of the line.
    last_user_text = ""
    current_role: str | None = None
    current_offset: int | None = None
    current_text: list[str] = []
    current_tools: list[str] = []
    offset = 0

    def _emit() -> Record | None:
        # Build one record from the buffered turn.
        if current_role != "assistant" or current_offset is None:
            return None
        text = "\n".join(current_text).strip()
        input_estimate = max(len(last_user_text) // 4, 0)
        output_estimate = max(len(text) // 4, 0)
        return Record(
            provider=provider,
            session_id=ref.session_id,
            seq=current_offset,
            timestamp=datetime.now(tz=UTC).isoformat(),
            role="assistant",
            model=model,
            input_tokens=input_estimate,
            output_tokens=output_estimate,
            cache_create_tokens=0,
            cache_read_tokens=0,
            content_text=text,
            tools=tuple(current_tools),
            cwd=None,
            is_sidechain=False,
            uuid=f"{ref.session_id}:{current_offset}",
            parent_uuid=None,
            raw={"format": "text", "cost_source": "estimated"},
        )

    for line_bytes in raw.splitlines(keepends=True):
        line_offset = offset
        offset += len(line_bytes)
        try:
            line = line_bytes.decode("utf-8", errors="replace").rstrip("\r\n")
        except UnicodeDecodeError:
            continue

        marker = _classify_text_line(line)
        if marker == "user":
            # Emit any pending assistant turn first (only if it would
            # land past the resume floor).
            if current_role == "assistant" and (
                not since_offset or (current_offset or -1) > since_offset
            ):
                rec = _emit()
                if rec is not None:
                    yield rec
            # Reset to user turn.
            current_role = "user"
            current_offset = line_offset
            current_text = [line.removeprefix("user:").strip()]
            current_tools = []
        elif marker == "assistant":
            # Closing previous turn.
            if current_role == "user":
                last_user_text = "\n".join(current_text).strip()
            elif current_role == "assistant" and (
                not since_offset or (current_offset or -1) > since_offset
            ):
                rec = _emit()
                if rec is not None:
                    yield rec
            current_role = "assistant"
            current_offset = line_offset
            current_text = [line.removeprefix("A:").strip()]
            current_tools = []
        elif marker == "thinking":
            if current_role == "assistant":
                current_text.append(line)
        elif marker == "tool_call":
            tool_name = _parse_tool_call_name(line)
            if tool_name and current_role == "assistant":
                current_tools.append(tool_name)
        elif marker == "tool_result":
            # Informational only; keep grouped with current turn.
            if current_role == "assistant":
                current_text.append(line)
        else:
            # Continuation line — append to whichever turn is active.
            if current_role is not None and line:
                current_text.append(line)

    # Flush trailing assistant turn.
    if current_role == "user":
        last_user_text = "\n".join(current_text).strip()
    elif current_role == "assistant" and (
        not since_offset or (current_offset or -1) > since_offset
    ):
        rec = _emit()
        if rec is not None:
            yield rec


# ── helpers ───────────────────────────────────────────────────────────


def _safe_jsonl_loads(line: bytes) -> dict | None:
    """Parse a JSONL line; return None on any failure."""
    if not line:
        return None
    try:
        obj = json.loads(line.decode("utf-8", errors="replace"))
    except (json.JSONDecodeError, ValueError, UnicodeDecodeError):
        return None
    return obj if isinstance(obj, dict) else None


def _jsonl_message_text(parsed: dict) -> str:
    """Pull text from ``message.content[]`` blocks (Composer 2 shape)."""
    msg = parsed.get("message")
    if not isinstance(msg, dict):
        return ""
    content = msg.get("content")
    if isinstance(content, str):
        return content
    if isinstance(content, list):
        pieces: list[str] = []
        for blk in content:
            if isinstance(blk, dict):
                if blk.get("type") == "text":
                    t = blk.get("text")
                    if isinstance(t, str):
                        pieces.append(t)
            elif isinstance(blk, str):
                pieces.append(blk)
        return "\n".join(pieces)
    return ""


def _jsonl_message_tools(parsed: dict) -> list[str]:
    """Pull tool names from ``message.content[]`` of type ``tool_use``."""
    msg = parsed.get("message")
    if not isinstance(msg, dict):
        return []
    content = msg.get("content")
    if not isinstance(content, list):
        return []
    tools: list[str] = []
    for blk in content:
        if not isinstance(blk, dict):
            continue
        if blk.get("type") == "tool_use":
            name = blk.get("name")
            if isinstance(name, str) and name:
                tools.append(name)
    return tools


def _classify_text_line(line: str) -> str | None:
    """Return one of: ``user``, ``assistant``, ``thinking``, ``tool_call``,
    ``tool_result``, or None if the line has no marker."""
    if line.startswith("user:"):
        return "user"
    if line.startswith("A:"):
        return "assistant"
    if line.startswith("[Thinking]"):
        return "thinking"
    if line.startswith("[Tool call]"):
        return "tool_call"
    if line.startswith("[Tool result]"):
        return "tool_result"
    return None


def _parse_tool_call_name(line: str) -> str | None:
    """Extract the tool name from a ``[Tool call]`` line.

    Accepts either ``[Tool call] name`` (just a bare token) or
    ``[Tool call] name args=...`` shapes.
    """
    rest = line.removeprefix("[Tool call]").strip()
    if not rest:
        return None
    # First whitespace-separated token wins.
    name = rest.split(None, 1)[0]
    return name or None


def _session_id_for(fp: Path, *, session_dir: Path | None = None) -> str:
    """Derive a session id from a transcript path.

    For Composer 2 JSONL, prefer the parent directory name when it's a UUID.
    Otherwise fall back to the file stem when it's a UUID, else a SHA1 of
    the absolute path.
    """
    if session_dir is not None:
        name = session_dir.name
        if _UUID_RE.match(name):
            return name
    stem = fp.stem
    if _UUID_RE.match(stem):
        return stem
    return hashlib.sha1(str(fp).encode("utf-8")).hexdigest()


def _prettify_project_name(name: str) -> str:
    """Apply project-name prettification.

    Strips leading path separators and trailing timestamp markers so a
    raw directory name like ``-Users-yad-myproj-2025-04-01T10-30-00``
    becomes ``Users-yad-myproj``. The result is a slug, not a path.
    """
    out = name
    out = _LEADING_PATH_RE.sub("", out)
    out = _TIMESTAMP_TAIL_RE.sub("", out)
    return out or name
