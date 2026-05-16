"""MCP ``file_risk`` tool tests (Spec 16).

Verifies the MCP layer's contract for the new tool:

1. Empty store (or missing store on disk) returns a zero-bucket payload
   without raising.
2. Path is resolved to an absolute form before the service is called.
3. The response carries the documented seven-key shape.
4. The tool is registered on the FastMCP instance.

Mirrors the patterns in ``test_discovery_tools.py``.
"""

from __future__ import annotations

import asyncio
import json
from pathlib import Path

import pytest

from stackunderflow import deps
from stackunderflow.mcp import server as mcp_server
from stackunderflow.store import db, schema

# ── fixtures ────────────────────────────────────────────────────────────────


@pytest.fixture
def empty_store(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Path:
    p = tmp_path / "store.db"
    c = db.connect(p)
    schema.apply(c)
    c.close()
    monkeypatch.setattr(deps, "store_path", p)
    return p


@pytest.fixture
def missing_store(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Path:
    p = tmp_path / "ghost.db"
    monkeypatch.setattr(deps, "store_path", p)
    return p


# ── happy-path seeding ──────────────────────────────────────────────────────


def _seed_failing(store_path: Path) -> None:
    conn = db.connect(store_path)
    schema.apply(conn)
    pcur = conn.execute(
        "INSERT INTO projects (provider, slug, path, display_name, "
        " first_seen, last_modified) VALUES "
        "('claude', '-Users-yad-dev-foo', NULL, 'foo', 0.0, 0.0)"
    )
    pid = int(pcur.lastrowid)
    sfk_cur = conn.execute(
        "INSERT INTO sessions (project_id, session_id, first_ts, last_ts, "
        " message_count) VALUES (?, 'fail-1', "
        "'2026-04-01T00:00:00+00:00', '2026-04-01T00:00:00+00:00', 2)",
        (pid,),
    )
    sfk = int(sfk_cur.lastrowid)
    edit_blob = json.dumps([{"name": "Edit", "input": {"file_path": "/x/cost.py"}}])
    for seq, (role, content_text, tools_json) in enumerate([
        ("assistant", "", edit_blob),
        ("user", "no, that broke the cost endpoint", "[]"),
    ]):
        conn.execute(
            "INSERT INTO messages (session_fk, seq, timestamp, role, model, "
            " input_tokens, output_tokens, cache_create_tokens, "
            " cache_read_tokens, content_text, tools_json, raw_json, "
            " is_sidechain) VALUES "
            "(?, ?, '2026-04-01T00:00:00+00:00', ?, 'claude-sonnet-4-5', "
            " 0, 0, 0, 0, ?, ?, '{}', 0)",
            (sfk, seq, role, content_text, tools_json),
        )
    conn.commit()
    conn.close()


# ── empty / missing store ───────────────────────────────────────────────────


def test_file_risk_missing_store_returns_zero_buckets(missing_store: Path) -> None:
    out = mcp_server.file_risk_impl(path="/x/cost.py")
    assert out["total_sessions"] == 0
    assert out["reverted"] == 0
    assert out["failed"] == 0
    assert out["worked"] == 0
    assert out["recent_session_ids"] == []
    # Path resolved to absolute even when the store is missing.
    assert out["path"].endswith("/x/cost.py")


def test_file_risk_empty_store_returns_zero_buckets(empty_store: Path) -> None:
    out = mcp_server.file_risk_impl(path="/x/cost.py")
    assert out["total_sessions"] == 0
    assert out["recent_session_ids"] == []


# ── happy path ──────────────────────────────────────────────────────────────


def test_file_risk_seeded_session_is_classified(empty_store: Path) -> None:
    _seed_failing(empty_store)
    out = mcp_server.file_risk_impl(path="/x/cost.py")
    assert out["failed"] == 1
    assert out["reverted"] == 0
    assert out["recent_session_ids"] == ["fail-1"]


def test_file_risk_response_shape_locked(empty_store: Path) -> None:
    """Meta-agent depends on this exact key set."""
    out = mcp_server.file_risk_impl(path="/x/cost.py")
    assert set(out) == {
        "path", "since", "total_sessions",
        "reverted", "failed", "worked", "recent_session_ids",
    }


def test_file_risk_path_resolution(empty_store: Path) -> None:
    """``~`` and relative paths are expanded before the service is called."""
    out = mcp_server.file_risk_impl(path="~/some/file.py")
    assert "~" not in out["path"]
    assert out["path"].startswith("/")


def test_file_risk_empty_path_raises(missing_store: Path) -> None:
    with pytest.raises(ValueError, match="path must be a non-empty string"):
        mcp_server.file_risk_impl(path="")


# ── FastMCP registration ────────────────────────────────────────────────────


def test_file_risk_registered_on_fastmcp() -> None:
    """The decorator should have added the tool to the server's catalogue."""
    tool_names = {t.name for t in asyncio.run(mcp_server.mcp.list_tools())}
    assert "file_risk" in tool_names
