"""Shared streaming reader helpers for JSONL adapters.

Defensive sizing for large session logs:

- ``MAX_SESSION_FILE_BYTES`` (128 MB) — files larger than this are
  **skipped** with a logged warning. The adapter treats it like a
  malformed file: ``read()`` yields nothing, no exception is raised.
  Catching the cap defensively keeps a single rogue 1 GB log from
  OOM'ing the ingest worker.
- ``STREAM_THRESHOLD_BYTES`` (8 MB) — a soft hint for callers; the
  ``iter_jsonl_lines`` helper streams line-by-line either way (Python's
  binary file iteration never slurps the whole file), but we keep the
  threshold available so adapters that *do* call ``read_bytes()`` /
  ``json.load()`` for single-document formats know when to switch to a
  streaming variant.

The helper is **non-invasive**: it preserves the byte-offset ``seq``
contract (``seq`` is the byte position where each line starts) so
existing resume semantics in every JSONL adapter keep working without
modification.
"""

from __future__ import annotations

import logging
from collections.abc import Iterator
from pathlib import Path
from typing import IO

_log = logging.getLogger(__name__)

# Files larger than this are skipped entirely. 128 MB is roughly two
# orders of magnitude over the largest real-world session log we've seen
# (~1 MB Claude project, ~1.6 MB Codex rollout); a file that big is
# almost certainly the result of a runaway logger or a corrupted state
# file, and we'd rather skip it than crash the ingest.
MAX_SESSION_FILE_BYTES: int = 128 * 1024 * 1024

# Files between ``STREAM_THRESHOLD_BYTES`` and ``MAX_SESSION_FILE_BYTES``
# are streamed line-by-line (no in-memory buffer); files smaller than
# the threshold can use either path (line iteration is already
# streaming, so there's no real benefit to slurping). The constant is
# exposed so adapters that handle non-JSONL formats (single-document
# JSON, etc.) can branch on it before deciding to call ``read_bytes()``.
STREAM_THRESHOLD_BYTES: int = 8 * 1024 * 1024


def _file_size_or_skip(path: Path) -> int | None:
    """Return the size of ``path`` if it's safe to read, else ``None``.

    Logs a warning when the file exceeds ``MAX_SESSION_FILE_BYTES``;
    callers should treat ``None`` as "yield nothing, do not raise".
    """
    try:
        size = path.stat().st_size
    except OSError as exc:
        _log.warning("Cannot stat %s: %s", path, exc)
        return None
    if size > MAX_SESSION_FILE_BYTES:
        _log.warning(
            "Skipping %s: size %d bytes exceeds cap %d bytes "
            "(adjust stackunderflow.adapters._streaming.MAX_SESSION_FILE_BYTES "
            "to read it anyway)",
            path,
            size,
            MAX_SESSION_FILE_BYTES,
        )
        return None
    return size


def iter_jsonl_lines(
    path: Path,
    *,
    since_offset: int = 0,
) -> Iterator[tuple[int, bytes]]:
    """Yield ``(line_offset, raw_line)`` tuples from a JSONL file.

    The ``line_offset`` is the byte position where the line started —
    same convention as the byte-offset ``seq`` used by every JSONL
    adapter so resumable reads keep working unchanged.

    Behaviour:

    - File missing / unreadable → log warning, yield nothing.
    - File size > ``MAX_SESSION_FILE_BYTES`` → log warning, yield
      nothing. **Never raises.**
    - File size > ``STREAM_THRESHOLD_BYTES`` → opened in binary mode and
      iterated line-by-line (peak memory is one line, not the file).
    - File size ≤ threshold → same iteration path (Python file
      iteration is streaming regardless), kept on the simple branch for
      clarity.

    The caller is responsible for parsing each ``raw_line`` (the helper
    is format-agnostic: it doesn't strip whitespace or attempt
    ``json.loads`` so adapters can keep their existing parse paths).
    """
    size = _file_size_or_skip(path)
    if size is None:
        return

    try:
        fh: IO[bytes] = path.open("rb")
    except OSError as exc:
        _log.warning("Cannot read %s: %s", path, exc)
        return

    with fh:
        if since_offset > 0:
            try:
                fh.seek(since_offset)
            except OSError as exc:
                _log.warning(
                    "Cannot seek %s to offset %d: %s",
                    path, since_offset, exc,
                )
                return
        offset = since_offset
        for raw_line in fh:
            line_offset = offset
            offset += len(raw_line)
            yield line_offset, raw_line


def stat_or_skip(path: Path) -> int | None:
    """Public entry point: return file size or ``None`` if oversized.

    Useful for adapters that don't iterate JSONL via
    ``iter_jsonl_lines`` (e.g. single-document JSON files read with
    ``json.load`` / ``read_bytes``) but still want the same defensive
    cap. Returns the size on success, ``None`` on error / oversize.
    """
    return _file_size_or_skip(path)
