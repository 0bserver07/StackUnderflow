"""Cross-project aggregation driven by Scope + include/exclude filters.

Queries the session store for per-project token rollups, computes costs,
and returns a dict ready to be rendered. One SQL pass replaces the former
per-project pipeline loop.
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

    Args:
        conn: Open connection to the session store.
        scope: Date-range window; unbounded scope includes every day.
        include: If set, only these slugs are included.
        exclude: If set, these slugs are skipped.

    Returns:
        Dict with total_cost, total_messages, total_sessions, by_project (sorted desc).
    """
    rows = queries.cross_project_daily_totals(
        conn, since=scope.since, until=scope.until
    )

    # Count distinct sessions per project within scope
    session_sql = (
        "SELECT projects.slug, COUNT(DISTINCT sessions.id) AS cnt "
        "FROM sessions "
        "JOIN projects ON projects.id = sessions.project_id "
        "JOIN messages ON messages.session_fk = sessions.id "
        "WHERE 1=1 "
    )
    s_params: list[str] = []
    if scope.since:
        session_sql += "AND messages.timestamp >= ? "
        s_params.append(scope.since)
    if scope.until:
        session_sql += "AND messages.timestamp < ? "
        s_params.append(scope.until)
    session_sql += "GROUP BY projects.slug"
    session_counts: dict[str, int] = dict(
        conn.execute(session_sql, s_params).fetchall()
    )

    # Accumulate per-project totals
    per_slug: dict[str, dict] = {}
    for slug, _day, model, input_tokens, output_tokens, msg_count in rows:
        entry = per_slug.setdefault(slug, {"messages": 0, "cost": 0.0})
        entry["messages"] += msg_count
        if model:
            entry["cost"] += compute_cost(
                {"input": input_tokens or 0, "output": output_tokens or 0}, model
            )["total_cost"]

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
        sessions = session_counts.get(slug, 0)
        per_project.append({
            "name": slug,
            "cost": data["cost"],
            "messages": data["messages"],
            "sessions": sessions,
        })
        total_cost += data["cost"]
        total_messages += data["messages"]
        total_sessions += sessions

    per_project.sort(key=lambda row: row["cost"], reverse=True)

    return {
        "scope_label": scope.label,
        "total_cost": total_cost,
        "total_messages": total_messages,
        "total_sessions": total_sessions,
        "by_project": per_project,
    }
