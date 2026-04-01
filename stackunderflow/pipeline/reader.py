"""Read Claude Code JSONL log files into raw entry dicts.

Discovers session files at multiple depths:
- ``<project>/<uuid>.jsonl`` — main sessions
- ``<project>/agent-<hash>.jsonl`` — top-level sub-agent sessions
- ``<project>/<uuid>/subagents/agent-<hash>.jsonl`` — nested sub-agents
"""

from __future__ import annotations

import logging
from pathlib import Path
from typing import NamedTuple

import orjson

_log = logging.getLogger(__name__)


class RawEntry(NamedTuple):
    """One line from a JSONL file, lightly annotated."""
    payload: dict
    session_id: str
    origin: str        # source filename (stem)


def scan(log_dir: str) -> list[RawEntry]:
    """Read every ``*.jsonl`` file under *log_dir* (recursively) and return raw entries."""
    base = Path(log_dir)
    if not base.is_dir():
        _log.warning("Log directory does not exist: %s", log_dir)
        return []

    # Collect all JSONL files at any depth
    files = sorted(base.rglob("*.jsonl"), key=lambda p: p.name)
    continuations = _detect_continuations(files, base)

    entries: list[RawEntry] = []
    for fp in files:
        session = continuations.get(str(fp), fp.stem)
        entries.extend(_read_file(fp, session))

    return entries


# ── internals ────────────────────────────────────────────────────────────────

def _read_file(fp: Path, session_id: str) -> list[RawEntry]:
    """Parse a single JSONL file into raw entries."""
    results: list[RawEntry] = []
    try:
        raw_bytes = fp.read_bytes()
    except OSError as exc:
        _log.warning("Cannot read %s: %s", fp, exc)
        return results

    for line in raw_bytes.split(b"\n"):
        stripped = line.strip()
        if not stripped:
            continue
        try:
            obj: dict = orjson.loads(stripped)
        except (orjson.JSONDecodeError, ValueError):
            continue

        # inject session_id from filename if not already in the data
        if "sessionId" not in obj:
            obj["sessionId"] = session_id

        results.append(RawEntry(payload=obj, session_id=session_id, origin=fp.stem))

    return results


def _read_session_id(fp: Path) -> str | None:
    """Read the sessionId from the first JSON line of a file."""
    try:
        with fp.open("rb") as f:
            for raw_line in f:
                stripped = raw_line.strip()
                if not stripped:
                    continue
                try:
                    obj = orjson.loads(stripped)
                    return obj.get("sessionId")
                except (orjson.JSONDecodeError, ValueError):
                    return None
    except OSError:
        return None
    return None


def _detect_continuations(files: list[Path], base: Path) -> dict[str, str]:
    """Map file paths to their canonical session ID.

    - Root-level UUID files are their own session
    - Root-level agent-* files are their own session
    - Nested ``<uuid>/subagents/agent-*.jsonl`` files belong to the parent <uuid> session
    """
    mapping: dict[str, str] = {}

    # Separate root-level from nested
    root_files = [f for f in files if f.parent == base]
    nested_files = [f for f in files if f.parent != base]

    # Root-level: group by sessionId from the first line of each file
    if len(root_files) >= 2:
        groups: dict[str, str] = {}  # sessionId → canonical stem
        for fp in root_files:
            sid = _read_session_id(fp)
            if sid is None:
                continue
            if sid not in groups:
                groups[sid] = fp.stem
            else:
                mapping[str(fp)] = groups[sid]

    # Nested: <session-uuid>/subagents/agent-*.jsonl → parent session uuid
    for fp in nested_files:
        # Walk up to find the session UUID directory
        # Structure: base / <uuid> / subagents / agent-xxx.jsonl
        rel = fp.relative_to(base)
        parts = rel.parts
        if len(parts) >= 2:
            # First part should be the parent session UUID
            parent_session = parts[0]
            mapping[str(fp)] = parent_session

    return mapping
