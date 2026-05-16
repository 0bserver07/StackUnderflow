"""Tests for ``stackunderflow/routes/webhooks.py`` — Spec 20.

Coverage:

* Signature validation: GitHub HMAC-SHA256, GitLab token-compare, generic
  CI HMAC-SHA256. Missing or mismatched secrets always return 403; the
  unset-env case returns 503 (opt-in by design).
* End-to-end: a valid GitHub PR / workflow_run event upserts into the
  store. A valid GitLab merge_request / pipeline event lands the same
  row shape.
* Token storage: confirms the secret is read fresh from the env on
  every request — never from a database column.
"""

from __future__ import annotations

import hashlib
import hmac
import json
from pathlib import Path

import pytest
from fastapi import FastAPI
from fastapi.testclient import TestClient

import stackunderflow.deps as deps
from stackunderflow.routes import webhooks as webhooks_route
from stackunderflow.store import db, schema

ENV_GITHUB_SECRET = webhooks_route.ENV_GITHUB_SECRET
ENV_GITLAB_SECRET = webhooks_route.ENV_GITLAB_SECRET
ENV_CI_SECRET = webhooks_route.ENV_CI_SECRET
webhook_router = webhooks_route.router

# ── fixtures ────────────────────────────────────────────────────────────────


@pytest.fixture()
def app_client(tmp_path: Path, monkeypatch):
    store_db = tmp_path / "store.db"
    conn = db.connect(store_db)
    schema.apply(conn)
    conn.close()
    monkeypatch.setattr(deps, "store_path", store_db)
    # Ensure env starts clean — each test sets only the secrets it needs.
    for var in (ENV_GITHUB_SECRET, ENV_GITLAB_SECRET, ENV_CI_SECRET):
        monkeypatch.delenv(var, raising=False)
    app = FastAPI()
    app.include_router(webhook_router)
    return TestClient(app), store_db


def _sign(secret: str, body: bytes) -> str:
    return "sha256=" + hmac.new(
        secret.encode("utf-8"), body, hashlib.sha256
    ).hexdigest()


# ── 503 when receiver not configured ────────────────────────────────────────


def test_github_webhook_returns_503_when_secret_unset(app_client) -> None:
    client, _ = app_client
    r = client.post("/api/webhooks/github", json={"ping": True})
    assert r.status_code == 503
    assert "STACKUNDERFLOW_GITHUB_WEBHOOK_SECRET" in r.json()["detail"]


def test_gitlab_webhook_returns_503_when_secret_unset(app_client) -> None:
    client, _ = app_client
    r = client.post("/api/webhooks/gitlab", json={"object_kind": "merge_request"})
    assert r.status_code == 503
    assert "STACKUNDERFLOW_GITLAB_WEBHOOK_SECRET" in r.json()["detail"]


def test_ci_webhook_returns_503_when_secret_unset(app_client) -> None:
    client, _ = app_client
    r = client.post("/api/webhooks/ci", json={"id": 1})
    assert r.status_code == 503


# ── 403 on missing / mismatched signatures ─────────────────────────────────


def test_github_webhook_403_on_missing_signature(app_client, monkeypatch) -> None:
    monkeypatch.setenv(ENV_GITHUB_SECRET, "supersecret")
    client, _ = app_client
    r = client.post("/api/webhooks/github", json={"action": "opened"})
    assert r.status_code == 403


def test_github_webhook_403_on_wrong_signature(app_client, monkeypatch) -> None:
    monkeypatch.setenv(ENV_GITHUB_SECRET, "supersecret")
    client, _ = app_client
    body = json.dumps({"action": "opened"}).encode()
    bad_sig = "sha256=" + "0" * 64
    r = client.post(
        "/api/webhooks/github",
        content=body,
        headers={
            "X-Hub-Signature-256": bad_sig,
            "X-GitHub-Event": "ping",
            "Content-Type": "application/json",
        },
    )
    assert r.status_code == 403


def test_gitlab_webhook_403_on_wrong_token(app_client, monkeypatch) -> None:
    monkeypatch.setenv(ENV_GITLAB_SECRET, "thesecret")
    client, _ = app_client
    r = client.post(
        "/api/webhooks/gitlab",
        json={"object_kind": "merge_request"},
        headers={"X-Gitlab-Token": "wrongsecret"},
    )
    assert r.status_code == 403


def test_gitlab_webhook_403_on_missing_token(app_client, monkeypatch) -> None:
    monkeypatch.setenv(ENV_GITLAB_SECRET, "thesecret")
    client, _ = app_client
    r = client.post(
        "/api/webhooks/gitlab",
        json={"object_kind": "merge_request"},
    )
    assert r.status_code == 403


def test_ci_webhook_403_on_wrong_signature(app_client, monkeypatch) -> None:
    monkeypatch.setenv(ENV_CI_SECRET, "ci-secret")
    client, _ = app_client
    body = json.dumps({"id": 1}).encode()
    r = client.post(
        "/api/webhooks/ci",
        content=body,
        headers={
            "X-Webhook-Signature-256": "sha256=" + "0" * 64,
            "Content-Type": "application/json",
        },
    )
    assert r.status_code == 403


