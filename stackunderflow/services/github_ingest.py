"""GitHub PR + CI backfill — opt-in REST pull into the local store.

This is the "fetch what you missed before you turned the webhook on"
side of Spec 20. The webhook receiver in
:mod:`stackunderflow.routes.webhooks` handles the live stream; this
service walks the GitHub REST API to fill in history.

Public surface
--------------

* :func:`backfill_repo` — fetch the recent PRs + CI runs for one repo
  and upsert into ``pr_outcomes`` + ``ci_runs``. Returns a small
  ``BackfillReport`` so the CLI can print progress.
* :func:`upsert_pr_outcome` / :func:`upsert_ci_run` — single-row
  helpers. Webhook handlers in :mod:`stackunderflow.routes.webhooks`
  share these so the on-conflict shape stays in lockstep across
  surfaces.
* :func:`normalise_pr_payload` / :func:`normalise_ci_run_payload` —
  pure functions: take a raw GitHub payload (PR object or workflow-run
  object) and return the column tuple ready for upsert. Pure so the
  webhook receiver and the backfill share one parser; tests exercise
  them without spinning up an HTTP client.

Token handling
--------------

The GitHub token is **never** persisted. The CLI passes
``token=os.environ.get("STACKUNDERFLOW_GITHUB_TOKEN")`` (or the
``--token`` flag's value) directly to :func:`backfill_repo`; it's used
only as the ``Authorization: token <pat>`` header for the duration of
the call. Encrypted-at-rest token storage is deferred to Spec 28.

Privacy
-------

This module makes outbound HTTPS calls to ``api.github.com`` — the
**only** non-local hop in the codebase outside the optional
:mod:`stackunderflow.services.pricing_service` and the currency
helper. Default timeouts are conservative; rate-limit retries back off
once and stop (no infinite loops).
"""

from __future__ import annotations

import json
import sqlite3
import time
from collections.abc import Callable
from dataclasses import dataclass, field
from typing import Any

import httpx

__all__ = [
    "BackfillReport",
    "GITHUB_API_BASE",
    "RateLimitedError",
    "backfill_repo",
    "normalise_ci_run_payload",
    "normalise_pr_payload",
    "upsert_ci_run",
    "upsert_pr_outcome",
]

GITHUB_API_BASE = "https://api.github.com"

# Per-page cap GitHub allows on the PR + workflow-runs endpoints.
# 100 is the documented maximum; using the full page reduces the number
# of HTTP hops the backfill needs.
_MAX_PER_PAGE = 100

# Default cap on pages fetched per backfill call. 10 pages * 100 PRs/page
# = 1000 PRs is enough for a typical mid-sized repo's recent history;
# callers can override via ``backfill_repo(..., max_pages=...)`` for
# long-tail repos (the meta-agent's read tool will never need more).
_DEFAULT_MAX_PAGES = 10


# ── data classes ───────────────────────────────────────────────────────────


@dataclass(frozen=True)
class BackfillReport:
    """Summary of what one ``backfill_repo`` call did.

    All fields are JSON-safe so the CLI can dump the report verbatim
    when ``--format json`` is set.
    """

    repo_slug: str
    pr_inserted: int = 0
    pr_updated: int = 0
    pr_pages_fetched: int = 0
    ci_inserted: int = 0
    ci_updated: int = 0
    ci_pages_fetched: int = 0
    duration_seconds: float = 0.0
    warnings: list[str] = field(default_factory=list)

    def to_dict(self) -> dict[str, Any]:
        return {
            "repo_slug": self.repo_slug,
            "pr_inserted": self.pr_inserted,
            "pr_updated": self.pr_updated,
            "pr_pages_fetched": self.pr_pages_fetched,
            "ci_inserted": self.ci_inserted,
            "ci_updated": self.ci_updated,
            "ci_pages_fetched": self.ci_pages_fetched,
            "duration_seconds": round(self.duration_seconds, 3),
            "warnings": list(self.warnings),
        }


class RateLimitedError(RuntimeError):
    """Raised when GitHub returned 403 + the rate-limit headers say zero."""


