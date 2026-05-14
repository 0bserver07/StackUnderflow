"""Defensive empty-source / malformed-data coverage for the Continue adapter.

Continue's on-disk schema is not formally documented; the adapter
introspects whatever DB it finds. These tests pin the empty-state and
schema-drift behaviour so a fresh install (or an unrelated SQLite file
under ``~/.continue/``) does not crash ingest.
"""

from __future__ import annotations

import os
import sqlite3
import sys
from pathlib import Path

import pytest

from stackunderflow.adapters.continue_adapter import ContinueAdapter


_IS_ROOT = hasattr(os, "geteuid") and os.geteuid() == 0
# Windows ignores Unix file permissions; chmod(0o000) is a no-op on NTFS, so the
# permission-denied path under test is unreachable there. Skip those tests on
# Windows the same way we skip them when running as root on POSIX.
_SKIP_CHMOD = _IS_ROOT or sys.platform == "win32"


# ── missing / empty source ────────────────────────────────────────────


def test_missing_root_yields_nothing(tmp_path: Path) -> None:
    adapter = ContinueAdapter(root=tmp_path / "nope")
    assert list(adapter.enumerate()) == []


def test_empty_root_yields_nothing(tmp_path: Path) -> None:
    root = tmp_path / ".continue"
    root.mkdir()
    adapter = ContinueAdapter(root=root)
    assert list(adapter.enumerate()) == []


def test_root_with_only_non_db_files_yields_nothing(tmp_path: Path) -> None:
    root = tmp_path / ".continue"
    root.mkdir()
    (root / "config.json").write_text("{}")
    (root / "logs.txt").write_text("nothing to see here")
    adapter = ContinueAdapter(root=root)
    assert list(adapter.enumerate()) == []


# ── corrupt / mis-shaped DB ───────────────────────────────────────────


def test_corrupt_sqlite_file_does_not_raise(tmp_path: Path) -> None:
    root = tmp_path / ".continue"
    root.mkdir()
    (root / "garbage.db").write_bytes(b"this is not sqlite")
    adapter = ContinueAdapter(root=root)
    assert list(adapter.enumerate()) == []


def test_db_with_unrelated_tables_yields_nothing(tmp_path: Path) -> None:
    """A DB whose tables don't match the sessions sniff signature is skipped."""
    root = tmp_path / ".continue"
    root.mkdir()
    db = root / "irrelevant.db"
    conn = sqlite3.connect(db)
    try:
        conn.execute("CREATE TABLE blobs (id INTEGER PRIMARY KEY, data BLOB)")
        conn.execute("CREATE TABLE settings (key TEXT, value TEXT)")
        conn.commit()
    finally:
        conn.close()
    adapter = ContinueAdapter(root=root)
    assert list(adapter.enumerate()) == []


def test_db_with_sessions_but_no_messages_table(tmp_path: Path) -> None:
    """A sessions-only DB enumerates but read() yields nothing (no messages)."""
    root = tmp_path / ".continue"
    root.mkdir()
    db = root / "sessions-only.db"
    conn = sqlite3.connect(db)
    try:
        conn.execute(
            "CREATE TABLE sessions (id TEXT PRIMARY KEY, title TEXT, createdAt INTEGER)"
        )
        conn.execute(
            "INSERT INTO sessions(id, title, createdAt) VALUES (?, ?, ?)",
            ("s1", "lonely session", 1),
        )
        conn.commit()
    finally:
        conn.close()
    adapter = ContinueAdapter(root=root)
    refs = list(adapter.enumerate())
    assert len(refs) == 1
    # No messages table → read yields nothing.
    assert list(adapter.read(refs[0])) == []


# ── schema drift on a messages row ────────────────────────────────────


def test_messages_table_missing_session_filter_column(tmp_path: Path) -> None:
    """Without a sessionId column, read returns every row across all sessions."""
    root = tmp_path / ".continue"
    root.mkdir()
    db = root / "no-filter.db"
    conn = sqlite3.connect(db)
    try:
        conn.execute(
            "CREATE TABLE sessions (id TEXT PRIMARY KEY, title TEXT, createdAt INTEGER)"
        )
        conn.execute(
            "CREATE TABLE messages (id INTEGER PRIMARY KEY AUTOINCREMENT,"
            " role TEXT, content TEXT, createdAt INTEGER)"
        )
        conn.execute(
            "INSERT INTO sessions(id, title, createdAt) VALUES (?, ?, ?)",
            ("s1", "x", 0),
        )
        conn.execute(
            "INSERT INTO messages(role, content, createdAt) VALUES (?, ?, ?)",
            ("assistant", "ok", 1),
        )
        conn.commit()
    finally:
        conn.close()
    adapter = ContinueAdapter(root=root)
    refs = list(adapter.enumerate())
    assert len(refs) == 1
    # Should read the row even without a session filter; must not raise.
    records = list(adapter.read(refs[0]))
    assert len(records) == 1


def test_message_with_garbage_content_field_does_not_raise(tmp_path: Path) -> None:
    """A row with non-string content (BLOB) must coerce defensively."""
    root = tmp_path / ".continue"
    root.mkdir()
    db = root / "blob-content.db"
    conn = sqlite3.connect(db)
    try:
        conn.execute(
            "CREATE TABLE sessions (id TEXT PRIMARY KEY, title TEXT, createdAt INTEGER)"
        )
        conn.execute(
            "CREATE TABLE messages (id INTEGER PRIMARY KEY AUTOINCREMENT,"
            " sessionId TEXT, role TEXT, content BLOB, createdAt INTEGER)"
        )
        conn.execute(
            "INSERT INTO sessions(id, title, createdAt) VALUES (?, ?, ?)",
            ("s1", "x", 0),
        )
        conn.execute(
            "INSERT INTO messages(sessionId, role, content, createdAt) "
            "VALUES (?, ?, ?, ?)",
            ("s1", "assistant", b"\x00\x01\x02 binary", 1),
        )
        conn.commit()
    finally:
        conn.close()
    adapter = ContinueAdapter(root=root)
    refs = list(adapter.enumerate())
    records = list(adapter.read(refs[0]))
    # Adapter coerces bytes via decode-with-replace; must not raise.
    assert len(records) == 1


# ── permission denied ─────────────────────────────────────────────────


@pytest.mark.skipif(_SKIP_CHMOD, reason="chmod 000 is a no-op on Windows / bypassed by root")
def test_permission_denied_db_does_not_raise(tmp_path: Path) -> None:
    root = tmp_path / ".continue"
    root.mkdir()
    db = root / "locked.db"
    conn = sqlite3.connect(db)
    try:
        conn.execute(
            "CREATE TABLE sessions (id TEXT PRIMARY KEY, title TEXT, createdAt INTEGER)"
        )
        conn.execute("INSERT INTO sessions VALUES ('s', 't', 0)")
        conn.commit()
    finally:
        conn.close()
    db.chmod(0o000)
    try:
        adapter = ContinueAdapter(root=root)
        refs = list(adapter.enumerate())
        # Adapter logs and skips the unreadable DB.
        assert refs == []
    finally:
        db.chmod(0o644)
