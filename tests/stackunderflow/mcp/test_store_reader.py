"""Tests for ``stackunderflow.mcp.store_reader``.

Builds a synthetic SQLite store with claude + codex + cursor sessions
and verifies each store-reader helper returns the right rows. These
tests **never** touch the user's real ``~/.stackunderflow/store.db`` —
every fixture creates its own DB under ``tmp_path``.
"""

from __future__ import annotations

import json
import sqlite3
import time
from pathlib import Path

import pytest

from stackunderflow.mcp import store_reader
from stackunderflow.store import db, schema


def _insert_project(
    conn: sqlite3.Connection,
    *,
    provider: str,
    slug: str,
    display_name: str | None = None,
    path: str | None = None,
    first_seen: float | None = None,
    last_modified: float | None = None,
) -> int:
    now = time.time()
    cur = conn.execute(
        "INSERT INTO projects (provider, slug, path, display_name, first_seen, last_modified) "
        "VALUES (?, ?, ?, ?, ?, ?)",
        (
            provider,
            slug,
            path,
            display_name or slug,
            first_seen if first_seen is not None else now,
            last_modified if last_modified is not None else now,
        ),
    )
    return cur.lastrowid


def _insert_session(
    conn: sqlite3.Connection,
    *,
    project_id: int,
    session_id: str,
    first_ts: str | None = None,
    last_ts: str | None = None,
    message_count: int = 0,
) -> int:
    cur = conn.execute(
        "INSERT INTO sessions (project_id, session_id, first_ts, last_ts, message_count) "
        "VALUES (?, ?, ?, ?, ?)",
        (project_id, session_id, first_ts, last_ts, message_count),
    )
    return cur.lastrowid


def _insert_message(
    conn: sqlite3.Connection,
    *,
    session_fk: int,
    seq: int,
    timestamp: str,
    role: str,
    model: str | None = None,
    content_text: str = "",
    tools: list[str] | None = None,
    raw: dict | None = None,
    input_tokens: int = 0,
    output_tokens: int = 0,
    is_sidechain: bool = False,
    uuid: str | None = None,
) -> None:
    conn.execute(
        "INSERT INTO messages (session_fk, seq, timestamp, role, model, "
        "  input_tokens, output_tokens, cache_create_tokens, cache_read_tokens, "
        "  content_text, tools_json, raw_json, is_sidechain, uuid, parent_uuid) "
        "VALUES (?, ?, ?, ?, ?, ?, ?, 0, 0, ?, ?, ?, ?, ?, NULL)",
        (
            session_fk,
            seq,
            timestamp,
            role,
            model,
            input_tokens,
            output_tokens,
            content_text,
            json.dumps(tools or []),
            json.dumps(raw or {}),
            1 if is_sidechain else 0,
            uuid,
        ),
    )


@pytest.fixture
def conn(tmp_path: Path) -> sqlite3.Connection:
    """A fully migrated empty store."""
    c = db.connect(tmp_path / "store.db")
    schema.apply(c)
    yield c
    c.close()


