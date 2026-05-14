"""Defensive empty-source / malformed-data coverage for the OpenCode adapter.

OpenCode is SQLite-backed and the most fragile beta adapter (the schema
varies across installs). These tests pin the empty-state and schema-drift
behaviour so a fresh install or a partially-migrated DB doesn't crash
ingest.

Covers:

  - Missing data dir
  - Empty data dir (no .db files)
  - Corrupt SQLite file
  - DB with no ``session`` table
  - DB with ``session`` but no ``message`` (or wrong message schema)
  - Malformed JSON in ``message.data`` column
  - Schema drift: a ``message`` row with extraneous JSON, missing keys
  - Permission-denied DB file
"""

from __future__ import annotations

import json
import os
import sqlite3
import sys
from pathlib import Path

import pytest

from stackunderflow.adapters.opencode import OpenCodeAdapter


_IS_ROOT = hasattr(os, "geteuid") and os.geteuid() == 0
# Windows ignores Unix file permissions; chmod(0o000) is a no-op on NTFS, so the
# permission-denied path under test is unreachable there. Skip those tests on
# Windows the same way we skip them when running as root on POSIX.
_SKIP_CHMOD = _IS_ROOT or sys.platform == "win32"


# ── missing / empty source ────────────────────────────────────────────


def test_missing_data_dir_yields_nothing(tmp_path: Path) -> None:
    adapter = OpenCodeAdapter(data_dir=tmp_path / "no-such-dir")
    assert list(adapter.enumerate()) == []


def test_empty_data_dir_yields_nothing(tmp_path: Path) -> None:
    """Directory exists but has no opencode*.db files."""
    adapter = OpenCodeAdapter(data_dir=tmp_path)
    assert list(adapter.enumerate()) == []


def test_data_dir_with_unrelated_files_yields_nothing(tmp_path: Path) -> None:
    """Random files that don't match opencode*.db are ignored."""
    (tmp_path / "config.json").write_text("{}")
    (tmp_path / "other.db").write_bytes(b"")
    adapter = OpenCodeAdapter(data_dir=tmp_path)
    assert list(adapter.enumerate()) == []


# ── corrupt / mis-shaped DB files ─────────────────────────────────────


def test_corrupt_db_file_does_not_raise(tmp_path: Path) -> None:
    """Garbage bytes in opencode*.db: enumerate logs and skips, no raise."""
    (tmp_path / "opencode.db").write_bytes(b"this is not sqlite")
    adapter = OpenCodeAdapter(data_dir=tmp_path)
    assert list(adapter.enumerate()) == []


def test_db_without_session_table_yields_nothing(tmp_path: Path) -> None:
    """Valid SQLite but no ``session`` table: skip cleanly."""
    db = tmp_path / "opencode.db"
    conn = sqlite3.connect(db)
    try:
        conn.execute("CREATE TABLE other_thing (x TEXT)")
        conn.commit()
    finally:
        conn.close()
    adapter = OpenCodeAdapter(data_dir=tmp_path)
    assert list(adapter.enumerate()) == []


def test_db_with_session_but_no_message_table(tmp_path: Path) -> None:
    """Sessions enumerate; reading them yields nothing because messages don't exist."""
    db = tmp_path / "opencode.db"
    conn = sqlite3.connect(db)
    try:
        conn.execute("CREATE TABLE session (id TEXT PRIMARY KEY)")
        conn.execute("INSERT INTO session VALUES ('s1')")
        conn.commit()
    finally:
        conn.close()
    adapter = OpenCodeAdapter(data_dir=tmp_path)
    refs = list(adapter.enumerate())
    assert len(refs) == 1
    # No message table: read tries the query, logs an error, yields nothing.
    assert list(adapter.read(refs[0])) == []


# ── malformed JSON inside message.data ────────────────────────────────


def test_malformed_message_data_row_is_skipped(tmp_path: Path) -> None:
    """A ``message.data`` blob that doesn't parse as JSON is skipped."""
    db = tmp_path / "opencode.db"
    conn = sqlite3.connect(db)
    try:
        conn.executescript(
            """
            CREATE TABLE session (id TEXT PRIMARY KEY, directory TEXT,
                title TEXT, time_created INTEGER, time_archived INTEGER,
                parent_id TEXT);
            CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT,
                time_created INTEGER, data TEXT);
            CREATE TABLE part (message_id TEXT, session_id TEXT, data TEXT);
            INSERT INTO session VALUES ('s1', '/tmp', 't', 0, 0, NULL);
            """
        )
        conn.execute(
            "INSERT INTO message VALUES (?, ?, ?, ?)",
            ("m1", "s1", 0, "not json at all"),
        )
        conn.execute(
            "INSERT INTO message VALUES (?, ?, ?, ?)",
            (
                "m2", "s1", 0,
                json.dumps({
                    "role": "assistant",
                    "modelID": "claude-3-5-sonnet",
                    "tokens": {"input": 1, "output": 1},
                }),
            ),
        )
        conn.commit()
    finally:
        conn.close()
    adapter = OpenCodeAdapter(data_dir=tmp_path)
    ref = next(iter(adapter.enumerate()))
    records = list(adapter.read(ref))
    # m1 is skipped (malformed JSON), m2 yields a record.
    assert len(records) == 1
    assert records[0].model == "claude-3-5-sonnet"


