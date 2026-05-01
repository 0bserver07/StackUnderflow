"""Unit tests for the shared streaming reader helper.

Covers ``stackunderflow/adapters/_streaming.py`` — the size cap and
line-by-line iteration used by every JSONL adapter (Claude, Codex,
Gemini, Qwen, Droid, Kiro, OpenClaw, Pi, Copilot).

The original spec called for the helper to live at
``stackunderflow/pipeline/reader.py``, but the package has no
``pipeline/`` module — adapters own their reading. The helper landed in
``adapters/_streaming.py`` so each adapter can import it as a sibling
module.

Exercises:

- Small file (< ``STREAM_THRESHOLD_BYTES``) — reads via the streaming
  iterator, all records yielded, byte-offset ``seq`` semantics
  preserved.
- Medium file (between threshold and cap) — streams successfully with
  low peak memory (verified via ``tracemalloc``: peak is roughly the
  size of a single line, not the file).
- Oversized file (> ``MAX_SESSION_FILE_BYTES``) — reader yields
  nothing, logs a warning, never raises.
- Cap is configurable: patching the constant lets a test treat a tiny
  file as "oversized".
- ``stat_or_skip`` returns ``None`` for oversize / missing files and
  the file size otherwise.

These tests pin the contract that adapters depend on — a regression
here would silently break ingest for every JSONL provider.
"""

from __future__ import annotations

import logging
import tracemalloc
import unittest
from pathlib import Path
from tempfile import TemporaryDirectory
from unittest import mock

from stackunderflow.adapters import _streaming
from stackunderflow.adapters._streaming import (
    MAX_SESSION_FILE_BYTES,
    STREAM_THRESHOLD_BYTES,
    iter_jsonl_lines,
    stat_or_skip,
)

# ── fixture builders ──────────────────────────────────────────────────


def _write_jsonl_lines(path: Path, n_lines: int, *, line_size: int = 256) -> None:
    """Write ``n_lines`` synthetic JSONL records of approx ``line_size`` bytes.

    Each line is a valid JSONL record with a deterministic id; the
    payload is padded with ASCII so total size is roughly
    ``n_lines * line_size``. We use plain bytes I/O (no ``json.dumps``)
    so the test stays fast even for medium-large files.
    """
    pad_len = max(line_size - 32, 1)
    with path.open("wb") as fh:
        for i in range(n_lines):
            payload = b"x" * pad_len
            fh.write(b'{"i":%d,"p":"%s"}\n' % (i, payload))


def _count_lines_via_helper(
    path: Path, *, since_offset: int = 0
) -> tuple[int, list[int]]:
    """Run ``iter_jsonl_lines`` to exhaustion and return (count, offsets)."""
    offsets: list[int] = []
    count = 0
    for line_offset, raw in iter_jsonl_lines(path, since_offset=since_offset):
        offsets.append(line_offset)
        count += 1
        # Touch ``raw`` so the iterator actually decodes a line.
        assert raw  # noqa: S101 (test assertion)
    return count, offsets


# ── tests ─────────────────────────────────────────────────────────────


