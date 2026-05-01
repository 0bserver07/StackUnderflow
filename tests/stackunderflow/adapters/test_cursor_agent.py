"""Unit tests for the Cursor Agent (transcripts + SQLite metadata) adapter.

Builds two synthetic transcripts in ``tmp_path``:

- A legacy ``.txt`` transcript with the documented marker set (``user:``,
  ``A:``, ``[Tool call]``).
- A Composer 2 ``.jsonl`` transcript laid out under a UUID-named subdir.

A small SQLite attribution DB (``ai-code-tracking.db``) is also built so
the model-lookup path is exercised. Both transcript types must:

- Parse cleanly into Records.
- Stamp ``cost_source="estimated"`` on every Record (no explicit tokens).
- Emit monotonically increasing ``seq`` values.

Inherits the shared ``AdapterContract`` mixin.

codeburn-catalog §5.
"""

from __future__ import annotations

import json
import sqlite3
import unittest
from pathlib import Path

import pytest

from stackunderflow.adapters.base import Record
from stackunderflow.adapters.cursor_agent import CursorAgentAdapter
from tests.stackunderflow.adapters.contract import AdapterContract


# Stable UUIDs so tests can assert session_id values.
TEXT_UUID = "11111111-2222-3333-4444-555555555555"
JSONL_UUID = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee"


# ── fixture builders ──────────────────────────────────────────────────


def _write_text_transcript(transcripts_dir: Path, *, uuid: str) -> Path:
    """Create one legacy text transcript with two assistant turns."""
    fp = transcripts_dir / f"{uuid}.txt"
    fp.write_text(
        "user: Refactor this module please.\n"
        "A: Sure, here's a plan.\n"
        "[Tool call] Read path=foo.py\n"
        "[Tool result] ok\n"
        "A: Done.\n"
        "user: Now run the tests.\n"
        "A: Tests pass.\n"
    )
    return fp


def _write_jsonl_transcript(transcripts_dir: Path, *, uuid: str) -> Path:
    """Create one Composer 2 JSONL transcript with two assistant messages."""
    sub = transcripts_dir / uuid
    sub.mkdir()
    fp = sub / "session.jsonl"
    lines = [
        json.dumps(
            {
                "role": "user",
                "message": {"content": [{"type": "text", "text": "Hello there."}]},
            }
        ),
        json.dumps(
            {
                "role": "assistant",
                "message": {
                    "content": [
                        {"type": "text", "text": "Hi! How can I help?"},
                        {"type": "tool_use", "name": "Read"},
                    ]
                },
            }
        ),
        json.dumps(
            {
                "role": "user",
                "message": {"content": [{"type": "text", "text": "Refactor this."}]},
            }
        ),
        json.dumps(
            {
                "role": "assistant",
                "message": {
                    "content": [{"type": "text", "text": "Done."}]
                },
            }
        ),
    ]
    fp.write_text("\n".join(lines) + "\n")
    return fp


def _build_attribution_db(db_path: Path, *, model_for: dict[str, str]) -> None:
    """Synthesize the ai-code-tracking.db with one row per session."""
    db_path.parent.mkdir(parents=True, exist_ok=True)
    conn = sqlite3.connect(db_path)
    try:
        conn.executescript(
            """
            CREATE TABLE conversation_summaries (
                conversationId TEXT PRIMARY KEY,
                model TEXT,
                updatedAt INTEGER
            );
            """
        )
        for sid, model in model_for.items():
            conn.execute(
                "INSERT INTO conversation_summaries VALUES (?, ?, ?)",
                (sid, model, 1714000000000),
            )
        conn.commit()
    finally:
        conn.close()


@pytest.fixture()
def cursor_agent_root(tmp_path: Path) -> tuple[Path, Path]:
    """Return ``(projects_root, tracking_db)`` with one project + DB."""
    projects_root = tmp_path / "projects"
    project_dir = projects_root / "-Users-yad-myproj"
    transcripts_dir = project_dir / "agent-transcripts"
    transcripts_dir.mkdir(parents=True)

    _write_text_transcript(transcripts_dir, uuid=TEXT_UUID)
    _write_jsonl_transcript(transcripts_dir, uuid=JSONL_UUID)

    tracking_db = tmp_path / "ai-tracking" / "ai-code-tracking.db"
    _build_attribution_db(
        tracking_db,
        model_for={
            TEXT_UUID: "claude-sonnet-4-6",
            JSONL_UUID: "claude-sonnet-4-6",
        },
    )
    return projects_root, tracking_db


