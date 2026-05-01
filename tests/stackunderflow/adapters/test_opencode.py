"""Unit tests for the OpenCode (SQLite) adapter.

Builds a synthetic ``opencode.db`` file in ``tmp_path`` carrying one
session, two messages (one with reasoning tokens, one with cache tokens),
and a few text + tool ``part`` rows. Exercises ``enumerate`` / ``read`` /
``read(since_offset=...)`` end-to-end. Inherits the shared
``AdapterContract`` mixin so the storage-aware resume invariant runs
against the database-backed adapter.

codeburn-catalog §11.
"""

from __future__ import annotations

import json
import sqlite3
import unittest
from pathlib import Path

import pytest

from stackunderflow.adapters.base import Record, SessionRef
from stackunderflow.adapters.opencode import OpenCodeAdapter
from tests.stackunderflow.adapters.contract import AdapterContract


SESSION_ID = "sess-aaa-111"
OTHER_SESSION_ID = "sess-bbb-222"


def _build_fixture(db_path: Path) -> None:
    """Create an opencode-shaped SQLite file with one session + 2 messages."""
    conn = sqlite3.connect(db_path)
    try:
        conn.executescript(
            """
            CREATE TABLE session (
                id TEXT PRIMARY KEY,
                directory TEXT,
                title TEXT,
                time_created INTEGER,
                time_archived INTEGER,
                parent_id TEXT
            );
            CREATE TABLE message (
                id TEXT PRIMARY KEY,
                session_id TEXT,
                time_created INTEGER,
                data TEXT
            );
            CREATE TABLE part (
                message_id TEXT,
                session_id TEXT,
                data TEXT
            );
            """
        )

        conn.execute(
            "INSERT INTO session VALUES (?, ?, ?, ?, ?, ?)",
            (SESSION_ID, "/tmp/proj", "My Project", 1714000000000, 0, None),
        )
        # A second session so enumerate emits two refs.
        conn.execute(
            "INSERT INTO session VALUES (?, ?, ?, ?, ?, ?)",
            (
                OTHER_SESSION_ID,
                "/tmp/other",
                "Other",
                1714000020000,
                0,
                None,
            ),
        )

        # Message 1: assistant w/ reasoning tokens (tests the
        # output = output + reasoning mapping).
        msg1 = {
            "role": "assistant",
            "modelID": "claude-sonnet-4-6",
            "tokens": {
                "input": 100,
                "output": 50,
                "reasoning": 30,
                "cache": {"read": 0, "write": 0},
            },
            "cost": 0.0123,
        }
        conn.execute(
            "INSERT INTO message VALUES (?, ?, ?, ?)",
            ("msg-1", SESSION_ID, 1714000001000, json.dumps(msg1)),
        )
        # Two parts on msg-1: one text + one tool.
        conn.execute(
            "INSERT INTO part VALUES (?, ?, ?)",
            (
                "msg-1",
                SESSION_ID,
                json.dumps({"type": "text", "text": "Here is the answer."}),
            ),
        )
        conn.execute(
            "INSERT INTO part VALUES (?, ?, ?)",
            (
                "msg-1",
                SESSION_ID,
                json.dumps({"type": "tool", "tool": "Bash", "state": {"input": {"command": "ls"}}}),
            ),
        )

        # Message 2: assistant w/ cache tokens.
        msg2 = {
            "role": "assistant",
            "modelID": "claude-sonnet-4-6",
            "tokens": {
                "input": 200,
                "output": 80,
                "reasoning": 0,
                "cache": {"read": 1500, "write": 300},
            },
        }
        conn.execute(
            "INSERT INTO message VALUES (?, ?, ?, ?)",
            ("msg-2", SESSION_ID, 1714000002000, json.dumps(msg2)),
        )
        conn.execute(
            "INSERT INTO part VALUES (?, ?, ?)",
            (
                "msg-2",
                SESSION_ID,
                json.dumps({"type": "text", "text": "Follow-up reply."}),
            ),
        )

        # A message in the OTHER session — must be filtered out of
        # SESSION_ID's read.
        conn.execute(
            "INSERT INTO message VALUES (?, ?, ?, ?)",
            (
                "msg-other",
                OTHER_SESSION_ID,
                1714000003000,
                json.dumps(
                    {
                        "role": "assistant",
                        "modelID": "gpt-4o-mini",
                        "tokens": {"input": 5, "output": 5},
                    }
                ),
            ),
        )

        conn.commit()
    finally:
        conn.close()


@pytest.fixture()
def opencode_dir(tmp_path: Path) -> Path:
    """Return a data dir containing one synthetic opencode.db."""
    db = tmp_path / "opencode.db"
    _build_fixture(db)
    return tmp_path


# ── targeted tests ────────────────────────────────────────────────────


def test_enumerate_yields_one_ref_per_session(opencode_dir: Path) -> None:
    adapter = OpenCodeAdapter(data_dir=opencode_dir)
    refs = list(adapter.enumerate())

    sessions = {r.source_hint["session_id"] for r in refs}
    assert sessions == {SESSION_ID, OTHER_SESSION_ID}

    ref = next(r for r in refs if r.source_hint["session_id"] == SESSION_ID)
    assert isinstance(ref, SessionRef)
    assert ref.provider == "opencode"
    assert ref.project_slug == "opencode"
    assert ref.source_kind == "database"
    # Session_id is encoded as {db_basename}:{inner_id}.
    assert ref.session_id == f"opencode.db:{SESSION_ID}"
    assert ref.source_hint["db_path"] == str(opencode_dir / "opencode.db")
    assert ref.file_size > 0


