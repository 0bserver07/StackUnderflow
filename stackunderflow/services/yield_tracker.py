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

Performance posture (post-fix-yield-timeout): the previous v1 path issued
one ``git rev-parse`` per session plus up to four ``git`` invocations per
*classified* session, which scaled with N(sessions) and timed the route
out (>15 s) on real-store-shape projects (247K messages, 95+ sessions in
a single project). The new pipeline batches per-distinct-cwd:

* one bulk SQL query resolves ``cwd`` for every session in the period
* one ``git rev-parse`` per *distinct* cwd, memoised in a workspace cache
* one ``git log`` per distinct cwd over the period's full session-window
  union, returning ``(committed_at, sha, subject)`` for every commit;
  per-session windowed lookups are answered from memory
* one ``git rev-list HEAD`` per distinct cwd, materialising the
  reachability set so ``--is-ancestor`` isn't re-shelled per session
* one ``git log --grep=revert`` per distinct cwd, building a short-sha
  hit set; per-session revert checks are O(1) lookups against the set

A configurable cap (env ``STACKUNDERFLOW_YIELD_MAX_SESSIONS_PER_PROJECT``,
default 200) trims the per-project tail to keep the route bounded even
if a single project has thousands of sessions in the window.

Public API:

    compute_yield(conn, period="month", project_filter=None) -> list[YieldEntry]
    yield_summary(entries) -> dict