# ── enumerate ─────────────────────────────────────────────────────────


def test_enumerate_yields_one_ref_per_transcript(
    cursor_agent_root: tuple[Path, Path]
) -> None:
    projects_root, tracking_db = cursor_agent_root
    adapter = CursorAgentAdapter(
        projects_root=projects_root, tracking_db=tracking_db
    )
    refs = list(adapter.enumerate())
    by_id = {r.session_id: r for r in refs}
    assert TEXT_UUID in by_id
    assert JSONL_UUID in by_id
    assert all(r.provider == "cursor-agent" for r in refs)
    assert all(r.source_kind == "file" for r in refs)


def test_enumerate_returns_empty_when_root_missing(tmp_path: Path) -> None:
    adapter = CursorAgentAdapter(projects_root=tmp_path / "nope")
    assert list(adapter.enumerate()) == []


def test_enumerate_prettifies_project_name(
    cursor_agent_root: tuple[Path, Path]
) -> None:
    projects_root, tracking_db = cursor_agent_root
    adapter = CursorAgentAdapter(
        projects_root=projects_root, tracking_db=tracking_db
    )
    refs = list(adapter.enumerate())
    # ``-Users-yad-myproj`` → strip leading dash → ``Users-yad-myproj``.
    assert all(r.project_slug == "Users-yad-myproj" for r in refs)


# ── text transcript reader ─────────────────────────────────────────────


def test_read_text_transcript_yields_one_record_per_assistant_turn(
    cursor_agent_root: tuple[Path, Path],
) -> None:
    projects_root, tracking_db = cursor_agent_root
    adapter = CursorAgentAdapter(
        projects_root=projects_root, tracking_db=tracking_db
    )
    ref = next(r for r in adapter.enumerate() if r.session_id == TEXT_UUID)
    records = list(adapter.read(ref))
    # Three "A:" lines in the fixture → three assistant turns.
    assert len(records) == 3
    for rec in records:
        assert isinstance(rec, Record)
        assert rec.role == "assistant"
        assert rec.raw.get("cost_source") == "estimated"


def test_read_text_uses_attribution_db_model(
    cursor_agent_root: tuple[Path, Path],
) -> None:
    projects_root, tracking_db = cursor_agent_root
    adapter = CursorAgentAdapter(
        projects_root=projects_root, tracking_db=tracking_db
    )
    ref = next(r for r in adapter.enumerate() if r.session_id == TEXT_UUID)
    records = list(adapter.read(ref))
    assert all(rec.model == "claude-sonnet-4-6" for rec in records)


def test_read_text_falls_back_to_default_model_when_db_missing(
    cursor_agent_root: tuple[Path, Path], tmp_path: Path
) -> None:
    projects_root, _ = cursor_agent_root
    adapter = CursorAgentAdapter(
        projects_root=projects_root, tracking_db=tmp_path / "nope.db"
    )
    ref = next(r for r in adapter.enumerate() if r.session_id == TEXT_UUID)
    records = list(adapter.read(ref))
    assert all(rec.model == "cursor-agent" for rec in records)


def test_read_text_extracts_tool_call_names(
    cursor_agent_root: tuple[Path, Path],
) -> None:
    projects_root, tracking_db = cursor_agent_root
    adapter = CursorAgentAdapter(
        projects_root=projects_root, tracking_db=tracking_db
    )
    ref = next(r for r in adapter.enumerate() if r.session_id == TEXT_UUID)
    records = list(adapter.read(ref))
    # First assistant turn had a `[Tool call] Read` line.
    assert "Read" in records[0].tools


def test_read_text_seq_is_byte_offset_and_monotonic(
    cursor_agent_root: tuple[Path, Path],
) -> None:
    projects_root, tracking_db = cursor_agent_root
    adapter = CursorAgentAdapter(
        projects_root=projects_root, tracking_db=tracking_db
    )
    ref = next(r for r in adapter.enumerate() if r.session_id == TEXT_UUID)
    records = list(adapter.read(ref))
    seqs = [r.seq for r in records]
    assert seqs == sorted(seqs)
    # seq is a byte offset, so all seqs >= 0 and strictly increasing.
    assert all(s >= 0 for s in seqs)


# ── JSONL transcript reader ────────────────────────────────────────────