@pytest.fixture
def populated(conn: sqlite3.Connection) -> sqlite3.Connection:
    """Three providers, four sessions, mixed messages — covers every helper."""
    # claude project
    cl = _insert_project(conn, provider="claude", slug="-Users-x-app", display_name="app")
    cl_a = _insert_session(
        conn,
        project_id=cl,
        session_id="s-claude-a",
        first_ts="2026-04-29T10:00:00Z",
        last_ts="2026-04-29T11:00:00Z",
        message_count=2,
    )
    _insert_message(
        conn,
        session_fk=cl_a,
        seq=0,
        timestamp="2026-04-29T10:00:00Z",
        role="user",
        content_text="hello world",
        raw={"type": "user", "message": {"role": "user", "content": "hello world"}},
    )
    _insert_message(
        conn,
        session_fk=cl_a,
        seq=1,
        timestamp="2026-04-29T10:30:00Z",
        role="assistant",
        model="claude-opus-4-7",
        input_tokens=100,
        output_tokens=200,
        tools=["Read"],
        content_text="reading file",
        raw={
            "type": "assistant",
            "message": {
                "role": "assistant",
                "model": "claude-opus-4-7",
                "content": [
                    {"type": "tool_use", "name": "Read", "id": "tu1",
                     "input": {"file_path": "foo.py"}}
                ],
            },
        },
    )

    # codex project
    cx = _insert_project(conn, provider="codex", slug="-Users-x-other")
    cx_a = _insert_session(
        conn,
        project_id=cx,
        session_id="s-codex-a",
        first_ts="2026-04-29T12:00:00Z",
        last_ts="2026-04-29T13:00:00Z",
        message_count=1,
    )
    _insert_message(
        conn,
        session_fk=cx_a,
        seq=0,
        timestamp="2026-04-29T12:00:00Z",
        role="user",
        content_text="error happened",
        raw={
            "type": "user",
            "message": {
                "role": "user",
                "content": [
                    {
                        "type": "tool_result",
                        "tool_use_id": "tu_x",
                        "is_error": True,
                        "content": "Traceback: ValueError",
                    }
                ],
            },
        },
    )

    # cursor project — same slug as claude one (cross-provider duplicate)
    cu = _insert_project(conn, provider="cursor", slug="-Users-x-app", display_name="app")
    cu_a = _insert_session(
        conn,
        project_id=cu,
        session_id="s-cursor-a",
        first_ts="2026-04-29T14:00:00Z",
        last_ts="2026-04-29T15:00:00Z",
        message_count=1,
    )
    _insert_message(
        conn,
        session_fk=cu_a,
        seq=0,
        timestamp="2026-04-29T14:00:00Z",
        role="assistant",
        model="claude-sonnet-4-5",
        input_tokens=50,
        output_tokens=80,
        content_text="cursor said this",
        raw={"type": "assistant"},
    )
    return conn


# ── store_available ─────────────────────────────────────────────────────────


def test_store_available_with_conn(populated: sqlite3.Connection) -> None:
    assert store_reader.store_available(conn=populated) is True


def test_store_available_missing_db(tmp_path: Path, monkeypatch) -> None:
    from stackunderflow import deps

    monkeypatch.setattr(deps, "store_path", tmp_path / "nope.db")
    assert store_reader.store_available() is False


# ── find_session ────────────────────────────────────────────────────────────


def test_find_session_returns_match(populated: sqlite3.Connection) -> None:
    sess = store_reader.find_session("s-claude-a", conn=populated)
    assert sess is not None
    assert sess.provider == "claude"
    assert sess.project_slug == "-Users-x-app"
    assert sess.message_count == 2
    # cost_usd computed from input/output tokens × claude-opus-4-7 rates → > 0
    assert sess.cost_usd > 0


def test_find_session_returns_none_for_unknown(populated: sqlite3.Connection) -> None:
    assert store_reader.find_session("nope", conn=populated) is None


def test_find_session_empty_db(conn: sqlite3.Connection) -> None:
    assert store_reader.find_session("anything", conn=conn) is None


# ── list_recent_sessions ────────────────────────────────────────────────────


def test_list_recent_sessions_orders_by_last_ts_desc(populated: sqlite3.Connection) -> None:
    out = store_reader.list_recent_sessions(limit=10, conn=populated)
    assert [s.session_id for s in out] == ["s-cursor-a", "s-codex-a", "s-claude-a"]


def test_list_recent_sessions_provider_filter(populated: sqlite3.Connection) -> None:
    out = store_reader.list_recent_sessions(limit=10, provider="codex", conn=populated)
    assert [s.session_id for s in out] == ["s-codex-a"]
    assert out[0].provider == "codex"


def test_list_recent_sessions_since_filter(populated: sqlite3.Connection) -> None:
    out = store_reader.list_recent_sessions(
        limit=10, since="2026-04-29T14:00:00Z", conn=populated
    )
    # only cursor session has last_ts ≥ 14:00 (it ends at 15:00)
    assert [s.session_id for s in out] == ["s-cursor-a"]


def test_list_recent_sessions_respects_limit(populated: sqlite3.Connection) -> None:
    out = store_reader.list_recent_sessions(limit=2, conn=populated)
    assert len(out) == 2


def test_list_recent_sessions_zero_limit(populated: sqlite3.Connection) -> None:
    assert store_reader.list_recent_sessions(limit=0, conn=populated) == []


