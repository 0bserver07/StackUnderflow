"""Outcome Attribution v2 service.

Links sessions to Git commits, pull requests, and CI runs.
"""

from __future__ import annotations

import json
import logging
import shutil
import sqlite3
import subprocess
from datetime import UTC, datetime, timedelta
from pathlib import Path
from typing import Any

logger = logging.getLogger(__name__)

_GIT_TIMEOUT_SECONDS = 5


def parse_iso_ts(ts_str: str) -> datetime:
    """Parse ISO-8601 string to a timezone-aware datetime."""
    ts_str = ts_str.replace("Z", "+00:00")
    try:
        return datetime.fromisoformat(ts_str)
    except ValueError:
        # Fallback for alternative or truncated formats
        return datetime.strptime(ts_str[:19], "%Y-%m-%dT%H:%M:%S").replace(tzinfo=UTC)


def get_session_cwd(conn: sqlite3.Connection, session_id: str) -> str:
    """Extract the first non-empty CWD recorded in messages for this session."""
    row = conn.execute(
        "SELECT json_extract(m.raw_json, '$.cwd') AS cwd "
        "FROM messages m "
        "JOIN sessions s ON s.id = m.session_fk "
        "WHERE s.session_id = ? "
        "  AND json_extract(m.raw_json, '$.cwd') IS NOT NULL "
        "  AND json_extract(m.raw_json, '$.cwd') != '' "
        "ORDER BY m.seq LIMIT 1",
        (session_id,),
    ).fetchone()
    return str(row["cwd"]) if row else ""


def get_git_repo_slug(cwd: str, fallback: str) -> str:
    """Try to determine the GitHub repo slug from git config remote.origin.url."""
    try:
        result = subprocess.run(
            ["git", "-C", cwd, "config", "--get", "remote.origin.url"],
            capture_output=True,
            text=True,
            timeout=5,
        )
        if result.returncode == 0 and result.stdout.strip():
            url = result.stdout.strip()
            url = url.removesuffix(".git")
            if ":" in url:
                parts = url.split(":")[-1].split("/")
            else:
                parts = url.split("/")
            if len(parts) >= 2:
                return f"{parts[-2]}/{parts[-1]}"
    except Exception:
        pass
    return fallback


def link_commits_to_sessions(conn: sqlite3.Connection) -> None:
    """Scan all sessions that don't have commit links and establish them.

    Runs as part of the post-ingest metadata hook.
    """
    # Fetch sessions without links that have a first_ts timestamp
    sessions = conn.execute(
        "SELECT s.session_id, s.first_ts AS started_at, p.slug AS project_slug "
        "FROM sessions s "
        "JOIN projects p ON p.id = s.project_id "
        "WHERE s.first_ts IS NOT NULL "
        "  AND s.session_id NOT IN (SELECT DISTINCT session_id FROM commit_session_link)"
    ).fetchall()

    for row in sessions:
        session_id = row["session_id"]
        started_at = row["started_at"]
        project_slug = row["project_slug"]

        cwd = get_session_cwd(conn, session_id)
        if not cwd:
            continue

        p = Path(cwd)
        if not p.exists() or not p.is_dir() or shutil.which("git") is None:
            continue

        # Check if it's a git repo
        try:
            res = subprocess.run(
                ["git", "-C", cwd, "rev-parse", "--git-dir"],
                capture_output=True,
                timeout=_GIT_TIMEOUT_SECONDS,
            )
            if res.returncode != 0:
                continue
        except Exception:
            continue

        try:
            started_dt = parse_iso_ts(started_at)
            end_dt = started_dt + timedelta(hours=24)
            since_str = started_dt.isoformat()
            until_str = end_dt.isoformat()
        except Exception as e:
            logger.debug("Failed to parse start time %s: %s", started_at, e)
            continue

        # Fetch commits in the 24h window
        try:
            result = subprocess.run(
                [
                    "git",
                    "-C",
                    cwd,
                    "log",
                    "--all",
                    f"--since={since_str}",
                    f"--until={until_str}",
                    "--format=%H|%cI",
                ],
                capture_output=True,
                text=True,
                timeout=_GIT_TIMEOUT_SECONDS,
            )
            if result.returncode != 0:
                continue
        except Exception as e:
            logger.debug("git log failed in %s: %s", cwd, e)
            continue

        repo_slug = get_git_repo_slug(cwd, fallback=project_slug)

        for line in result.stdout.splitlines():
            line = line.strip()
            if not line or "|" not in line:
                continue
            sha, commit_time = line.split("|", 1)

            # Insert into database
            conn.execute(
                "INSERT OR IGNORE INTO commit_session_link (session_id, commit_sha, repo_slug, committed_at) "
                "VALUES (?, ?, ?, ?)",
                (session_id, sha, repo_slug, commit_time),
            )
    conn.commit()