# ── normalisers (pure) ─────────────────────────────────────────────────────


def _normalise_ci_status(raw: str | None) -> str:
    """Map a GitHub workflow-run status/conclusion to our small enum.

    GitHub's workflow-run object carries both ``status`` (queued /
    in_progress / completed) and ``conclusion`` (success / failure /
    cancelled / skipped / timed_out / action_required / neutral / null).
    We collapse them into:
      success | failure | cancelled | in_progress | pending | skipped

    The mapping is conservative — anything we don't recognise becomes
    ``in_progress`` so the row is still inserted (the consumer can
    inspect ``raw_json`` for the original).
    """
    if raw is None:
        return "in_progress"
    s = str(raw).lower()
    if s in {"success", "successful"}:
        return "success"
    if s in {"failure", "failed", "timed_out"}:
        return "failure"
    if s in {"cancelled", "canceled"}:
        return "cancelled"
    if s in {"skipped", "neutral"}:
        return "skipped"
    if s in {"queued", "waiting", "pending", "requested", "action_required"}:
        return "pending"
    return "in_progress"


def normalise_pr_payload(
    payload: dict[str, Any], *, provider: str = "github", repo_slug: str | None = None
) -> dict[str, Any]:
    """Extract the indexed columns from a raw PR payload.

    Accepts the GitHub REST API PR object shape (also the ``pull_request``
    sub-object inside a webhook event). Returns a dict with the keys
    ``upsert_pr_outcome`` consumes; everything else lives in ``raw_json``.

    ``repo_slug`` falls back to the PR object's ``base.repo.full_name``
    when the caller doesn't pass it explicitly (webhook events carry the
    repo separately from the PR object).
    """
    pr_number = int(payload.get("number") or payload.get("id") or 0)
    title = payload.get("title")
    if title is not None:
        title = str(title)
    user = payload.get("user") or {}
    author = user.get("login") if isinstance(user, dict) else None
    if author is not None:
        author = str(author)

    state = (payload.get("state") or "open").lower()
    merged = bool(payload.get("merged"))
    merged_at = payload.get("merged_at")
    if merged_at is not None:
        merged_at = str(merged_at)
    if state == "closed" and (merged or merged_at):
        state = "merged"

    if repo_slug is None:
        base = payload.get("base") or {}
        repo = base.get("repo") if isinstance(base, dict) else None
        if isinstance(repo, dict):
            repo_slug = repo.get("full_name") or repo.get("name")
    if repo_slug is None:
        repo_slug = ""
    repo_slug = str(repo_slug)

    return {
        "provider": str(provider),
        "repo_slug": repo_slug,
        "pr_number": pr_number,
        "title": title,
        "state": state,
        "merged_at": merged_at,
        "reverted_at": None,  # downstream — Spec 22 fills this in
        "author": author,
        "raw_json": json.dumps(payload, default=str),
    }


def normalise_ci_run_payload(
    payload: dict[str, Any], *, provider: str = "github-actions", repo_slug: str | None = None
) -> dict[str, Any]:
    """Extract the indexed columns from a raw workflow-run payload.

    Accepts the GitHub Actions workflow-run object. ``status`` is
    normalised through :func:`_normalise_ci_status` against the
    ``conclusion`` field first (the terminal verdict), falling back to
    the ``status`` field for in-flight runs.
    """
    run_id = payload.get("id")
    if run_id is None:
        run_id = payload.get("run_id") or 0
    run_id = str(run_id)

    commit_sha = (
        payload.get("head_sha") or payload.get("sha") or payload.get("head_commit", {}).get("id")
    )
    if commit_sha is None:
        commit_sha = ""
    commit_sha = str(commit_sha)

    workflow_name = payload.get("name") or payload.get("workflow_name")
    if workflow_name is not None:
        workflow_name = str(workflow_name)

    started_ts = payload.get("run_started_at") or payload.get("created_at")
    completed_ts = payload.get("updated_at") if payload.get("conclusion") else None
    if started_ts is not None:
        started_ts = str(started_ts)
    if completed_ts is not None:
        completed_ts = str(completed_ts)

    conclusion = payload.get("conclusion")
    status_raw = conclusion if conclusion else payload.get("status")
    status = _normalise_ci_status(status_raw)

    if repo_slug is None:
        repo = payload.get("repository") or {}
        if isinstance(repo, dict):
            repo_slug = repo.get("full_name") or repo.get("name")
    if repo_slug is None:
        repo_slug = ""
    repo_slug = str(repo_slug)

    return {
        "provider": str(provider),
        "repo_slug": repo_slug,
        "run_id": run_id,
        "commit_sha": commit_sha,
        "status": status,
        "workflow_name": workflow_name,
        "started_ts": started_ts,
        "completed_ts": completed_ts,
        "raw_json": json.dumps(payload, default=str),
    }


