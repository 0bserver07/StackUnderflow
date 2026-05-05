"""Read helpers for the ETL marts (Wave 3A — hot-path routes).

The Wave 3A route migration reads dashboard / cost / project totals out
of the five marts shipped in ``v006_etl_layer.sql`` instead of running
the per-request aggregator pass against the raw ``messages`` table.
Each helper here is one indexed ``SELECT`` — sub-millisecond even on
the user's 28K-message project.

Empty mart → caller falls back to the aggregator path. That's the
contract Wave 3A locks in: routes are mart-aware *and* aggregator-safe
so users with un-materialised stores keep working while users with a
populated ETL pipeline get the speedup. ``mart_has_project_row`` is
the gate.

Filter parity: ``provider`` and ``model`` accept the same case-insensitive
sequence the route layer normalises in (lower-cased, empties dropped).
Empty filter == "all", same as the aggregator path.
"""

from __future__ import annotations

import sqlite3
from collections.abc import Sequence
from typing import Any

# ── existence gate ──────────────────────────────────────────────────────────


def mart_has_project_row(conn: sqlite3.Connection, *, project_id: int) -> bool:
    """Return True iff ``project_mart`` has a row for ``project_id``.

    Used as the "is this project materialised?" gate by every route in
    Wave 3A. We deliberately key on ``project_mart`` rather than
    ``daily_mart`` because the project-level summary is the smallest
    unit of "this project has been processed by the ETL pipeline".
    A project with zero billable activity still gets a row in
    ``project_mart`` (totals all zero), so the gate doesn't misfire on
    projects that exist but haven't accrued usage events.
    """
    if not _table_exists(conn, "project_mart"):
        return False
    row = conn.execute(
        "SELECT 1 FROM project_mart WHERE project_id = ? LIMIT 1",
        (project_id,),
    ).fetchone()
    return row is not None


def _table_exists(conn: sqlite3.Connection, name: str) -> bool:
    row = conn.execute(
        "SELECT 1 FROM sqlite_master WHERE type='table' AND name=?", (name,)
    ).fetchone()
    return row is not None


# ── project_mart reads ──────────────────────────────────────────────────────


def list_project_mart(
    conn: sqlite3.Connection,
    *,
    provider_filter: set[str] | None = None,
) -> list[dict[str, Any]]:
    """Return every row from ``project_mart``, optionally narrowed by provider.

    One indexed scan over a small table (one row per project). The
    provider filter is applied in SQL because ``project_mart`` is wide
    enough that pushing it down beats iterating in Python.
    """
    if not _table_exists(conn, "project_mart"):
        return []
    sql = (
        "SELECT project_id, provider, slug, display_name, first_ts, last_ts, "
        "       total_messages, total_sessions, total_input_tokens, "
        "       total_output_tokens, total_cache_read, total_cache_create, "
        "       total_cost_usd FROM project_mart"
    )
    params: list[Any] = []
    if provider_filter:
        placeholders = ",".join(["?"] * len(provider_filter))
        sql += f" WHERE LOWER(provider) IN ({placeholders})"
        params.extend(p.lower() for p in provider_filter)
    rows = conn.execute(sql, params).fetchall()
    return [dict(r) for r in rows]


def get_project_mart_row(
    conn: sqlite3.Connection, *, project_id: int
) -> dict[str, Any] | None:
    """Return the ``project_mart`` row for ``project_id`` or ``None``."""
    if not _table_exists(conn, "project_mart"):
        return None
    row = conn.execute(
        "SELECT project_id, provider, slug, display_name, first_ts, last_ts, "
        "       total_messages, total_sessions, total_input_tokens, "
        "       total_output_tokens, total_cache_read, total_cache_create, "
        "       total_cost_usd "
        "FROM project_mart WHERE project_id = ?",
        (project_id,),
    ).fetchone()
    return dict(row) if row is not None else None


# ── daily_mart reads ────────────────────────────────────────────────────────


