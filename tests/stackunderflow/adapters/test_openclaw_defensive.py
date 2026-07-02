"""Defensive empty-source / malformed-data coverage for the OpenClaw adapter.

OpenClaw walks four candidate base directories (``~/.openclaw``,
``~/.clawdbot``, ``~/.moltbot``, ``~/.moldbot``); these tests pin the
empty-state and malformed-input behaviour for each layer.
"""

from __future__ import annotations

import json
import os
import sys
from pathlib import Path

import pytest

from stackunderflow.adapters.openclaw import OpenClawAdapter


_IS_ROOT = hasattr(os, "geteuid") and os.geteuid() == 0
# Windows ignores Unix file permissions; chmod(0o000) is a no-op on NTFS, so the
# permission-denied path under test is unreachable there. Skip those tests on
# Windows the same way we skip them when running as root on POSIX.
_SKIP_CHMOD = _IS_ROOT or sys.platform == "win32"


# ── missing / empty source ────────────────────────────────────────────


def test_all_bases_missing_yields_nothing(tmp_path: Path) -> None:
    adapter = OpenClawAdapter(
        base_dirs=[
            tmp_path / "no-a",
            tmp_path / "no-b",
            tmp_path / "no-c",
        ]
    )
    assert list(adapter.enumerate()) == []


def test_empty_base_dirs_yield_nothing(tmp_path: Path) -> None:
    base = tmp_path / "openclaw" / "agents"
    base.mkdir(parents=True)
    adapter = OpenClawAdapter(base_dirs=[base])
    assert list(adapter.enumerate()) == []


def test_agent_dir_without_sessions_subdir(tmp_path: Path) -> None:
    base = tmp_path / "openclaw" / "agents"
    (base / "agent-x").mkdir(parents=True)  # no `sessions/` inside
    adapter = OpenClawAdapter(base_dirs=[base])
    assert list(adapter.enumerate()) == []


def test_empty_sessions_dir(tmp_path: Path) -> None:
    base = tmp_path / "openclaw" / "agents"
    (base / "agent" / "sessions").mkdir(parents=True)
    adapter = OpenClawAdapter(base_dirs=[base])
    assert list(adapter.enumerate()) == []


# ── malformed jsonl content ───────────────────────────────────────────


def test_malformed_jsonl_lines_are_skipped(tmp_path: Path) -> None:
    base = tmp_path / "openclaw" / "agents"
    sessions = base / "agent" / "sessions"
    sessions.mkdir(parents=True)
    (sessions / "s.jsonl").write_text(
        json.dumps({"type": "session", "id": "s"}) + "\n"
        + "this is not json\n"
        + "{}\n"
        + json.dumps(
            {
                "type": "message",
                "id": "a",
                "message": {
                    "role": "assistant",
                    "model": "claude-3-5-sonnet",
                    "content": [{"type": "text", "text": "ok"}],
                    "usage": {"input": 1, "output": 1, "cacheRead": 0, "cacheWrite": 0},
                },
            }
        )
        + "\n"
    )
    adapter = OpenClawAdapter(base_dirs=[base])
    ref = next(iter(adapter.enumerate()))
    records = list(adapter.read(ref))
    assert len(records) == 1
    assert records[0].model == "claude-3-5-sonnet"


def test_jsonl_file_with_only_garbage_yields_nothing(tmp_path: Path) -> None:
    base = tmp_path / "openclaw" / "agents"
    sessions = base / "agent" / "sessions"
    sessions.mkdir(parents=True)
    (sessions / "s.jsonl").write_text(
        "not json\n"
        "still not json\n"
        "\x00\x01\x02 binary garbage\n"
    )
    adapter = OpenClawAdapter(base_dirs=[base])
    refs = list(adapter.enumerate())
    assert len(refs) == 1
    assert list(adapter.read(refs[0])) == []


# ── schema drift on a message event ───────────────────────────────────


