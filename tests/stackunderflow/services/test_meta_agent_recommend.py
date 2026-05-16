"""Meta-agent dispatch tests for the ``recommend_skills`` tool."""

from __future__ import annotations

import json
from pathlib import Path

import pytest

from stackunderflow.services import meta_agent
from stackunderflow.store import db, schema


def _claude_raw(role: str, text: str | None, tool_uses: list) -> dict:
    content: list[dict] = []
    if text:
        content.append({"type": "text", "text": text})
    for i, (name, inp) in enumerate(tool_uses):
        content.append({"type": "tool_use", "id": f"toolu_{i}", "name": name, "input": inp})
    return {"type": role, "uuid": "u", "message": {"role": role, "content": content}}


@pytest.fixture
def cache_in_tmp(tmp_path, monkeypatch):
    fake_home = tmp_path / "home"
    fake_home.mkdir()
    monkeypatch.setattr(Path, "home", classmethod(lambda cls: fake_home))
    return fake_home


def _seed(conn, *, slug: str, n: int) -> None:
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
        edit_args = {"file_path": "/x/y.py", "old_string": "a", "new_string": "b"}
        turns = [
            ("user", "do a thing", []),
            ("assistant", "editing", [("Edit", edit_args)]),
            ("assistant", "running", [("Bash", {"command": "pytest tests/ -q"})]),
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


def test_tool_in_catalog():
    names = meta_agent.tool_names()
    assert "recommend_skills" in names


def test_dispatch_with_explicit_project(tmp_path, cache_in_tmp):
    conn = db.connect(tmp_path / "store.db")
    schema.apply(conn)
    _seed(conn, slug="-myproj", n=7)
    result = meta_agent.execute_tool(
        conn, "recommend_skills", {"project": "-myproj", "window_days": 365},
    )
    assert result.ok is True
    assert isinstance(result.data["recommendations"], list)
    assert result.data["recommendations"]
    # Heavy field stripped to keep the tool result inside the LLM budget.
    rec = result.data["recommendations"][0]
    assert "suggested_skill_template" not in rec
    assert "accept_command" in rec


def test_dispatch_uses_current_slug_when_project_omitted(tmp_path, cache_in_tmp):
    conn = db.connect(tmp_path / "store.db")
    schema.apply(conn)
    _seed(conn, slug="-currentproj", n=7)
    result = meta_agent.execute_tool(
        conn, "recommend_skills", {"window_days": 365},
        current_slug="-currentproj",
    )
    assert result.ok is True
    assert result.data["project"] == "-currentproj"


def test_dispatch_errors_when_no_project_anywhere(tmp_path, cache_in_tmp):
    conn = db.connect(tmp_path / "store.db")
    schema.apply(conn)
    result = meta_agent.execute_tool(
        conn, "recommend_skills", {},
    )
    assert result.ok is False
    assert "project is required" in result.data["error"]


def test_dispatch_errors_on_bad_threshold(tmp_path, cache_in_tmp):
    conn = db.connect(tmp_path / "store.db")
    schema.apply(conn)
    _seed(conn, slug="-myproj", n=2)
    # threshold gets clamped into [1, 50] so 0 becomes 1 — any low-occurrence
    # input should still produce a result without raising.
    result = meta_agent.execute_tool(
        conn, "recommend_skills",
        {"project": "-myproj", "threshold": 0, "window_days": 365},
    )
    assert result.ok is True


def test_dispatch_truncates_huge_payloads(tmp_path, cache_in_tmp):
    """The meta-agent caps result text; recommend_skills shouldn't blow it."""
    conn = db.connect(tmp_path / "store.db")
    schema.apply(conn)
    _seed(conn, slug="-myproj", n=7)
    result = meta_agent.execute_tool(
        conn, "recommend_skills",
        {"project": "-myproj", "window_days": 365},
    )
    encoded = json.dumps(result.data, default=str)
    assert len(encoded) <= 4_000  # _RESULT_CHAR_BUDGET in meta_agent