# ── upsert helpers ─────────────────────────────────────────────────────────


def upsert_pr_outcome(conn: sqlite3.Connection, row: dict[str, Any]) -> str:
    """Insert or update one ``pr_outcomes`` row.

    Returns ``"inserted"`` or ``"updated"`` so the caller can keep
    counts. Conflict key is ``UNIQUE (provider, repo_slug, pr_number)``.
    """
    existing = conn.execute(
        "SELECT id FROM pr_outcomes WHERE provider=? AND repo_slug=? AND pr_number=?",
        (row["provider"], row["repo_slug"], int(row["pr_number"])),
    ).fetchone()
    if existing is None:
        conn.execute(
            "INSERT INTO pr_outcomes "
            "(provider, repo_slug, pr_number, title, state, merged_at, "
            " reverted_at, author, raw_json) "
            "VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
            (
                row["provider"],
                row["repo_slug"],
                int(row["pr_number"]),
                row.get("title"),
                row["state"],
                row.get("merged_at"),
                row.get("reverted_at"),
                row.get("author"),
                row["raw_json"],
            ),
        )
        return "inserted"
    conn.execute(
        "UPDATE pr_outcomes SET title=?, state=?, merged_at=?, "
        " reverted_at=COALESCE(?, reverted_at), author=?, raw_json=? "
        "WHERE provider=? AND repo_slug=? AND pr_number=?",
        (
            row.get("title"),
            row["state"],
            row.get("merged_at"),
            row.get("reverted_at"),
            row.get("author"),
            row["raw_json"],
            row["provider"],
            row["repo_slug"],
            int(row["pr_number"]),
        ),
    )
    return "updated"


def upsert_ci_run(conn: sqlite3.Connection, row: dict[str, Any]) -> str:
    """Insert or update one ``ci_runs`` row.

    Conflict key is ``UNIQUE (provider, run_id)``. ``commit_sha`` /
    ``status`` always come from the most recent payload.
    """
    existing = conn.execute(
        "SELECT id FROM ci_runs WHERE provider=? AND run_id=?",
        (row["provider"], row["run_id"]),
    ).fetchone()
    if existing is None:
        conn.execute(
            "INSERT INTO ci_runs "
            "(provider, repo_slug, run_id, commit_sha, status, "
            " workflow_name, started_ts, completed_ts, raw_json) "
            "VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
            (
                row["provider"],
                row["repo_slug"],
                row["run_id"],
                row["commit_sha"],
                row["status"],
                row.get("workflow_name"),
                row.get("started_ts"),
                row.get("completed_ts"),
                row["raw_json"],
            ),
        )
        return "inserted"
    conn.execute(
        "UPDATE ci_runs SET repo_slug=?, commit_sha=?, status=?, "
        " workflow_name=?, started_ts=?, completed_ts=?, raw_json=? "
        "WHERE provider=? AND run_id=?",
        (
            row["repo_slug"],
            row["commit_sha"],
            row["status"],
            row.get("workflow_name"),
            row.get("started_ts"),
            row.get("completed_ts"),
            row["raw_json"],
            row["provider"],
            row["run_id"],
        ),
    )
    return "updated"


# ── REST backfill ──────────────────────────────────────────────────────────


def _auth_headers(token: str | None) -> dict[str, str]:
    headers = {
        "Accept": "application/vnd.github+json",
        "X-GitHub-Api-Version": "2022-11-28",
        "User-Agent": "stackunderflow-ingest",
    }
    if token:
        headers["Authorization"] = f"token {token}"
    return headers


