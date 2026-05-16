"""``_detect_unused_mcp_servers`` reads ``tool_mart`` when populated.

The legacy detector scanned ``messages.tools_json`` row-by-row to roll
up the set of MCP tool names called in the lookback window — ~1.3s on
the maintainer's 60K-row store. The mart fast path issues one indexed
``SELECT DISTINCT tool_name FROM tool_mart WHERE tool_name LIKE 'mcp__%'``
instead, dropping the detector to <5ms.

Empty mart → the original ``tools_json`` scan still runs (tests in
``test_optimize.py::TestUnusedMcpServers`` cover that path).
"""

from __future__ import annotations

import json
import tempfile
from pathlib import Path
from unittest.mock import patch

import pytest

from stackunderflow.reports import optimize as optimize_mod
from stackunderflow.store import db, schema


@pytest.fixture
def env():
    """Open a fresh store + fake HOME so MCP registry reads are deterministic."""
    store_tmp = tempfile.TemporaryDirectory()
    home_tmp = tempfile.TemporaryDirectory()
    conn = db.connect(Path(store_tmp.name) / "store.db")
    schema.apply(conn)
    conn.execute(
        "INSERT INTO projects (id, provider, slug, display_name, first_seen, last_modified) "
        "VALUES (1, 'claude', 'demo', 'demo', 0, 0)"
    )
    conn.commit()
    home_patch = patch.object(
        Path, "home", classmethod(lambda cls: Path(home_tmp.name))  # noqa: ARG005
    )
    home_patch.start()
    yield conn, Path(home_tmp.name)
    home_patch.stop()
    conn.close()
    store_tmp.cleanup()
    home_tmp.cleanup()


def _write_mcp_registry(home: Path, servers: dict) -> None:
    (home / ".claude.json").write_text(json.dumps({"mcpServers": servers}))


def _insert_tool_mart(conn, *, day, tool_name, project_id=1):
    conn.execute(
        "INSERT OR REPLACE INTO tool_mart "
        "(day, project_id, provider, tool_name, event_count, cost_usd, "
        " tokens_in, tokens_out, session_count, calls_total) "
        "VALUES (?, ?, 'claude', ?, 1, 0, 0, 0, 1, 1)",
        (day, project_id, tool_name),
    )
    conn.commit()


def test_mart_fast_path_marks_used_servers(env):
    conn, home = env
    _write_mcp_registry(home, {
        "taco": {"command": "x"},
        "abandoned": {"command": "y"},
    })
    # tool_mart row for taco → detector should skip taco; abandoned remains.
    _insert_tool_mart(conn, day="2099-01-01", tool_name="mcp__taco__order")

    findings = optimize_mod._detect_unused_mcp_servers(conn)

    assert len(findings) == 1
    assert findings[0].affected_count == 1
    assert findings[0].details["unused_servers"] == ["abandoned"]


def test_mart_fast_path_no_finding_when_all_used(env):
    conn, home = env
    _write_mcp_registry(home, {"taco": {"command": "x"}})
    _insert_tool_mart(conn, day="2099-01-01", tool_name="mcp__taco__order")

    assert optimize_mod._detect_unused_mcp_servers(conn) == []


def test_mart_fast_path_ignores_non_mcp_tools(env):
    conn, home = env
    _write_mcp_registry(home, {"taco": {"command": "x"}})
    # Only non-MCP rows → mart is populated but no MCP usage at all.
    _insert_tool_mart(conn, day="2099-01-01", tool_name="Read")
    _insert_tool_mart(conn, day="2099-01-01", tool_name="Bash")

    findings = optimize_mod._detect_unused_mcp_servers(conn)
    assert len(findings) == 1
    assert "taco" in findings[0].details["unused_servers"]


def test_mart_fast_path_respects_lookback_window(env):
    conn, home = env
    _write_mcp_registry(home, {"taco": {"command": "x"}})
    # Old usage that falls outside the 30-day lookback.
    _insert_tool_mart(conn, day="2019-01-01", tool_name="mcp__taco__order")

    findings = optimize_mod._detect_unused_mcp_servers(conn)
    assert len(findings) == 1
    assert "taco" in findings[0].details["unused_servers"]