def test_read_jsonl_transcript_yields_one_record_per_assistant_message(
    cursor_agent_root: tuple[Path, Path],
) -> None:
    projects_root, tracking_db = cursor_agent_root
    adapter = CursorAgentAdapter(
        projects_root=projects_root, tracking_db=tracking_db
    )
    ref = next(r for r in adapter.enumerate() if r.session_id == JSONL_UUID)
    records = list(adapter.read(ref))
    assert len(records) == 2
    assert all(rec.role == "assistant" for rec in records)
    assert all(rec.raw.get("cost_source") == "estimated" for rec in records)


def test_read_jsonl_extracts_tool_use_names(
    cursor_agent_root: tuple[Path, Path],
) -> None:
    projects_root, tracking_db = cursor_agent_root
    adapter = CursorAgentAdapter(
        projects_root=projects_root, tracking_db=tracking_db
    )
    ref = next(r for r in adapter.enumerate() if r.session_id == JSONL_UUID)
    records = list(adapter.read(ref))
    assert "Read" in records[0].tools
    assert records[1].tools == ()


def test_read_jsonl_uses_attribution_db_model(
    cursor_agent_root: tuple[Path, Path],
) -> None:
    projects_root, tracking_db = cursor_agent_root
    adapter = CursorAgentAdapter(
        projects_root=projects_root, tracking_db=tracking_db
    )
    ref = next(r for r in adapter.enumerate() if r.session_id == JSONL_UUID)
    records = list(adapter.read(ref))
    assert all(rec.model == "claude-sonnet-4-6" for rec in records)


# ── resume (since_offset) ──────────────────────────────────────────────


def test_read_text_resume_skips_earlier_records(
    cursor_agent_root: tuple[Path, Path],
) -> None:
    projects_root, tracking_db = cursor_agent_root
    adapter = CursorAgentAdapter(
        projects_root=projects_root, tracking_db=tracking_db
    )
    ref = next(r for r in adapter.enumerate() if r.session_id == TEXT_UUID)
    full = list(adapter.read(ref))
    midpoint = full[len(full) // 2].seq
    resumed = list(adapter.read(ref, since_offset=midpoint))
    assert all(r.seq > midpoint for r in resumed)
    assert len(resumed) < len(full)


def test_read_jsonl_resume_skips_earlier_records(
    cursor_agent_root: tuple[Path, Path],
) -> None:
    projects_root, tracking_db = cursor_agent_root
    adapter = CursorAgentAdapter(
        projects_root=projects_root, tracking_db=tracking_db
    )
    ref = next(r for r in adapter.enumerate() if r.session_id == JSONL_UUID)
    full = list(adapter.read(ref))
    midpoint = full[0].seq
    resumed = list(adapter.read(ref, since_offset=midpoint))
    assert all(r.seq > midpoint for r in resumed)
    assert len(resumed) < len(full)


# ── shared adapter contract ────────────────────────────────────────────


class TestCursorAgentTextContract(unittest.TestCase, AdapterContract):
    """Run AdapterContract against the text-only fixture."""

    def setUp(self) -> None:
        import tempfile

        self._tmpdir = tempfile.TemporaryDirectory()
        root = Path(self._tmpdir.name)
        projects_root = root / "projects"
        transcripts = projects_root / "proj-text" / "agent-transcripts"
        transcripts.mkdir(parents=True)
        _write_text_transcript(transcripts, uuid=TEXT_UUID)
        # No attribution DB — adapter must still work.
        self.adapter = CursorAgentAdapter(
            projects_root=projects_root,
            tracking_db=root / "missing.db",
        )

    def tearDown(self) -> None:
        self._tmpdir.cleanup()


class TestCursorAgentJSONLContract(unittest.TestCase, AdapterContract):
    """Run AdapterContract against the JSONL-only fixture."""

    def setUp(self) -> None:
        import tempfile

        self._tmpdir = tempfile.TemporaryDirectory()
        root = Path(self._tmpdir.name)
        projects_root = root / "projects"
        transcripts = projects_root / "proj-jsonl" / "agent-transcripts"
        transcripts.mkdir(parents=True)
        _write_jsonl_transcript(transcripts, uuid=JSONL_UUID)
        self.adapter = CursorAgentAdapter(
            projects_root=projects_root,
            tracking_db=root / "missing.db",
        )

    def tearDown(self) -> None:
        self._tmpdir.cleanup()
