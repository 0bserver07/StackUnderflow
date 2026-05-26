"""Unit tests for outcome attribution service."""

from __future__ import annotations

import json
import sqlite3
import subprocess
from pathlib import Path
from unittest.mock import MagicMock, patch

import pytest
from fastapi.testclient import TestClient

from stackunderflow.routes.yield_route import router
from stackunderflow.services.outcome_attribution import (
    get_outcomes_for_session,
    link_commits_to_sessions,
)
from stackunderflow.store import db, schema


@pytest.fixture
def conn(tmp_path: Path) -> sqlite3.Connection:
    c = db.connect(tmp_path / "store.db")
    schema.apply(c)
    yield c
    c.close()


def test_link_commits_to_sessions_with_git_mock(conn: sqlite3.Connection, tmp_path: Path) -> None:
    # 1. Create a project and a session
    conn.execute(
        "INSERT INTO projects (id, provider, slug, display_name, first_seen, last_modified) "
        "VALUES (1, 'claude', 'my-widgets', 'My Widgets', 1700000000.0, 1700000000.0)"
    )
    conn.execute(
        "INSERT INTO sessions (id, project_id, session_id, first_ts, last_ts, message_count) "
        "VALUES (1, 1, 'sess_abc', '2026-05-01T12:00:00Z', '2026-05-01T13:00:00Z', 5)"
    )
    # Seed messages with a cwd
    conn.execute(
        "INSERT INTO messages (session_fk, seq, timestamp, role, raw_json) "
        "VALUES (1, 0, '2026-05-01T12:05:00Z', 'user', ?)",
        (json.dumps({"cwd": str(tmp_path)}),)
    )
    conn.commit()

    # Create dummy git directory so path.exists() & path.is_dir() succeed
    (tmp_path / ".git").mkdir()

    # Mock subprocess.run for git commands
    def mock_subprocess_run(args, **kwargs):
        # Check remote origin url config
        if "config" in args:
            return MagicMock(returncode=0, stdout="git@github.com:my-org/my-widgets.git\n")
        # Check rev-parse for git-dir
        if "rev-parse" in args:
            return MagicMock(returncode=0)
        # Check git log
        if "log" in args:
            stdout_content = "abc123commitsha|2026-05-01T12:10:00Z\ndef456commitsha|2026-05-01T12:20:00Z\n"
            return MagicMock(returncode=0, stdout=stdout_content)
        return MagicMock(returncode=1)

    with patch("subprocess.run", side_effect=mock_subprocess_run), \
         patch("shutil.which", return_value="/usr/bin/git"):
        link_commits_to_sessions(conn)

    # Verify that links were created
    rows = conn.execute("SELECT session_id, commit_sha, repo_slug, committed_at FROM commit_session_link").fetchall()
    assert len(rows) == 2
    assert rows[0]["session_id"] == "sess_abc"
    assert rows[0]["commit_sha"] == "abc123commitsha"
    assert rows[0]["repo_slug"] == "my-org/my-widgets"
    assert rows[0]["committed_at"] == "2026-05-01T12:10:00Z"


def test_get_outcomes_for_session(conn: sqlite3.Connection) -> None:
    # 1. Insert project and session
    conn.execute(
        "INSERT INTO projects (id, provider, slug, display_name, first_seen, last_modified) "
        "VALUES (1, 'claude', 'my-widgets', 'My Widgets', 1700000000.0, 1700000000.0)"
    )
    conn.execute(
        "INSERT INTO sessions (id, project_id, session_id, first_ts, last_ts, message_count) "
        "VALUES (1, 1, 'sess_abc', '2026-05-01T12:00:00Z', '2026-05-01T13:00:00Z', 5)"
    )
    # Insert commit session link
    conn.execute(
        "INSERT INTO commit_session_link (session_id, commit_sha, repo_slug, committed_at) "
        "VALUES ('sess_abc', 'abc123commitsha', 'my-org/my-widgets', '2026-05-01T12:10:00Z')"
    )

    # Seed PR outcomes with matching raw_json
    pr_raw = {
        "pull_request": {
            "head": {"sha": "abc123commitsha"},
            "merge_commit_sha": "some_other_sha"
        }
    }
    conn.execute(
        "INSERT INTO pr_outcomes (provider, repo_slug, pr_number, title, state, raw_json) "
        "VALUES ('github', 'my-org/my-widgets', 10, 'Test PR', 'merged', ?)",
        (json.dumps(pr_raw),)
    )

    # Seed CI runs matching commit_sha
    conn.execute(
        "INSERT INTO ci_runs (provider, repo_slug, run_id, commit_sha, status, raw_json) "
        "VALUES ('github-actions', 'my-org/my-widgets', 'run_999', 'abc123commitsha', 'success', '{}')"
    )
    conn.commit()

    # Retrieve outcomes
    outcomes = get_outcomes_for_session(conn, "sess_abc")
    assert len(outcomes["commits"]) == 1
    assert outcomes["commits"][0]["commit_sha"] == "abc123commitsha"

    assert len(outcomes["prs"]) == 1
    assert outcomes["prs"][0]["pr_number"] == 10
    assert outcomes["prs"][0]["title"] == "Test PR"

    assert len(outcomes["ci_runs"]) == 1
    assert outcomes["ci_runs"][0]["run_id"] == "run_999"
    assert outcomes["ci_runs"][0]["status"] == "success"
