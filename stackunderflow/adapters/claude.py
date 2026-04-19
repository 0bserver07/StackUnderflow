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
        raise NotImplementedError  # task 3.2

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
