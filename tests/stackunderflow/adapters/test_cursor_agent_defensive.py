"""Defensive empty-source / malformed-data coverage for the Cursor Agent adapter.

The 7 default-on adapters have been validated against real local data;
the 9 beta adapters (this one included) have only synthetic-fixture
tests. The Cursor v3 bug (PR #52) was exactly the kind of latent failure
this gap creates. We can't validate against real data we don't have, but
we can lock in defensive behaviour so the next user who installs Cursor
Agent doesn't crash.

Covers:

  - Missing projects root (default path on a machine without Cursor)
  - Empty projects root (directory exists but no project subdirs)
  - Project dir without ``agent-transcripts/``
  - Malformed text + JSONL transcripts (truncated / garbage)
  - Permission-denied transcript file (skipped, no raise)
  - Schema drift: JSONL record missing ``role``
  - Tracking DB present but with wrong schema → falls back to default model
"""

from __future__ import annotations

import json
import os
import sqlite3
from pathlib import Path

import pytest

from stackunderflow.adapters.cursor_agent import CursorAgentAdapter


_IS_ROOT = hasattr(os, "geteuid") and os.geteuid() == 0


# ── missing / empty source ────────────────────────────────────────────


def test_missing_projects_root_yields_nothing(tmp_path: Path) -> None:
    adapter = CursorAgentAdapter(
        projects_root=tmp_path / "no-such-dir",
        tracking_db=tmp_path / "no-such-db",
    )
    assert list(adapter.enumerate()) == []


def test_empty_projects_root_yields_nothing(tmp_path: Path) -> None:
    root = tmp_path / "projects"
    root.mkdir()
    adapter = CursorAgentAdapter(projects_root=root, tracking_db=tmp_path / "no-db")
    assert list(adapter.enumerate()) == []


def test_project_without_agent_transcripts_subdir(tmp_path: Path) -> None:
    """Project dir exists but has no ``agent-transcripts/``: skip cleanly."""
    root = tmp_path / "projects"
    (root / "proj-no-transcripts").mkdir(parents=True)
    adapter = CursorAgentAdapter(projects_root=root, tracking_db=tmp_path / "no-db")
    assert list(adapter.enumerate()) == []


def test_empty_agent_transcripts_dir(tmp_path: Path) -> None:
    root = tmp_path / "projects"
    (root / "proj" / "agent-transcripts").mkdir(parents=True)
    adapter = CursorAgentAdapter(projects_root=root, tracking_db=tmp_path / "no-db")
    assert list(adapter.enumerate()) == []


# ── malformed transcript content ──────────────────────────────────────


def test_malformed_text_transcript_does_not_raise(tmp_path: Path) -> None:
    """Garbage in a ``.txt`` file: enumerate finds it but read yields nothing
    parseable (no marker lines = no assistant turns) without raising."""
    root = tmp_path / "projects"
    transcripts = root / "proj" / "agent-transcripts"
    transcripts.mkdir(parents=True)
    (transcripts / "11111111-2222-3333-4444-555555555555.txt").write_text(
        "this is not a transcript\n"
        "no markers here\n"
        "\x00\x01\x02 binary garbage\n"
    )
    adapter = CursorAgentAdapter(projects_root=root, tracking_db=tmp_path / "no-db")
    refs = list(adapter.enumerate())
    assert len(refs) == 1  # file enumerated
    records = list(adapter.read(refs[0]))
    # No "A:" markers → no assistant records, but no exception either.
    assert records == []


def test_truncated_jsonl_transcript_yields_only_valid_lines(tmp_path: Path) -> None:
    """Truncated trailing line is skipped; valid earlier records still surface."""
    root = tmp_path / "projects"
    sub = root / "proj" / "agent-transcripts" / "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee"
    sub.mkdir(parents=True)
    fp = sub / "session.jsonl"
    fp.write_text(
        json.dumps({"role": "user", "message": {"content": "hi"}}) + "\n"
        + json.dumps(
            {
                "role": "assistant",
                "message": {"content": [{"type": "text", "text": "ok"}]},
            }
        )
        + "\n"
        + '{"role":"assistant","message":{"content":[{"type":"text","text":"trun'
    )
    adapter = CursorAgentAdapter(projects_root=root, tracking_db=tmp_path / "no-db")
    refs = list(adapter.enumerate())
    records = list(adapter.read(refs[0]))
    # One valid assistant record from the second line; truncated third line skipped.
    assert len(records) == 1
    assert records[0].role == "assistant"


