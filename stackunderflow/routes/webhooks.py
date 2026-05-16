"""Webhook receiver — opt-in PR / CI ingest from GitHub + GitLab.

Three endpoints land here:

* ``POST /api/webhooks/github`` — PR + workflow-run events from
  ``github.com``.
* ``POST /api/webhooks/gitlab`` — Merge Request events from
  ``gitlab.com`` / self-hosted GitLab.
* ``POST /api/webhooks/ci``     — generic CI status events that don't
  fit the two provider shapes (CircleCI, Buildkite, etc.) — see
  :mod:`stackunderflow.services.github_ingest` for the row shape.

Hard rules — non-negotiable
---------------------------

Every endpoint validates a signature header **before** parsing the body:

* GitHub uses HMAC-SHA256: ``X-Hub-Signature-256: sha256=<hex>``.
* GitLab uses a static-token compare: ``X-Gitlab-Token: <secret>``.

Both comparisons go through :func:`hmac.compare_digest` so a timing
side-channel can't reveal the secret. Missing or mismatched signatures
return ``403`` with no body. The expected secret is read from the
environment, never the database (Spec 28 will move this into
encrypted-at-rest settings):

* GitHub: ``$STACKUNDERFLOW_GITHUB_WEBHOOK_SECRET``
* GitLab: ``$STACKUNDERFLOW_GITLAB_WEBHOOK_SECRET``
* Generic CI: ``$STACKUNDERFLOW_CI_WEBHOOK_SECRET`` (HMAC-SHA256 in the
  ``X-Webhook-Signature-256`` header).

If the env var is unset on a fresh install, the receiver returns 503 —
opt-in by design, never accepts anonymous payloads.

Token storage
-------------

GitHub PATs (used by the REST backfill) are NOT stored here either.
They're read from ``$STACKUNDERFLOW_GITHUB_TOKEN`` /
``$GITHUB_TOKEN`` by the CLI's ``ingest github`` command. This route
only knows about webhook signing secrets, and only at request time.
"""

from __future__ import annotations

import hashlib
import hmac
import json
import logging
import os
from typing import Any

from fastapi import APIRouter, HTTPException, Request, Response

import stackunderflow.deps as deps
from stackunderflow.services import github_ingest
from stackunderflow.store import db, schema

router = APIRouter()
_log = logging.getLogger(__name__)

# Env var names — exposed as constants so tests don't drift from prod.
ENV_GITHUB_SECRET = "STACKUNDERFLOW_GITHUB_WEBHOOK_SECRET"
ENV_GITLAB_SECRET = "STACKUNDERFLOW_GITLAB_WEBHOOK_SECRET"
ENV_CI_SECRET = "STACKUNDERFLOW_CI_WEBHOOK_SECRET"


# ── signature validation ──────────────────────────────────────────────────


def _verify_hmac_sha256(
    body: bytes, signature_header: str | None, secret: str
) -> bool:
    """Return True iff ``body`` HMAC-SHA256 with ``secret`` matches the header.

    The header is the GitHub format: ``"sha256=<hex>"``. We accept both
    the prefixed and bare hex forms so the same helper covers the
    generic-CI endpoint (some providers emit the bare hex).
    Comparison goes through :func:`hmac.compare_digest`.
    """
    if not signature_header or not secret:
        return False
    expected = hmac.new(secret.encode("utf-8"), body, hashlib.sha256).hexdigest()
    received = signature_header.strip()
    if received.startswith("sha256="):
        received = received[len("sha256="):]
    # Both sides must be the same length for compare_digest to be useful.
    if len(received) != len(expected):
        return False
    return hmac.compare_digest(received, expected)


def _require_secret(env_var: str) -> str:
    """Return the env-var-stored secret or raise 503 if unset.

    503 (Service Unavailable) is the right code: the server is alive
    but the receiver is opt-in and the operator hasn't configured it.
    """
    secret = os.environ.get(env_var, "").strip()
    if not secret:
        raise HTTPException(
            status_code=503,
            detail=(
                f"webhook receiver not configured (set ${env_var} "
                "and restart the server)"
            ),
        )
    return secret


def _reject_signature() -> None:
    """Raise the canonical 403 for a missing / mismatched signature."""
    raise HTTPException(status_code=403, detail="invalid or missing signature")


# ── handlers ──────────────────────────────────────────────────────────────


def _open_store():
    """Open the configured store and ensure the schema is current."""
    conn = db.connect(deps.store_path)
    schema.apply(conn)
    return conn


