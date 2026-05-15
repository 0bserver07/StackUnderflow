"""Cross-project aggregation driven by Scope + include/exclude filters.

Reads stored per-event costs out of ``usage_events`` (normalised once on
ingest with the current pricer) and rolls them up per-project. The
``messages``-based legacy path is kept as a fallback for pre-backfill
stores where ``usage_events`` is empty or absent — re-computing cost
off ``messages`` mis-prices any row whose model alias was canonicalised
differently than the live pricer expects, drops the ``speed='fast'``
priority multiplier on rows it can't reconstruct, and misses the 1/N
attribution contract the marts already encode. ``usage_events.cost_usd``
is the source of truth; this aggregator just sums it.
"""

from __future__ import annotations

import sqlite3

from stackunderflow.infra.costs import compute_cost
from stackunderflow.reports.scope import Scope
from stackunderflow.store import queries

__all__ = ["build_report"]


def build_report(
    conn: sqlite3.Connection,
    *,
    scope: Scope,
    include: list[str] | None,
    exclude: list[str] | None,
) -> dict:
    """Aggregate stats across all projects in the session store.

    Reads from ``usage_events.cost_usd`` when the table is populated
    (post-backfill); falls back to the legacy ``messages``-based path
    otherwise (fresh install, pre-backfill).

    Args:
        conn: Open connection to the session store.
        scope: Date-range window; unbounded scope includes every day.
        include: If set, only these slugs are included.
        exclude: If set, these slugs are skipped.

    Returns:
        Dict with total_cost, total_messages, total_sessions, by_project (sorted desc).
    """
    if _has_usage_events(conn):
        per_slug = _per_slug_from_usage_events(
            conn, since=scope.since, until=scope.until
        )
    else:
        per_slug = _per_slug_from_messages(
            conn, since=scope.since, until=scope.until
        )

    # Apply include/exclude
    if include is not None:
        per_slug = {k: v for k, v in per_slug.items() if k in include}
    if exclude is not None:
        per_slug = {k: v for k, v in per_slug.items() if k not in exclude}

    per_project: list[dict] = []
    total_cost = 0.0
    total_messages = 0
    total_sessions = 0

    for slug, data in per_slug.items():
        per_project.append({
            "name": slug,
            "cost": data["cost"],
            "messages": data["messages"],
            "sessions": data["sessions"],
        })
        total_cost += data["cost"]
        total_messages += data["messages"]
        total_sessions += data["sessions"]

    per_project.sort(key=lambda row: row["cost"], reverse=True)

    return {
        "scope_label": scope.label,
        "total_cost": total_cost,
        "total_messages": total_messages,
        "total_sessions": total_sessions,
        "by_project": per_project,
    }


# ── usage_events path (post-backfill, source of truth) ───────────────────────


def _has_usage_events(conn: sqlite3.Connection) -> bool:
    """True iff ``usage_events`` exists AND has at least one row.

    Matches the empty-mart-fallback gate the routes use: an empty
    ``usage_events`` means the ETL backfill has not run yet, so we
    must fall back to the legacy ``messages``-based path.
    """
    exists = conn.execute(
        "SELECT 1 FROM sqlite_master WHERE type='table' AND name='usage_events'"
    ).fetchone()
    if exists is None:
        return False
    row = conn.execute("SELECT 1 FROM usage_events LIMIT 1").fetchone()
    return row is not None


