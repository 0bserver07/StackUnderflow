"""Yield analysis — correlate AI sessions with the git commit history of their cwd.

The basic question this service answers: of the money spent on AI coding
sessions in a window, how much produced *kept* commits versus reverted or
abandoned work?

Each session has a recorded ``cwd`` (working directory of the editor when the
session ran). For every session we:

1. Pull session start time, ``cwd``, and an estimated cost from the store.
2. Verify ``cwd`` resolves to a git repository. If not → ``no_repo``.
3. Run ``git log`` in that repo over ``[started_at, started_at + 24h]``. If no
   commits land in that window → ``abandoned``.
4. For the first commit in the window:
     * If a later commit's subject contains ``revert`` plus the short SHA, OR
       the commit is no longer reachable from ``HEAD`` (hard-reset / rebase
       wiped it) → ``reverted``.
     * Otherwise → ``productive``, attaching that commit as the credited
       follow-up.

Heuristic warning: this correlates by **time**, not by content. A commit
that lands in the 24h window after a session is *credited* to that session
even if it's about something else entirely. Multiple sessions in one day
in the same repo will share the same follow-up commit attribution. Treat
this as a smoke signal, not a verdict.

All git invocations have a 5s timeout and are wrapped defensively — any
git error (not a repo, command not found, timeout, malformed output) is
treated as ``no_repo`` so a single bad path can't break a whole report.

Public API:

    compute_yield(conn, period="month", project_filter=None) -> list[YieldEntry]
    yield_summary(entries) -> dict
"""

from __future__ import annotations

import json
import logging
import re
import shutil
import sqlite3
import subprocess
from dataclasses import asdict, dataclass
from pathlib import Path
from typing import Literal

from stackunderflow.infra.costs import compute_cost
from stackunderflow.reports.scope import Scope, parse_period

logger = logging.getLogger(__name__)

# 5s is enough for any local git query against a healthy repo and short
# enough that a hung repo (e.g. NFS, lock contention) can't stall a report.
_GIT_TIMEOUT_SECONDS = 5

# 24h credit window — see module docstring.
_FOLLOW_WINDOW_HOURS = 24

Classification = Literal["productive", "reverted", "abandoned", "no_repo"]


@dataclass
class YieldEntry:
    """One session's yield classification."""

    session_id: str
    project_slug: str
    cwd: str
    started_at: str  # ISO-8601 (UTC)
    cost_usd: float
    classification: Classification
    follow_commit_sha: str | None = None
    follow_commit_msg: str | None = None
    follow_commit_age_hours: float | None = None

    def to_dict(self) -> dict:
        return asdict(self)


# ── public API ──────────────────────────────────────────────────────────────


def compute_yield(
    conn: sqlite3.Connection,
    period: str = "month",
    project_filter: list[str] | None = None,
) -> list[YieldEntry]:
    """Return one ``YieldEntry`` per session inside ``period``.

    ``period``: any spec accepted by ``reports.scope.parse_period`` —
    ``today``, ``7days``, ``30days``, ``month``, ``all``. We also accept
    ``week`` as a friendly alias for ``7days`` so the CLI surface can
    speak human and reuse the same parser.

    ``project_filter``: optional list of project slugs; sessions whose
    project slug is not in the list are dropped before any git work runs.

    Sessions are returned in start-time order. Sessions with no recorded
    ``cwd`` fall through to ``no_repo``.
    """
    scope = parse_period(_normalize_period(period))
    rows = _query_sessions(conn, scope=scope, project_filter=project_filter)

    entries: list[YieldEntry] = []
    # Memoise the (cwd, started_at, started_at+24h) classification so repeated
    # sessions in the same repo+window don't pay the subprocess cost twice.
    git_cache: dict[tuple[str, str], _GitOutcome] = {}

    for row in rows:
        cwd = row["cwd"] or ""
        started_at = row["started_at"]
        cost_usd = float(row["cost_usd"] or 0.0)

        if not cwd or not _is_git_repo(cwd):
            entries.append(
                YieldEntry(
                    session_id=row["session_id"],
                    project_slug=row["project_slug"],
                    cwd=cwd,
                    started_at=started_at,
                    cost_usd=cost_usd,
                    classification="no_repo",
                )
            )
            continue

        cache_key = (cwd, started_at)
        outcome = git_cache.get(cache_key)
        if outcome is None:
            outcome = _classify_session(cwd, started_at)
            git_cache[cache_key] = outcome

        entries.append(
            YieldEntry(
                session_id=row["session_id"],
                project_slug=row["project_slug"],
                cwd=cwd,
                started_at=started_at,
                cost_usd=cost_usd,
                classification=outcome.classification,
                follow_commit_sha=outcome.commit_sha,
                follow_commit_msg=outcome.commit_msg,
                follow_commit_age_hours=outcome.commit_age_hours,
            )
        )
    return entries


