"""Defensive empty-source / malformed-data coverage for the Copilot adapter.

Copilot has two source layouts (legacy + VS Code transcripts) and the
adapter must enumerate / read each defensively. These tests pin the
empty-state and malformed-input behaviour for both.
"""

from __future__ import annotations

import json
import os
from pathlib import Path

import pytest

from stackunderflow.adapters.copilot import CopilotAdapter


_IS_ROOT = hasattr(os, "geteuid") and os.geteuid() == 0


# ── missing / empty source ────────────────────────────────────────────


def test_both_roots_missing_yields_nothing(tmp_path: Path) -> None:
    adapter = CopilotAdapter(
        legacy_root=tmp_path / "no-legacy",
        vscode_workspace_storage=tmp_path / "no-vscode",
    )
    assert list(adapter.enumerate()) == []


def test_empty_legacy_root_yields_nothing(tmp_path: Path) -> None:
    legacy = tmp_path / "legacy"
    legacy.mkdir()
    adapter = CopilotAdapter(
        legacy_root=legacy, vscode_workspace_storage=tmp_path / "no-vscode"
    )
    assert list(adapter.enumerate()) == []


def test_empty_vscode_workspace_storage_yields_nothing(tmp_path: Path) -> None:
    vscode = tmp_path / "ws"
    vscode.mkdir()
    adapter = CopilotAdapter(
        legacy_root=tmp_path / "no-legacy", vscode_workspace_storage=vscode
    )
    assert list(adapter.enumerate()) == []


def test_legacy_session_dir_without_events_jsonl(tmp_path: Path) -> None:
    """A session directory missing ``events.jsonl`` is silently skipped."""
    legacy = tmp_path / "legacy"
    (legacy / "sess-no-events").mkdir(parents=True)
    adapter = CopilotAdapter(
        legacy_root=legacy, vscode_workspace_storage=tmp_path / "no-vscode"
    )
    assert list(adapter.enumerate()) == []


def test_workspace_dir_without_copilot_chat_subdir(tmp_path: Path) -> None:
    """Workspace dir without ``GitHub.copilot-chat/transcripts/`` is skipped."""
    vscode = tmp_path / "ws"
    (vscode / "ws-hash" / "OtherExtension").mkdir(parents=True)
    adapter = CopilotAdapter(
        legacy_root=tmp_path / "no-legacy", vscode_workspace_storage=vscode
    )
    assert list(adapter.enumerate()) == []


# ── malformed transcript content ──────────────────────────────────────


def test_malformed_legacy_events_jsonl_does_not_raise(tmp_path: Path) -> None:
    """Garbage events.jsonl: ref enumerated, read yields valid records only."""
    legacy = tmp_path / "legacy"
    sess = legacy / "sess-bad"
    sess.mkdir(parents=True)
    (sess / "events.jsonl").write_text(
        "this is not json\n"
        "{}\n"
        '{"type":"user.message","content":"hello"}\n'
        '{"type":"assistant.message","content":"hi","outputTokens":5,'
        '"model":"claude-3-5-sonnet"}\n'
        "trailing garbage line"
    )
    adapter = CopilotAdapter(
        legacy_root=legacy, vscode_workspace_storage=tmp_path / "no-vscode"
    )
    ref = next(iter(adapter.enumerate()))
    records = list(adapter.read(ref))
    assert len(records) == 1
    assert records[0].output_tokens == 5


def test_legacy_events_with_only_garbage_yields_nothing(tmp_path: Path) -> None:
    legacy = tmp_path / "legacy"
    sess = legacy / "sess-garbage"
    sess.mkdir(parents=True)
    (sess / "events.jsonl").write_text(
        "not json\n"
        "still not json\n"
        "\x00binary\x01\n"
    )
    adapter = CopilotAdapter(
        legacy_root=legacy, vscode_workspace_storage=tmp_path / "no-vscode"
    )
    ref = next(iter(adapter.enumerate()))
    assert list(adapter.read(ref)) == []


def test_vscode_transcript_with_malformed_lines(tmp_path: Path) -> None:
    vscode = tmp_path / "ws"
    transcripts = vscode / "ws-h" / "GitHub.copilot-chat" / "transcripts"
    transcripts.mkdir(parents=True)
    (transcripts / "t1.jsonl").write_text(
        json.dumps({"type": "session.start", "data": {}}) + "\n"
        + "garbage\n"
        + json.dumps(
            {
                "type": "assistant.message",
                "model": "claude-3-5-sonnet",
                "content": "ok",
                "outputTokens": 10,
            }
        )
        + "\n"
    )
    adapter = CopilotAdapter(
        legacy_root=tmp_path / "no-legacy", vscode_workspace_storage=vscode
    )
    ref = next(iter(adapter.enumerate()))
    records = list(adapter.read(ref))
    assert len(records) == 1
    assert records[0].output_tokens == 10


# ── schema drift ──────────────────────────────────────────────────────


def test_assistant_event_missing_type_is_skipped(tmp_path: Path) -> None:
    """An event without a ``type`` is not classified and is skipped."""
    legacy = tmp_path / "legacy"
    sess = legacy / "sess-drift"
    sess.mkdir(parents=True)
    (sess / "events.jsonl").write_text(
        json.dumps({"content": "no type here", "outputTokens": 5}) + "\n"
        + json.dumps(
            {"type": "assistant.message", "content": "ok", "outputTokens": 7}
        )
        + "\n"
    )
    adapter = CopilotAdapter(
        legacy_root=legacy, vscode_workspace_storage=tmp_path / "no-vscode"
    )
    ref = next(iter(adapter.enumerate()))
    records = list(adapter.read(ref))
    assert len(records) == 1
    assert records[0].output_tokens == 7


def test_workspace_json_with_garbage_does_not_raise(tmp_path: Path) -> None:
    """Malformed workspace.json falls back to default project_slug."""
    legacy = tmp_path / "legacy"
    sess = legacy / "sess-ws"
    sess.mkdir(parents=True)
    (sess / "events.jsonl").write_text(
        json.dumps(
            {"type": "assistant.message", "content": "x", "outputTokens": 1}
        )
        + "\n"
    )
    (sess / "workspace.json").write_text("not json {[")
    adapter = CopilotAdapter(
        legacy_root=legacy, vscode_workspace_storage=tmp_path / "no-vscode"
    )
    refs = list(adapter.enumerate())
    assert len(refs) == 1
    # Defaults to "copilot" when workspace.json fails to parse.
    assert refs[0].project_slug == "copilot"


# ── permission denied ─────────────────────────────────────────────────


@pytest.mark.skipif(_IS_ROOT, reason="root bypasses chmod 000")
def test_permission_denied_events_jsonl_does_not_raise(tmp_path: Path) -> None:
    legacy = tmp_path / "legacy"
    sess = legacy / "sess-perm"
    sess.mkdir(parents=True)
    fp = sess / "events.jsonl"
    fp.write_text(
        json.dumps(
            {"type": "assistant.message", "content": "x", "outputTokens": 1}
        )
        + "\n"
    )
    fp.chmod(0o000)
    try:
        adapter = CopilotAdapter(
            legacy_root=legacy, vscode_workspace_storage=tmp_path / "no-vscode"
        )
        refs = list(adapter.enumerate())
        # Adapter may or may not enumerate (stat may succeed); read() must not raise.
        for ref in refs:
            assert list(adapter.read(ref)) == []
    finally:
        fp.chmod(0o644)