def _per_slug_from_usage_events(
    conn: sqlite3.Connection,
    *,
    since: str | None,
    until: str | None,
) -> dict[str, dict]:
    """Per-project rollup straight from ``usage_events``.

    ``cost_usd`` is the normalised, stored-once value — the same number
    every mart sums. ``ts`` is ISO-8601 like ``messages.timestamp``, so
    the boundary semantics match the legacy path: half-open if you pass
    a future ``until``, inclusive lexicographic compare otherwise.

    One pass for cost + message count; one pass for distinct session
    count per slug (a SELECT DISTINCT inside a GROUP BY is cheaper than
    a JOIN-to-sessions roundtrip on a populated event log).
    """
    sql = (
        "SELECT projects.slug AS slug, "
        "       COALESCE(SUM(usage_events.cost_usd), 0.0) AS cost, "
        "       COUNT(*) AS messages "
        "FROM usage_events "
        "JOIN projects ON projects.id = usage_events.project_id "
        "WHERE 1=1 "
    )
    params: list[str] = []
    if since:
        sql += "AND usage_events.ts >= ? "
        params.append(since)
    if until:
        sql += "AND usage_events.ts <= ? "
        params.append(until)
    sql += "GROUP BY projects.slug"

    per_slug: dict[str, dict] = {}
    for row in conn.execute(sql, params).fetchall():
        slug = row["slug"]
        per_slug[slug] = {
            "cost": float(row["cost"] or 0.0),
            "messages": int(row["messages"] or 0),
            "sessions": 0,
        }

    # Session count: one row per project_slug, counting distinct session_id
    # within the same window. Keeps the contract of the legacy path,
    # which counted ``COUNT(DISTINCT sessions.id)`` over the JOIN.
    session_sql = (
        "SELECT projects.slug AS slug, "
        "       COUNT(DISTINCT usage_events.session_id) AS cnt "
        "FROM usage_events "
        "JOIN projects ON projects.id = usage_events.project_id "
        "WHERE 1=1 "
    )
    s_params: list[str] = []
    if since:
        session_sql += "AND usage_events.ts >= ? "
        s_params.append(since)
    if until:
        session_sql += "AND usage_events.ts <= ? "
        s_params.append(until)
    session_sql += "GROUP BY projects.slug"

    for row in conn.execute(session_sql, s_params).fetchall():
        slug = row["slug"]
        if slug in per_slug:
            per_slug[slug]["sessions"] = int(row["cnt"] or 0)

    return per_slug


# ── messages path (legacy fallback, pre-backfill stores) ─────────────────────


def _per_slug_from_messages(
    conn: sqlite3.Connection,
    *,
    since: str | None,
    until: str | None,
) -> dict[str, dict]:
    """Legacy aggregation off ``messages`` — used when ``usage_events`` is empty.

    Recomputes cost off (input_tokens, output_tokens, model, speed) via
    ``compute_cost``. This is the pre-v0.7.2 path; it survives only for
    fresh installs that haven't run the backfill yet.
    """
    rows = queries.cross_project_daily_totals(conn, since=since, until=until)

    # Count distinct sessions per project within scope
    session_sql = (
        "SELECT projects.slug, COUNT(DISTINCT sessions.id) AS cnt "
        "FROM sessions "
        "JOIN projects ON projects.id = sessions.project_id "
        "JOIN messages ON messages.session_fk = sessions.id "
        "WHERE 1=1 "
    )
    s_params: list[str] = []
    if since:
        session_sql += "AND messages.timestamp >= ? "
        s_params.append(since)
    if until:
        session_sql += "AND messages.timestamp < ? "
        s_params.append(until)
    session_sql += "GROUP BY projects.slug"
    session_counts: dict[str, int] = dict(
        conn.execute(session_sql, s_params).fetchall()
    )

    per_slug: dict[str, dict] = {}
    for row in rows:
        slug, _day, model, input_tokens, output_tokens, msg_count = row[:6]
        speed = row[6] if len(row) >= 7 else "standard"
        entry = per_slug.setdefault(
            slug, {"messages": 0, "cost": 0.0, "sessions": 0}
        )
        entry["messages"] += msg_count
        if model:
            entry["cost"] += compute_cost(
                {"input": input_tokens or 0, "output": output_tokens or 0},
                model,
                speed=speed,
            )["total_cost"]

    for slug in per_slug:
        per_slug[slug]["sessions"] = session_counts.get(slug, 0)

    return per_slug