def _check_rate_limit(response: httpx.Response) -> None:
    """Raise ``RateLimitedError`` when GitHub says we're tapped out.

    Conservative: only raises when the documented headers actively say
    zero remaining. Other 403s bubble up as the underlying HTTPStatusError.
    """
    if response.status_code != 403:
        return
    remaining = response.headers.get("x-ratelimit-remaining")
    if remaining is not None and remaining.strip() == "0":
        reset = response.headers.get("x-ratelimit-reset", "<unknown>")
        raise RateLimitedError(
            f"GitHub rate-limit exhausted; resets at unix ts {reset}"
        )


def _paged_fetch(
    client: httpx.Client,
    url: str,
    *,
    headers: dict[str, str],
    max_pages: int,
    extra_params: dict[str, str] | None = None,
) -> tuple[list[dict[str, Any]], int]:
    """Walk a GitHub paginated endpoint, returning the rows + page count.

    Stops when the response carries fewer than ``per_page`` rows (last
    page) or when ``max_pages`` is reached. Honours the ``X-RateLimit-Remaining``
    header by raising ``RateLimitedError``; one retry on a transient
    network error before giving up.
    """
    rows: list[dict[str, Any]] = []
    pages_fetched = 0
    for page in range(1, max_pages + 1):
        params = {"per_page": str(_MAX_PER_PAGE), "page": str(page)}
        if extra_params:
            params.update(extra_params)
        attempt = 0
        while True:
            try:
                response = client.get(url, headers=headers, params=params)
                break
            except httpx.HTTPError:
                if attempt >= 1:
                    raise
                attempt += 1
                time.sleep(0.5)
        _check_rate_limit(response)
        response.raise_for_status()
        body = response.json()
        if not isinstance(body, list):
            break
        pages_fetched += 1
        rows.extend(body)
        if len(body) < _MAX_PER_PAGE:
            break
    return rows, pages_fetched


