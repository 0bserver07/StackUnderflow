"""Defensive empty-source / malformed-data coverage for the Roo Code adapter.

Roo Code reuses the Cline parser (codeburn-catalog §14) — only the
extension dir differs. The KiloCode and RooCode defensive files together
exercise the shared parser against pathological VS Code globalStorage
inputs.
"""

from __future__ import annotations

import json
import os
import sys
from pathlib import Path

import pytest

from stackunderflow.adapters.cline import RooCodeAdapter


_IS_ROOT = hasattr(os, "geteuid") and os.geteuid() == 0
# Windows ignores Unix file permissions; chmod(0o000) is a no-op on NTFS, so the
# permission-denied path under test is unreachable there. Skip those tests on
# Windows the same way we skip them when running as root on POSIX.
_SKIP_CHMOD = _IS_ROOT or sys.platform == "win32"


# ── missing / empty source ────────────────────────────────────────────


def test_missing_tasks_root_yields_nothing(tmp_path: Path) -> None:
    adapter = RooCodeAdapter(tasks_root=tmp_path / "no-such-tasks")
    assert list(adapter.enumerate()) == []


def test_empty_tasks_root_yields_nothing(tmp_path: Path) -> None:
    tasks = tmp_path / "tasks"
    tasks.mkdir()
    adapter = RooCodeAdapter(tasks_root=tasks)
    assert list(adapter.enumerate()) == []


def test_task_dir_missing_ui_messages(tmp_path: Path) -> None:
    tasks = tmp_path / "tasks"
    (tasks / "task-x").mkdir(parents=True)
    adapter = RooCodeAdapter(tasks_root=tasks)
    assert list(adapter.enumerate()) == []


# ── malformed ui_messages.json ────────────────────────────────────────


def test_truncated_ui_messages_does_not_raise(tmp_path: Path) -> None:
    tasks = tmp_path / "tasks"
    task_dir = tasks / "t"
    task_dir.mkdir(parents=True)
    (task_dir / "ui_messages.json").write_text('[{"type":"say","say":"api')
    adapter = RooCodeAdapter(tasks_root=tasks)
    refs = list(adapter.enumerate())
    assert len(refs) == 1
    assert list(adapter.read(refs[0])) == []


def test_ui_messages_with_dict_at_top_level(tmp_path: Path) -> None:
    """Top-level dict instead of list: skip cleanly."""
    tasks = tmp_path / "tasks"
    task_dir = tasks / "t"
    task_dir.mkdir(parents=True)
    (task_dir / "ui_messages.json").write_text(json.dumps({"events": []}))
    adapter = RooCodeAdapter(tasks_root=tasks)
    refs = list(adapter.enumerate())
    assert list(adapter.read(refs[0])) == []


def test_ui_messages_with_no_api_req_events(tmp_path: Path) -> None:
    """Only user_feedback events, no api_req_started → zero records."""
    tasks = tmp_path / "tasks"
    task_dir = tasks / "t"
    task_dir.mkdir(parents=True)
    (task_dir / "ui_messages.json").write_text(
        json.dumps(
            [
                {"type": "say", "say": "user_feedback", "ts": 1, "text": "hi"},
                {"type": "say", "say": "user_feedback", "ts": 2, "text": "hello"},
            ]
        )
    )
    adapter = RooCodeAdapter(tasks_root=tasks)
    refs = list(adapter.enumerate())
    records = list(adapter.read(refs[0]))
    assert records == []


# ── schema drift ──────────────────────────────────────────────────────


def test_api_req_with_missing_token_keys(tmp_path: Path) -> None:
    """A valid api_req event but with no token keys → all zeros, record still emits."""
    tasks = tmp_path / "tasks"
    task_dir = tasks / "t"
    task_dir.mkdir(parents=True)
    (task_dir / "ui_messages.json").write_text(
        json.dumps(
            [
                {
                    "type": "say",
                    "say": "api_req_started",
                    "ts": 1700000000000,
                    "text": json.dumps({"cost": 0.0}),  # no token fields at all
                },
            ]
        )
    )
    adapter = RooCodeAdapter(tasks_root=tasks)
    refs = list(adapter.enumerate())
    records = list(adapter.read(refs[0]))
    assert len(records) == 1
    rec = records[0]
    assert rec.input_tokens == 0
    assert rec.output_tokens == 0


def test_api_req_with_negative_token_counts(tmp_path: Path) -> None:
    """Negative counts are clamped to zero."""
    tasks = tmp_path / "tasks"
    task_dir = tasks / "t"
    task_dir.mkdir(parents=True)
    (task_dir / "ui_messages.json").write_text(
        json.dumps(
            [
                {
                    "type": "say",
                    "say": "api_req_started",
                    "ts": 1700000000000,
                    "text": json.dumps({
                        "tokensIn": -50, "tokensOut": -50,
                        "cacheWrites": -1, "cacheReads": -1,
                    }),
                },
            ]
        )
    )
    adapter = RooCodeAdapter(tasks_root=tasks)
    refs = list(adapter.enumerate())
    records = list(adapter.read(refs[0]))
    assert len(records) == 1
    rec = records[0]
    assert rec.input_tokens == 0
    assert rec.output_tokens == 0
    assert rec.cache_create_tokens == 0
    assert rec.cache_read_tokens == 0


def test_event_without_type_say_is_skipped(tmp_path: Path) -> None:
    """An event without ``type=='say'`` is dropped."""
    tasks = tmp_path / "tasks"
    task_dir = tasks / "t"
    task_dir.mkdir(parents=True)
    (task_dir / "ui_messages.json").write_text(
        json.dumps(
            [
                {"type": "other", "say": "api_req_started", "text": "{}"},
                {
                    "type": "say",
                    "say": "api_req_started",
                    "ts": 1700000000000,
                    "text": json.dumps({"tokensIn": 1, "tokensOut": 1,
                                        "cacheWrites": 0, "cacheReads": 0}),
                },
            ]
        )
    )
    adapter = RooCodeAdapter(tasks_root=tasks)
    refs = list(adapter.enumerate())
    records = list(adapter.read(refs[0]))
    assert len(records) == 1


# ── permission denied ─────────────────────────────────────────────────


@pytest.mark.skipif(_SKIP_CHMOD, reason="chmod 000 is a no-op on Windows / bypassed by root")
def test_permission_denied_does_not_raise(tmp_path: Path) -> None:
    tasks = tmp_path / "tasks"
    task_dir = tasks / "t"
    task_dir.mkdir(parents=True)
    fp = task_dir / "ui_messages.json"
    fp.write_text("[]")
    fp.chmod(0o000)
    try:
        adapter = RooCodeAdapter(tasks_root=tasks)
        refs = list(adapter.enumerate())
        for ref in refs:
            assert list(adapter.read(ref)) == []
    finally:
        fp.chmod(0o644)
