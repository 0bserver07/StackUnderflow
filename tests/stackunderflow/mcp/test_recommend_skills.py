"""MCP-layer tests for the ``recommend_skills`` tool."""

from __future__ import annotations

import json
from pathlib import Path

import pytest

from stackunderflow import deps
from stackunderflow.mcp import server as mcp_server
from stackunderflow.store import db, schema

# ── seeding helpers ─────────────────────────────────────────────────────────


def _claude_raw(role: str, text: str | None, tool_uses: list) -> dict:
    content: list[dict] = []
    if text:
        content.append({"type": "text", "text": text})
    for i, (name, inp) in enumerate(tool_uses):
        content.append({"type": "tool_use", "id": f"toolu_{i}", "name": name, "input": inp})
    return {"type": role, "uuid": "u", "message": {"role": role, "content": content}}


def _seed_store(store_db: Path, *, slug: str, n: int) -> None:
    conn = db.connect(store_db)
    schema.apply(conn)
    pid = int(
        conn.execute(
            "INSERT INTO projects (provider, slug, path, display_name, "
            "first_seen, last_modified) VALUES ('claude', ?, NULL, ?, 0.0, 0.0)",
            (slug, slug),
        ).lastrowid
    )
    for k in range(n):
        sfk = int(
            conn.execute(
                "INSERT INTO sessions (project_id, session_id, first_ts, last_ts, message_count) "
                "VALUES (?, ?, '2026-05-01T00:00:00+00:00', '2026-05-01T01:00:00+00:00', 3)",
                (pid, f"s-{k}"),
            ).lastrowid
        )
        edit_args = {"file_path": "/Users/yad/dev/foo/pkg/m.py", "old_string": "a", "new_string": "b"}
        turns = [
            ("user", "do a thing", []),
            ("assistant", "editing", [("Edit", edit_args)]),
            ("assistant", "running tests", [("Bash", {"command": "pytest tests/ -q"})]),
        ]
        for i, (role, text, tcs) in enumerate(turns):
            conn.execute(
                "INSERT INTO messages (session_fk, seq, timestamp, role, model, "
                " input_tokens, output_tokens, cache_create_tokens, cache_read_tokens, "
                " content_text, tools_json, raw_json, is_sidechain) "
                "VALUES (?, ?, ?, ?, 'claude-sonnet-4-5', 0, 0, 0, 0, ?, ?, ?, 0)",
                (sfk, i, f"2026-05-01T00:0{i}:00+00:00", role, text,
                 json.dumps([t[0] for t in tcs]),
                 json.dumps(_claude_raw(role, text, tcs))),
            )
    conn.commit()
    conn.close()


@pytest.fixture
def cache_in_tmp(tmp_path, monkeypatch):
    """Redirect the home dir so the cache lives under tmp_path."""
    fake_home = tmp_path / "home"
    fake_home.mkdir()
    monkeypatch.setattr(Path, "home", classmethod(lambda cls: fake_home))
    return fake_home


# ── the tool itself ────────────────────────────────────────────────────────


def test_missing_store_returns_empty_list(tmp_path, monkeypatch, cache_in_tmp):
    monkeypatch.setattr(deps, "store_path", tmp_path / "ghost.db")
    out = mcp_server.recommend_skills_impl(project="-myproj")
    assert out["recommendations"] == []
    assert out["project"] == "-myproj"


def test_seeded_store_returns_recommendations(tmp_path, monkeypatch, cache_in_tmp):
    p = tmp_path / "store.db"
    _seed_store(p, slug="-myproj", n=7)
    monkeypatch.setattr(deps, "store_path", p)
    out = mcp_server.recommend_skills_impl(
        project="-myproj", threshold=5, window_days=365,
    )
    assert isinstance(out["recommendations"], list)
    assert out["recommendations"]
    rec = out["recommendations"][0]
    assert {"pattern_id", "pattern_kind", "occurrences", "accept_command",
            "suggested_skill_name", "suggested_skill_template"} <= set(rec)


def test_validates_project(tmp_path, monkeypatch, cache_in_tmp):
    monkeypatch.setattr(deps, "store_path", tmp_path / "ghost.db")
    with pytest.raises(ValueError, match="project"):
        mcp_server.recommend_skills_impl(project="")


def test_validates_threshold(tmp_path, monkeypatch, cache_in_tmp):
    monkeypatch.setattr(deps, "store_path", tmp_path / "ghost.db")
    with pytest.raises(ValueError, match="threshold"):
        mcp_server.recommend_skills_impl(project="-x", threshold=0)


def test_validates_window_days(tmp_path, monkeypatch, cache_in_tmp):
    monkeypatch.setattr(deps, "store_path", tmp_path / "ghost.db")
    with pytest.raises(ValueError, match="window_days"):
        mcp_server.recommend_skills_impl(project="-x", window_days=0)


def test_tool_registered_on_mcp(tmp_path):
    """The decorated MCP tool is reachable via the FastMCP instance."""
    # FastMCP exposes registered tool names on its internal manager.
    # We import the module-level `mcp` and check our tool is present.
    import asyncio

    async def _list_tool_names() -> list[str]:
        tools = await mcp_server.mcp.list_tools()
        return [t.name for t in tools]

    names = asyncio.run(_list_tool_names())
    assert "recommend_skills" in names