def yield_summary(entries: list[YieldEntry]) -> dict:
    """Roll a list of ``YieldEntry`` into a count + cost summary.

    Output shape:

        {
          "productive":      <int>,
          "reverted":        <int>,
          "abandoned":       <int>,
          "no_repo":         <int>,
          "total":           <int>,
          "productive_cost": <float USD>,
          "reverted_cost":   <float USD>,
          "abandoned_cost":  <float USD>,
          "no_repo_cost":    <float USD>,
          "total_cost":      <float USD>,
        }

    Costs are USD. Currency conversion happens at the API boundary, just
    like everywhere else in the codebase.
    """
    out = {
        "productive": 0,
        "reverted": 0,
        "abandoned": 0,
        "no_repo": 0,
        "total": 0,
        "productive_cost": 0.0,
        "reverted_cost": 0.0,
        "abandoned_cost": 0.0,
        "no_repo_cost": 0.0,
        "total_cost": 0.0,
    }
    for e in entries:
        out[e.classification] += 1
        out["total"] += 1
        out[f"{e.classification}_cost"] += e.cost_usd
        out["total_cost"] += e.cost_usd
    return out


# ── internals ────────────────────────────────────────────────────────────────


@dataclass
class _GitOutcome:
    """Cache-friendly result of inspecting one repo for one session window."""

    classification: Classification
    commit_sha: str | None = None
    commit_msg: str | None = None
    commit_age_hours: float | None = None


def _normalize_period(period: str) -> str:
    """Map friendly aliases (``week``) to canonical period specs."""
    return {"week": "7days"}.get(period, period)


def _query_sessions(
    conn: sqlite3.Connection,
    *,
    scope: Scope,
    project_filter: list[str] | None,
) -> list[dict]:
    """Return one row per session inside ``scope`` with ``cwd`` and est. cost.

    ``cwd`` lives in ``messages.raw_json`` (Claude / Codex / Droid / Pi /
    OpenCode all stamp it on the first event). We pull the first non-empty
    value per session via ``json_extract`` and price the session via
    ``compute_cost`` so this service stays decoupled from the aggregator's
    full pipeline.
    """
    sql = (
        "SELECT s.session_id AS session_id, "
        "       p.slug AS project_slug, "
        "       p.provider AS provider, "
        "       s.first_ts AS started_at, "
        "       s.id AS session_fk "
        "FROM sessions s "
        "JOIN projects p ON p.id = s.project_id "
        "WHERE s.first_ts IS NOT NULL "
    )
    params: list[str] = []
    if scope.since is not None:
        sql += "AND s.first_ts >= ? "
        params.append(scope.since)
    if scope.until is not None:
        sql += "AND s.first_ts <= ? "
        params.append(scope.until)
    if project_filter:
        placeholders = ",".join("?" for _ in project_filter)
        sql += f"AND p.slug IN ({placeholders}) "
        params.extend(project_filter)
    sql += "ORDER BY s.first_ts"

    sessions = conn.execute(sql, params).fetchall()
    out: list[dict] = []
    for sess in sessions:
        cwd = _first_cwd_for_session(conn, session_fk=sess["session_fk"])
        cost_usd = _estimate_session_cost(
            conn,
            session_fk=sess["session_fk"],
            provider=sess["provider"] or "anthropic",
        )
        out.append(
            {
                "session_id": sess["session_id"],
                "project_slug": sess["project_slug"],
                "cwd": cwd,
                "started_at": sess["started_at"],
                "cost_usd": cost_usd,
            }
        )
    return out