def test_enumerate_returns_empty_when_data_dir_missing(tmp_path: Path) -> None:
    adapter = OpenCodeAdapter(data_dir=tmp_path / "nope")
    assert list(adapter.enumerate()) == []


def test_enumerate_finds_multiple_dbs(tmp_path: Path) -> None:
    """Two ``opencode*.db`` files both contribute refs."""
    db1 = tmp_path / "opencode.db"
    db2 = tmp_path / "opencode-2.db"
    _build_fixture(db1)
    _build_fixture(db2)

    adapter = OpenCodeAdapter(data_dir=tmp_path)
    refs = list(adapter.enumerate())
    encoded = {r.session_id for r in refs}
    assert f"opencode.db:{SESSION_ID}" in encoded
    assert f"opencode-2.db:{SESSION_ID}" in encoded


def test_read_yields_records_filtered_to_target_session(opencode_dir: Path) -> None:
    adapter = OpenCodeAdapter(data_dir=opencode_dir)
    refs = [
        r for r in adapter.enumerate()
        if (r.source_hint or {}).get("session_id") == SESSION_ID
    ]
    assert len(refs) == 1
    records = list(adapter.read(refs[0]))
    assert len(records) == 2
    assert all(isinstance(r, Record) for r in records)
    seqs = [r.seq for r in records]
    assert seqs == sorted(seqs)


def test_read_reasoning_folds_into_output_tokens(opencode_dir: Path) -> None:
    """``output_tokens == tokens.output + tokens.reasoning`` per spec."""
    adapter = OpenCodeAdapter(data_dir=opencode_dir)
    ref = next(
        r for r in adapter.enumerate()
        if (r.source_hint or {}).get("session_id") == SESSION_ID
    )
    records = list(adapter.read(ref))
    msg1 = records[0]
    # Message 1 had output=50, reasoning=30 → 80.
    assert msg1.input_tokens == 100
    assert msg1.output_tokens == 80
    assert msg1.cache_create_tokens == 0
    assert msg1.cache_read_tokens == 0


def test_read_cache_tokens_map_to_canonical_keys(opencode_dir: Path) -> None:
    adapter = OpenCodeAdapter(data_dir=opencode_dir)
    ref = next(
        r for r in adapter.enumerate()
        if (r.source_hint or {}).get("session_id") == SESSION_ID
    )
    records = list(adapter.read(ref))
    msg2 = records[1]
    assert msg2.input_tokens == 200
    assert msg2.output_tokens == 80
    # cache.read → cache_read_tokens, cache.write → cache_create_tokens.
    assert msg2.cache_read_tokens == 1500
    assert msg2.cache_create_tokens == 300


def test_read_embedded_cost_stamped_on_raw(opencode_dir: Path) -> None:
    adapter = OpenCodeAdapter(data_dir=opencode_dir)
    ref = next(
        r for r in adapter.enumerate()
        if (r.source_hint or {}).get("session_id") == SESSION_ID
    )
    records = list(adapter.read(ref))
    assert records[0].raw.get("embedded_cost") == 0.0123


def test_read_content_text_assembles_from_text_parts(opencode_dir: Path) -> None:
    adapter = OpenCodeAdapter(data_dir=opencode_dir)
    ref = next(
        r for r in adapter.enumerate()
        if (r.source_hint or {}).get("session_id") == SESSION_ID
    )
    records = list(adapter.read(ref))
    assert records[0].content_text == "Here is the answer."
    assert records[1].content_text == "Follow-up reply."


def test_read_tools_extracted_from_tool_parts(opencode_dir: Path) -> None:
    adapter = OpenCodeAdapter(data_dir=opencode_dir)
    ref = next(
        r for r in adapter.enumerate()
        if (r.source_hint or {}).get("session_id") == SESSION_ID
    )
    records = list(adapter.read(ref))
    assert records[0].tools == ("Bash",)
    assert records[1].tools == ()


def test_read_resume_with_since_offset_drops_earlier_rows(
    opencode_dir: Path,
) -> None:
    adapter = OpenCodeAdapter(data_dir=opencode_dir)
    ref = next(
        r for r in adapter.enumerate()
        if (r.source_hint or {}).get("session_id") == SESSION_ID
    )
    full = list(adapter.read(ref))
    midpoint = full[0].seq
    resumed = list(adapter.read(ref, since_offset=midpoint))
    assert all(r.seq > midpoint for r in resumed)
    assert len(resumed) == 1


def test_record_uuid_uses_public_session_id(opencode_dir: Path) -> None:
    adapter = OpenCodeAdapter(data_dir=opencode_dir)
    ref = next(
        r for r in adapter.enumerate()
        if (r.source_hint or {}).get("session_id") == SESSION_ID
    )
    records = list(adapter.read(ref))
    for rec in records:
        assert rec.uuid == f"{ref.session_id}:{rec.seq}"
        assert rec.parent_uuid is None


# ── shared adapter contract ────────────────────────────────────────────


class TestOpenCodeAdapterContract(unittest.TestCase, AdapterContract):
    """Runs every AdapterContract invariant against the OpenCode fixture."""

    def setUp(self) -> None:
        import tempfile

        self._tmpdir = tempfile.TemporaryDirectory()
        data_dir = Path(self._tmpdir.name)
        _build_fixture(data_dir / "opencode.db")
        self.adapter = OpenCodeAdapter(data_dir=data_dir)

    def tearDown(self) -> None:
        self._tmpdir.cleanup()
