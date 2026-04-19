"""Claude Code session adapter.

Handles two on-disk formats:
1. Modern per-project JSONL files at ~/.claude/projects/<slug>/<uuid>.jsonl
2. Legacy centralised ~/.claude/history.jsonl for projects that pre-date
   the per-project format (directories with only .continuation_cache.json).
"""

from __future__ import annotations

import logging
from pathlib import Path
from typing import Iterable

import orjson

from .base import Record, SessionRef

_log = logging.getLogger(__name__)


class ClaudeAdapter:
    name = "claude"

    def enumerate(self) -> Iterable[SessionRef]:
        root = Path.home() / ".claude" / "projects"
        if not root.is_dir():
            return

        for project_dir in root.iterdir():
            if not project_dir.is_dir():
                continue

            jsonl_files = sorted(project_dir.glob("*.jsonl"))
            if jsonl_files:
                yield from self._refs_from_jsonl(project_dir, jsonl_files)
            elif (project_dir / ".continuation_cache.json").exists():
                yield from self._refs_from_history(project_dir)

    def read(self, ref: SessionRef, *, since_offset: int = 0) -> Iterable[Record]:
        if ref.session_id.startswith("legacy-"):
            yield from self._read_history(ref)
            return
        yield from self._read_jsonl(ref, since_offset=since_offset)

    # ── internals ─────────────────────────────────────────────────────

    def _refs_from_jsonl(self, project_dir: Path, files: list[Path]) -> Iterable[SessionRef]:
        for fp in files:
            stat = fp.stat()
            yield SessionRef(
                provider=self.name,
                project_slug=project_dir.name,
                session_id=fp.stem,
                file_path=fp,
                file_mtime=stat.st_mtime,
                file_size=stat.st_size,
            )

    def _refs_from_history(self, project_dir: Path) -> Iterable[SessionRef]:
        # One synthetic ref per legacy project; all history entries for that
        # project get yielded by read() as one pseudo-session.
        history_file = Path.home() / ".claude" / "history.jsonl"
        if not history_file.is_file():
            return
        stat = history_file.stat()
        yield SessionRef(
            provider=self.name,
            project_slug=project_dir.name,
            session_id=f"legacy-{project_dir.name}",
            file_path=history_file,
            file_mtime=stat.st_mtime,
            file_size=stat.st_size,
        )

    # ── reading modern JSONL ──────────────────────────────────────────

    def _read_jsonl(self, ref: SessionRef, *, since_offset: int) -> Iterable[Record]:
        try:
            fp = ref.file_path.open("rb")
        except OSError as exc:
            _log.warning("Cannot read %s: %s", ref.file_path, exc)
            return
        with fp:
            fp.seek(since_offset)
            offset = since_offset
            for raw_line in fp:
                line_offset = offset
                offset += len(raw_line)
                stripped = raw_line.strip()
                if not stripped:
                    continue
                try:
                    obj = orjson.loads(stripped)
                except (orjson.JSONDecodeError, ValueError):
                    continue
                record = self._parse_line(obj, ref=ref, seq=line_offset)
                if record is not None:
                    yield record

    def _parse_line(self, obj: dict, *, ref: SessionRef, seq: int) -> Record | None:
        msg = obj.get("message") if isinstance(obj.get("message"), dict) else {}
        role = _role_from(obj, msg)
        if role is None:
            return None
        usage = msg.get("usage", {}) if isinstance(msg, dict) else {}
        return Record(
            provider=self.name,
            session_id=obj.get("sessionId") or ref.session_id,
            seq=seq,
            timestamp=str(obj.get("timestamp", "")),
            role=role,
            model=(msg.get("model") if isinstance(msg, dict) else None) or None,
            input_tokens=int(usage.get("input_tokens", 0) or 0),
            output_tokens=int(usage.get("output_tokens", 0) or 0),
            cache_create_tokens=int(usage.get("cache_creation_input_tokens", 0) or 0),
            cache_read_tokens=int(usage.get("cache_read_input_tokens", 0) or 0),
            content_text=_text_from(msg),
            tools=_tools_from(msg),
            cwd=obj.get("cwd") or None,
            is_sidechain=bool(obj.get("isSidechain", False)),
            uuid=obj.get("uuid", ""),
            parent_uuid=obj.get("parentUuid"),
            raw=obj,
        )

    def _read_history(self, ref: SessionRef) -> Iterable[Record]:
        raise NotImplementedError  # task 3.3


def _role_from(obj: dict, msg: dict) -> str | None:
    raw_type = obj.get("type", "")
    if raw_type == "user":
        return "user"
    if raw_type == "assistant":
        return "assistant"
    if raw_type in ("summary", "compact_summary"):
        return None  # not a conversational record
    if isinstance(msg, dict):
        role = msg.get("role")
        if role in ("user", "assistant"):
            return role
    return None


def _text_from(msg: dict) -> str:
    if not isinstance(msg, dict):
        return ""
    body = msg.get("content", "")
    if isinstance(body, str):
        return body
    if not isinstance(body, list):
        return ""
    pieces: list[str] = []
    for blk in body:
        if isinstance(blk, dict) and blk.get("type") == "text":
            pieces.append(blk.get("text", ""))
        elif isinstance(blk, str):
            pieces.append(blk)
    return "\n".join(pieces)


def _tools_from(msg: dict) -> tuple[str, ...]:
    if not isinstance(msg, dict):
        return ()
    body = msg.get("content")
    if not isinstance(body, list):
        return ()
    names: list[str] = []
    for blk in body:
        if isinstance(blk, dict) and blk.get("type") == "tool_use":
            name = blk.get("name", "")
            if name:
                names.append(name)
    return tuple(names)