def _first_cwd_for_session(
    conn: sqlite3.Connection,
    *,
    session_fk: int,
) -> str:
    """Return the first non-empty ``cwd`` recorded in this session's messages."""
    row = conn.execute(
        "SELECT json_extract(raw_json, '$.cwd') AS cwd "
        "FROM messages "
        "WHERE session_fk = ? "
        "  AND json_extract(raw_json, '$.cwd') IS NOT NULL "
        "  AND json_extract(raw_json, '$.cwd') != '' "
        "ORDER BY seq LIMIT 1",
        (session_fk,),
    ).fetchone()
    if row is None:
        return ""
    return str(row["cwd"] or "")


def _estimate_session_cost(
    conn: sqlite3.Connection,
    *,
    session_fk: int,
    provider: str,
) -> float:
    """Sum cost across (model, token-type) groups for one session.

    Mirrors the per-(day, model) rollup in ``queries.get_global_stats`` so a
    session's number here is consistent with the dashboard's daily-cost
    chart. Sessions that mix models still total correctly because we group
    by model before pricing.
    """
    rows = conn.execute(
        "SELECT COALESCE(model, '') AS model, "
        "       SUM(input_tokens) AS inp, "
        "       SUM(output_tokens) AS out, "
        "       SUM(cache_create_tokens) AS cc, "
        "       SUM(cache_read_tokens) AS cr "
        "FROM messages WHERE session_fk = ? "
        "GROUP BY model",
        (session_fk,),
    ).fetchall()

    total = 0.0
    for r in rows:
        model = r["model"]
        if not model:
            continue
        tokens = {
            "input": int(r["inp"] or 0),
            "output": int(r["out"] or 0),
            "cache_creation": int(r["cc"] or 0),
            "cache_read": int(r["cr"] or 0),
        }
        try:
            total += compute_cost(tokens, model, provider=provider).get("total_cost", 0.0)
        except Exception as e:  # noqa: BLE001 - cost issues should not stall yield
            logger.debug("compute_cost failed for model %s: %s", model, e)
    return total


# ── git introspection ───────────────────────────────────────────────────────


def _is_git_repo(cwd: str) -> bool:
    """Cheap pre-flight: does ``cwd`` exist and resolve to a git repo?"""
    p = Path(cwd)
    if not p.exists() or not p.is_dir():
        return False
    if shutil.which("git") is None:
        return False
    try:
        result = subprocess.run(  # noqa: S603 — git args are not user-controllable
            ["git", "-C", str(p), "rev-parse", "--git-dir"],  # noqa: S607
            capture_output=True,
            text=True,
            timeout=_GIT_TIMEOUT_SECONDS,
        )
    except (subprocess.TimeoutExpired, OSError) as e:
        logger.debug("rev-parse failed for %s: %s", cwd, e)
        return False
    return result.returncode == 0


def _classify_session(cwd: str, started_at: str) -> _GitOutcome:
    """Inspect ``cwd``'s git log in the 24h window after ``started_at``.

    Returns the first commit's classification (or ``abandoned`` if no
    commits land). Errors are swallowed and reported as ``no_repo`` so a
    single broken repo can't stall a yield run.
    """
    try:
        end_iso = _add_hours_iso(started_at, _FOLLOW_WINDOW_HOURS)
    except ValueError:
        return _GitOutcome(classification="no_repo")

    commits = _git_log_window(cwd, since=started_at, until=end_iso)
    if not commits:
        return _GitOutcome(classification="abandoned")

    sha, subject = commits[0]
    age_hours = _hours_between(started_at, _commit_time(cwd, sha) or started_at)

    if _is_reverted(cwd, sha):
        return _GitOutcome(
            classification="reverted",
            commit_sha=sha,
            commit_msg=subject,
            commit_age_hours=age_hours,
        )

    return _GitOutcome(
        classification="productive",
        commit_sha=sha,
        commit_msg=subject,
        commit_age_hours=age_hours,
    )


def _git_log_window(cwd: str, *, since: str, until: str) -> list[tuple[str, str]]:
    """Return ``[(sha, subject), ...]`` for commits in ``[since, until)``.

    Uses ``--all`` so commits authored on any branch in this repo count —
    we don't want to miss a commit that landed on a feature branch and is
    now reachable from ``HEAD`` via a merge.
    """
    out = _run_git(
        cwd,
        [
            "log",
            "--all",
            f"--since={since}",
            f"--until={until}",
            "--format=%H|%s",
        ],
    )
    if out is None:
        return []
    commits: list[tuple[str, str]] = []
    for line in out.splitlines():
        if not line.strip():
            continue
        sha, _, subject = line.partition("|")
        if sha:
            commits.append((sha, subject))
    return commits


