"""Shared fixtures for mart-builder tests.

Each test gets a fresh in-memory-ish SQLite store (file-backed in
``tmp_path`` so WAL mode works) with the schema fully migrated and a
small fixture project + session FKs in place. Tests insert synthetic
``usage_events`` rows directly — they don't go through the normalizer
because Wave 2B mart builders only depend on the ``usage_events``
contents, by design.
"""

from __future__ import annotations

import sqlite3
from collections.abc import Iterator
from pathlib import Path

import pytest

from stackunderflow.store import db, schema


@pytest.fixture()
def conn(tmp_path: Path) -> Iterator[sqlite3.Connection]:
    """Fresh migrated store. Yields a sqlite3.Connection; closes on teardown."""
    c = db.connect(tmp_path / "store.db")
    schema.apply(c)
    # Seed two projects so tests can mix/match without re-seeding.
    c.execute(
        "INSERT INTO projects (id, provider, slug, display_name, "
        "first_seen, last_modified) "
        "VALUES (1, 'claude', 'alpha', 'Alpha', 0, 0)"
    )
    c.execute(
        "INSERT INTO projects (id, provider, slug, display_name, "
        "first_seen, last_modified) "
        "VALUES (2, 'codex', 'beta', 'Beta', 0, 0)"
    )
    # One session FK per project. The mart layer doesn't read from
    # ``sessions`` — it joins on session_id strings — but the messages
    # table requires a session_fk so we create one.
    c.execute(
        "INSERT INTO sessions (id, project_id, session_id) VALUES (1, 1, 'sess-1')"
    )
    c.execute(
        "INSERT INTO sessions (id, project_id, session_id) VALUES (2, 2, 'sess-2')"
    )
    yield c
    c.close()


def insert_message(
    conn: sqlite3.Connection,
    *,
    msg_id: int,
    session_fk: int = 1,
    seq: int | None = None,
    role: str = "assistant",
    timestamp: str = "2024-01-01T00:00:00Z",
    tools_json: str | None = None,
    content_text: str | None = None,
) -> None:
    """Insert a placeholder ``messages`` row so a usage_event can FK to it.

    Wave 5 callers may pass ``tools_json`` (a JSON-encoded array of tool
    name strings) and ``content_text`` for the per-tool / per-command
    mart fixtures. Older callers leave both ``None`` and the table's
    column defaults take over (``tools_json='[]'``, empty content).
    """
    if seq is None:
        seq = msg_id
    if tools_json is None:
        tools_json = "[]"
    if content_text is None:
        content_text = ""
    conn.execute(
        "INSERT INTO messages "
        "(id, session_fk, seq, timestamp, role, tools_json, content_text, raw_json) "
        "VALUES (?, ?, ?, ?, ?, ?, ?, '{}')",
        (msg_id, session_fk, seq, timestamp, role, tools_json, content_text),
    )


def insert_event(
    conn: sqlite3.Connection,
    *,
    event_id: int,
    msg_id: int | None = None,
    project_id: int = 1,
    provider: str = "claude",
    session_id: str = "sess-1",
    ts: str = "2024-01-01T00:00:00Z",
    day: str = "2024-01-01",
    model: str = "sonnet",
    speed: str = "standard",
    input_tokens: int = 0,
    output_tokens: int = 0,
    cache_read: int = 0,
    cache_create: int = 0,
    cost_usd: float = 0.0,
    role: str = "assistant",
    tools_json: str | None = None,
    seq: int | None = None,
    session_fk: int | None = None,
) -> None:
    """Insert one synthetic ``usage_events`` row + its placeholder message.

    The Wave 5 marts (``tool_mart``, ``command_mart``) read ``tools_json``
    and walk back to the preceding user message, so the helper accepts
    optional ``tools_json`` and ``seq`` overrides for richer fixtures.
    Older callers that don't pass them get the same defaults as before.
    """
    if msg_id is None:
        msg_id = event_id
    if session_fk is None:
        session_fk = 1 if project_id == 1 else 2
    if seq is None:
        seq = event_id
    insert_message(
        conn, msg_id=msg_id, role=role, timestamp=ts,
        session_fk=session_fk, seq=seq,
        tools_json=tools_json,
    )
    conn.execute(
        """
        INSERT INTO usage_events (
            id, source_message_fk, provider, project_id, session_id,
            ts, day, model, speed,
            input_tokens, output_tokens, cache_read_tokens, cache_create_tokens,
            cost_usd, cost_source, role
        ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 'rate_card', ?)
        """,
        (
            event_id, msg_id, provider, project_id, session_id,
            ts, day, model, speed,
            input_tokens, output_tokens, cache_read, cache_create,
            cost_usd, role,
        ),
    )


def insert_user_prompt(
    conn: sqlite3.Connection,
    *,
    msg_id: int,
    session_fk: int,
    seq: int,
    content_text: str,
    timestamp: str = "2024-01-01T00:00:00Z",
) -> None:
    """Insert a ``role='user'`` message used by ``command_mart`` tests.

    The mart walks ``messages`` back from each event to the most recent
    ``role='user'`` row; tests stage these with explicit ``seq`` so the
    walk lands deterministically.
    """
    conn.execute(
        "INSERT INTO messages (id, session_fk, seq, timestamp, role, "
        "content_text, raw_json) "
        "VALUES (?, ?, ?, ?, 'user', ?, '{}')",
        (msg_id, session_fk, seq, timestamp, content_text),
    )
