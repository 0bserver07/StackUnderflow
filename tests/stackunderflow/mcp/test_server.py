"""MCP server tool-level tests covering the store-backed paths.

The legacy JSONL-walk path is exhaustively covered by
``tests/stackunderflow/test_mcp.py``. These tests target the new
behaviour:

* ``session_query`` reads from the store when the session id is present.
* ``session_query`` falls back to JSONL when the id is missing from the store.
* ``list_sessions`` returns cross-provider rows.
* ``list_projects`` returns the unified project list.
"""

from __future__ import annotations

import json
import sqlite3
import time
from pathlib import Path

import pytest

from stackunderflow import deps
from stackunderflow.mcp import server as mcp_server
from stackunderflow.store import db, schema


def _insert_project(
    conn: sqlite3.Connection, *, provider: str, slug: str, display_name: str | None = None,
    last_modified: float | None = None,
) -> int:
    cur = conn.execute(
        "INSERT INTO projects (provider, slug, path, display_name, first_seen, last_modified) "
        "VALUES (?, ?, NULL, ?, ?, ?)",
        (
            provider,
            slug,
            display_name or slug,
            time.time(),
            last_modified if last_modified is not None else time.time(),
        ),
    )
    return cur.lastrowid


def _insert_session(
    conn: sqlite3.Connection, *, project_id: int, session_id: str,
    first_ts: str, last_ts: str, message_count: int = 0,
) -> int:
    cur = conn.execute(
        "INSERT INTO sessions (project_id, session_id, first_ts, last_ts, message_count) "
        "VALUES (?, ?, ?, ?, ?)",
        (project_id, session_id, first_ts, last_ts, message_count),
    )
    return cur.lastrowid


def _insert_message(
    conn: sqlite3.Connection, *, session_fk: int, seq: int, timestamp: str, role: str,
    model: str | None = None, content_text: str = "",
    tools: list[str] | None = None, raw: dict | None = None,
    input_tokens: int = 0, output_tokens: int = 0,
) -> None:
    conn.execute(
        "INSERT INTO messages (session_fk, seq, timestamp, role, model, "
        "  input_tokens, output_tokens, cache_create_tokens, cache_read_tokens, "
        "  content_text, tools_json, raw_json, is_sidechain, uuid, parent_uuid) "
        "VALUES (?, ?, ?, ?, ?, ?, ?, 0, 0, ?, ?, ?, 0, NULL, NULL)",
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
        ),
    )


