"""Unit tests for the Continue (defensive SQLite parser) adapter.

Two fixture cases:

  1. **Empty state** — ``~/.continue/`` exists but has no SQLite DB or
     a DB with no sessions-shaped table. ``enumerate()`` yields nothing
     and the adapter does not raise.

  2. **Synthetic schema** — a hand-rolled SQLite DB with a ``sessions``
     and ``messages`` table mimicking what Continue *might* produce.
     Asserts ``enumerate()`` yields one ``SessionRef`` per sessions-row,
     ``read()`` filters messages by sessionId, and malformed rows are
     skipped (not raised).
"""

from __future__ import annotations

import sqlite3
import unittest
from pathlib import Path

import pytest

from stackunderflow.adapters.base import Record, SessionRef
from stackunderflow.adapters.continue_adapter import ContinueAdapter
from tests.stackunderflow.adapters.contract import AdapterContract


# ── fixture builders ──────────────────────────────────────────────────


def _build_continue_db(path: Path) -> None:
    """Create a small sessions+messages SQLite that the adapter can sniff."""
    conn = sqlite3.connect(path)
    try:
        conn.execute("""
            CREATE TABLE sessions (
                id TEXT PRIMARY KEY,
                title TEXT,
                createdAt INTEGER
            )
        """)
        conn.execute("""
            CREATE TABLE messages (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                sessionId TEXT,
                role TEXT,
                content TEXT,
                model TEXT,
                inputTokens INTEGER,
                outputTokens INTEGER,
                createdAt INTEGER
            )
        """)
        conn.executemany(
            "INSERT INTO sessions(id, title, createdAt) VALUES (?, ?, ?)",
            [
                ("sess-aaa", "Refactor the parser", 1714000000000),
                ("sess-bbb", "Write tests", 1714000010000),
            ],
        )
        conn.executemany(
            "INSERT INTO messages"
            "(sessionId, role, content, model, inputTokens, outputTokens, createdAt)"
            "VALUES (?, ?, ?, ?, ?, ?, ?)",
            [
                ("sess-aaa", "user", "Refactor please", "claude-3-5-sonnet",
                 0, 0, 1714000001000),
                ("sess-aaa", "assistant", "Done.", "claude-3-5-sonnet",
                 30, 80, 1714000002000),
                ("sess-aaa", "assistant", "Follow-up.", "claude-3-5-sonnet",
                 40, 100, 1714000003000),
                # Different session — must NOT show up in sess-aaa read.
                ("sess-bbb", "user", "Hi", "claude-3-5-sonnet", 0, 0, 1714000004000),
            ],
        )
        conn.commit()
    finally:
        conn.close()


@pytest.fixture
def continue_root(tmp_path: Path) -> Path:
    root = tmp_path / ".continue"
    root.mkdir()
    _build_continue_db(root / "index.db")
    return root


# ── empty-state behaviour ─────────────────────────────────────────────


def test_enumerate_yields_nothing_when_root_missing(tmp_path: Path) -> None:
    adapter = ContinueAdapter(root=tmp_path / "does-not-exist")
    assert list(adapter.enumerate()) == []


def test_enumerate_yields_nothing_for_empty_continue_dir(tmp_path: Path) -> None:
    root = tmp_path / ".continue"
    root.mkdir()
    adapter = ContinueAdapter(root=root)
    assert list(adapter.enumerate()) == []


def test_enumerate_yields_nothing_when_no_sessions_table(tmp_path: Path) -> None:
    """A DB without a sessions-shaped table is a clean no-op."""
    root = tmp_path / ".continue"
    root.mkdir()
    db_path = root / "irrelevant.db"
    conn = sqlite3.connect(db_path)
    conn.execute("CREATE TABLE blobs (id INTEGER PRIMARY KEY, data BLOB)")
    conn.commit()
    conn.close()
    adapter = ContinueAdapter(root=root)
    assert list(adapter.enumerate()) == []


# ── populated-DB behaviour ────────────────────────────────────────────


def test_enumerate_yields_one_ref_per_session_row(continue_root: Path) -> None:
    adapter = ContinueAdapter(root=continue_root)
    refs = list(adapter.enumerate())
    by_id = {r.session_id: r for r in refs}
    assert set(by_id.keys()) == {"sess-aaa", "sess-bbb"}
    ref = by_id["sess-aaa"]
    assert isinstance(ref, SessionRef)
    assert ref.provider == "continue"
    assert ref.source_kind == "database"
    hint = ref.source_hint or {}
    assert hint.get("sessions_table") == "sessions"
    assert hint.get("messages_table") == "messages"


def test_read_filters_messages_by_session_id(continue_root: Path) -> None:
    adapter = ContinueAdapter(root=continue_root)
    refs = [r for r in adapter.enumerate() if r.session_id == "sess-aaa"]
    records = list(adapter.read(refs[0]))
    # 3 messages in sess-aaa, 1 in sess-bbb → 3 yielded for sess-aaa.
    assert len(records) == 3
    for rec in records:
        assert isinstance(rec, Record)
        assert rec.session_id == "sess-aaa"
    seqs = [r.seq for r in records]
    assert seqs == sorted(seqs)


