"""Defensive empty-source / malformed-data coverage for the Pi+OMP adapter."""

from __future__ import annotations

import json
import os
import sys
from pathlib import Path

import pytest

from stackunderflow.adapters.pi import PiAdapter


_IS_ROOT = hasattr(os, "geteuid") and os.geteuid() == 0
# Windows ignores Unix file permissions; chmod(0o000) is a no-op on NTFS, so the
# permission-denied path under test is unreachable there. Skip those tests on
# Windows the same way we skip them when running as root on POSIX.
_SKIP_CHMOD = _IS_ROOT or sys.platform == "win32"


# ── missing / empty source ────────────────────────────────────────────


def test_both_roots_missing_yields_nothing(tmp_path: Path) -> None:
    adapter = PiAdapter(
        roots=[
            (tmp_path / "no-pi", "pi"),
            (tmp_path / "no-omp", "omp"),
        ]
    )
    assert list(adapter.enumerate()) == []


def test_one_root_missing_one_empty(tmp_path: Path) -> None:
    pi = tmp_path / "pi"
    pi.mkdir()
    adapter = PiAdapter(
        roots=[(pi, "pi"), (tmp_path / "no-omp", "omp")]
    )
    assert list(adapter.enumerate()) == []


def test_both_roots_empty(tmp_path: Path) -> None:
    pi = tmp_path / "pi"
    omp = tmp_path / "omp"
    pi.mkdir()
    omp.mkdir()
    adapter = PiAdapter(roots=[(pi, "pi"), (omp, "omp")])
    assert list(adapter.enumerate()) == []


def test_root_with_non_jsonl_files_is_ignored(tmp_path: Path) -> None:
    pi = tmp_path / "pi"
    pi.mkdir()
    (pi / "session.txt").write_text("nope")
    (pi / "config.json").write_text("{}")
    adapter = PiAdapter(roots=[(pi, "pi")])
    assert list(adapter.enumerate()) == []


# ── malformed jsonl content ───────────────────────────────────────────


def test_malformed_jsonl_lines_are_skipped(tmp_path: Path) -> None:
    pi = tmp_path / "pi"
    pi.mkdir()
    (pi / "s.jsonl").write_text(
        json.dumps({"type": "session", "id": "s", "cwd": "/tmp"}) + "\n"
        + "garbage line\n"
        + "{}\n"
        + json.dumps(
            {
                "type": "message",
                "id": "a",
                "message": {
                    "role": "assistant",
                    "model": "gpt-5",
                    "content": [{"type": "text", "text": "ok"}],
                    "usage": {"input": 1, "output": 1, "cacheRead": 0, "cacheWrite": 0},
                },
            }
        )
        + "\n"
    )
    adapter = PiAdapter(roots=[(pi, "pi")])
    ref = next(iter(adapter.enumerate()))
    records = list(adapter.read(ref))
    assert len(records) == 1
    assert records[0].model == "gpt-5"


def test_jsonl_with_only_garbage_yields_nothing(tmp_path: Path) -> None:
    pi = tmp_path / "pi"
    pi.mkdir()
    (pi / "s.jsonl").write_text("not json\nstill not json\n")
    adapter = PiAdapter(roots=[(pi, "pi")])
    refs = list(adapter.enumerate())
    assert len(refs) == 1
    assert list(adapter.read(refs[0])) == []


def test_session_event_with_garbage_does_not_break_enumerate(tmp_path: Path) -> None:
    """If the leading session event is malformed, we still enumerate the file
    using the filename stem as the session id."""
    pi = tmp_path / "pi"
    pi.mkdir()
    (pi / "weird-id.jsonl").write_text("totally not json\n")
    adapter = PiAdapter(roots=[(pi, "pi")])
    refs = list(adapter.enumerate())
    assert len(refs) == 1
    # Falls back to the file stem.
    assert refs[0].session_id == "weird-id"


# ── schema drift on a message event ───────────────────────────────────


def test_message_without_usage_is_skipped(tmp_path: Path) -> None:
    pi = tmp_path / "pi"
    pi.mkdir()
    (pi / "s.jsonl").write_text(
        json.dumps({"type": "session", "id": "s"}) + "\n"
        + json.dumps(
            {
                "type": "message",
                "id": "no-usage",
                "message": {
                    "role": "assistant",
                    "content": [{"type": "text", "text": "no usage here"}],
                },
            }
        )
        + "\n"
        + json.dumps(
            {
                "type": "message",
                "id": "with-usage",
                "message": {
                    "role": "assistant",
                    "content": [{"type": "text", "text": "ok"}],
                    "usage": {"input": 1, "output": 1, "cacheRead": 0, "cacheWrite": 0},
                },
            }
        )
        + "\n"
    )
    adapter = PiAdapter(roots=[(pi, "pi")])
    ref = next(iter(adapter.enumerate()))
    records = list(adapter.read(ref))
    assert len(records) == 1
    assert records[0].uuid == "with-usage"


