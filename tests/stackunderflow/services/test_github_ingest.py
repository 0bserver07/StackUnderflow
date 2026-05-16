"""Tests for ``stackunderflow.services.github_ingest``.

Covers:

* ``normalise_pr_payload`` / ``normalise_ci_run_payload`` — pure
  parsers; assert the column tuple lined up against the spec.
* ``upsert_pr_outcome`` / ``upsert_ci_run`` — insert + update the same
  row; assert the verb returned and the column changes.
* ``backfill_repo`` — wired to a mocked ``httpx.Client`` via
  ``client_factory``. Exercises pagination, the ``workflow_runs``
  envelope unwrap, and the 404-on-actions warning path.
"""

from __future__ import annotations

import json
import sqlite3
from pathlib import Path
from typing import Any

import httpx
import pytest

from stackunderflow.services import github_ingest
from stackunderflow.store import db, schema

# ── fixtures ────────────────────────────────────────────────────────────────


@pytest.fixture
def conn(tmp_path: Path) -> sqlite3.Connection:
    c = db.connect(tmp_path / "store.db")
    schema.apply(c)
    yield c
    c.close()


# ── normalise_pr_payload ────────────────────────────────────────────────────


def test_normalise_pr_payload_open_pr() -> None:
    payload = {
        "number": 42,
        "title": "Add cool feature",
        "state": "open",
        "merged": False,
        "merged_at": None,
        "user": {"login": "octocat"},
        "base": {"repo": {"full_name": "octocat/hello-world"}},
    }
    row = github_ingest.normalise_pr_payload(payload)
    assert row["provider"] == "github"
    assert row["repo_slug"] == "octocat/hello-world"
    assert row["pr_number"] == 42
    assert row["title"] == "Add cool feature"
    assert row["state"] == "open"
    assert row["merged_at"] is None
    assert row["author"] == "octocat"
    assert row["reverted_at"] is None
    # raw_json round-trips back to the original payload.
    assert json.loads(row["raw_json"]) == payload


def test_normalise_pr_payload_merged_pr_promotes_state() -> None:
    payload = {
        "number": 7,
        "title": "Fix bug",
        "state": "closed",
        "merged": True,
        "merged_at": "2026-05-01T10:00:00Z",
        "user": {"login": "alice"},
    }
    row = github_ingest.normalise_pr_payload(
        payload, repo_slug="acme/widgets"
    )
    # closed + merged_at => state collapses to "merged".
    assert row["state"] == "merged"
    assert row["merged_at"] == "2026-05-01T10:00:00Z"
    assert row["repo_slug"] == "acme/widgets"


def test_normalise_pr_payload_handles_missing_user() -> None:
    row = github_ingest.normalise_pr_payload(
        {"number": 1, "state": "open"}, repo_slug="x/y"
    )
    assert row["author"] is None
    assert row["title"] is None


# ── normalise_ci_run_payload ────────────────────────────────────────────────


def test_normalise_ci_run_payload_success() -> None:
    payload = {
        "id": 9876,
        "head_sha": "abc123def456",
        "status": "completed",
        "conclusion": "success",
        "name": "test",
        "run_started_at": "2026-05-01T10:00:00Z",
        "updated_at": "2026-05-01T10:05:00Z",
        "repository": {"full_name": "octocat/hello-world"},
    }
    row = github_ingest.normalise_ci_run_payload(payload)
    assert row["provider"] == "github-actions"
    assert row["repo_slug"] == "octocat/hello-world"
    assert row["run_id"] == "9876"
    assert row["commit_sha"] == "abc123def456"
    assert row["status"] == "success"
    assert row["workflow_name"] == "test"
    assert row["started_ts"] == "2026-05-01T10:00:00Z"
    assert row["completed_ts"] == "2026-05-01T10:05:00Z"


def test_normalise_ci_run_payload_in_progress_no_conclusion() -> None:
    payload = {
        "id": 1, "head_sha": "abc",
        "status": "in_progress", "conclusion": None,
        "name": "build",
    }
    row = github_ingest.normalise_ci_run_payload(payload, repo_slug="x/y")
    assert row["status"] == "in_progress"
    # No conclusion => completed_ts is None.
    assert row["completed_ts"] is None