# ── 200 on valid signatures ────────────────────────────────────────────────


def test_github_ping_event_returns_pong(app_client, monkeypatch) -> None:
    monkeypatch.setenv(ENV_GITHUB_SECRET, "supersecret")
    client, _ = app_client
    body = json.dumps({"zen": "Hi"}).encode()
    r = client.post(
        "/api/webhooks/github",
        content=body,
        headers={
            "X-Hub-Signature-256": _sign("supersecret", body),
            "X-GitHub-Event": "ping",
            "Content-Type": "application/json",
        },
    )
    assert r.status_code == 200
    assert r.json() == {"status": "pong"}


def test_github_pull_request_event_upserts(app_client, monkeypatch) -> None:
    monkeypatch.setenv(ENV_GITHUB_SECRET, "supersecret")
    client, store_db = app_client
    payload = {
        "action": "opened",
        "pull_request": {
            "number": 42, "state": "open", "merged": False,
            "title": "Add cool feature",
            "user": {"login": "octocat"},
        },
        "repository": {"full_name": "octocat/hello-world"},
    }
    body = json.dumps(payload).encode()
    r = client.post(
        "/api/webhooks/github",
        content=body,
        headers={
            "X-Hub-Signature-256": _sign("supersecret", body),
            "X-GitHub-Event": "pull_request",
            "Content-Type": "application/json",
        },
    )
    assert r.status_code == 200
    out = r.json()
    assert out["status"] == "ok"
    assert out["kind"] == "pr"
    assert out["pr_number"] == 42
    # Row landed in the store.
    conn = db.connect(store_db)
    try:
        row = conn.execute(
            "SELECT title, state, author FROM pr_outcomes WHERE pr_number = 42"
        ).fetchone()
    finally:
        conn.close()
    assert row["title"] == "Add cool feature"
    assert row["state"] == "open"
    assert row["author"] == "octocat"


def test_github_workflow_run_event_upserts(app_client, monkeypatch) -> None:
    monkeypatch.setenv(ENV_GITHUB_SECRET, "supersecret")
    client, store_db = app_client
    payload = {
        "action": "completed",
        "workflow_run": {
            "id": 9876, "head_sha": "abc123",
            "status": "completed", "conclusion": "success",
            "name": "test", "run_started_at": "2026-05-01T10:00:00Z",
            "updated_at": "2026-05-01T10:05:00Z",
        },
        "repository": {"full_name": "octocat/hello-world"},
    }
    body = json.dumps(payload).encode()
    r = client.post(
        "/api/webhooks/github",
        content=body,
        headers={
            "X-Hub-Signature-256": _sign("supersecret", body),
            "X-GitHub-Event": "workflow_run",
            "Content-Type": "application/json",
        },
    )
    assert r.status_code == 200
    out = r.json()
    assert out["kind"] == "ci"
    assert out["run_id"] == "9876"
    conn = db.connect(store_db)
    try:
        row = conn.execute(
            "SELECT status, commit_sha, repo_slug FROM ci_runs WHERE run_id = '9876'"
        ).fetchone()
    finally:
        conn.close()
    assert row["status"] == "success"
    assert row["commit_sha"] == "abc123"
    assert row["repo_slug"] == "octocat/hello-world"


def test_github_unknown_event_is_ignored(app_client, monkeypatch) -> None:
    monkeypatch.setenv(ENV_GITHUB_SECRET, "supersecret")
    client, _ = app_client
    body = json.dumps({"action": "labeled"}).encode()
    r = client.post(
        "/api/webhooks/github",
        content=body,
        headers={
            "X-Hub-Signature-256": _sign("supersecret", body),
            "X-GitHub-Event": "issues",
            "Content-Type": "application/json",
        },
    )
    assert r.status_code == 200
    assert r.json()["status"] == "ignored"


def test_gitlab_merge_request_event_upserts(app_client, monkeypatch) -> None:
    monkeypatch.setenv(ENV_GITLAB_SECRET, "gl-secret")
    client, store_db = app_client
    payload = {
        "object_kind": "merge_request",
        "object_attributes": {
            "iid": 7, "state": "opened", "title": "GL: add feature",
        },
        "project": {"path_with_namespace": "group/project"},
        "user": {"username": "alice"},
    }
    r = client.post(
        "/api/webhooks/gitlab",
        json=payload,
        headers={
            "X-Gitlab-Token": "gl-secret",
            "X-Gitlab-Event": "Merge Request Hook",
        },
    )
    assert r.status_code == 200
    out = r.json()
    assert out["kind"] == "pr"
    assert out["pr_number"] == 7
    conn = db.connect(store_db)
    try:
        row = conn.execute(
            "SELECT provider, repo_slug, state, author FROM pr_outcomes "
            "WHERE pr_number = 7"
        ).fetchone()
    finally:
        conn.close()
    assert row["provider"] == "gitlab"
    assert row["repo_slug"] == "group/project"
    # GitLab "opened" => our "open" enum.
    assert row["state"] == "open"
    assert row["author"] == "alice"


