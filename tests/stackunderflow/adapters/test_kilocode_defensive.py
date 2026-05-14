"""Defensive empty-source / malformed-data coverage for the KiloCode adapter.

KiloCode reuses the Cline parser (codeburn-catalog §8) — only the
extension dir differs — so these tests double as defensive coverage for
the shared ``_VsCodeClineAdapter`` parser.
"""

from __future__ import annotations

import json
import os
import sys
from pathlib import Path

import pytest

from stackunderflow.adapters.cline import KiloCodeAdapter


_IS_ROOT = hasattr(os, "geteuid") and os.geteuid() == 0
# Windows ignores Unix file permissions; chmod(0o000) is a no-op on NTFS, so the
# permission-denied path under test is unreachable there. Skip those tests on
# Windows the same way we skip them when running as root on POSIX.
_SKIP_CHMOD = _IS_ROOT or sys.platform == "win32"


# ── missing / empty source ────────────────────────────────────────────


def test_missing_tasks_root_yields_nothing(tmp_path: Path) -> None:
    adapter = KiloCodeAdapter(tasks_root=tmp_path / "no-such-tasks")
    assert list(adapter.enumerate()) == []


def test_empty_tasks_root_yields_nothing(tmp_path: Path) -> None:
    tasks = tmp_path / "tasks"
    tasks.mkdir()
    adapter = KiloCodeAdapter(tasks_root=tasks)
    assert list(adapter.enumerate()) == []


def test_task_dir_without_ui_messages_json(tmp_path: Path) -> None:
    """A task dir missing ``ui_messages.json`` is skipped (no enumerate)."""
    tasks = tmp_path / "tasks"
    (tasks / "task-no-ui").mkdir(parents=True)
    adapter = KiloCodeAdapter(tasks_root=tasks)
    assert list(adapter.enumerate()) == []


def test_tasks_root_with_only_files_yields_nothing(tmp_path: Path) -> None:
    """Files (not dirs) directly under tasks_root are ignored."""
    tasks = tmp_path / "tasks"
    tasks.mkdir()
    (tasks / "stray-file.json").write_text("{}")
    adapter = KiloCodeAdapter(tasks_root=tasks)
    assert list(adapter.enumerate()) == []


# ── malformed ui_messages.json ────────────────────────────────────────


def test_garbage_ui_messages_does_not_raise(tmp_path: Path) -> None:
    """Truncated JSON: enumerate succeeds (file exists), read yields nothing."""
    tasks = tmp_path / "tasks"
    task_dir = tasks / "task-garbage"
    task_dir.mkdir(parents=True)
    (task_dir / "ui_messages.json").write_text("[{not valid json")
    adapter = KiloCodeAdapter(tasks_root=tasks)
    refs = list(adapter.enumerate())
    assert len(refs) == 1
    assert list(adapter.read(refs[0])) == []


def test_ui_messages_that_is_not_a_list(tmp_path: Path) -> None:
    """JSON object instead of an array → adapter logs and yields nothing."""
    tasks = tmp_path / "tasks"
    task_dir = tasks / "task-shape"
    task_dir.mkdir(parents=True)
    (task_dir / "ui_messages.json").write_text(json.dumps({"not": "a list"}))
    adapter = KiloCodeAdapter(tasks_root=tasks)
    refs = list(adapter.enumerate())
    assert len(refs) == 1
    assert list(adapter.read(refs[0])) == []


def test_empty_ui_messages_array(tmp_path: Path) -> None:
    """A valid empty array: zero records, no raise."""
    tasks = tmp_path / "tasks"
    task_dir = tasks / "task-empty"
    task_dir.mkdir(parents=True)
    (task_dir / "ui_messages.json").write_text("[]")
    adapter = KiloCodeAdapter(tasks_root=tasks)
    refs = list(adapter.enumerate())
    assert len(refs) == 1
    assert list(adapter.read(refs[0])) == []


# ── schema drift inside ui_messages ───────────────────────────────────


def test_non_dict_ui_events_are_skipped(tmp_path: Path) -> None:
    tasks = tmp_path / "tasks"
    task_dir = tasks / "task-mixed"
    task_dir.mkdir(parents=True)
    events = [
        "not a dict",
        None,
        42,
        {
            "type": "say",
            "say": "api_req_started",
            "ts": 1700000000000,
            "text": json.dumps({"tokensIn": 1, "tokensOut": 1,
                                "cacheWrites": 0, "cacheReads": 0}),
        },
    ]
    (task_dir / "ui_messages.json").write_text(json.dumps(events))
    adapter = KiloCodeAdapter(tasks_root=tasks)
    refs = list(adapter.enumerate())
    records = list(adapter.read(refs[0]))
    # Non-dict entries dropped; one valid api_req_started yields a record.
    assert len(records) == 1


def test_api_req_with_garbage_text_field(tmp_path: Path) -> None:
    """`text` field that isn't valid JSON falls back to all-zero tokens."""
    tasks = tmp_path / "tasks"
    task_dir = tasks / "task-garbage-text"
    task_dir.mkdir(parents=True)
    events = [
        {
            "type": "say",
            "say": "api_req_started",
            "ts": 1700000000000,
            "text": "not json {[",
        },
    ]
    (task_dir / "ui_messages.json").write_text(json.dumps(events))
    adapter = KiloCodeAdapter(tasks_root=tasks)
    refs = list(adapter.enumerate())
    records = list(adapter.read(refs[0]))
    assert len(records) == 1
    rec = records[0]
    assert rec.input_tokens == 0
    assert rec.output_tokens == 0


def test_corrupt_api_conversation_history_does_not_break(tmp_path: Path) -> None:
    """``api_conversation_history.json`` malformed → model defaults to ``cline-auto``."""
    tasks = tmp_path / "tasks"
    task_dir = tasks / "task-bad-history"
    task_dir.mkdir(parents=True)
    events = [
        {
            "type": "say",
            "say": "api_req_started",
            "ts": 1700000000000,
            "text": json.dumps({"tokensIn": 5, "tokensOut": 5,
                                "cacheWrites": 0, "cacheReads": 0}),
        },
    ]
    (task_dir / "ui_messages.json").write_text(json.dumps(events))
    (task_dir / "api_conversation_history.json").write_text("not json {[")
    adapter = KiloCodeAdapter(tasks_root=tasks)
    refs = list(adapter.enumerate())
    records = list(adapter.read(refs[0]))
    assert len(records) == 1
    assert records[0].model == "cline-auto"


# ── permission denied ─────────────────────────────────────────────────


@pytest.mark.skipif(_SKIP_CHMOD, reason="chmod 000 is a no-op on Windows / bypassed by root")
def test_permission_denied_ui_messages_does_not_raise(tmp_path: Path) -> None:
    tasks = tmp_path / "tasks"
    task_dir = tasks / "task-locked"
    task_dir.mkdir(parents=True)
    fp = task_dir / "ui_messages.json"
    fp.write_text("[]")
    fp.chmod(0o000)
    try:
        adapter = KiloCodeAdapter(tasks_root=tasks)
        refs = list(adapter.enumerate())
        # Adapter may stat fine but fail to read; never raises.
        for ref in refs:
            assert list(adapter.read(ref)) == []
    finally:
        fp.chmod(0o644)
