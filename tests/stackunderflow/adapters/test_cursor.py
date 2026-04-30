"""Unit tests for the Cursor (vscdb) adapter.

Builds a synthetic ``state.vscdb`` SQLite fixture in ``tmp_path`` carrying
two ``bubbleId:%`` rows (one user, one assistant with explicit
``tokenCount``) and one ``agentKv:blob:%`` row, all sharing one
``conversationId``. Then exercises ``enumerate`` / ``read`` /
``read(since_offset=...)`` end-to-end. Inherits the shared
``AdapterContract`` mixin so the storage-aware resume invariant runs
against a database-backed adapter.
"""

from __future__ import annotations

import json
import sqlite3
import unittest
from pathlib import Path

import pytest

from stackunderflow.adapters.base import Record, SessionRef
from stackunderflow.adapters.cursor import CursorAdapter
from tests.stackunderflow.adapters.contract import AdapterContract


CONV_ID = "conv-abc-123"
OTHER_CONV_ID = "conv-other-999"


def _build_fixture(path: Path) -> None:
    """Create a vscdb-shaped SQLite file with 3 rows for one conversation."""
    conn = sqlite3.connect(path)
    try:
        conn.execute(
            "CREATE TABLE cursorDiskKV (key TEXT PRIMARY KEY, value BLOB)"
        )
        rows = [
            (
                "bubbleId:b1",
                json.dumps(
                    {
                        "conversationId": CONV_ID,
                        "type": 1,  # user
                        "text": "Refactor this please.",
                        "modelInfo": {"modelName": "claude-sonnet-4-6"},
                        "tokenCount": {"inputTokens": 0, "outputTokens": 0},
                        "createdAt": 1714000000000,
                    }
                ),
            ),
            (
                "bubbleId:b2",
                json.dumps(
                    {
                        "conversationId": CONV_ID,
                        "type": 2,  # assistant
                        "text": "Here is a refactor.",
                        "modelInfo": {"modelName": "claude-sonnet-4-6"},
                        "tokenCount": {"inputTokens": 120, "outputTokens": 480},
                        "createdAt": 1714000010000,
                    }
                ),
            ),
            (
                "agentKv:blob:k1",
                json.dumps(
                    {
                        "conversationId": CONV_ID,
                        "role": "tool",
                        "content": [{"type": "text", "text": "ran tests"}],
                        "providerOptions": {
                            "cursor": {
                                "modelName": "cursor-auto",
                                "requestId": "req-xyz",
                            }
                        },
                        "createdAt": "2026-04-29T10:00:00Z",
                    }
                ),
            ),
            (
                # A row from a different conversation — must NOT appear in
                # the per-conversation read for CONV_ID.
                "bubbleId:b3",
                json.dumps(
                    {
                        "conversationId": OTHER_CONV_ID,
                        "type": 1,
                        "text": "Different convo.",
                        "modelInfo": {"modelName": "claude-sonnet-4-6"},
                        "tokenCount": {"inputTokens": 0, "outputTokens": 0},
                        "createdAt": 1714000020000,
                    }
                ),
            ),
        ]
        conn.executemany(
            "INSERT INTO cursorDiskKV(key, value) VALUES (?, ?)", rows
        )
        conn.commit()
    finally:
        conn.close()


@pytest.fixture()
def vscdb_path(tmp_path: Path) -> Path:
    fp = tmp_path / "state.vscdb"
    _build_fixture(fp)
    return fp


# ── targeted tests ────────────────────────────────────────────────────


def test_enumerate_yields_one_session_ref_per_conversation(vscdb_path: Path) -> None:
    adapter = CursorAdapter(vscdb_path=vscdb_path)
    refs = list(adapter.enumerate())
    by_id = {r.session_id: r for r in refs}
    assert set(by_id.keys()) == {CONV_ID, OTHER_CONV_ID}
    ref = by_id[CONV_ID]
    assert isinstance(ref, SessionRef)
    assert ref.provider == "cursor"
    assert ref.project_slug == "cursor"
    assert ref.source_kind == "database"
    assert ref.source_hint == {"conversation_id": CONV_ID}
    assert ref.file_path == vscdb_path
    assert ref.file_size > 0
    assert ref.file_mtime > 0