def test_gitlab_pipeline_event_upserts(app_client, monkeypatch) -> None:
    monkeypatch.setenv(ENV_GITLAB_SECRET, "gl-secret")
    client, store_db = app_client
    payload = {
        "object_kind": "pipeline",
        "object_attributes": {
            "id": 1234, "sha": "deadbeef",
            "status": "success", "ref": "main",
            "created_at": "2026-05-01T10:00:00Z",
            "finished_at": "2026-05-01T10:05:00Z",
        },
        "project": {"path_with_namespace": "group/project"},
    }
    r = client.post(
        "/api/webhooks/gitlab",
        json=payload,
        headers={
            "X-Gitlab-Token": "gl-secret",
            "X-Gitlab-Event": "Pipeline Hook",
        },
    )
    assert r.status_code == 200
    out = r.json()
    assert out["kind"] == "ci"
    conn = db.connect(store_db)
    try:
        row = conn.execute(
            "SELECT provider, status, commit_sha FROM ci_runs WHERE run_id = '1234'"
        ).fetchone()
    finally:
        conn.close()
    assert row["provider"] == "gitlab-ci"
    assert row["status"] == "success"
    assert row["commit_sha"] == "deadbeef"


def test_ci_webhook_upserts_generic_payload(app_client, monkeypatch) -> None:
    monkeypatch.setenv(ENV_CI_SECRET, "ci-secret")
    client, store_db = app_client
    payload = {
        "id": 5005,
        "head_sha": "feedface",
        "conclusion": "failure",
        "name": "circleci-tests",
        "provider": "circleci",
        "repository": "team/proj",
        "run_started_at": "2026-05-01T10:00:00Z",
        "updated_at": "2026-05-01T10:05:00Z",
    }
    body = json.dumps(payload).encode()
    r = client.post(
        "/api/webhooks/ci",
        content=body,
        headers={
            "X-Webhook-Signature-256": _sign("ci-secret", body),
            "Content-Type": "application/json",
        },
    )
    assert r.status_code == 200
    out = r.json()
    assert out["status"] == "ok"
    assert out["run_id"] == "5005"
    conn = db.connect(store_db)
    try:
        row = conn.execute(
            "SELECT provider, status, commit_sha FROM ci_runs WHERE run_id = '5005'"
        ).fetchone()
    finally:
        conn.close()
    assert row["provider"] == "circleci"
    assert row["status"] == "failure"
    assert row["commit_sha"] == "feedface"


# ── token storage invariant ─────────────────────────────────────────────────


def test_secrets_are_read_from_env_each_request(app_client, monkeypatch) -> None:
    """Rotating the env between requests is honored — proves the receiver
    reads from the environment, never from a database column.
    """
    monkeypatch.setenv(ENV_GITHUB_SECRET, "first-secret")
    client, _ = app_client
    body = json.dumps({"zen": "Hi"}).encode()
    r1 = client.post(
        "/api/webhooks/github",
        content=body,
        headers={
            "X-Hub-Signature-256": _sign("first-secret", body),
            "X-GitHub-Event": "ping",
            "Content-Type": "application/json",
        },
    )
    assert r1.status_code == 200

    monkeypatch.setenv(ENV_GITHUB_SECRET, "second-secret")
    r2 = client.post(
        "/api/webhooks/github",
        content=body,
        headers={
            # Sign with the OLD secret — must now be rejected.
            "X-Hub-Signature-256": _sign("first-secret", body),
            "X-GitHub-Event": "ping",
            "Content-Type": "application/json",
        },
    )
    assert r2.status_code == 403
    r3 = client.post(
        "/api/webhooks/github",
        content=body,
        headers={
            "X-Hub-Signature-256": _sign("second-secret", body),
            "X-GitHub-Event": "ping",
            "Content-Type": "application/json",
        },
    )
    assert r3.status_code == 200


def test_no_secret_columns_in_pr_outcomes() -> None:
    """The schema must not have a ``token`` or ``secret`` column anywhere
    on the new tables — tokens come from env, NOT the database."""
    import tempfile

    with tempfile.TemporaryDirectory() as td:
        c = db.connect(Path(td) / "store.db")
        try:
            schema.apply(c)
            for table in ("pr_outcomes", "ci_runs"):
                cols = [
                    r["name"].lower()
                    for r in c.execute(f"PRAGMA table_info({table})").fetchall()
                ]
                for forbidden in ("token", "secret", "password", "api_key", "auth"):
                    assert forbidden not in cols, (
                        f"{table} must not carry a '{forbidden}' column"
                    )
        finally:
            c.close()