def daily_for_project(
    conn: sqlite3.Connection,
    *,
    project_id: int,
    day_from: str | None = None,
    day_to: str | None = None,
    provider_filter: set[str] | None = None,
    model_filter: set[str] | None = None,
) -> list[dict[str, Any]]:
    """Return ``daily_mart`` rows for one project, optionally bounded.

    ``day_from`` / ``day_to`` are inclusive ISO ``YYYY-MM-DD`` strings.
    Caller is responsible for applying any timezone offset to the day
    window before calling — marts store UTC days.
    """
    if not _table_exists(conn, "daily_mart"):
        return []
    sql = (
        "SELECT day, project_id, provider, model, speed, "
        "       input_tokens, output_tokens, cache_read, cache_create, "
        "       message_count, session_count, cost_usd "
        "FROM daily_mart WHERE project_id = ?"
    )
    params: list[Any] = [project_id]
    if day_from:
        sql += " AND day >= ?"
        params.append(day_from)
    if day_to:
        sql += " AND day <= ?"
        params.append(day_to)
    if provider_filter:
        placeholders = ",".join(["?"] * len(provider_filter))
        sql += f" AND LOWER(provider) IN ({placeholders})"
        params.extend(p.lower() for p in provider_filter)
    if model_filter:
        placeholders = ",".join(["?"] * len(model_filter))
        sql += f" AND LOWER(model) IN ({placeholders})"
        params.extend(m.lower() for m in model_filter)
    sql += " ORDER BY day"
    return [dict(r) for r in conn.execute(sql, params).fetchall()]


def daily_global(
    conn: sqlite3.Connection,
    *,
    day_from: str | None = None,
    day_to: str | None = None,
    provider_filter: set[str] | None = None,
    model_filter: set[str] | None = None,
) -> list[dict[str, Any]]:
    """Return ``daily_mart`` rows across all projects, optionally bounded.

    Used by the cost-data totals/by_day/by_model rollups when no project
    scope is set. ``provider_filter`` lets the FilterBar narrow the
    dashboard's global cost view to a subset of providers.
    """
    if not _table_exists(conn, "daily_mart"):
        return []
    sql = (
        "SELECT day, project_id, provider, model, speed, "
        "       input_tokens, output_tokens, cache_read, cache_create, "
        "       message_count, session_count, cost_usd FROM daily_mart WHERE 1=1"
    )
    params: list[Any] = []
    if day_from:
        sql += " AND day >= ?"
        params.append(day_from)
    if day_to:
        sql += " AND day <= ?"
        params.append(day_to)
    if provider_filter:
        placeholders = ",".join(["?"] * len(provider_filter))
        sql += f" AND LOWER(provider) IN ({placeholders})"
        params.extend(p.lower() for p in provider_filter)
    if model_filter:
        placeholders = ",".join(["?"] * len(model_filter))
        sql += f" AND LOWER(model) IN ({placeholders})"
        params.extend(m.lower() for m in model_filter)
    sql += " ORDER BY day"
    return [dict(r) for r in conn.execute(sql, params).fetchall()]


# ── provider_day_mart reads ─────────────────────────────────────────────────


def provider_day_rollup(
    conn: sqlite3.Connection,
    *,
    day_from: str | None = None,
    day_to: str | None = None,
    provider_filter: set[str] | None = None,
) -> list[dict[str, Any]]:
    """Return per-provider rollups for the ``cost-data/by-provider`` route.

    Pre-aggregated by the ``provider_day_mart`` builder so this is a
    single GROUP BY over a tiny table (one row per (day, provider)).
    """
    if not _table_exists(conn, "provider_day_mart"):
        return []
    sql = (
        "SELECT provider, "
        "       SUM(cost_usd) AS cost_usd, "
        "       SUM(message_count) AS message_count, "
        "       SUM(session_count) AS session_count, "
        "       SUM(project_count) AS project_count "
        "FROM provider_day_mart WHERE 1=1"
    )
    params: list[Any] = []
    if day_from:
        sql += " AND day >= ?"
        params.append(day_from)
    if day_to:
        sql += " AND day <= ?"
        params.append(day_to)
    if provider_filter:
        placeholders = ",".join(["?"] * len(provider_filter))
        sql += f" AND LOWER(provider) IN ({placeholders})"
        params.extend(p.lower() for p in provider_filter)
    sql += " GROUP BY provider ORDER BY SUM(cost_usd) DESC"
    return [dict(r) for r in conn.execute(sql, params).fetchall()]


# ── shape helpers ───────────────────────────────────────────────────────────