def backfill_repo(
    conn: sqlite3.Connection,
    repo_slug: str,
    *,
    token: str | None = None,
    state: str = "all",
    max_pages: int = _DEFAULT_MAX_PAGES,
    include_ci: bool = True,
    client_factory: Callable[[], httpx.Client] | None = None,
) -> BackfillReport:
    """Fetch recent PRs + workflow runs for ``repo_slug`` and upsert.

    Parameters
    ----------
    conn:
        Main store connection (``~/.stackunderflow/store.db``). Schema
        must be at v17+ (call ``schema.apply(conn)`` first).
    repo_slug:
        ``owner/repo`` identifier.
    token:
        GitHub PAT. Read from ``$STACKUNDERFLOW_GITHUB_TOKEN`` /
        ``$GITHUB_TOKEN`` by the CLI; this function takes it directly so
        the token never lives in the database.
    state:
        PR state filter passed through to the API. ``"all"`` (default),
        ``"open"``, or ``"closed"``.
    max_pages:
        Cap on per-endpoint pagination. Two endpoints (PRs + CI runs)
        each get up to ``max_pages`` pages.
    include_ci:
        When ``False``, skip the workflow-runs fetch — useful for quick
        PR-only refreshes.
    client_factory:
        Override the ``httpx.Client`` constructor (tests pass a transport-
        mounted client). When ``None``, we open a real client with a 30s
        timeout.
    """
    started = time.monotonic()
    warnings: list[str] = []
    headers = _auth_headers(token)
    factory = client_factory or (lambda: httpx.Client(timeout=30.0))

    pr_inserted = pr_updated = pr_pages = 0
    ci_inserted = ci_updated = ci_pages = 0

    with factory() as client:
        # PRs
        pr_url = f"{GITHUB_API_BASE}/repos/{repo_slug}/pulls"
        pr_rows, pr_pages = _paged_fetch(
            client,
            pr_url,
            headers=headers,
            max_pages=max_pages,
            extra_params={"state": state, "sort": "updated", "direction": "desc"},
        )
        for raw in pr_rows:
            row = normalise_pr_payload(raw, provider="github", repo_slug=repo_slug)
            verb = upsert_pr_outcome(conn, row)
            if verb == "inserted":
                pr_inserted += 1
            else:
                pr_updated += 1

        # CI runs
        if include_ci:
            try:
                ci_url = f"{GITHUB_API_BASE}/repos/{repo_slug}/actions/runs"
                ci_rows, ci_pages = _paged_fetch(
                    client,
                    ci_url,
                    headers=headers,
                    max_pages=max_pages,
                )
                for raw in ci_rows:
                    if not isinstance(raw, dict):
                        continue
                    row = normalise_ci_run_payload(
                        raw, provider="github-actions", repo_slug=repo_slug
                    )
                    verb = upsert_ci_run(conn, row)
                    if verb == "inserted":
                        ci_inserted += 1
                    else:
                        ci_updated += 1
            except httpx.HTTPStatusError as exc:
                # The actions endpoint returns 404 on repos without any
                # workflow runs. Don't fail the whole backfill.
                if exc.response.status_code == 404:
                    warnings.append("no GitHub Actions workflow runs found")
                else:
                    raise

    # workflow-runs returns ``{"workflow_runs": [...]}`` not a bare list,
    # so _paged_fetch's ``isinstance(body, list)`` check skipped them.
    # Re-do the CI pass with a small wrapper if we got zero rows on a
    # repo that does have workflow runs.
    if include_ci and ci_pages == 0 and ci_inserted == 0 and ci_updated == 0:
        with factory() as client:
            try:
                ci_url = f"{GITHUB_API_BASE}/repos/{repo_slug}/actions/runs"
                runs, ci_pages = _paged_fetch_workflow_runs(
                    client,
                    ci_url,
                    headers=headers,
                    max_pages=max_pages,
                )
                for raw in runs:
                    if not isinstance(raw, dict):
                        continue
                    row = normalise_ci_run_payload(
                        raw, provider="github-actions", repo_slug=repo_slug
                    )
                    verb = upsert_ci_run(conn, row)
                    if verb == "inserted":
                        ci_inserted += 1
                    else:
                        ci_updated += 1
            except httpx.HTTPStatusError as exc:
                if exc.response.status_code == 404:
                    if "no GitHub Actions workflow runs found" not in warnings:
                        warnings.append("no GitHub Actions workflow runs found")
                else:
                    raise

    return BackfillReport(
        repo_slug=repo_slug,
        pr_inserted=pr_inserted,
        pr_updated=pr_updated,
        pr_pages_fetched=pr_pages,
        ci_inserted=ci_inserted,
        ci_updated=ci_updated,
        ci_pages_fetched=ci_pages,
        duration_seconds=time.monotonic() - started,
        warnings=warnings,
    )


def _paged_fetch_workflow_runs(
    client: httpx.Client,
    url: str,
    *,
    headers: dict[str, str],
    max_pages: int,
) -> tuple[list[dict[str, Any]], int]:
    """Walk ``/actions/runs`` — its envelope is ``{"workflow_runs": [...]}``.

    Mirrors :func:`_paged_fetch` but unwraps the ``workflow_runs`` key
    before checking if the page was full. Stops when fewer than
    ``per_page`` runs come back or the cap is hit.
    """
    rows: list[dict[str, Any]] = []
    pages_fetched = 0
    for page in range(1, max_pages + 1):
        params = {"per_page": str(_MAX_PER_PAGE), "page": str(page)}
        attempt = 0
        while True:
            try:
                response = client.get(url, headers=headers, params=params)
                break
            except httpx.HTTPError:
                if attempt >= 1:
                    raise
                attempt += 1
                time.sleep(0.5)
        _check_rate_limit(response)
        response.raise_for_status()
        body = response.json()
        if not isinstance(body, dict):
            break
        page_rows = body.get("workflow_runs") or []
        if not isinstance(page_rows, list):
            break
        pages_fetched += 1
        rows.extend(page_rows)
        if len(page_rows) < _MAX_PER_PAGE:
            break
    return rows, pages_fetched