"""

from __future__ import annotations

import json
import logging
import os
import re
import shutil
import sqlite3
import subprocess
from dataclasses import asdict, dataclass, field
from pathlib import Path
from typing import Literal

from stackunderflow.infra.costs import compute_cost
from stackunderflow.reports.scope import Scope, parse_period
from stackunderflow.store import mart_queries

logger = logging.getLogger(__name__)

# 5s is enough for any local git query against a healthy repo and short
# enough that a hung repo (e.g. NFS, lock contention) can't stall a report.
_GIT_TIMEOUT_SECONDS = 5

# 24h credit window — see module docstring.
_FOLLOW_WINDOW_HOURS = 24

# Per-project session cap to bound the route's worst case on pathological
# projects. The most-recent N sessions per project are kept; older ones are
# silently dropped. Set to a non-positive value (or ``unlimited``) to
# disable. The cap applies *after* scope/project filters and *before* any
# git work, so it's the last knob before the expensive subprocess fan-out.
_DEFAULT_MAX_SESSIONS_PER_PROJECT = 200
_MAX_SESSIONS_ENV = "STACKUNDERFLOW_YIELD_MAX_SESSIONS_PER_PROJECT"

# Hard cap on how many commits we keep per cwd in the per-request workspace
# cache. ``--max-count`` is passed to ``git log`` so a repo with 200K
# commits in the window doesn't dominate memory; we only need enough rows
# to find the *first* commit per session window.
_GIT_LOG_MAX_COMMITS = 5000

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
    rows = _cap_sessions_per_project(rows)

    # Bucket sessions by cwd so we can do per-distinct-cwd batched git work.
    # Sessions with empty cwd get short-circuited to ``no_repo`` without
    # touching git at all.
    by_cwd: dict[str, list[dict]] = {}
    for row in rows:
        cwd = row["cwd"] or ""
        by_cwd.setdefault(cwd, []).append(row)

    # Build one workspace per distinct cwd — git pre-flight + bulk log +
    # reachability set + revert short-sha set. Empty cwd is special-cased
    # so it never triggers a subprocess.
    workspaces: dict[str, _GitWorkspace] = {}
    for cwd, sessions in by_cwd.items():
        if not cwd:
            workspaces[cwd] = _GitWorkspace.empty(cwd)
            continue
        starts = sorted(s["started_at"] for s in sessions if s["started_at"])
        workspaces[cwd] = _build_workspace(cwd, session_starts=starts)

    # Re-emit entries in the original (start-time) order so the public
    # contract stays stable for callers that depend on the ordering.
    entries: list[YieldEntry] = []
    for row in rows:
        cwd = row["cwd"] or ""
        ws = workspaces[cwd]
        outcome = ws.classify(row["started_at"])
        entries.append(
            YieldEntry(
                session_id=row["session_id"],
                project_slug=row["project_slug"],
                cwd=cwd,
                started_at=row["started_at"],
                cost_usd=float(row["cost_usd"] or 0.0),
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


def _max_sessions_per_project() -> int | None:
    """Resolve the per-project cap from env, falling back to the default.

    Returns ``None`` to mean *no cap*. Accepted forms for the env var:
    a positive integer keeps that many; ``0`` / negative / ``unlimited``
    disables the cap. Anything unparseable falls back to the default.
    """
    raw = os.environ.get(_MAX_SESSIONS_ENV)
    if raw is None:
        return _DEFAULT_MAX_SESSIONS_PER_PROJECT
    raw = raw.strip().lower()
    if raw in ("", "unlimited", "none"):
        return None
    try:
        n = int(raw)
    except ValueError:
        logger.debug("Bad %s value %r; falling back to default", _MAX_SESSIONS_ENV, raw)
        return _DEFAULT_MAX_SESSIONS_PER_PROJECT
    if n <= 0:
        return None
    return n


def _cap_sessions_per_project(rows: list[dict]) -> list[dict]:
    """Trim each project's session list to the most-recent ``cap`` rows.

    Rows are assumed to be in chronological order (the underlying SQL
    sorts by ``first_ts``). After capping, the original order is
    preserved so downstream ordering contracts hold.
    """
    cap = _max_sessions_per_project()
    if cap is None or len(rows) <= cap:
        return rows

    # Group by project, keep last ``cap`` rows (chronological tail).
    by_project: dict[str, list[dict]] = {}
    for r in rows:
        by_project.setdefault(r["project_slug"], []).append(r)

    keep_ids: set[str] = set()
    dropped = 0
    for project, sessions in by_project.items():
        if len(sessions) <= cap:
            keep_ids.update(s["session_id"] for s in sessions)
            continue
        tail = sessions[-cap:]
        dropped += len(sessions) - cap
        keep_ids.update(s["session_id"] for s in tail)
        logger.info(
            "yield: capped project %s from %d to %d sessions (most recent kept)",
            project,
            len(sessions),
            cap,
        )
    if dropped:
        logger.info("yield: dropped %d session(s) total via per-project cap", dropped)
    return [r for r in rows if r["session_id"] in keep_ids]


def _query_sessions(
    conn: sqlite3.Connection,
    *,
    scope: Scope,
    project_filter: list[str] | None,
) -> list[dict]:
    """Return one row per session inside ``scope`` with ``cwd`` and cost.

    Wave 4A — when ``session_mart`` is materialised we read the session
    list (cwd, started_at, cost_usd, primary_model) from there instead
    of running a per-session ``compute_cost`` pass. ``cwd`` is still
    sourced from ``messages.raw_json`` because the v1 ``session_mart``
    leaves the column ``NULL`` per the builder docstring; the bulk
    JSON lookup is a single indexed scan, dwarfed by the git correlation
    work that happens later.

    Empty mart → fall back to the legacy aggregator path so users
    without a populated ETL pipeline keep working.
    """
    if mart_queries.mart_has_session_rows(conn):
        return _query_sessions_from_mart(
            conn,
            scope=scope,
            project_filter=project_filter,
        )
    return _query_sessions_from_messages(
        conn,
        scope=scope,
        project_filter=project_filter,
    )


def _query_sessions_from_mart(
    conn: sqlite3.Connection,
    *,
    scope: Scope,
    project_filter: list[str] | None,
) -> list[dict]:
    """Mart-backed session enumeration for ``compute_yield``."""
    rows = mart_queries.session_mart_rows_for_yield(
        conn,
        since_iso=scope.since,
        until_iso=scope.until,
        project_slugs=project_filter or None,
    )
    # Bulk-resolve cwd for every session in one SQL pass instead of
    # one per session — the per-session lookup was N round-trips against
    # the partitioned ``messages`` view, which dominated when the mart
    # carried hundreds of sessions.
    session_fks = [
        int(s["session_fk"]) for s in rows if s.get("session_fk") is not None
    ]
    cwd_by_fk = _bulk_first_cwd_for_sessions(conn, session_fks)

    out: list[dict] = []
    for sess in rows:
        session_fk = sess.get("session_fk")
        cwd = cwd_by_fk.get(int(session_fk), "") if session_fk is not None else ""
        out.append(
            {
                "session_id": sess["session_id"],
                "project_slug": sess["project_slug"],
                "cwd": cwd,
                "started_at": sess["first_ts"],
                "cost_usd": float(sess.get("cost_usd", 0.0) or 0.0),
            }
        )
    return out


def _query_sessions_from_messages(
    conn: sqlite3.Connection,
    *,
    scope: Scope,
    project_filter: list[str] | None,
) -> list[dict]:
    """Aggregator-path session enumeration — kept as the empty-mart fallback.

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
    session_fks = [int(s["session_fk"]) for s in sessions]
    cwd_by_fk = _bulk_first_cwd_for_sessions(conn, session_fks)

    out: list[dict] = []
    for sess in sessions:
        cwd = cwd_by_fk.get(int(sess["session_fk"]), "")
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
    """Return the first non-empty ``cwd`` recorded in this session's messages.

    Kept for backwards compatibility with any external caller that
    happens to import it (none in tree). New code should use
    ``_bulk_first_cwd_for_sessions`` to avoid the N+1 lookup pattern.
    """
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


