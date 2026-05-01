"""Fingerprint cache for Cursor's vscdb parse output.

Cursor's ``state.vscdb`` is a single SQLite file that grows monotonically;
on a developer's machine it can hit 1+ GB. Every cold start of
StackUnderflow re-parses the whole DB even when nothing changed. This
module persists the parsed record stream keyed by a cheap ``(mtime, size)``
fingerprint so an unchanged DB skips the parse entirely.

Cache file layout — ``~/.stackunderflow/cache/cursor-results.json``:

    {
      "version": 1,
      "entries": {
        "<absolute_db_path>": {
          "fingerprint": {"mtime": 1730..., "size": 1234567},
          "records": [ <Record-as-dict>, ... ]
        }
      }
    }

The cache is opt-IN-by-default (always on). Single-writer is assumed —
one StackUnderflow server at a time, so no file locking. Any read or
parse failure is treated as a cache miss and falls through to the live
SQLite parse so the cache can never break ingest.

Failure modes silently fall back to "miss":
  - cache file does not exist
  - cache file is unparseable JSON
  - schema version mismatch (top-level ``version != 1``)
  - per-entry shape mismatch (missing keys, wrong types)
  - fingerprint mismatch (mtime or size differs by any amount)

See ``stackunderflow/adapters/cursor.py`` for the call sites.
"""

from __future__ import annotations

import json
import logging
from pathlib import Path
from typing import Any

from stackunderflow.adapters.base import Record

_log = logging.getLogger(__name__)

# Cache schema version. Bump when the on-disk shape changes
# incompatibly — older payloads are then discarded silently.
_CACHE_VERSION = 1

# A parsed Cursor record in cache form: the kwargs of ``Record(...)``
# after ``dataclasses.asdict``. Lists round-trip through JSON, so
# ``tools`` is a list on disk and gets coerced back to a tuple on load.
ParsedRecord = dict[str, Any]


def _default_cache_path() -> Path:
    """Return the on-disk cache file path."""
    return Path.home() / ".stackunderflow" / "cache" / "cursor-results.json"


def fingerprint(db_path: Path) -> tuple[str, float, int]:
    """Identify a vscdb state by ``(absolute_path, mtime, size)``.

    Returned tuple is what we compare against the cached fingerprint.
    A 1-byte size delta or a stat-bumped mtime invalidates the cache.
    """
    p = Path(db_path)
    abs_path = str(p.resolve())
    try:
        st = p.stat()
    except OSError:
        # Caller is responsible for handling missing files; we still
        # return a tuple so the API stays uniform.
        return abs_path, 0.0, 0
    return abs_path, float(st.st_mtime), int(st.st_size)


def _load_raw_cache(cache_path: Path) -> dict | None:
    """Read+parse the cache file. Returns ``None`` on any failure."""
    if not cache_path.is_file():
        return None
    try:
        text = cache_path.read_text(encoding="utf-8")
    except OSError as exc:
        _log.debug("cursor_cache: cannot read %s: %s", cache_path, exc)
        return None
    try:
        data = json.loads(text)
    except json.JSONDecodeError as exc:
        _log.debug("cursor_cache: corrupt JSON at %s: %s", cache_path, exc)
        return None
    if not isinstance(data, dict):
        return None
    if data.get("version") != _CACHE_VERSION:
        _log.debug(
            "cursor_cache: schema version mismatch (got %r, want %r) at %s",
            data.get("version"),
            _CACHE_VERSION,
            cache_path,
        )
        return None
    if not isinstance(data.get("entries"), dict):
        return None
    return data


def _record_dict_from_storage(payload: dict) -> ParsedRecord:
    """Coerce a JSON-decoded record back into ``Record(**kwargs)`` shape.

    JSON has no tuple type, so ``tools`` round-trips as a list and we
    must coerce it back. Everything else is JSON-native already.
    """
    coerced = dict(payload)
    tools = coerced.get("tools")
    if isinstance(tools, list):
        coerced["tools"] = tuple(tools)
    return coerced