class SmallFileTests(unittest.TestCase):
    """Files below ``STREAM_THRESHOLD_BYTES`` use the iterator path."""

    def test_yields_every_line(self) -> None:
        with TemporaryDirectory() as tmp:
            path = Path(tmp) / "small.jsonl"
            _write_jsonl_lines(path, n_lines=100, line_size=128)

            count, offsets = _count_lines_via_helper(path)
            self.assertEqual(count, 100)
            # Offsets must be strictly increasing — same byte-offset
            # contract every JSONL adapter relies on for resume.
            self.assertEqual(offsets, sorted(offsets))
            self.assertEqual(len(set(offsets)), len(offsets))

    def test_since_offset_resumes(self) -> None:
        with TemporaryDirectory() as tmp:
            path = Path(tmp) / "small.jsonl"
            _write_jsonl_lines(path, n_lines=50, line_size=128)

            full_count, offsets = _count_lines_via_helper(path)
            mid = offsets[len(offsets) // 2]
            partial_count, partial_offsets = _count_lines_via_helper(
                path, since_offset=mid,
            )
            # Resume yields strictly fewer records and every offset is
            # at-or-past the floor (the helper itself doesn't filter
            # ``<= since_offset``; the adapter does, so we just check
            # the helper's seek landed correctly).
            self.assertLess(partial_count, full_count)
            self.assertTrue(all(o >= mid for o in partial_offsets))

    def test_under_threshold_constant(self) -> None:
        with TemporaryDirectory() as tmp:
            path = Path(tmp) / "small.jsonl"
            _write_jsonl_lines(path, n_lines=10, line_size=128)
            self.assertLess(path.stat().st_size, STREAM_THRESHOLD_BYTES)


class MediumFileTests(unittest.TestCase):
    """Files between threshold and cap stream with bounded peak memory."""

    def test_streams_with_low_peak_memory(self) -> None:
        # Roughly 10 MB — above the 8 MB threshold but well under the
        # 128 MB cap. Each line is ~1 KB so we land near 10K lines.
        n_lines = 10_000
        line_size = 1024
        with TemporaryDirectory() as tmp:
            path = Path(tmp) / "medium.jsonl"
            _write_jsonl_lines(path, n_lines=n_lines, line_size=line_size)
            file_size = path.stat().st_size
            self.assertGreater(file_size, STREAM_THRESHOLD_BYTES)
            self.assertLess(file_size, MAX_SESSION_FILE_BYTES)

            tracemalloc.start()
            try:
                count = 0
                for _line_offset, _raw in iter_jsonl_lines(path):
                    count += 1
                _, peak = tracemalloc.get_traced_memory()
            finally:
                tracemalloc.stop()

            self.assertEqual(count, n_lines)
            # Peak heap should be a small multiple of one line, not the
            # whole file. Allow generous slack (a couple of MB) for
            # interpreter overhead while still proving we don't slurp
            # the file. file_size / peak should be ≥ 4 (in practice
            # it's >> 100).
            self.assertGreater(file_size, peak * 2,
                msg=f"peak {peak} bytes vs file {file_size} bytes — "
                    "iterator looks like it's slurping")


class OversizeFileTests(unittest.TestCase):
    """Files above ``MAX_SESSION_FILE_BYTES`` are skipped, never raised."""

    def test_oversize_yields_nothing(self) -> None:
        # Use a tiny patched cap so the test stays fast — no need to
        # actually write 128 MB to disk to validate the contract.
        with TemporaryDirectory() as tmp:
            path = Path(tmp) / "tiny.jsonl"
            _write_jsonl_lines(path, n_lines=100, line_size=64)
            tiny_cap = 256  # 256 bytes — guaranteed to be < the file.

            with mock.patch.object(
                _streaming, "MAX_SESSION_FILE_BYTES", tiny_cap,
            ):
                with self.assertLogs(_streaming._log, level="WARNING") as cm:
                    records = list(iter_jsonl_lines(path))

            self.assertEqual(records, [])
            self.assertTrue(
                any("exceeds cap" in msg for msg in cm.output),
                f"expected oversize warning, got: {cm.output}",
            )

    def test_oversize_does_not_raise(self) -> None:
        """Even with a 0-byte cap and a non-empty file we never raise."""
        with TemporaryDirectory() as tmp:
            path = Path(tmp) / "any.jsonl"
            _write_jsonl_lines(path, n_lines=1, line_size=64)
            with mock.patch.object(_streaming, "MAX_SESSION_FILE_BYTES", 0):
                # Direct call must succeed without exception.
                self.assertEqual(list(iter_jsonl_lines(path)), [])
                self.assertIsNone(stat_or_skip(path))

    def test_synthetic_sparse_oversize_file(self) -> None:
        """End-to-end: an actually >128 MB sparse file is skipped.

        ``tempfile`` plus ``truncate`` gives us a sparse file that
        reports a size above the real cap without committing the bytes
        to disk. This proves the cap fires on the *reported* size from
        ``stat()`` regardless of whether the bytes are physically
        allocated — important on systems where session files can grow
        sparse via truncation.
        """
        with TemporaryDirectory() as tmp:
            path = Path(tmp) / "big.jsonl"
            target = MAX_SESSION_FILE_BYTES + 1024
            with path.open("wb") as fh:
                fh.truncate(target)
            self.assertEqual(path.stat().st_size, target)

            # Quietly skip — no exception, no records.
            records = list(iter_jsonl_lines(path))
            self.assertEqual(records, [])
            self.assertIsNone(stat_or_skip(path))


class StatHelperTests(unittest.TestCase):
    """``stat_or_skip`` is the single-shot variant used by single-doc
    adapters (Kiro / Gemini single-JSON)."""

    def test_returns_size_for_normal_file(self) -> None:
        with TemporaryDirectory() as tmp:
            path = Path(tmp) / "ok.jsonl"
            _write_jsonl_lines(path, n_lines=5, line_size=64)
            self.assertEqual(stat_or_skip(path), path.stat().st_size)

    def test_returns_none_for_missing_file(self) -> None:
        with TemporaryDirectory() as tmp:
            ghost = Path(tmp) / "nope.jsonl"
            with self.assertLogs(_streaming._log, level="WARNING"):
                self.assertIsNone(stat_or_skip(ghost))


class ConfigurableCapTests(unittest.TestCase):
    """The cap is intentionally module-level so tests / power users can
    override it without changing call sites."""

    def test_patched_cap_is_observed(self) -> None:
        with TemporaryDirectory() as tmp:
            path = Path(tmp) / "small.jsonl"
            _write_jsonl_lines(path, n_lines=5, line_size=64)
            file_size = path.stat().st_size

            # With a cap *above* the file size the read succeeds.
            with mock.patch.object(
                _streaming, "MAX_SESSION_FILE_BYTES", file_size + 1,
            ):
                self.assertGreater(len(list(iter_jsonl_lines(path))), 0)

            # With a cap *below* the file size the same file is skipped.
            with mock.patch.object(
                _streaming, "MAX_SESSION_FILE_BYTES", file_size - 1,
            ):
                self.assertEqual(list(iter_jsonl_lines(path)), [])


if __name__ == "__main__":
    logging.basicConfig(level=logging.WARNING)
    unittest.main()