def _ingest_github_event(
    event: str, payload: dict[str, Any]
) -> dict[str, Any]:
    """Route a GitHub webhook event to the right upsert.

    Returns a small status dict the receiver echoes back. Unknown event
    types are accepted with ``status="ignored"`` — GitHub sends a ping
    on hook installation we don't want to fail.
    """
    if event == "ping":
        return {"status": "pong"}

    if event == "pull_request":
        pr = payload.get("pull_request") or {}
        repo = (payload.get("repository") or {}).get("full_name")
        if not pr or not repo:
            raise HTTPException(
                status_code=400,
                detail="missing pull_request / repository fields",
            )
        row = github_ingest.normalise_pr_payload(
            pr, provider="github", repo_slug=str(repo)
        )
        conn = _open_store()
        try:
            verb = github_ingest.upsert_pr_outcome(conn, row)
        finally:
            conn.close()
        return {
            "status": "ok",
            "kind": "pr",
            "verb": verb,
            "pr_number": int(row["pr_number"]),
        }

    if event == "workflow_run":
        run = payload.get("workflow_run") or {}
        repo = (payload.get("repository") or {}).get("full_name")
        if not run:
            raise HTTPException(
                status_code=400,
                detail="missing workflow_run field",
            )
        row = github_ingest.normalise_ci_run_payload(
            run,
            provider="github-actions",
            repo_slug=str(repo) if repo else None,
        )
        conn = _open_store()
        try:
            verb = github_ingest.upsert_ci_run(conn, row)
        finally:
            conn.close()
        return {
            "status": "ok",
            "kind": "ci",
            "verb": verb,
            "run_id": row["run_id"],
        }

    return {"status": "ignored", "event": event}


def _ingest_gitlab_event(
    event: str, payload: dict[str, Any]
) -> dict[str, Any]:
    """Route a GitLab webhook event to the right upsert.

    GitLab's vocabulary differs: PRs are "Merge Requests" and the
    object kind sits in ``object_kind``. We handle ``merge_request``
    and ``pipeline`` (their CI run) and ignore everything else.
    """
    object_kind = payload.get("object_kind") or event

    if object_kind == "merge_request":
        attrs = payload.get("object_attributes") or {}
        project = payload.get("project") or {}
        repo_slug = (
            project.get("path_with_namespace")
            or project.get("name_with_namespace")
            or project.get("name")
            or ""
        )
        # GitLab uses iid for the per-project MR number.
        pr_number = int(attrs.get("iid") or attrs.get("id") or 0)
        state = (attrs.get("state") or "open").lower()
        # GitLab states: opened / closed / merged / locked. Map to our enum.
        if state == "opened":
            state = "open"
        elif state == "locked":
            state = "open"
        merged_at = attrs.get("merged_at")
        if merged_at is not None:
            merged_at = str(merged_at)
        title = attrs.get("title")
        if title is not None:
            title = str(title)
        author = (payload.get("user") or {}).get("username")
        if author is not None:
            author = str(author)
        row = {
            "provider": "gitlab",
            "repo_slug": str(repo_slug),
            "pr_number": pr_number,
            "title": title,
            "state": state,
            "merged_at": merged_at,
            "reverted_at": None,
            "author": author,
            "raw_json": json.dumps(payload, default=str),
        }
        conn = _open_store()
        try:
            verb = github_ingest.upsert_pr_outcome(conn, row)
        finally:
            conn.close()
        return {
            "status": "ok",
            "kind": "pr",
            "verb": verb,
            "pr_number": pr_number,
        }

    if object_kind == "pipeline":
        attrs = payload.get("object_attributes") or {}
        project = payload.get("project") or {}
        repo_slug = (
            project.get("path_with_namespace")
            or project.get("name")
            or ""
        )
        run_id = str(attrs.get("id") or 0)
        commit_sha = (
            attrs.get("sha")
            or (payload.get("commit") or {}).get("id")
            or ""
        )
        status_raw = attrs.get("status")
        # GitLab status enum: created / waiting_for_resource / preparing /
        # pending / running / success / failed / canceled / skipped /
        # manual / scheduled. Map onto ours.
        gitlab_map = {
            "success": "success",
            "failed": "failure",
            "canceled": "cancelled",
            "cancelled": "cancelled",
            "skipped": "skipped",
            "running": "in_progress",
            "manual": "pending",
            "pending": "pending",
            "preparing": "pending",
            "scheduled": "pending",
            "created": "pending",
            "waiting_for_resource": "pending",
        }
        status = gitlab_map.get(str(status_raw or "").lower(), "in_progress")
        row = {
            "provider": "gitlab-ci",
            "repo_slug": str(repo_slug),
            "run_id": run_id,
            "commit_sha": str(commit_sha),
            "status": status,
            "workflow_name": (
                str(attrs.get("ref")) if attrs.get("ref") else None
            ),
            "started_ts": (
                str(attrs.get("created_at"))
                if attrs.get("created_at") else None
            ),
            "completed_ts": (
                str(attrs.get("finished_at"))
                if attrs.get("finished_at") else None
            ),
            "raw_json": json.dumps(payload, default=str),
        }
        conn = _open_store()
        try:
            verb = github_ingest.upsert_ci_run(conn, row)
        finally:
            conn.close()
        return {
            "status": "ok",
            "kind": "ci",
            "verb": verb,
            "run_id": run_id,
        }

    return {"status": "ignored", "object_kind": object_kind}