def load_cached(
    db_path: Path,
    *,
    cache_path: Path | None = None,
) -> list[Record] | None:
    """Return cached ``Record`` instances if the fingerprint matches.

    Returns ``None`` on any miss (missing cache, stale fingerprint,
    corrupt file, schema mismatch). The caller falls back to a live
    parse and then calls :func:`save_cached` to refresh the entry.
    """
    cache_file = cache_path or _default_cache_path()
    data = _load_raw_cache(cache_file)
    if data is None:
        return None

    abs_path, current_mtime, current_size = fingerprint(db_path)
    entry = data["entries"].get(abs_path)
    if not isinstance(entry, dict):
        return None

    fp = entry.get("fingerprint")
    if not isinstance(fp, dict):
        return None
    try:
        cached_mtime = float(fp.get("mtime"))
        cached_size = int(fp.get("size"))
    except (TypeError, ValueError):
        return None

    # Strict equality: any mtime / size delta invalidates. We don't
    # apply tolerance — if the DB changed at all, we re-parse.
    if cached_mtime != current_mtime or cached_size != current_size:
        return None

    raw_records = entry.get("records")
    if not isinstance(raw_records, list):
        return None

    records: list[Record] = []
    for payload in raw_records:
        if not isinstance(payload, dict):
            return None
        coerced = _record_dict_from_storage(payload)
        try:
            records.append(Record(**coerced))
        except TypeError as exc:
            # Field shape changed in code but cache is from an older
            # release — treat as a miss so we re-parse.
            _log.debug(
                "cursor_cache: record shape mismatch in %s: %s",
                cache_file,
                exc,
            )
            return None
    return records


def save_cached(
    db_path: Path,
    records: list[Record],
    *,
    cache_path: Path | None = None,
) -> None:
    """Persist parse output keyed by the current fingerprint.

    Writes atomically: stages to ``<file>.tmp`` then renames. Any
    write error is logged at debug level and swallowed — the cache is
    a perf optimization, never a correctness dependency.
    """
    from dataclasses import asdict

    cache_file = cache_path or _default_cache_path()
    cache_file.parent.mkdir(parents=True, exist_ok=True)

    abs_path, mtime, size = fingerprint(db_path)
    if size == 0 and mtime == 0.0:
        # Stat failed — don't cache a bogus entry.
        return

    # Start from whatever's on disk so multiple DBs (e.g. the user
    # ran with --vscdb-path pointing at a backup) coexist.
    existing = _load_raw_cache(cache_file)
    if existing is None:
        existing = {"version": _CACHE_VERSION, "entries": {}}

    existing["entries"][abs_path] = {
        "fingerprint": {"mtime": mtime, "size": size},
        "records": [asdict(r) for r in records],
    }

    tmp = cache_file.with_suffix(cache_file.suffix + ".tmp")
    try:
        tmp.write_text(
            json.dumps(existing, separators=(",", ":")),
            encoding="utf-8",
        )
        tmp.replace(cache_file)
    except OSError as exc:
        _log.debug("cursor_cache: cannot write %s: %s", cache_file, exc)
        # Best effort: clean up a half-written staging file.
        try:
            tmp.unlink(missing_ok=True)
        except OSError:
            pass


def clear_cache(*, cache_path: Path | None = None) -> bool:
    """Delete the cache file. Returns ``True`` if a file was removed."""
    cache_file = cache_path or _default_cache_path()
    if not cache_file.exists():
        return False
    try:
        cache_file.unlink()
        return True
    except OSError as exc:
        _log.debug("cursor_cache: cannot remove %s: %s", cache_file, exc)
        return False


__all__ = [
    "ParsedRecord",
    "clear_cache",
    "fingerprint",
    "load_cached",
    "save_cached",
]
