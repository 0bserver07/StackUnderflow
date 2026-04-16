"""Read legacy session data from the centralized ``~/.claude/history.jsonl``.

Before ~January 2026, Claude Code stored only user prompts in a single
``history.jsonl`` file rather than per-project JSONL conversation logs.
These entries contain the prompt text, timestamp, and project path — but
**no** token counts, model info, or assistant responses.

This module parses that file and converts entries into the same
:class:`~stackunderflow.pipeline.reader.RawEntry` format the rest of the
pipeline expects, so old projects appear in the dashboard with whatever
metadata is available.
"""

from __future__ import annotations

import logging
import os
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

import orjson

from .reader import RawEntry

_log = logging.getLogger(__name__)

_HISTORY_FILE = Path.home() / ".claude" / "history.jsonl"

# Entries within this gap (seconds) that share a project and have no
# sessionId are grouped into the same synthetic session.
_SESSION_GAP_SECONDS = 2 * 60 * 60  # 2 hours

# Cache parsed results so repeated calls don't re-read the file.
_cache: dict[str, list[RawEntry]] | None = None


def _path_to_slug(project_path: str) -> str:
    """Convert a project path to the directory slug Claude Code uses.

    Claude replaces ``/`` *and* ``_`` with ``-``.
    """
    return (
        os.path.abspath(project_path)
        .rstrip(os.sep)
        .replace(os.sep, "-")
        .replace("_", "-")
    )


def _build_index() -> dict[str, list[dict]]:
    """Read history.jsonl once and group raw dicts by project slug."""
    if not _HISTORY_FILE.is_file():
        return {}

    by_slug: dict[str, list[dict]] = {}
    try:
        raw = _HISTORY_FILE.read_bytes()
    except OSError as exc:
        _log.warning("Cannot read %s: %s", _HISTORY_FILE, exc)
        return {}

    for line in raw.split(b"\n"):
        stripped = line.strip()
        if not stripped:
            continue
        try:
            obj: dict = orjson.loads(stripped)
        except (orjson.JSONDecodeError, ValueError):
            continue

        project = obj.get("project")
        if not project:
            continue

        slug = _path_to_slug(project)
        by_slug.setdefault(slug, []).append(obj)

    return by_slug


def _assign_sessions(entries: list[dict]) -> list[tuple[dict, str]]:
    """Assign a session ID to each entry.

    Entries that already have a ``sessionId`` keep it.  Others are grouped
    by time proximity: consecutive entries within *_SESSION_GAP_SECONDS*
    of each other get the same synthetic session ID.
    """
    entries.sort(key=lambda e: e.get("timestamp", 0))
    results: list[tuple[dict, str]] = []
    synth_counter = 0
    prev_ts = 0

    for entry in entries:
        sid = entry.get("sessionId")
        if sid:
            results.append((entry, sid))
            prev_ts = entry.get("timestamp", 0)
            continue

        ts = entry.get("timestamp", 0)
        if ts - prev_ts > _SESSION_GAP_SECONDS * 1000:  # timestamps are ms
            synth_counter += 1

        synth_id = f"legacy-{synth_counter:04d}"
        results.append((entry, synth_id))
        prev_ts = ts

    return results


def _epoch_ms_to_iso(ts_ms: int) -> str:
    """Convert epoch milliseconds to ISO 8601 string."""
    return datetime.fromtimestamp(ts_ms / 1000, tz=timezone.utc).isoformat()


def _to_raw_entry(entry: dict, session_id: str) -> RawEntry:
    """Convert a history.jsonl dict into a RawEntry the pipeline understands."""
    display = entry.get("display", "")
    ts_ms = entry.get("timestamp", 0)
    ts_iso = _epoch_ms_to_iso(ts_ms) if ts_ms else ""

    # Build a payload that looks like a standard user message
    payload: dict[str, Any] = {
        "type": "user",
        "message": {
            "role": "user",
            "content": [{"type": "text", "text": display}],
        },
        "timestamp": ts_iso,
        "sessionId": session_id,
        "source": "history",  # flag for legacy data
    }

    return RawEntry(payload=payload, session_id=session_id, origin="history")


def entries_for_slug(slug: str) -> list[RawEntry]:
    """Return RawEntry objects for a project identified by its directory slug.

    Results are cached after the first call that triggers a full parse.
    """
    global _cache
    if _cache is None:
        index = _build_index()
        _cache = {}
        for s, raw_entries in index.items():
            assigned = _assign_sessions(raw_entries)
            _cache[s] = [_to_raw_entry(e, sid) for e, sid in assigned]

    return _cache.get(slug, [])


def known_slugs() -> set[str]:
    """Return the set of project slugs that have entries in history.jsonl."""
    global _cache
    if _cache is None:
        entries_for_slug("")  # triggers full parse
    assert _cache is not None
    return set(_cache.keys())


def clear_cache() -> None:
    """Drop the in-memory index (useful for tests)."""
    global _cache
    _cache = None