# ── routes ────────────────────────────────────────────────────────────────


@router.post("/api/webhooks/github")
async def github_webhook(request: Request) -> Response:
    """Receive a GitHub PR / workflow-run webhook event.

    Validates the ``X-Hub-Signature-256`` HMAC header against
    ``$STACKUNDERFLOW_GITHUB_WEBHOOK_SECRET`` before parsing the body.
    """
    secret = _require_secret(ENV_GITHUB_SECRET)
    body = await request.body()
    sig = request.headers.get("X-Hub-Signature-256")
    if not _verify_hmac_sha256(body, sig, secret):
        _reject_signature()
    try:
        payload = json.loads(body or b"{}")
    except json.JSONDecodeError as exc:
        raise HTTPException(status_code=400, detail=f"invalid JSON: {exc}") from exc
    if not isinstance(payload, dict):
        raise HTTPException(status_code=400, detail="payload must be an object")
    event = request.headers.get("X-GitHub-Event") or "unknown"
    result = _ingest_github_event(event, payload)
    return Response(content=json.dumps(result), media_type="application/json")


@router.post("/api/webhooks/gitlab")
async def gitlab_webhook(request: Request) -> Response:
    """Receive a GitLab merge-request / pipeline webhook event.

    Validates the ``X-Gitlab-Token`` static-token header against
    ``$STACKUNDERFLOW_GITLAB_WEBHOOK_SECRET`` via ``hmac.compare_digest``.
    """
    secret = _require_secret(ENV_GITLAB_SECRET)
    received = (request.headers.get("X-Gitlab-Token") or "").strip()
    # compare_digest is the documented mitigation against timing
    # side-channels even for non-HMAC equality checks; both sides are
    # short ASCII strings so the constant-time path applies.
    if not received or not hmac.compare_digest(received, secret):
        _reject_signature()
    body = await request.body()
    try:
        payload = json.loads(body or b"{}")
    except json.JSONDecodeError as exc:
        raise HTTPException(status_code=400, detail=f"invalid JSON: {exc}") from exc
    if not isinstance(payload, dict):
        raise HTTPException(status_code=400, detail="payload must be an object")
    event = request.headers.get("X-Gitlab-Event") or payload.get("object_kind") or "unknown"
    result = _ingest_gitlab_event(str(event), payload)
    return Response(content=json.dumps(result), media_type="application/json")


@router.post("/api/webhooks/ci")
async def ci_webhook(request: Request) -> Response:
    """Receive a generic CI status webhook.

    Expects an HMAC-SHA256 signature in ``X-Webhook-Signature-256``,
    verified against ``$STACKUNDERFLOW_CI_WEBHOOK_SECRET``. The body is
    treated as a workflow-run-shaped object — same fields as GitHub
    Actions but with a flexible ``provider`` field (defaults to
    ``"generic-ci"``).
    """
    secret = _require_secret(ENV_CI_SECRET)
    body = await request.body()
    sig = request.headers.get("X-Webhook-Signature-256")
    if not _verify_hmac_sha256(body, sig, secret):
        _reject_signature()
    try:
        payload = json.loads(body or b"{}")
    except json.JSONDecodeError as exc:
        raise HTTPException(status_code=400, detail=f"invalid JSON: {exc}") from exc
    if not isinstance(payload, dict):
        raise HTTPException(status_code=400, detail="payload must be an object")
    provider = str(payload.get("provider") or "generic-ci")
    repo_slug = str(payload.get("repository") or payload.get("repo_slug") or "")
    row = github_ingest.normalise_ci_run_payload(
        payload, provider=provider, repo_slug=repo_slug or None
    )
    conn = _open_store()
    try:
        verb = github_ingest.upsert_ci_run(conn, row)
    finally:
        conn.close()
    return Response(
        content=json.dumps({"status": "ok", "verb": verb, "run_id": row["run_id"]}),
        media_type="application/json",
    )