def test_message_block_wrong_type(tmp_path: Path) -> None:
    pi = tmp_path / "pi"
    pi.mkdir()
    (pi / "s.jsonl").write_text(
        json.dumps({"type": "session", "id": "s"}) + "\n"
        + json.dumps(
            {"type": "message", "id": "x", "message": "not a dict"}
        )
        + "\n"
    )
    adapter = PiAdapter(roots=[(pi, "pi")])
    ref = next(iter(adapter.enumerate()))
    assert list(adapter.read(ref)) == []


def test_user_message_yields_no_record(tmp_path: Path) -> None:
    """``role != assistant`` does not drive a Record (matches happy-path)."""
    pi = tmp_path / "pi"
    pi.mkdir()
    (pi / "s.jsonl").write_text(
        json.dumps({"type": "session", "id": "s"}) + "\n"
        + json.dumps(
            {
                "type": "message",
                "id": "u",
                "message": {
                    "role": "user",
                    "content": [{"type": "text", "text": "hello"}],
                    "usage": {"input": 1, "output": 0, "cacheRead": 0, "cacheWrite": 0},
                },
            }
        )
        + "\n"
    )
    adapter = PiAdapter(roots=[(pi, "pi")])
    ref = next(iter(adapter.enumerate()))
    assert list(adapter.read(ref)) == []


# ── permission denied ─────────────────────────────────────────────────


@pytest.mark.skipif(_SKIP_CHMOD, reason="chmod 000 is a no-op on Windows / bypassed by root")
def test_permission_denied_jsonl_does_not_raise(tmp_path: Path) -> None:
    pi = tmp_path / "pi"
    pi.mkdir()
    fp = pi / "s.jsonl"
    fp.write_text(json.dumps({"type": "session", "id": "s"}) + "\n")
    fp.chmod(0o000)
    try:
        adapter = PiAdapter(roots=[(pi, "pi")])
        refs = list(adapter.enumerate())
        # Adapter may fail to peek, but enumerate doesn't raise.
        for ref in refs:
            assert list(adapter.read(ref)) == []
    finally:
        fp.chmod(0o644)


# ── malformed-input hardening (ingest-surface sweep, 2026-07) ─────────


def test_non_dict_json_lines_are_skipped(tmp_path: Path) -> None:
    """Lines that parse as JSON but aren't objects (list/str/number) must be
    skipped by read(), not crash the generator."""
    root = tmp_path / "pi" / "agent" / "sessions"
    root.mkdir(parents=True)
    (root / "s.jsonl").write_text(
        json.dumps({"type": "session", "id": "s", "cwd": "/tmp/w"}) + "\n"
        + "[1, 2, 3]\n"
        + '"just a string"\n'
        + "42\n"
        + json.dumps(
            {
                "type": "message",
                "id": "a",
                "message": {
                    "role": "assistant",
                    "model": "gpt-5",
                    "content": [{"type": "text", "text": "ok"}],
                    "usage": {"input": 1, "output": 1},
                },
            }
        )
        + "\n"
    )
    adapter = PiAdapter(roots=[(root, "pi")])
    ref = next(iter(adapter.enumerate()))
    records = list(adapter.read(ref))
    assert len(records) == 1
    assert records[0].uuid == "a"


def test_enumerate_survives_non_dict_first_line(tmp_path: Path) -> None:
    """A session file whose first line is ``[1,2]`` must not crash
    enumerate() (the peek helper) — it falls back to the filename stem."""
    root = tmp_path / "pi" / "agent" / "sessions"
    root.mkdir(parents=True)
    (root / "weird.jsonl").write_text("[1, 2]\n")
    adapter = PiAdapter(roots=[(root, "pi")])
    refs = list(adapter.enumerate())
    assert len(refs) == 1
    assert refs[0].session_id == "weird"
    assert list(adapter.read(refs[0])) == []


def test_non_string_cwd_is_dropped(tmp_path: Path) -> None:
    """A numeric/dict ``cwd`` must not leak into the Record."""
    root = tmp_path / "pi" / "agent" / "sessions"
    root.mkdir(parents=True)
    (root / "s.jsonl").write_text(
        json.dumps(
            {
                "type": "message",
                "id": "a",
                "cwd": {"bad": 1},
                "message": {
                    "role": "assistant",
                    "model": "gpt-5",
                    "content": "x",
                    "usage": {"input": 1, "output": 1},
                },
            }
        )
        + "\n"
    )
    adapter = PiAdapter(roots=[(root, "pi")])
    ref = next(iter(adapter.enumerate()))
    records = list(adapter.read(ref))
    assert len(records) == 1
    assert records[0].cwd is None