def test_list_recent_sessions_empty_db(conn: sqlite3.Connection) -> None:
    assert store_reader.list_recent_sessions(conn=conn) == []


# ── list_stored_projects ────────────────────────────────────────────────────


def test_list_stored_projects_returns_all(populated: sqlite3.Connection) -> None:
    out = store_reader.list_stored_projects(conn=populated)
    providers = {p.provider for p in out}
    assert providers == {"claude", "codex", "cursor"}


def test_list_stored_projects_filter_by_provider(populated: sqlite3.Connection) -> None:
    out = store_reader.list_stored_projects(provider="cursor", conn=populated)
    assert len(out) == 1
    assert out[0].slug == "-Users-x-app"
    assert out[0].provider == "cursor"


def test_list_stored_projects_iso_timestamps(populated: sqlite3.Connection) -> None:
    out = store_reader.list_stored_projects(conn=populated)
    for p in out:
        # ISO8601 has a 'T' between date and time
        assert p.first_seen and "T" in p.first_seen
        assert p.last_modified and "T" in p.last_modified


def test_list_stored_projects_empty_db(conn: sqlite3.Connection) -> None:
    assert store_reader.list_stored_projects(conn=conn) == []


# ── get_session_messages ────────────────────────────────────────────────────


def test_get_session_messages_kind_all(populated: sqlite3.Connection) -> None:
    out = store_reader.get_session_messages("s-claude-a", kind="all", conn=populated)
    assert len(out) == 2
    assert out[0]["agent"] == "claude"
    assert out[0]["role"] == "user"
    assert out[1]["role"] == "assistant"


def test_get_session_messages_kind_tool_calls(populated: sqlite3.Connection) -> None:
    out = store_reader.get_session_messages(
        "s-claude-a", kind="tool_calls", conn=populated
    )
    assert len(out) == 1
    assert out[0]["tools"] == ["Read"]


def test_get_session_messages_kind_errors(populated: sqlite3.Connection) -> None:
    """Errors filter requires the caller to pass an is_error detector."""

    def _is_error(raw: dict) -> bool:
        msg = raw.get("message", {})
        body = msg.get("content")
        if not isinstance(body, list):
            return False
        return any(isinstance(b, dict) and b.get("is_error") for b in body)

    out = store_reader.get_session_messages(
        "s-codex-a", kind="errors", conn=populated, is_error=_is_error
    )
    assert len(out) == 1
    assert out[0]["session_id"] == "s-codex-a"


def test_get_session_messages_unknown_session(populated: sqlite3.Connection) -> None:
    assert store_reader.get_session_messages("nope", conn=populated) == []


def test_get_session_messages_zero_limit(populated: sqlite3.Connection) -> None:
    assert store_reader.get_session_messages("s-claude-a", limit=0, conn=populated) == []


def test_get_session_messages_respects_limit(populated: sqlite3.Connection) -> None:
    out = store_reader.get_session_messages("s-claude-a", limit=1, conn=populated)
    assert len(out) == 1


def test_get_session_messages_truncates_long_preview(conn: sqlite3.Connection) -> None:
    pid = _insert_project(conn, provider="claude", slug="-z")
    sid = _insert_session(conn, project_id=pid, session_id="long",
                          first_ts="2026-04-29T00:00:00Z",
                          last_ts="2026-04-29T00:00:01Z", message_count=1)
    long_text = "x" * 500
    _insert_message(
        conn, session_fk=sid, seq=0,
        timestamp="2026-04-29T00:00:00Z", role="assistant",
        content_text=long_text, raw={"type": "assistant"},
    )
    out = store_reader.get_session_messages("long", conn=conn)
    assert out[0]["content_preview"].endswith("…")
    assert len(out[0]["content_preview"]) <= 201


# ── default-conn path: opens the user store unless monkeypatched away ──────


def test_default_conn_returns_empty_when_db_missing(tmp_path: Path, monkeypatch) -> None:
    """When the default store doesn't exist, helpers degrade silently."""
    from stackunderflow import deps

    monkeypatch.setattr(deps, "store_path", tmp_path / "ghost.db")
    assert store_reader.find_session("anything") is None
    assert store_reader.list_recent_sessions() == []
    assert store_reader.list_stored_projects() == []
    assert store_reader.get_session_messages("any") == []
