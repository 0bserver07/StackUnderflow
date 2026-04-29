"""Smoke tests for the StackUnderflow MCP server.

Sanity-checks that:
  * the FastMCP server registers `session_query` as a tool,
  * the server entrypoint exists and is callable,
  * `session_query_impl` correctly discovers, parses, filters, and
    sorts records from a fake agent-home directory tree.
"""

from __future__ import annotations

import json
from pathlib import Path

import pytest

from stackunderflow.mcp import server as mcp_server


def _write_jsonl(path: Path, lines: list[dict]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("w") as fh:
        for obj in lines:
            fh.write(json.dumps(obj) + "\n")


def _user(session_id: str, ts: str, text: str, uuid: str = "u") -> dict:
    return {
        "sessionId": session_id,
        "type": "user",
        "timestamp": ts,
        "uuid": uuid,
        "message": {"role": "user", "content": text},
    }


def _assistant_tool_use(
    session_id: str,
    ts: str,
    tool_name: str,
    tool_input: dict,
    uuid: str = "ua",
) -> dict:
    return {
        "sessionId": session_id,
        "type": "assistant",
        "timestamp": ts,
        "uuid": uuid,
        "message": {
            "role": "assistant",
            "model": "claude-opus-4-7",
            "content": [
                {
                    "type": "tool_use",
                    "name": tool_name,
                    "id": f"toolu_{uuid}",
                    "input": tool_input,
                }
            ],
            "usage": {"input_tokens": 1, "output_tokens": 1},
        },
    }


def _user_tool_result(
    session_id: str,
    ts: str,
    text: str,
    *,
    is_error: bool = False,
    uuid: str = "ur",
) -> dict:
    return {
        "sessionId": session_id,
        "type": "user",
        "timestamp": ts,
        "uuid": uuid,
        "message": {
            "role": "user",
            "content": [
                {
                    "type": "tool_result",
                    "tool_use_id": "toolu_x",
                    "content": text,
                    "is_error": is_error,
                }
            ],
        },
    }


@pytest.fixture
def fake_agent_home(tmp_path: Path) -> Path:
    """Lay out two agent roots: ~/.claude and ~/.claude-opus."""
    claude = tmp_path / ".claude" / "projects" / "-Users-me-app"
    opus = tmp_path / ".claude-opus" / "projects" / "-home-andy-repo"

    _write_jsonl(
        claude / "s-claude.jsonl",
        [
            _user("s-claude", "2026-04-29T01:00:00Z", "hello", uuid="u1"),
            _assistant_tool_use(
                "s-claude",
                "2026-04-29T01:00:01Z",
                "Read",
                {"file_path": "fixture/foo.py"},
                uuid="u2",
            ),
        ],
    )
    _write_jsonl(
        opus / "s-opus.jsonl",
        [
            _user("s-opus", "2026-04-29T02:00:00Z", "do the thing", uuid="o1"),
            _assistant_tool_use(
                "s-opus",
                "2026-04-29T02:00:01Z",
                "Bash",
                {"command": "ls"},
                uuid="o2",
            ),
            _user_tool_result(
                "s-opus",
                "2026-04-29T02:00:02Z",
                "Traceback (most recent call last):\nValueError: bad",
                is_error=True,
                uuid="o3",
            ),
        ],
    )
    return tmp_path


def test_server_registered() -> None:
    """FastMCP server exists with the right name."""
    assert mcp_server.mcp.name == "stackunderflow"


def test_main_entrypoint_callable() -> None:
    """The console-script entry point is a real callable."""
    assert callable(mcp_server.main)


def test_session_query_impl_returns_records(fake_agent_home: Path) -> None:
    roots = [fake_agent_home / ".claude", fake_agent_home / ".claude-opus"]
    out = mcp_server.session_query_impl(roots=roots)
    assert isinstance(out, list)
    assert len(out) >= 1
    # Most-recent-first sort: opus session at 02:00 should beat claude at 01:00.
    assert out[0]["timestamp"].startswith("2026-04-29T02:")
    assert {r["agent"] for r in out} <= {"claude", "claude-opus"}
    for r in out:
        assert "project_slug" in r
        assert "session_id" in r
        assert "role" in r


def test_session_query_filters_by_session_id(fake_agent_home: Path) -> None:
    roots = [fake_agent_home / ".claude", fake_agent_home / ".claude-opus"]
    out = mcp_server.session_query_impl(session_id="s-opus", roots=roots)
    assert len(out) >= 1
    assert all(r["session_id"] == "s-opus" for r in out)


def test_session_query_filters_tool_calls(fake_agent_home: Path) -> None:
    roots = [fake_agent_home / ".claude", fake_agent_home / ".claude-opus"]
    out = mcp_server.session_query_impl(kind="tool_calls", roots=roots)
    assert len(out) >= 1
    for r in out:
        assert r["tools"], f"expected tool calls, got {r}"
        assert r["tool_calls"], f"expected summarized tool_calls, got {r}"
        assert r["tool_calls"][0]["name"] in {"Read", "Bash"}


def test_session_query_filters_errors(fake_agent_home: Path) -> None:
    roots = [fake_agent_home / ".claude", fake_agent_home / ".claude-opus"]
    out = mcp_server.session_query_impl(kind="errors", roots=roots)
    assert len(out) == 1
    assert out[0]["session_id"] == "s-opus"


def test_session_query_respects_limit(fake_agent_home: Path) -> None:
    roots = [fake_agent_home / ".claude", fake_agent_home / ".claude-opus"]
    out = mcp_server.session_query_impl(limit=2, roots=roots)
    assert len(out) <= 2


def test_session_query_zero_limit_returns_empty(fake_agent_home: Path) -> None:
    out = mcp_server.session_query_impl(limit=0, roots=[fake_agent_home / ".claude"])
    assert out == []


def test_session_query_skips_missing_roots(tmp_path: Path) -> None:
    """Non-existent roots are silently ignored."""
    out = mcp_server.session_query_impl(roots=[tmp_path / "does-not-exist"])
    assert out == []


def test_tool_args_summary_truncates_long_strings(fake_agent_home: Path) -> None:
    """Long tool-input strings should be truncated in the summary."""
    long_cmd = "x" * 500
    project = fake_agent_home / ".claude" / "projects" / "-Users-me-long"
    _write_jsonl(
        project / "s-long.jsonl",
        [_assistant_tool_use("s-long", "2026-04-29T03:00:00Z", "Bash", {"command": long_cmd})],
    )
    out = mcp_server.session_query_impl(
        session_id="s-long",
        kind="tool_calls",
        roots=[fake_agent_home / ".claude"],
    )
    assert out, "expected at least one record"
    args = out[0]["tool_calls"][0]["args"]
    assert args["command"].endswith("…")
    assert len(args["command"]) < 500