def _bulk_first_cwd_for_sessions(
    conn: sqlite3.Connection,
    session_fks: list[int],
) -> dict[int, str]:
    """Return ``{session_fk: first_non_empty_cwd}`` for every fk in one query.

    Replaces N separate ``SELECT ... LIMIT 1`` round-trips against the
    partitioned ``messages`` view — each of those had to fan out to
    every monthly subtable, which dominated wall-clock on real-store-
    shape projects. The bulk variant uses a single windowed query per
    chunk: ``ROW_NUMBER() OVER (PARTITION BY session_fk ORDER BY seq)``
    asks SQLite to surface only the *first* matching row per session
    instead of streaming all of them back to Python.

    We chunk the ``IN`` list to stay under SQLite's default
    ``SQLITE_MAX_VARIABLE_NUMBER`` (commonly 999, raised in newer
    builds) so very large session counts don't trip the parser. Empty
    fk list → empty dict.
    """
    if not session_fks:
        return {}

    out: dict[int, str] = {}
    chunk_size = 500
    for start in range(0, len(session_fks), chunk_size):
        chunk = session_fks[start : start + chunk_size]
        # ``placeholders`` is a fixed ``?`` skeleton derived from the
        # chunk size, never user input; every value below is bound
        # parametrically. Same posture as the long-standing dynamic
        # ``IN (...)`` builders in ``mart_queries.session_mart_*``.
        placeholders = ",".join("?" for _ in chunk)
        sql = "WITH ranked AS (SELECT session_fk, json_extract(raw_json, '$.cwd') AS cwd, ROW_NUMBER() OVER (PARTITION BY session_fk ORDER BY seq) AS rn FROM messages WHERE session_fk IN (" + placeholders + ") AND json_extract(raw_json, '$.cwd') IS NOT NULL AND json_extract(raw_json, '$.cwd') != '') SELECT session_fk, cwd FROM ranked WHERE rn = 1"  # noqa: S608, E501
        for row in conn.execute(sql, chunk):
            out[int(row["session_fk"])] = str(row["cwd"] or "")
    return out


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
    # Group by ``(model, speed)`` so the Anthropic priority/fast tier rows
    # price at 6× via compute_cost(speed=...). Without this dimension, a
    # session that ran a few Opus prompts on the priority tier would report
    # 1× cost — silently understating the spend yield is correlating against.
    rows = conn.execute(
        "SELECT COALESCE(model, '') AS model, "
        "       COALESCE(speed, 'standard') AS speed, "
        "       SUM(input_tokens) AS inp, "
        "       SUM(output_tokens) AS out, "
        "       SUM(cache_create_tokens) AS cc, "
        "       SUM(cache_read_tokens) AS cr "
        "FROM messages WHERE session_fk = ? "
        "GROUP BY model, speed",
        (session_fk,),
    ).fetchall()

    total = 0.0
    for r in rows:
        model = r["model"]
        if not model:
            continue
        speed = r["speed"] or "standard"
        tokens = {
            "input": int(r["inp"] or 0),
            "output": int(r["out"] or 0),
            "cache_creation": int(r["cc"] or 0),
            "cache_read": int(r["cr"] or 0),
        }
        try:
            total += compute_cost(
                tokens, model, provider=provider, speed=speed,
            ).get("total_cost", 0.0)
        except Exception as e:  # noqa: BLE001 - cost issues should not stall yield
            logger.debug("compute_cost failed for model %s: %s", model, e)
    return total


