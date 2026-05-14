"""Defensive empty-source / malformed-data coverage for the Kiro adapter."""

from __future__ import annotations

import json
import os
import sys
from pathlib import Path

import pytest

from stackunderflow.adapters.kiro import KiroAdapter


_IS_ROOT = hasattr(os, "geteuid") and os.geteuid() == 0
# Windows ignores Unix file permissions; chmod(0o000) is a no-op on NTFS, so the
# permission-denied path under test is unreachable there. Skip those tests on
# Windows the same way we skip them when running as root on POSIX.
_SKIP_CHMOD = _IS_ROOT or sys.platform == "win32"


# ── missing / empty source ────────────────────────────────────────────


def test_missing_storage_root_yields_nothing(tmp_path: Path) -> None:
    adapter = KiroAdapter(storage_root=tmp_path / "no-such-storage")
    assert list(adapter.enumerate()) == []


def test_empty_storage_root_yields_nothing(tmp_path: Path) -> None:
    storage = tmp_path / "kiro.kiroagent"
    storage.mkdir()
    adapter = KiroAdapter(storage_root=storage)
    assert list(adapter.enumerate()) == []


def test_storage_with_only_non_chat_files(tmp_path: Path) -> None:
    """Files without ``.chat`` extension are ignored."""
    storage = tmp_path / "kiro.kiroagent"
    storage.mkdir()
    (storage / "config.json").write_text("{}")
    (storage / "session.txt").write_text("not relevant")
    adapter = KiroAdapter(storage_root=storage)
    assert list(adapter.enumerate()) == []


# ── malformed .chat content ───────────────────────────────────────────


def test_garbage_chat_file_does_not_raise(tmp_path: Path) -> None:
    storage = tmp_path / "storage"
    storage.mkdir()
    (storage / "broken.chat").write_text("not json at all {[")
    adapter = KiroAdapter(storage_root=storage)
    refs = list(adapter.enumerate())
    assert len(refs) == 1
    # Read should yield nothing — JSON parse fails, logged and skipped.
    assert list(adapter.read(refs[0])) == []


def test_chat_file_that_is_not_a_dict(tmp_path: Path) -> None:
    """A JSON array / scalar at the top level is treated as no data."""
    storage = tmp_path / "storage"
    storage.mkdir()
    (storage / "weird.chat").write_text(json.dumps([1, 2, 3]))
    adapter = KiroAdapter(storage_root=storage)
    refs = list(adapter.enumerate())
    assert len(refs) == 1
    assert list(adapter.read(refs[0])) == []


def test_chat_file_with_no_chat_array(tmp_path: Path) -> None:
    """``chat: []`` (or absent) yields one record with empty content; must not raise."""
    storage = tmp_path / "storage"
    storage.mkdir()
    (storage / "minimal.chat").write_text(
        json.dumps({"executionId": "e", "metadata": {"workflowId": "w"}})
    )
    adapter = KiroAdapter(storage_root=storage)
    refs = list(adapter.enumerate())
    records = list(adapter.read(refs[0]))
    # Adapter still emits one rolled-up record; content is empty.
    assert len(records) == 1
    assert records[0].input_tokens == 0
    assert records[0].output_tokens == 0


# ── schema drift ──────────────────────────────────────────────────────


def test_chat_entries_with_wrong_types_are_skipped(tmp_path: Path) -> None:
    """Non-dict entries inside the chat array are filtered out."""
    storage = tmp_path / "storage"
    storage.mkdir()
    (storage / "drift.chat").write_text(
        json.dumps(
            {
                "executionId": "e",
                "actionId": "a",
                "chat": [
                    "not a dict",
                    {"role": "human", "content": "x" * 40},
                    None,
                    42,
                    {"role": "bot", "content": "y" * 20},
                ],
                "metadata": {
                    "modelId": "claude.3.5.sonnet",
                    "workflowId": "wf-x",
                },
            }
        )
    )
    adapter = KiroAdapter(storage_root=storage)
    refs = list(adapter.enumerate())
    records = list(adapter.read(refs[0]))
    assert len(records) == 1
    rec = records[0]
    # Tokens estimated from the only valid human + bot entries.
    assert rec.input_tokens > 0
    assert rec.output_tokens > 0


def test_metadata_block_wrong_type(tmp_path: Path) -> None:
    """``metadata`` field is a string instead of a dict — adapter copes."""
    storage = tmp_path / "storage"
    storage.mkdir()
    (storage / "bad-meta.chat").write_text(
        json.dumps(
            {
                "executionId": "e",
                "chat": [
                    {"role": "human", "content": "hi"},
                    {"role": "bot", "content": "ok"},
                ],
                "metadata": "should be a dict",
            }
        )
    )
    adapter = KiroAdapter(storage_root=storage)
    refs = list(adapter.enumerate())
    records = list(adapter.read(refs[0]))
    assert len(records) == 1
    # No modelId in metadata → falls back to default.
    assert records[0].model == "kiro-auto"


# ── permission denied ─────────────────────────────────────────────────


@pytest.mark.skipif(_SKIP_CHMOD, reason="chmod 000 is a no-op on Windows / bypassed by root")
def test_permission_denied_chat_file_does_not_raise(tmp_path: Path) -> None:
    storage = tmp_path / "storage"
    storage.mkdir()
    fp = storage / "locked.chat"
    fp.write_text(json.dumps({"chat": [], "metadata": {}}))
    fp.chmod(0o000)
    try:
        adapter = KiroAdapter(storage_root=storage)
        refs = list(adapter.enumerate())
        # enumerate may enumerate it (stat works); read() must not raise.
        for ref in refs:
            assert list(adapter.read(ref)) == []
    finally:
        fp.chmod(0o644)