# ── schema drift on a message row ─────────────────────────────────────


def test_message_row_missing_role_is_skipped(tmp_path: Path) -> None:
    """A row whose JSON has no ``role`` is dropped without raising."""
    db = tmp_path / "opencode.db"
    conn = sqlite3.connect(db)
    try:
        conn.executescript(
            """
            CREATE TABLE session (id TEXT PRIMARY KEY, directory TEXT,
                title TEXT, time_created INTEGER, time_archived INTEGER,
                parent_id TEXT);
            CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT,
                time_created INTEGER, data TEXT);
            CREATE TABLE part (message_id TEXT, session_id TEXT, data TEXT);
            INSERT INTO session VALUES ('s1', '/tmp', 't', 0, 0, NULL);
            """
        )
        conn.execute(
            "INSERT INTO message VALUES (?, ?, ?, ?)",
            ("m-noroleg", "s1", 0, json.dumps({"modelID": "x", "tokens": {}})),
        )
        conn.execute(
            "INSERT INTO message VALUES (?, ?, ?, ?)",
            (
                "m-good", "s1", 0,
                json.dumps({"role": "assistant", "modelID": "y", "tokens": {}}),
            ),
        )
        conn.commit()
    finally:
        conn.close()
    adapter = OpenCodeAdapter(data_dir=tmp_path)
    ref = next(iter(adapter.enumerate()))
    records = list(adapter.read(ref))
    assert len(records) == 1
    assert records[0].model == "y"


def test_message_with_garbage_token_shape_falls_back_to_zero(tmp_path: Path) -> None:
    """Tokens block with non-numeric values defaults to zero, no crash."""
    db = tmp_path / "opencode.db"
    conn = sqlite3.connect(db)
    try:
        conn.executescript(
            """
            CREATE TABLE session (id TEXT PRIMARY KEY, directory TEXT,
                title TEXT, time_created INTEGER, time_archived INTEGER,
                parent_id TEXT);
            CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT,
                time_created INTEGER, data TEXT);
            CREATE TABLE part (message_id TEXT, session_id TEXT, data TEXT);
            INSERT INTO session VALUES ('s1', '/tmp', 't', 0, 0, NULL);
            """
        )
        conn.execute(
            "INSERT INTO message VALUES (?, ?, ?, ?)",
            (
                "m", "s1", 0,
                json.dumps({
                    "role": "assistant",
                    "modelID": "z",
                    "tokens": {"input": "not a number", "output": None,
                               "cache": "wrong-shape"},
                }),
            ),
        )
        conn.commit()
    finally:
        conn.close()
    adapter = OpenCodeAdapter(data_dir=tmp_path)
    ref = next(iter(adapter.enumerate()))
    records = list(adapter.read(ref))
    assert len(records) == 1
    rec = records[0]
    assert rec.input_tokens == 0
    assert rec.output_tokens == 0
    assert rec.cache_create_tokens == 0
    assert rec.cache_read_tokens == 0


# ── permission denied ─────────────────────────────────────────────────


@pytest.mark.skipif(_SKIP_CHMOD, reason="chmod 000 is a no-op on Windows / bypassed by root")
def test_permission_denied_db_does_not_raise(tmp_path: Path) -> None:
    """An unreadable opencode.db is logged and skipped during enumerate."""
    db = tmp_path / "opencode.db"
    conn = sqlite3.connect(db)
    try:
        conn.execute(
            "CREATE TABLE session (id TEXT PRIMARY KEY, directory TEXT,"
            " title TEXT, time_created INTEGER, time_archived INTEGER, parent_id TEXT)"
        )
        conn.execute("INSERT INTO session VALUES ('s', '/tmp', 't', 0, 0, NULL)")
        conn.commit()
    finally:
        conn.close()
    db.chmod(0o000)
    try:
        adapter = OpenCodeAdapter(data_dir=tmp_path)
        # Must not raise; refs is empty when the DB can't be opened.
        refs = list(adapter.enumerate())
        assert refs == []
    finally:
        db.chmod(0o644)