def daily_mart_to_overview(
    rows: Sequence[dict[str, Any]],
    *,
    project_mart_row: dict[str, Any] | None = None,
) -> dict[str, Any]:
    """Aggregate ``daily_mart`` rows into the dashboard overview shape.

    Mirrors the keys ``aggregator.summarise()`` writes into the
    ``overview`` block — total tokens, total cost, date_range — so the
    route layer can swap the data source without touching its
    consumer.

    When ``project_mart_row`` is provided we trust its lifetime totals
    over the daily aggregate (faster + tolerant of partial day coverage
    when filters narrow the daily window).
    """
    if project_mart_row is not None:
        return {
            "total_tokens": {
                "input": int(project_mart_row.get("total_input_tokens", 0) or 0),
                "output": int(project_mart_row.get("total_output_tokens", 0) or 0),
                "cache_read": int(project_mart_row.get("total_cache_read", 0) or 0),
                "cache_creation": int(project_mart_row.get("total_cache_create", 0) or 0),
            },
            "total_cost": float(project_mart_row.get("total_cost_usd", 0.0) or 0.0),
            "date_range": {
                "start": project_mart_row.get("first_ts"),
                "end": project_mart_row.get("last_ts"),
            },
            "total_messages": int(project_mart_row.get("total_messages", 0) or 0),
            "total_sessions": int(project_mart_row.get("total_sessions", 0) or 0),
        }

    inp = sum(int(r.get("input_tokens", 0) or 0) for r in rows)
    out = sum(int(r.get("output_tokens", 0) or 0) for r in rows)
    cache_r = sum(int(r.get("cache_read", 0) or 0) for r in rows)
    cache_c = sum(int(r.get("cache_create", 0) or 0) for r in rows)
    cost = sum(float(r.get("cost_usd", 0.0) or 0.0) for r in rows)
    msgs = sum(int(r.get("message_count", 0) or 0) for r in rows)
    days = sorted({r["day"] for r in rows if r.get("day")})
    return {
        "total_tokens": {
            "input": inp,
            "output": out,
            "cache_read": cache_r,
            "cache_creation": cache_c,
        },
        "total_cost": cost,
        "date_range": {
            "start": days[0] if days else None,
            "end": days[-1] if days else None,
        },
        "total_messages": msgs,
        "total_sessions": 0,
    }


def daily_mart_by_day(
    rows: Sequence[dict[str, Any]],
) -> list[dict[str, Any]]:
    """Group ``daily_mart`` rows by ``day`` → one entry per day.

    Output shape matches the ``daily_stats`` / ``daily_costs`` arrays
    the legacy aggregator emits: ``{date, cost, by_model, total_input,
    total_output, total_cache_read, total_cache_create, message_count}``.
    """
    by_day: dict[str, dict[str, Any]] = {}
    for r in rows:
        day = r.get("day")
        if not day:
            continue
        bucket = by_day.setdefault(
            day,
            {
                "date": day,
                "cost": 0.0,
                "by_model": {},
                "total_input": 0,
                "total_output": 0,
                "total_cache_read": 0,
                "total_cache_create": 0,
                "message_count": 0,
            },
        )
        cost = float(r.get("cost_usd", 0.0) or 0.0)
        bucket["cost"] += cost
        bucket["total_input"] += int(r.get("input_tokens", 0) or 0)
        bucket["total_output"] += int(r.get("output_tokens", 0) or 0)
        bucket["total_cache_read"] += int(r.get("cache_read", 0) or 0)
        bucket["total_cache_create"] += int(r.get("cache_create", 0) or 0)
        bucket["message_count"] += int(r.get("message_count", 0) or 0)
        model = r.get("model") or ""
        if model:
            bucket["by_model"][model] = bucket["by_model"].get(model, 0.0) + cost
    return [by_day[d] for d in sorted(by_day)]


def daily_mart_by_model(
    rows: Sequence[dict[str, Any]],
) -> dict[str, dict[str, Any]]:
    """Group ``daily_mart`` rows by model → ``models`` map shape.

    ``aggregator.summarise()`` emits a ``models`` dict keyed by model
    id with ``{count, cost, ...}`` values. We recover the same shape
    from the daily mart so the dashboard's per-model breakdown card
    keeps rendering unchanged.
    """
    out: dict[str, dict[str, Any]] = {}
    for r in rows:
        model = r.get("model") or ""
        if not model:
            continue
        bucket = out.setdefault(
            model,
            {
                "count": 0,
                "cost": 0.0,
                "input_tokens": 0,
                "output_tokens": 0,
                "cache_read": 0,
                "cache_creation": 0,
            },
        )
        bucket["count"] += int(r.get("message_count", 0) or 0)
        bucket["cost"] += float(r.get("cost_usd", 0.0) or 0.0)
        bucket["input_tokens"] += int(r.get("input_tokens", 0) or 0)
        bucket["output_tokens"] += int(r.get("output_tokens", 0) or 0)
        bucket["cache_read"] += int(r.get("cache_read", 0) or 0)
        bucket["cache_creation"] += int(r.get("cache_create", 0) or 0)
    return out