def test_normalise_ci_run_payload_status_normalisation() -> None:
    cases = [
        ("success", "success"),
        ("failure", "failure"),
        ("timed_out", "failure"),
        ("cancelled", "cancelled"),
        ("canceled", "cancelled"),
        ("skipped", "skipped"),
        ("queued", "pending"),
        ("garbage", "in_progress"),
    ]
    for raw, expected in cases:
        payload = {"id": 1, "head_sha": "x", "conclusion": raw}
        row = github_ingest.normalise_ci_run_payload(payload, repo_slug="x/y")
        assert row["status"] == expected, f"{raw!r} should map to {expected!r}"


# ── upsert_pr_outcome ───────────────────────────────────────────────────────


def test_upsert_pr_outcome_insert_then_update(conn: sqlite3.Connection) -> None:
    row = github_ingest.normalise_pr_payload(
        {"number": 1, "state": "open", "title": "first"},
        repo_slug="x/y",
    )
    assert github_ingest.upsert_pr_outcome(conn, row) == "inserted"
    # Same key, different state — update wins.
    row2 = github_ingest.normalise_pr_payload(
        {"number": 1, "state": "closed", "merged": True,
         "merged_at": "2026-05-01T00:00:00Z", "title": "first"},
        repo_slug="x/y",
    )
    assert github_ingest.upsert_pr_outcome(conn, row2) == "updated"
    out = conn.execute(
        "SELECT state, merged_at FROM pr_outcomes WHERE pr_number = 1"
    ).fetchone()
    assert out["state"] == "merged"
    assert out["merged_at"] == "2026-05-01T00:00:00Z"
    assert conn.execute("SELECT COUNT(*) FROM pr_outcomes").fetchone()[0] == 1


def test_upsert_pr_outcome_preserves_reverted_at(conn: sqlite3.Connection) -> None:
    """The downstream Spec 22 will set ``reverted_at`` independently;
    a follow-up webhook from GitHub must not clobber it back to NULL.
    """
    row = github_ingest.normalise_pr_payload(
        {"number": 1, "state": "merged", "merged": True,
         "merged_at": "2026-05-01T00:00:00Z"},
        repo_slug="x/y",
    )
    github_ingest.upsert_pr_outcome(conn, row)
    # Spec 22 sets reverted_at directly.
    conn.execute(
        "UPDATE pr_outcomes SET reverted_at = ? WHERE pr_number = 1",
        ("2026-05-02T12:00:00Z",),
    )
    # A new webhook arrives — the reverted_at field must survive.
    row2 = github_ingest.normalise_pr_payload(
        {"number": 1, "state": "merged", "merged": True,
         "merged_at": "2026-05-01T00:00:00Z", "title": "updated"},
        repo_slug="x/y",
    )
    github_ingest.upsert_pr_outcome(conn, row2)
    out = conn.execute(
        "SELECT reverted_at FROM pr_outcomes WHERE pr_number = 1"
    ).fetchone()
    assert out["reverted_at"] == "2026-05-02T12:00:00Z"


# ── upsert_ci_run ───────────────────────────────────────────────────────────


def test_upsert_ci_run_insert_then_update(conn: sqlite3.Connection) -> None:
    row = github_ingest.normalise_ci_run_payload(
        {"id": 1, "head_sha": "abc", "status": "in_progress"},
        repo_slug="x/y",
    )
    assert github_ingest.upsert_ci_run(conn, row) == "inserted"
    row2 = github_ingest.normalise_ci_run_payload(
        {"id": 1, "head_sha": "abc", "conclusion": "success",
         "updated_at": "2026-05-01T10:05:00Z"},
        repo_slug="x/y",
    )
    assert github_ingest.upsert_ci_run(conn, row2) == "updated"
    out = conn.execute(
        "SELECT status, completed_ts FROM ci_runs WHERE run_id = '1'"
    ).fetchone()
    assert out["status"] == "success"
    assert out["completed_ts"] == "2026-05-01T10:05:00Z"


# ── backfill_repo (mocked HTTP) ─────────────────────────────────────────────


def _mock_transport(handler):
    return httpx.MockTransport(handler)


def _client_factory(transport):
    return lambda: httpx.Client(transport=transport, timeout=5.0)