# ── git introspection ───────────────────────────────────────────────────────


@dataclass
class _Commit:
    """One git commit, normalized for in-memory windowed lookups.

    ``committed_at`` is the raw ISO-8601 string from ``git log %cI`` (which
    can carry any local offset). ``committed_at_utc`` is the same instant
    parsed into a UTC ``datetime`` so per-session window comparisons can
    be done without depending on string-comparable offsets — the ``Z`` /
    ``+00:00`` / ``-04:00`` mix that ``session_mart.first_ts`` and ``%cI``
    produce broke an earlier string-comparison version of this code.
    """

    sha: str
    subject: str
    committed_at: str  # ISO-8601 (raw, may carry any UTC offset)
    committed_at_utc: object = None  # datetime; opaque here for type-friendliness


@dataclass
class _GitWorkspace:
    """Per-cwd batched git state, computed once per ``compute_yield`` call.

    Holds enough information to classify *every* session whose cwd is
    this directory without shelling out to git per-session. Built by
    ``_build_workspace``.
    """

    cwd: str
    is_repo: bool = False
    # Commits in the union of every session window, ascending by time.
    commits: list[_Commit] = field(default_factory=list)
    # SHAs reachable from HEAD — anything outside this set is treated as
    # ``reverted`` (was wiped by a hard reset / non-ff push).
    reachable_from_head: set[str] = field(default_factory=set)
    # Short-sha (7-char) hits found by scanning ``git log --grep=revert``;
    # if a candidate commit's short-sha is in here, classify as ``reverted``.
    revert_short_shas: set[str] = field(default_factory=set)
    # Single-call default: when reachable_from_head is empty *and* the
    # rev-list call failed (returncode != 0) we don't want to mark every
    # commit as reverted-by-unreachability. ``head_known`` distinguishes
    # "we successfully read HEAD's reachability set" from "we have no
    # info, be conservative".
    head_known: bool = False

    @classmethod
    def empty(cls, cwd: str) -> _GitWorkspace:
        """Workspace for an empty cwd / non-repo / no sessions case."""
        return cls(cwd=cwd, is_repo=False)

    def classify(self, started_at: str) -> _GitOutcome:
        """Classify one session's ``started_at`` against this workspace."""
        if not self.is_repo:
            return _GitOutcome(classification="no_repo")

        try:
            start_dt = _parse_iso(started_at)
        except ValueError:
            return _GitOutcome(classification="no_repo")
        from datetime import timedelta

        window_end_dt = start_dt + timedelta(hours=_FOLLOW_WINDOW_HOURS)

        # First commit (chronologically) inside [start_dt, window_end_dt].
        # Compare on the UTC-parsed datetime, *not* the raw string — git's
        # ``%cI`` carries the committer's local offset and string-comparing
        # ``...Z`` against ``...-04:00`` silently drops valid commits.
        first: _Commit | None = None
        for c in self.commits:
            ts = c.committed_at_utc
            if ts is None:
                continue
            if ts < start_dt or ts > window_end_dt:
                continue
            if first is None or ts < first.committed_at_utc:
                first = c
        if first is None:
            return _GitOutcome(classification="abandoned")

        age = _hours_between(started_at, first.committed_at)
        if self._is_reverted(first):
            return _GitOutcome(
                classification="reverted",
                commit_sha=first.sha,
                commit_msg=first.subject,
                commit_age_hours=age,
            )
        return _GitOutcome(
            classification="productive",
            commit_sha=first.sha,
            commit_msg=first.subject,
            commit_age_hours=age,
        )

    def _is_reverted(self, c: _Commit) -> bool:
        """Mirror the original two-signal revert check, in-memory.

        1. Subject scan: short-sha appears in a ``Revert "..."`` subject.
        2. Reachability: commit not reachable from HEAD (only consulted
           when the rev-list call succeeded; otherwise we stay
           conservative and don't flag).
        """
        if c.sha[:7] in self.revert_short_shas:
            return True
        if self.head_known and c.sha not in self.reachable_from_head:
            return True
        return False