def test_message_event_without_usage_is_skipped(tmp_path: Path) -> None:
    """An assistant message without a ``usage`` block yields no Record."""
    base = tmp_path / "openclaw" / "agents"
    sessions = base / "agent" / "sessions"
    sessions.mkdir(parents=True)
    (sessions / "s.jsonl").write_text(
        json.dumps({"type": "session", "id": "s"}) + "\n"
        + json.dumps(
            {
                "type": "message",
                "id": "a",
                "message": {
                    "role": "assistant",
                    "model": "claude-3-5-sonnet",
                    "content": [{"type": "text", "text": "no usage here"}],
                },
            }
        )
        + "\n"
        + json.dumps(
            {
                "type": "message",
                "id": "b",
                "message": {
                    "role": "assistant",
                    "model": "claude-3-5-sonnet",
                    "content": [{"type": "text", "text": "with usage"}],
                    "usage": {"input": 1, "output": 1, "cacheRead": 0, "cacheWrite": 0},
                },
            }
        )
        + "\n"
    )
    adapter = OpenClawAdapter(base_dirs=[base])
    ref = next(iter(adapter.enumerate()))
    records = list(adapter.read(ref))
    assert len(records) == 1
    assert records[0].uuid == "b"


def test_message_event_with_garbage_message_block(tmp_path: Path) -> None:
    """``message`` is a string instead of a dict — adapter skips defensively."""
    base = tmp_path / "openclaw" / "agents"
    sessions = base / "agent" / "sessions"
    sessions.mkdir(parents=True)
    (sessions / "s.jsonl").write_text(
        json.dumps({"type": "session", "id": "s"}) + "\n"
        + json.dumps(
            {"type": "message", "id": "garbage", "message": "should be a dict"}
        )
        + "\n"
    )
    adapter = OpenClawAdapter(base_dirs=[base])
    ref = next(iter(adapter.enumerate()))
    assert list(adapter.read(ref)) == []


def test_usage_with_non_numeric_values(tmp_path: Path) -> None:
    """``usage`` with strings instead of ints coerces to 0; record still emits."""
    base = tmp_path / "openclaw" / "agents"
    sessions = base / "agent" / "sessions"
    sessions.mkdir(parents=True)
    (sessions / "s.jsonl").write_text(
        json.dumps({"type": "session", "id": "s"}) + "\n"
        + json.dumps(
            {
                "type": "message",
                "id": "a",
                "message": {
                    "role": "assistant",
                    "model": "claude-3-5-sonnet",
                    "content": [{"type": "text", "text": "ok"}],
                    "usage": {
                        "input": "not a number",
                        "output": None,
                        "cacheRead": "x",
                        "cacheWrite": True,
                    },
                },
            }
        )
        + "\n"
    )
    adapter = OpenClawAdapter(base_dirs=[base])
    ref = next(iter(adapter.enumerate()))
    records = list(adapter.read(ref))
    assert len(records) == 1
    rec = records[0]
    assert rec.input_tokens == 0
    assert rec.output_tokens == 0
    assert rec.cache_read_tokens == 0
    # ``True`` coerces to int(True) == 1 in Python; that's fine — the test pins
    # "doesn't crash" not specific arithmetic.
    assert rec.cache_create_tokens >= 0


# ── permission denied ─────────────────────────────────────────────────


@pytest.mark.skipif(_SKIP_CHMOD, reason="chmod 000 is a no-op on Windows / bypassed by root")
def test_permission_denied_jsonl_does_not_raise(tmp_path: Path) -> None:
    base = tmp_path / "openclaw" / "agents"
    sessions = base / "agent" / "sessions"
    sessions.mkdir(parents=True)
    fp = sessions / "s.jsonl"
    fp.write_text(json.dumps({"type": "session", "id": "s"}) + "\n")
    fp.chmod(0o000)
    try:
        adapter = OpenClawAdapter(base_dirs=[base])
        refs = list(adapter.enumerate())
        for ref in refs:
            assert list(adapter.read(ref)) == []
    finally:
        fp.chmod(0o644)


# ── malformed-input hardening (ingest-surface sweep, 2026-07) ─────────


