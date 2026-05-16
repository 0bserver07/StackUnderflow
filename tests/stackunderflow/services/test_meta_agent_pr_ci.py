"""Tests for the Spec 20 meta-agent tools.

Exercises ``execute_tool`` against the seeded ``pr_outcomes`` /
``ci_runs`` tables for both ``get_pr_outcomes`` and ``get_ci_runs``.
The catalogue + executor wiring is asserted in lockstep so a future
contributor adding a tool can't slip past the "catalogue + dispatcher
+ tests" three-place rule.
"""

from __future__ import annotations

import json
import sqlite3
from pathlib import Path

import pytest

from stackunderflow.services import meta_agent
from stackunderflow.store import db, schema


@pytest.fixture
def conn(tmp_path: Path) -> sqlite3.Connection:
    c = db.connect(tmp_path / "store.db")
    schema.apply(c)
    yield c
    c.close()


def _seed_prs(conn: sqlite3.Connection) -> None:
    rows = [
        ("github", "octo/widgets", 1, "PR1", "open", None, None, "alice"),
        ("github", "octo/widgets", 2, "PR2", "merged",
         "2026-05-01T00:00:00Z", None, "bob"),
        ("github", "octo/widgets", 3, "PR3", "closed", None, None, "alice"),
        ("github", "other/repo", 1, "Other PR", "open", None, None, "alice"),
    ]
    for r in rows:
        conn.execute(
            "INSERT INTO pr_outcomes "
            "(provider, repo_slug, pr_number, title, state, "
            " merged_at, reverted_at, author, raw_json) "
            "VALUES (?, ?, ?, ?, ?, ?, ?, ?, '{}')",
            r,
        )


def _seed_ci_runs(conn: sqlite3.Connection) -> None:
    rows = [
        ("github-actions", "octo/widgets", "100", "abc",
         "success", "tests", "2026-05-01T10:00:00Z", "2026-05-01T10:05:00Z"),
        ("github-actions", "octo/widgets", "101", "abc",
         "failure", "lint", "2026-05-01T10:00:00Z", "2026-05-01T10:01:00Z"),
        ("github-actions", "octo/widgets", "102", "def",
         "success", "tests", "2026-05-02T10:00:00Z", "2026-05-02T10:05:00Z"),
        ("gitlab-ci", "other/repo", "200", "xyz",
         "in_progress", None, "2026-05-03T10:00:00Z", None),
    ]
    for r in rows:
        conn.execute(
            "INSERT INTO ci_runs "
            "(provider, repo_slug, run_id, commit_sha, status, "
            " workflow_name, started_ts, completed_ts, raw_json) "
            "VALUES (?, ?, ?, ?, ?, ?, ?, ?, '{}')",
            r,
        )


# ── catalogue + executor wiring ─────────────────────────────────────────────


def test_catalogue_includes_pr_and_ci_tools() -> None:
    names = {t["function"]["name"] for t in meta_agent.TOOL_CATALOG}
    assert "get_pr_outcomes" in names
    assert "get_ci_runs" in names


def test_executor_dispatches_pr_and_ci_tools() -> None:
    assert "get_pr_outcomes" in meta_agent._EXECUTORS
    assert "get_ci_runs" in meta_agent._EXECUTORS


def test_tool_catalogue_and_executor_keys_match() -> None:
    """Every tool in the catalogue MUST have a matching executor."""
    catalogue = {t["function"]["name"] for t in meta_agent.TOOL_CATALOG}
    executors = set(meta_agent._EXECUTORS.keys())
    assert catalogue == executors


# ── get_pr_outcomes ─────────────────────────────────────────────────────────


def test_get_pr_outcomes_requires_repo(conn: sqlite3.Connection) -> None:
    result = meta_agent.execute_tool(conn, "get_pr_outcomes", {})
    assert result.ok is False
    assert "repo" in result.data["error"]