def test_garbage_jsonl_lines_are_skipped(tmp_path: Path) -> None:
    root = tmp_path / "projects"
    sub = root / "proj" / "agent-transcripts" / "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee"
    sub.mkdir(parents=True)
    (sub / "session.jsonl").write_text(
        "not json at all\n"
        "{}\n"
        '{"role":"assistant","message":{"content":[{"type":"text","text":"ok"}]}}\n'
    )
    adapter = CursorAgentAdapter(projects_root=root, tracking_db=tmp_path / "no-db")
    refs = list(adapter.enumerate())
    records = list(adapter.read(refs[0]))
    assert len(records) == 1
    assert records[0].content_text == "ok"


# ── schema drift ──────────────────────────────────────────────────────


def test_jsonl_record_missing_role_is_skipped(tmp_path: Path) -> None:
    root = tmp_path / "projects"
    sub = root / "proj" / "agent-transcripts" / "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee"
    sub.mkdir(parents=True)
    (sub / "session.jsonl").write_text(
        json.dumps({"message": {"content": "no role here"}}) + "\n"
        + json.dumps(
            {
                "role": "assistant",
                "message": {"content": [{"type": "text", "text": "valid"}]},
            }
        )
        + "\n"
    )
    adapter = CursorAgentAdapter(projects_root=root, tracking_db=tmp_path / "no-db")
    refs = list(adapter.enumerate())
    records = list(adapter.read(refs[0]))
    assert len(records) == 1
    assert records[0].content_text == "valid"


# ── attribution DB drift ──────────────────────────────────────────────


def test_tracking_db_with_wrong_schema_falls_back_to_default(tmp_path: Path) -> None:
    """A tracking DB without ``conversation_summaries`` does not raise — the
    adapter logs and uses the ``cursor-agent`` default model."""
    root = tmp_path / "projects"
    transcripts = root / "proj" / "agent-transcripts"
    transcripts.mkdir(parents=True)
    fp = transcripts / "11111111-2222-3333-4444-555555555555.txt"
    fp.write_text("user: hi\nA: hello\n")

    db_path = tmp_path / "tracking.db"
    conn = sqlite3.connect(db_path)
    try:
        conn.execute("CREATE TABLE wrong_table (foo TEXT)")
        conn.commit()
    finally:
        conn.close()

    adapter = CursorAgentAdapter(projects_root=root, tracking_db=db_path)
    refs = list(adapter.enumerate())
    records = list(adapter.read(refs[0]))
    assert len(records) == 1
    assert records[0].model == "cursor-agent"


def test_tracking_db_corrupt_file_does_not_raise(tmp_path: Path) -> None:
    """A corrupt SQLite file is treated as "no model info"."""
    root = tmp_path / "projects"
    transcripts = root / "proj" / "agent-transcripts"
    transcripts.mkdir(parents=True)
    fp = transcripts / "11111111-2222-3333-4444-555555555555.txt"
    fp.write_text("user: hi\nA: hello\n")

    db_path = tmp_path / "corrupt.db"
    db_path.write_bytes(b"not a sqlite file at all")

    adapter = CursorAgentAdapter(projects_root=root, tracking_db=db_path)
    refs = list(adapter.enumerate())
    records = list(adapter.read(refs[0]))
    assert len(records) == 1
    assert records[0].model == "cursor-agent"


# ── permission denied ─────────────────────────────────────────────────


@pytest.mark.skipif(_IS_ROOT, reason="root bypasses chmod 000")
def test_permission_denied_text_transcript_is_skipped(tmp_path: Path) -> None:
    """Unreadable transcript file: enumerate succeeds (stat works), read yields
    nothing without raising."""
    root = tmp_path / "projects"
    transcripts = root / "proj" / "agent-transcripts"
    transcripts.mkdir(parents=True)
    fp = transcripts / "11111111-2222-3333-4444-555555555555.txt"
    fp.write_text("user: hi\nA: hello\n")
    fp.chmod(0o000)
    try:
        adapter = CursorAgentAdapter(projects_root=root, tracking_db=tmp_path / "no-db")
        refs = list(adapter.enumerate())
        # enumerate may or may not yield this ref depending on stat permission;
        # what we care about is read() not raising.
        if refs:
            records = list(adapter.read(refs[0]))
            assert records == []
    finally:
        fp.chmod(0o644)