def test_backfill_repo_fetches_prs_and_ci(conn: sqlite3.Connection) -> None:
    pr_payloads = [
        {"number": i, "state": "open" if i % 2 else "closed",
         "merged": False, "title": f"PR {i}", "user": {"login": "alice"}}
        for i in range(1, 4)
    ]
    ci_payloads = [
        {"id": 100 + i, "head_sha": f"sha{i}",
         "status": "completed", "conclusion": "success",
         "name": "tests", "repository": {"full_name": "acme/widgets"}}
        for i in range(1, 4)
    ]

    def handler(request: httpx.Request) -> httpx.Response:
        if "/pulls" in str(request.url):
            return httpx.Response(200, json=pr_payloads)
        if "/actions/runs" in str(request.url):
            # Workflow-runs uses an envelope.
            return httpx.Response(
                200, json={"total_count": 3, "workflow_runs": ci_payloads}
            )
        return httpx.Response(404)

    report = github_ingest.backfill_repo(
        conn, "acme/widgets",
        token="ghp_fake",
        max_pages=1,
        client_factory=_client_factory(_mock_transport(handler)),
    )
    assert report.repo_slug == "acme/widgets"
    assert report.pr_inserted == 3
    assert report.ci_inserted == 3
    assert report.pr_pages_fetched == 1
    assert report.ci_pages_fetched == 1
    assert report.warnings == []
    # Sanity: the rows actually landed.
    assert conn.execute(
        "SELECT COUNT(*) FROM pr_outcomes WHERE repo_slug = 'acme/widgets'"
    ).fetchone()[0] == 3
    assert conn.execute(
        "SELECT COUNT(*) FROM ci_runs WHERE repo_slug = 'acme/widgets'"
    ).fetchone()[0] == 3


def test_backfill_repo_handles_404_on_actions(conn: sqlite3.Connection) -> None:
    pr_payloads = [
        {"number": 1, "state": "open", "merged": False, "title": "PR 1"}
    ]

    def handler(request: httpx.Request) -> httpx.Response:
        if "/pulls" in str(request.url):
            return httpx.Response(200, json=pr_payloads)
        if "/actions/runs" in str(request.url):
            return httpx.Response(404)
        return httpx.Response(404)

    report = github_ingest.backfill_repo(
        conn, "acme/widgets",
        token="ghp_fake",
        max_pages=1,
        client_factory=_client_factory(_mock_transport(handler)),
    )
    assert report.pr_inserted == 1
    assert report.ci_inserted == 0
    assert "no GitHub Actions workflow runs found" in report.warnings


def test_backfill_repo_rate_limit_raises(conn: sqlite3.Connection) -> None:
    def handler(request: httpx.Request) -> httpx.Response:
        return httpx.Response(
            403,
            headers={
                "x-ratelimit-remaining": "0",
                "x-ratelimit-reset": "1717000000",
            },
            text="rate limit exceeded",
        )

    with pytest.raises(github_ingest.RateLimitedError):
        github_ingest.backfill_repo(
            conn, "acme/widgets",
            token=None,  # unauthenticated requests hit the lower limit faster
            max_pages=1,
            client_factory=_client_factory(_mock_transport(handler)),
        )


def test_backfill_repo_no_token_omits_auth_header(conn: sqlite3.Connection) -> None:
    seen_headers: dict[str, Any] = {}

    def handler(request: httpx.Request) -> httpx.Response:
        seen_headers.update(dict(request.headers))
        if "/pulls" in str(request.url):
            return httpx.Response(200, json=[])
        return httpx.Response(200, json={"workflow_runs": []})

    github_ingest.backfill_repo(
        conn, "acme/widgets",
        token=None,
        max_pages=1,
        client_factory=_client_factory(_mock_transport(handler)),
    )
    # Confirms tokens aren't fabricated when the caller doesn't supply
    # one — corollary of the "tokens never persisted" rule.
    assert "authorization" not in {k.lower() for k in seen_headers}


def test_backfill_repo_token_in_auth_header(conn: sqlite3.Connection) -> None:
    captured = {}

    def handler(request: httpx.Request) -> httpx.Response:
        captured.setdefault("auth", request.headers.get("authorization"))
        if "/pulls" in str(request.url):
            return httpx.Response(200, json=[])
        return httpx.Response(200, json={"workflow_runs": []})

    github_ingest.backfill_repo(
        conn, "acme/widgets",
        token="ghp_secrettoken",
        max_pages=1,
        client_factory=_client_factory(_mock_transport(handler)),
    )
    assert captured["auth"] == "token ghp_secrettoken"