def _pr_matches_commit(raw_json_str: str, commit_sha: str) -> bool:
    """Parse raw_json webhook payload to see if it matches the commit_sha."""
    try:
        data = json.loads(raw_json_str)
    except Exception:
        return False

    # Check various standard locations in GitHub / GitLab webhooks
    pr = data.get("pull_request", data)
    if not isinstance(pr, dict):
        return False

    head = pr.get("head", {})
    head_sha = head.get("sha") if isinstance(head, dict) else None
    merge_sha = pr.get("merge_commit_sha")

    if head_sha == commit_sha or merge_sha == commit_sha:
        return True

    # GitLab MR webhook structure
    obj_attr = data.get("object_attributes", {})
    if isinstance(obj_attr, dict):
        last_commit = obj_attr.get("last_commit", {})
        last_sha = last_commit.get("id") if isinstance(last_commit, dict) else None
        merge_commit_sha = obj_attr.get("merge_commit_sha")
        if last_sha == commit_sha or merge_commit_sha == commit_sha:
            return True

    return False


def get_outcomes_for_session(conn: sqlite3.Connection, session_id: str) -> dict[str, Any]:
    """Retrieve commits, PRs, and CI runs linked to a session."""
    commits = conn.execute(
        "SELECT commit_sha, repo_slug, committed_at FROM commit_session_link WHERE session_id = ?",
        (session_id,)
    ).fetchall()

    pr_list = []
    ci_list = []

    for c in commits:
        sha = c["commit_sha"]

        # PR outcomes: scan by raw_json LIKE containing commit_sha
        candidates = conn.execute(
            "SELECT provider, repo_slug, pr_number, title, state, merged_at, reverted_at, author, raw_json "
            "FROM pr_outcomes WHERE raw_json LIKE ?",
            (f"%{sha}%",)
        ).fetchall()

        for cand in candidates:
            if _pr_matches_commit(cand["raw_json"], sha):
                pr_list.append({
                    "provider": cand["provider"],
                    "repo_slug": cand["repo_slug"],
                    "pr_number": cand["pr_number"],
                    "title": cand["title"],
                    "state": cand["state"],
                    "merged_at": cand["merged_at"],
                    "reverted_at": cand["reverted_at"],
                    "author": cand["author"],
                })

        # CI runs matching commit_sha
        runs = conn.execute(
            "SELECT provider, repo_slug, run_id, commit_sha, status, workflow_name, started_ts, completed_ts "
            "FROM ci_runs WHERE commit_sha = ?",
            (sha,)
        ).fetchall()

        for run in runs:
            ci_list.append({
                "provider": run["provider"],
                "repo_slug": run["repo_slug"],
                "run_id": run["run_id"],
                "commit_sha": run["commit_sha"],
                "status": run["status"],
                "workflow_name": run["workflow_name"],
                "started_ts": run["started_ts"],
                "completed_ts": run["completed_ts"],
            })

    # Deduplicate PRs by (provider, repo_slug, pr_number)
    unique_prs = {}
    for pr in pr_list:
        key = (pr["provider"], pr["repo_slug"], pr["pr_number"])
        unique_prs[key] = pr

    # Deduplicate CI runs by (provider, run_id)
    unique_cis = {}
    for ci in ci_list:
        key = (ci["provider"], ci["run_id"])
        unique_cis[key] = ci

    return {
        "commits": [dict(c) for c in commits],
        "prs": list(unique_prs.values()),
        "ci_runs": list(unique_cis.values()),
    }