@pytest.fixture
def store_path(tmp_path: Path, monkeypatch) -> Path:
    """Build a multi-provider store under tmp_path and point deps at it."""
    p = tmp_path / "store.db"
    c = db.connect(p)
    schema.apply(c)

    cl = _insert_project(c, provider="claude", slug="-Users-x-app", display_name="app")
    cl_a = _insert_session(
        c, project_id=cl, session_id="s-claude-a",
        first_ts="2026-04-29T10:00:00Z", last_ts="2026-04-29T11:00:00Z",
        message_count=2,
    )
    _insert_message(
        c, session_fk=cl_a, seq=0, timestamp="2026-04-29T10:00:00Z",
        role="user", content_text="hello",
        raw={"type": "user", "message": {"role": "user", "content": "hello"}},
    )
    _insert_message(
        c, session_fk=cl_a, seq=1, timestamp="2026-04-29T10:30:00Z",
        role="assistant", model="claude-opus-4-7",
        input_tokens=100, output_tokens=200, tools=["Read"],
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

    cx = _insert_project(c, provider="codex", slug="-Users-x-other")
    cx_a = _insert_session(
        c, project_id=cx, session_id="s-codex-a",
        first_ts="2026-04-29T12:00:00Z", last_ts="2026-04-29T13:00:00Z",
        message_count=1,
    )
    _insert_message(
        c, session_fk=cx_a, seq=0, timestamp="2026-04-29T12:00:00Z",
        role="user", content_text="boom",
        raw={
            "type": "user",
            "message": {
                "role": "user",
                "content": [
                    {"type": "tool_result", "tool_use_id": "tu_x",
                     "is_error": True, "content": "Traceback: oops"}
                ],
            },
        },
    )

    cu = _insert_project(c, provider="cursor", slug="-Users-x-cursor")
    cu_a = _insert_session(
        c, project_id=cu, session_id="s-cursor-a",
        first_ts="2026-04-29T14:00:00Z", last_ts="2026-04-29T15:00:00Z",
        message_count=1,
    )
    _insert_message(
        c, session_fk=cu_a, seq=0, timestamp="2026-04-29T14:00:00Z",
        role="assistant", model="claude-sonnet-4-5",
        input_tokens=50, output_tokens=80,
        content_text="cursor said this",
        raw={"type": "assistant"},
    )
    c.close()
    monkeypatch.setattr(deps, "store_path", p)
    return p


# ── session_query: store-backed paths ──────────────────────────────────────


def test_session_query_store_backed_specific_session(store_path: Path) -> None:
    out = mcp_server.session_query_impl(session_id="s-codex-a")
    assert len(out) >= 1
    assert all(r["session_id"] == "s-codex-a" for r in out)
    assert out[0]["agent"] == "codex"


def test_session_query_store_backed_cross_session(store_path: Path) -> None:
    out = mcp_server.session_query_impl(limit=10)
    # cross-provider events surface together
    agents = {r["agent"] for r in out}
    assert agents == {"claude", "codex", "cursor"}
    # newest first
    assert out[0]["timestamp"].startswith("2026-04-29T14:")


def test_session_query_store_backed_tool_calls(store_path: Path) -> None:
    out = mcp_server.session_query_impl(session_id="s-claude-a", kind="tool_calls")
    assert len(out) == 1
    assert out[0]["tools"] == ["Read"]
    # tool_calls were derived from raw payload, with summarised args
    assert out[0]["tool_calls"]
    assert out[0]["tool_calls"][0]["name"] == "Read"
    assert out[0]["tool_calls"][0]["args"]["file_path"] == "foo.py"


def test_session_query_store_backed_errors(store_path: Path) -> None:
    out = mcp_server.session_query_impl(session_id="s-codex-a", kind="errors")
    assert len(out) == 1
    assert out[0]["session_id"] == "s-codex-a"


def test_session_query_response_drops_raw_blob(store_path: Path) -> None:
    """The 'raw' blob is internal — should not leak into MCP output."""
    out = mcp_server.session_query_impl(limit=5)
    for row in out:
        assert "raw" not in row


# ── session_query: JSONL fallback ──────────────────────────────────────────


def _write_jsonl(path: Path, lines: list[dict]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("w") as fh:
        for obj in lines:
            fh.write(json.dumps(obj) + "\n")


def test_session_query_falls_back_to_jsonl_when_id_missing(
    store_path: Path, tmp_path: Path
) -> None:
    """An unknown session_id triggers the legacy JSONL walk."""
    project = tmp_path / "agent-home" / ".claude" / "projects" / "-Users-me-app"
    _write_jsonl(
        project / "s-orphan.jsonl",
        [
            {
                "sessionId": "s-orphan",
                "type": "user",
                "timestamp": "2026-04-29T20:00:00Z",
                "uuid": "u1",
                "message": {"role": "user", "content": "fallback hi"},
            }
        ],
    )
    out = mcp_server.session_query_impl(
        session_id="s-orphan",
        roots=[tmp_path / "agent-home" / ".claude"],
    )
    assert len(out) == 1
    assert out[0]["session_id"] == "s-orphan"
    assert out[0]["agent"] == "claude"


def test_session_query_no_store_falls_back(
    monkeypatch, tmp_path: Path
) -> None:
    """If the store DB doesn't exist at all, JSONL walk handles everything."""
    monkeypatch.setattr(deps, "store_path", tmp_path / "missing.db")
    project = tmp_path / "h" / ".claude" / "projects" / "-foo"
    _write_jsonl(
        project / "s-fb.jsonl",
        [
            {
                "sessionId": "s-fb",
                "type": "user",
                "timestamp": "2026-04-29T21:00:00Z",
                "uuid": "u1",
                "message": {"role": "user", "content": "hi"},
            }
        ],
    )
    out = mcp_server.session_query_impl(roots=[tmp_path / "h" / ".claude"])
    assert len(out) == 1
    assert out[0]["session_id"] == "s-fb"


def test_session_query_zero_limit(store_path: Path) -> None:
    assert mcp_server.session_query_impl(limit=0) == []


# ── list_sessions ──────────────────────────────────────────────────────────


def test_list_sessions_returns_cross_provider(store_path: Path) -> None:
    out = mcp_server.list_sessions_impl()
    assert len(out) == 3
    providers = {r["provider"] for r in out}
    assert providers == {"claude", "codex", "cursor"}
    # ordered by last_ts desc
    assert [r["session_id"] for r in out] == ["s-cursor-a", "s-codex-a", "s-claude-a"]


def test_list_sessions_provider_filter(store_path: Path) -> None:
    out = mcp_server.list_sessions_impl(provider="claude")
    assert len(out) == 1
    assert out[0]["session_id"] == "s-claude-a"


def test_list_sessions_since_filter(store_path: Path) -> None:
    out = mcp_server.list_sessions_impl(since="2026-04-29T14:00:00Z")
    assert [r["session_id"] for r in out] == ["s-cursor-a"]


def test_list_sessions_includes_cost(store_path: Path) -> None:
    out = mcp_server.list_sessions_impl(provider="claude")
    assert out[0]["cost_usd"] > 0
    # message_count, started_at, last_ts populated
    assert out[0]["message_count"] == 2
    assert out[0]["started_at"] == "2026-04-29T10:00:00Z"
    assert out[0]["last_ts"] == "2026-04-29T11:00:00Z"


def test_list_sessions_empty_store(monkeypatch, tmp_path: Path) -> None:
    monkeypatch.setattr(deps, "store_path", tmp_path / "ghost.db")
    assert mcp_server.list_sessions_impl() == []


# ── list_projects ──────────────────────────────────────────────────────────


def test_list_projects_returns_all(store_path: Path) -> None:
    out = mcp_server.list_projects_impl()
    providers = {p["provider"] for p in out}
    assert providers == {"claude", "codex", "cursor"}


def test_list_projects_provider_filter(store_path: Path) -> None:
    out = mcp_server.list_projects_impl(provider="codex")
    assert len(out) == 1
    assert out[0]["slug"] == "-Users-x-other"
    assert out[0]["provider"] == "codex"


def test_list_projects_iso_timestamps(store_path: Path) -> None:
    out = mcp_server.list_projects_impl()
    for p in out:
        assert "T" in (p["first_seen"] or "")
        assert "T" in (p["last_modified"] or "")


def test_list_projects_empty_store(monkeypatch, tmp_path: Path) -> None:
    monkeypatch.setattr(deps, "store_path", tmp_path / "ghost.db")
    assert mcp_server.list_projects_impl() == []


# ── tool registration ──────────────────────────────────────────────────────


def test_all_three_tools_registered() -> None:
    """The MCP server exposes the original tool plus the two new ones."""
    # FastMCP exposes registered tools via list_tools (async).
    import asyncio

    async def _names() -> list[str]:
        tools = await mcp_server.mcp.list_tools()
        return [t.name for t in tools]

    names = asyncio.run(_names())
    assert "session_query" in names
    assert "list_sessions" in names
    assert "list_projects" in names