def _build_workspace(cwd: str, *, session_starts: list[str]) -> _GitWorkspace:
    """Materialise a ``_GitWorkspace`` for one cwd in the bounded session set.

    All git work for a single repo is funneled through here:

    * one ``git rev-parse`` to confirm the path is a repo
    * one ``git log --since=earliest --until=latest+24h`` to get every
      commit that could possibly land in any session's 24h window
    * one ``git rev-list --all HEAD`` to materialise the reachability set
    * one ``git log --grep=revert -i`` to harvest short-sha mentions in
      revert subjects (used for the per-commit short-sha test)
    """
    ws = _GitWorkspace(cwd=cwd)
    if not _is_git_repo(cwd):
        return ws
    ws.is_repo = True

    if not session_starts:
        return ws

    earliest = session_starts[0]
    try:
        window_end = _add_hours_iso(session_starts[-1], _FOLLOW_WINDOW_HOURS)
    except ValueError:
        return ws

    ws.commits = _bulk_git_log_window(cwd, since=earliest, until=window_end)
    ws.reachable_from_head, ws.head_known = _bulk_reachable_from_head(cwd)
    ws.revert_short_shas = _bulk_revert_short_shas(cwd)
    return ws


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


def _bulk_git_log_window(
    cwd: str, *, since: str, until: str,
) -> list[_Commit]:
    """One ``git log`` over ``[since, until]`` for ``cwd``.

    Returns the full commit list (sha, subject, ISO timestamp) for the
    requested window across all branches. Per-session windowed queries
    walk this in-memory list rather than re-shelling out to git.
    """
    out = _run_git(
        cwd,
        [
            "log",
            "--all",
            f"--since={since}",
            f"--until={until}",
            f"--max-count={_GIT_LOG_MAX_COMMITS}",
            "--format=%H|%cI|%s",
        ],
    )
    if out is None:
        return []
    commits: list[_Commit] = []
    for line in out.splitlines():
        if not line.strip():
            continue
        sha, _, rest = line.partition("|")
        committed_at, _, subject = rest.partition("|")
        if not sha:
            continue
        # Pre-parse the timestamp into a tz-aware UTC datetime so the
        # per-session windowed lookup never has to. Bad / unparseable
        # stamps (shouldn't happen with %cI) are skipped — including the
        # commit at all would force string-compare branches downstream.
        try:
            ts = _parse_iso(committed_at)
        except ValueError:
            logger.debug("skipping commit %s: bad %cI=%s", sha, committed_at)
            continue
        commits.append(
            _Commit(
                sha=sha,
                subject=subject,
                committed_at=committed_at,
                committed_at_utc=ts,
            ),
        )
    # Sort ascending by parsed UTC time so the windowed first-commit
    # lookup is cheap and unambiguous (git log defaults to newest-first).
    commits.sort(key=lambda c: c.committed_at_utc)
    return commits


def _bulk_reachable_from_head(cwd: str) -> tuple[set[str], bool]:
    """Return ``(reachable_shas, head_known)`` for a single cwd.

    ``head_known`` is False when we couldn't enumerate HEAD's history
    (detached, broken HEAD, or a brand-new repo); callers should *not*
    use the empty set as evidence of unreachability in that case.
    """
    out = _run_git(cwd, ["rev-list", "HEAD"])
    if out is None:
        return set(), False
    shas = {s for s in (line.strip() for line in out.splitlines()) if s}
    return shas, True


def _bulk_revert_short_shas(cwd: str) -> set[str]:
    """Scan all branches' commit subjects for ``revert <shortsha>``.

    Returns the set of 7-char short SHAs mentioned in any revert subject.
    Used to satisfy the classic ``git revert`` flow ("Revert ..." subject
    line includes the original short sha) without re-shelling per session.
    """
    out = _run_git(
        cwd,
        ["log", "--all", "--format=%s", "-i", "--grep=revert"],
    )
    if out is None or not out.strip():
        return set()
    short_shas: set[str] = set()
    # Find every 7-hex-char run in any matching subject; cheap and lets
    # both ``Revert "..." (deadbee)`` and ``revert deadbee...`` register.
    pattern = re.compile(r"\b([0-9a-fA-F]{7})\b")
    for line in out.splitlines():
        for m in pattern.finditer(line):
            short_shas.add(m.group(1).lower())
    return short_shas


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
    """Variant of ``_run_git`` that surfaces the return code (for ``--is-ancestor``).

    Retained as a module-level helper for callers / tests that import it.
    The new pipeline uses ``_bulk_reachable_from_head`` instead so the
    per-session ``--is-ancestor`` fan-out is gone.
    """
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
