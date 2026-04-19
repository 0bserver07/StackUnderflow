"""Locate Claude Code project logs on the local filesystem.

Claude Code writes JSONL conversation logs into per-project directories
under ``~/.claude/projects/``.  The directory name is derived from the
project's absolute path by replacing path separators with hyphens.

This module resolves project paths to their log directories and provides
inventory functions for listing all known projects.
"""

from __future__ import annotations

import logging
import os
from collections.abc import Iterator
from dataclasses import dataclass
from pathlib import Path

_log = logging.getLogger(__name__)


@dataclass(frozen=True, slots=True)
class ProjectInfo:
    dir_name: str
    log_path: str
    file_count: int
    total_size_mb: float
    last_modified: float
    first_seen: float
    display_name: str

    def as_dict(self) -> dict:
        return {
            "dir_name": self.dir_name,
            "log_path": self.log_path,
            "file_count": self.file_count,
            "total_size_mb": self.total_size_mb,
            "last_modified": self.last_modified,
            "first_seen": self.first_seen,
            "display_name": self.display_name,
        }


def _base_dir() -> Path:
    return Path.home() / ".claude" / "projects"


# ── path resolution ──────────────────────────────────────────────────────────

def _project_path_to_slug(project_dir: str) -> str:
    """Convert an absolute project path to the slug Claude Code uses.

    Claude replaces ``/`` and ``_`` with ``-``, producing e.g.
    ``/Users/me/code/my_app`` → ``-Users-me-code-my-app``.
    """
    return (
        os.path.abspath(project_dir)
        .rstrip(os.sep)
        .replace(os.sep, "-")
        .replace("_", "-")
    )


def _candidate_dirs(slug: str) -> list[str]:
    """Return possible directory names for a slug (handles format variations)."""
    candidates = [slug]
    # older Claude versions sometimes omitted the leading separator
    stripped = slug.lstrip("-")
    if stripped != slug:
        candidates.append(stripped)
    return candidates


def locate_logs(project_dir: str) -> str | None:
    """Given a real project directory, find its Claude log directory."""
    root = _base_dir()
    if not root.exists():
        return None
    slug = _project_path_to_slug(project_dir)
    for name in _candidate_dirs(slug):
        target = root / name
        if target.is_dir() and (_contains_logs(target) or _is_legacy_project(target)):
            return str(target)
    return None


def _contains_logs(d: Path) -> bool:
    """True if directory has at least one .jsonl file (non-recursive)."""
    return any(True for _ in d.glob("*.jsonl"))


def _is_legacy_project(d: Path) -> bool:
    """True if directory is an old-format project (no JSONL, but has cache marker)."""
    return (d / ".continuation_cache.json").exists() and not _contains_logs(d)


# ── inventory ────────────────────────────────────────────────────────────────

def enumerate_projects() -> list[tuple[str, str]]:
    root = _base_dir()
    if not root.exists():
        return []
    out: list[tuple[str, str]] = []
    for child in root.iterdir():
        if child.is_dir() and (_contains_logs(child) or _is_legacy_project(child)):
            out.append((_humanise(child.name), str(child)))
    return out


def _scan_projects() -> Iterator[ProjectInfo]:
    """Yield metadata for every project without reading log content."""
    root = _base_dir()
    if not root.exists():
        return
    for child in root.iterdir():
        if not child.is_dir():
            continue
        logs = list(child.glob("*.jsonl"))
        if logs:
            stat_results = [f.stat() for f in logs]
            byte_total = sum(s.st_size for s in stat_results)
            mod_times = [s.st_mtime for s in stat_results]
            yield ProjectInfo(
                dir_name=child.name,
                log_path=str(child),
                file_count=len(logs),
                total_size_mb=round(byte_total / (1 << 20), 2),
                last_modified=max(mod_times),
                first_seen=min(mod_times),
                display_name=child.name,
            )
        elif _is_legacy_project(child):
            yield _legacy_project_info(child)


def project_metadata() -> list[dict]:
    try:
        return [p.as_dict() for p in _scan_projects()]
    except Exception as exc:
        _log.info("Error scanning projects: %s", exc)
        return []


def check_project(project_dir: str) -> tuple[bool, str]:
    if not project_dir:
        return False, "Project path cannot be empty"
    p = Path(project_dir)
    if not p.exists():
        return False, f"Path does not exist: {project_dir}"
    if not p.is_dir():
        return False, f"Not a directory: {project_dir}"
    lp = locate_logs(project_dir)
    if lp is None:
        return False, f"No Claude logs for: {project_dir}"
    return True, f"Logs at: {lp}"


def _legacy_project_info(child: Path) -> ProjectInfo:
    st = child.stat()
    return ProjectInfo(
        dir_name=child.name,
        log_path=str(child),
        file_count=0,
        total_size_mb=0.0,
        last_modified=st.st_mtime,
        first_seen=st.st_mtime,
        display_name=child.name,
    )


# ── helpers ──────────────────────────────────────────────────────────────────

def _humanise(slug: str) -> str:
    """Best-effort conversion of a slug back to a readable path."""
    if slug.startswith("-"):
        return os.sep + slug[1:].replace("-", os.sep)
    return slug.replace("-", os.sep)