def test_read_assistant_record_uses_explicit_tokens(continue_root: Path) -> None:
    adapter = ContinueAdapter(root=continue_root)
    refs = [r for r in adapter.enumerate() if r.session_id == "sess-aaa"]
    records = list(adapter.read(refs[0]))
    assistants = [r for r in records if r.role == "assistant"]
    assert len(assistants) == 2
    # First assistant row has explicit (30, 80).
    first = assistants[0]
    assert first.input_tokens == 30
    assert first.output_tokens == 80
    assert first.model == "claude-3-5-sonnet"
    assert first.raw.get("cost_source") != "estimated"


def test_read_estimates_when_tokens_missing(tmp_path: Path) -> None:
    """A row with no token columns falls back to len/4 estimation."""
    root = tmp_path / ".continue"
    root.mkdir()
    db_path = root / "minimal.db"
    conn = sqlite3.connect(db_path)
    try:
        conn.execute(
            "CREATE TABLE sessions (id TEXT PRIMARY KEY, title TEXT, createdAt INTEGER)"
        )
        conn.execute(
            "CREATE TABLE messages ("
            " id INTEGER PRIMARY KEY AUTOINCREMENT,"
            " sessionId TEXT, role TEXT, content TEXT, createdAt INTEGER)"
        )
        conn.execute(
            "INSERT INTO sessions(id, title, createdAt) VALUES (?, ?, ?)",
            ("sess-est", "estimate me", 1714000000000),
        )
        body = "x" * 200  # 200 // 4 == 50 tokens
        conn.execute(
            "INSERT INTO messages(sessionId, role, content, createdAt) "
            "VALUES (?, ?, ?, ?)",
            ("sess-est", "assistant", body, 1714000001000),
        )
        conn.commit()
    finally:
        conn.close()

    adapter = ContinueAdapter(root=root)
    refs = list(adapter.enumerate())
    assert len(refs) == 1
    records = list(adapter.read(refs[0]))
    assert len(records) == 1
    rec = records[0]
    assert rec.output_tokens == 50
    assert rec.input_tokens == 0
    assert rec.model == "continue-auto"
    assert rec.raw.get("cost_source") == "estimated"


def test_read_skips_malformed_rows_without_raising(tmp_path: Path) -> None:
    """A row with no role is skipped; the iterator continues to the next."""
    root = tmp_path / ".continue"
    root.mkdir()
    db_path = root / "messy.db"
    conn = sqlite3.connect(db_path)
    try:
        conn.execute(
            "CREATE TABLE sessions (id TEXT PRIMARY KEY, title TEXT, createdAt INTEGER)"
        )
        conn.execute(
            "CREATE TABLE messages ("
            " id INTEGER PRIMARY KEY AUTOINCREMENT,"
            " sessionId TEXT, role TEXT, content TEXT, createdAt INTEGER)"
        )
        conn.execute(
            "INSERT INTO sessions(id, title, createdAt) VALUES (?, ?, ?)",
            ("sess-mix", "mixed", 1714000000000),
        )
        conn.executemany(
            "INSERT INTO messages(sessionId, role, content, createdAt) "
            "VALUES (?, ?, ?, ?)",
            [
                ("sess-mix", None, "no role", 1714000001000),
                ("sess-mix", "assistant", "valid", 1714000002000),
                ("sess-mix", "", "empty role", 1714000003000),
            ],
        )
        conn.commit()
    finally:
        conn.close()

    adapter = ContinueAdapter(root=root)
    refs = list(adapter.enumerate())
    records = list(adapter.read(refs[0]))
    # Only the one valid row survives.
    assert len(records) == 1
    assert records[0].content_text == "valid"


def test_read_resumes_with_since_offset(continue_root: Path) -> None:
    adapter = ContinueAdapter(root=continue_root)
    refs = [r for r in adapter.enumerate() if r.session_id == "sess-aaa"]
    full = list(adapter.read(refs[0]))
    midpoint = full[len(full) // 2].seq
    resumed = list(adapter.read(refs[0], since_offset=midpoint))
    assert all(r.seq > midpoint for r in resumed)
    assert len(resumed) < len(full)


# ── shared adapter contract ───────────────────────────────────────────


class TestContinueAdapterContract(unittest.TestCase, AdapterContract):
    """Run the shared invariants against the populated synthetic DB."""

    def setUp(self) -> None:
        import tempfile

        self._tmp = tempfile.TemporaryDirectory()
        root = Path(self._tmp.name) / ".continue"
        root.mkdir()
        _build_continue_db(root / "index.db")
        self.adapter = ContinueAdapter(root=root)

    def tearDown(self) -> None:
        self._tmp.cleanup()