def test_enumerate_returns_empty_when_db_missing(tmp_path: Path) -> None:
    """Missing vscdb is not an error — Cursor simply isn't installed."""
    missing = tmp_path / "does-not-exist.vscdb"
    adapter = CursorAdapter(vscdb_path=missing)
    assert list(adapter.enumerate()) == []


def test_read_yields_records_for_target_conversation(vscdb_path: Path) -> None:
    adapter = CursorAdapter(vscdb_path=vscdb_path)
    refs = [r for r in adapter.enumerate() if r.session_id == CONV_ID]
    assert len(refs) == 1
    records = list(adapter.read(refs[0]))
    # 2 bubbles + 1 agentKv = 3, all in CONV_ID; the OTHER_CONV_ID row
    # is filtered out.
    assert len(records) == 3
    seqs = [r.seq for r in records]
    assert seqs == sorted(seqs)
    assert all(isinstance(r, Record) for r in records)
    roles = [r.role for r in records]
    assert "user" in roles and "assistant" in roles and "tool" in roles


def test_read_assistant_record_has_explicit_tokens(vscdb_path: Path) -> None:
    adapter = CursorAdapter(vscdb_path=vscdb_path)
    refs = [r for r in adapter.enumerate() if r.session_id == CONV_ID]
    records = list(adapter.read(refs[0]))
    assistant = next(r for r in records if r.role == "assistant")
    assert assistant.input_tokens == 120
    assert assistant.output_tokens == 480
    assert assistant.cache_create_tokens == 0
    assert assistant.cache_read_tokens == 0
    assert assistant.model == "claude-sonnet-4-6"
    # Explicit tokens => not estimated.
    assert assistant.raw.get("cost_source") != "estimated"


def test_read_user_record_estimates_tokens_when_zero(vscdb_path: Path) -> None:
    adapter = CursorAdapter(vscdb_path=vscdb_path)
    refs = [r for r in adapter.enumerate() if r.session_id == CONV_ID]
    records = list(adapter.read(refs[0]))
    user = next(r for r in records if r.role == "user")
    # "Refactor this please." is 21 chars → 21 // 4 == 5
    assert user.input_tokens == len("Refactor this please.") // 4
    assert user.output_tokens == 0
    assert user.raw.get("cost_source") == "estimated"


def test_read_agent_kv_record_uses_provider_options_model(vscdb_path: Path) -> None:
    adapter = CursorAdapter(vscdb_path=vscdb_path)
    refs = [r for r in adapter.enumerate() if r.session_id == CONV_ID]
    records = list(adapter.read(refs[0]))
    tool = next(r for r in records if r.role == "tool")
    assert tool.model == "cursor-auto"
    assert tool.content_text == "ran tests"


def test_read_since_offset_drops_earlier_rows(vscdb_path: Path) -> None:
    adapter = CursorAdapter(vscdb_path=vscdb_path)
    refs = [r for r in adapter.enumerate() if r.session_id == CONV_ID]
    full = list(adapter.read(refs[0]))
    midpoint = full[len(full) // 2].seq
    resumed = list(adapter.read(refs[0], since_offset=midpoint))
    assert all(r.seq > midpoint for r in resumed)
    assert len(resumed) < len(full)


def test_record_uuid_is_stable_session_plus_rowid(vscdb_path: Path) -> None:
    adapter = CursorAdapter(vscdb_path=vscdb_path)
    refs = [r for r in adapter.enumerate() if r.session_id == CONV_ID]
    records = list(adapter.read(refs[0]))
    for rec in records:
        assert rec.uuid == f"{CONV_ID}:{rec.seq}"
        assert rec.parent_uuid is None


# ── shared adapter contract ────────────────────────────────────────────


class TestCursorAdapterContract(unittest.TestCase, AdapterContract):
    """Runs every AdapterContract invariant against the Cursor fixture."""

    def setUp(self) -> None:
        # Build a fresh fixture per test method into a tmpdir we own.
        import tempfile

        self._tmpdir = tempfile.TemporaryDirectory()
        path = Path(self._tmpdir.name) / "state.vscdb"
        _build_fixture(path)
        self.adapter = CursorAdapter(vscdb_path=path)

    def tearDown(self) -> None:
        self._tmpdir.cleanup()