def _commit_time(cwd: str, sha: str) -> str | None:
    """Return the commit's ISO-8601 timestamp, or ``None`` on any error."""
    out = _run_git(cwd, ["show", "-s", "--format=%cI", sha])
    if out is None:
        return None
    out = out.strip()
    return out or None


def _is_reverted(cwd: str, sha: str) -> bool:
    """Return True if ``sha`` was reverted *or* is no longer reachable from HEAD.

    Two checks:

    1. **Subject scan**. ``git revert`` writes ``Revert "<original subject>"``
       and includes the short sha. We grep ``git log`` for ``revert`` plus
       the 7-char short sha — cheap and catches the standard flow.
    2. **Reachability**. ``git merge-base --is-ancestor sha HEAD`` returns 0
       if the commit is reachable from HEAD, 1 otherwise. A commit that
       was wiped by a hard reset / non-fast-forward force push is no longer
       reachable, so we classify it as reverted.

    Either signal trips the verdict.
    """
    short = sha[:7]
    revert_pattern = rf"revert.*{re.escape(short)}"
    log_out = _run_git(cwd, ["log", "--all", "--format=%s", "-i", f"--grep={revert_pattern}", "-E"])
    if log_out and log_out.strip():
        return True

    rc = _run_git_returncode(cwd, ["merge-base", "--is-ancestor", sha, "HEAD"])
    # 0 means reachable (kept), 1 means not reachable (treat as reverted),
    # any other code (or None for error) means we can't tell — be conservative
    # and don't flag as reverted.
    if rc == 1:
        return True
    return False


def _run_git(cwd: str, args: list[str]) -> str | None:
    """Run a git subcommand, return stdout (text) or ``None`` on any error."""
    try:
        result = subprocess.run(  # noqa: S603 — git args are not user-controllable
            ["git", "-C", cwd, *args],  # noqa: S607 — relies on git on PATH
            capture_output=True,
            text=True,
            timeout=_GIT_TIMEOUT_SECONDS,
        )
    except (subprocess.TimeoutExpired, OSError) as e:
        logger.debug("git %s failed in %s: %s", args[0] if args else "?", cwd, e)
        return None
    if result.returncode != 0:
        return None
    return result.stdout


def _run_git_returncode(cwd: str, args: list[str]) -> int | None:
    """Variant of ``_run_git`` that surfaces the return code (for ``--is-ancestor``)."""
    try:
        result = subprocess.run(  # noqa: S603 — git args are not user-controllable
            ["git", "-C", cwd, *args],  # noqa: S607 — relies on git on PATH
            capture_output=True,
            text=True,
            timeout=_GIT_TIMEOUT_SECONDS,
        )
    except (subprocess.TimeoutExpired, OSError) as e:
        logger.debug("git %s failed in %s: %s", args[0] if args else "?", cwd, e)
        return None
    return result.returncode


# ── time helpers ────────────────────────────────────────────────────────────


def _add_hours_iso(iso_ts: str, hours: int) -> str:
    """Return ``iso_ts + hours`` as an ISO-8601 string (preserves UTC ``Z``)."""
    from datetime import timedelta

    dt = _parse_iso(iso_ts)
    return (dt + timedelta(hours=hours)).isoformat()


def _hours_between(start_iso: str, end_iso: str) -> float | None:
    try:
        delta = _parse_iso(end_iso) - _parse_iso(start_iso)
    except ValueError:
        return None
    return delta.total_seconds() / 3600.0


def _parse_iso(ts: str):
    from datetime import datetime

    return datetime.fromisoformat(ts.replace("Z", "+00:00"))


# ── JSON helpers (for callers that want a serialisable response) ────────────


def to_dicts(entries: list[YieldEntry]) -> list[dict]:
    """Return ``entries`` as plain dicts (one per session)."""
    return [e.to_dict() for e in entries]


def dumps(entries: list[YieldEntry], *, indent: int | None = 2) -> str:
    """JSON-encode ``entries`` for the CLI's ``--format json`` path."""
    return json.dumps(to_dicts(entries), indent=indent)