def test_get_pr_outcomes_returns_repo_rows(conn: sqlite3.Connection) -> None:
    _seed_prs(conn)
    result = meta_agent.execute_tool(
        conn, "get_pr_outcomes", {"repo": "octo/widgets"}
    )
    assert result.ok is True
    assert result.data["count"] == 3
    numbers = {r["pr_number"] for r in result.data["pr_outcomes"]}
    assert numbers == {1, 2, 3}
    # Other repo's PR shouldn't leak in.
    repos = {r["repo_slug"] for r in result.data["pr_outcomes"]}
    assert repos == {"octo/widgets"}


def test_get_pr_outcomes_filters_by_state(conn: sqlite3.Connection) -> None:
    _seed_prs(conn)
    result = meta_agent.execute_tool(
        conn, "get_pr_outcomes", {"repo": "octo/widgets", "state": "merged"}
    )
    assert result.ok is True
    assert result.data["count"] == 1
    assert result.data["pr_outcomes"][0]["pr_number"] == 2


def test_get_pr_outcomes_respects_limit(conn: sqlite3.Connection) -> None:
    _seed_prs(conn)
    result = meta_agent.execute_tool(
        conn, "get_pr_outcomes", {"repo": "octo/widgets", "limit": 1}
    )
    assert result.ok is True
    assert result.data["count"] == 1


def test_get_pr_outcomes_invalid_since_returns_error(conn: sqlite3.Connection) -> None:
    _seed_prs(conn)
    result = meta_agent.execute_tool(
        conn, "get_pr_outcomes",
        {"repo": "octo/widgets", "since": "not-a-date"},
    )
    assert result.ok is False
    assert "since" in result.data["error"].lower()


# ── get_ci_runs ─────────────────────────────────────────────────────────────


def test_get_ci_runs_no_filters_returns_recent(conn: sqlite3.Connection) -> None:
    _seed_ci_runs(conn)
    result = meta_agent.execute_tool(conn, "get_ci_runs", {})
    assert result.ok is True
    assert result.data["count"] == 4


def test_get_ci_runs_filter_by_commit_sha(conn: sqlite3.Connection) -> None:
    _seed_ci_runs(conn)
    result = meta_agent.execute_tool(
        conn, "get_ci_runs", {"commit_sha": "abc"}
    )
    assert result.ok is True
    assert result.data["count"] == 2
    statuses = {r["status"] for r in result.data["ci_runs"]}
    assert statuses == {"success", "failure"}


def test_get_ci_runs_filter_by_status(conn: sqlite3.Connection) -> None:
    _seed_ci_runs(conn)
    result = meta_agent.execute_tool(
        conn, "get_ci_runs", {"status": "failure"}
    )
    assert result.ok is True
    assert result.data["count"] == 1
    assert result.data["ci_runs"][0]["run_id"] == "101"


def test_get_ci_runs_filter_by_repo(conn: sqlite3.Connection) -> None:
    _seed_ci_runs(conn)
    result = meta_agent.execute_tool(
        conn, "get_ci_runs", {"repo": "other/repo"}
    )
    assert result.ok is True
    assert result.data["count"] == 1
    assert result.data["ci_runs"][0]["provider"] == "gitlab-ci"


def test_get_ci_runs_combined_filters(conn: sqlite3.Connection) -> None:
    _seed_ci_runs(conn)
    result = meta_agent.execute_tool(
        conn, "get_ci_runs",
        {"commit_sha": "abc", "status": "success"},
    )
    assert result.ok is True
    assert result.data["count"] == 1
    assert result.data["ci_runs"][0]["run_id"] == "100"


def test_executor_results_are_json_safe(conn: sqlite3.Connection) -> None:
    _seed_prs(conn)
    _seed_ci_runs(conn)
    pr_result = meta_agent.execute_tool(
        conn, "get_pr_outcomes", {"repo": "octo/widgets"}
    )
    ci_result = meta_agent.execute_tool(conn, "get_ci_runs", {})
    # Both must serialise without exception.
    json.dumps(pr_result.to_dict(), default=str)
    json.dumps(ci_result.to_dict(), default=str)