def test_message_event_with_non_dict_usage_is_skipped(tmp_path: Path) -> None:
    """Regression for the cost-audit #16a item (openclaw ``inner['usage']``):
    ``usage`` present but not a dict must skip the record, not raise."""
    base = tmp_path / "openclaw" / "agents"
    sessions = base / "agent" / "sessions"
    sessions.mkdir(parents=True)
    (sessions / "s.jsonl").write_text(
        json.dumps({"type": "session", "id": "s"}) + "\n"
        + json.dumps(
            {
                "type": "message",
                "id": "bad-usage",
                "message": {
                    "role": "assistant",
                    "model": "claude-3-5-sonnet",
                    "content": [{"type": "text", "text": "x"}],
                    "usage": "not a dict",
                },
            }
        )
        + "\n"
        + json.dumps(
            {
                "type": "message",
                "id": "good",
                "message": {
                    "role": "assistant",
                    "model": "claude-3-5-sonnet",
                    "content": [{"type": "text", "text": "y"}],
                    "usage": {"input": 1, "output": 1},
                },
            }
        )
        + "\n"
    )
    adapter = OpenClawAdapter(base_dirs=[base])
    ref = next(iter(adapter.enumerate()))
    records = list(adapter.read(ref))
    assert [r.uuid for r in records] == ["good"]


def test_non_dict_json_lines_are_skipped(tmp_path: Path) -> None:
    """Lines that parse as JSON but aren't objects (list/str/number) must be
    skipped by read(), not crash the generator."""
    base = tmp_path / "openclaw" / "agents"
    sessions = base / "agent" / "sessions"
    sessions.mkdir(parents=True)
    (sessions / "s.jsonl").write_text(
        json.dumps({"type": "session", "id": "s"}) + "\n"
        + "[1, 2, 3]\n"
        + '"just a string"\n'
        + "42\n"
        + json.dumps(
            {
                "type": "message",
                "id": "a",
                "message": {
                    "role": "assistant",
                    "model": "claude-3-5-sonnet",
                    "content": [{"type": "text", "text": "ok"}],
                    "usage": {"input": 1, "output": 1},
                },
            }
        )
        + "\n"
    )
    adapter = OpenClawAdapter(base_dirs=[base])
    ref = next(iter(adapter.enumerate()))
    records = list(adapter.read(ref))
    assert len(records) == 1
    assert records[0].uuid == "a"


def test_enumerate_survives_non_dict_first_line(tmp_path: Path) -> None:
    """A session file whose first line is ``[1,2]`` must not crash
    enumerate() (the peek helper) — it falls back to the filename stem."""
    base = tmp_path / "openclaw" / "agents"
    sessions = base / "agent" / "sessions"
    sessions.mkdir(parents=True)
    (sessions / "weird.jsonl").write_text("[1, 2]\n")
    adapter = OpenClawAdapter(base_dirs=[base])
    refs = list(adapter.enumerate())
    assert len(refs) == 1
    assert refs[0].session_id == "weird"
    assert list(adapter.read(refs[0])) == []


def test_usage_with_inf_values_coerces_to_zero(tmp_path: Path) -> None:
    """JSON ``1e999`` parses to float('inf'); int() on it raises — the
    coercer must return 0 instead."""
    base = tmp_path / "openclaw" / "agents"
    sessions = base / "agent" / "sessions"
    sessions.mkdir(parents=True)
    (sessions / "s.jsonl").write_text(
        json.dumps({"type": "session", "id": "s"}) + "\n"
        + '{"type": "message", "id": "a", "message": {"role": "assistant",'
        ' "model": "m", "content": "x",'
        ' "usage": {"input": 1e999, "output": 1}}}\n'
    )
    adapter = OpenClawAdapter(base_dirs=[base])
    ref = next(iter(adapter.enumerate()))
    records = list(adapter.read(ref))
    assert len(records) == 1
    assert records[0].input_tokens == 0
    assert records[0].output_tokens == 1
