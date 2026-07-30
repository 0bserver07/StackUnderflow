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
    project_ids: Sequence[int] | None = None,
) -> list[dict[str, Any]]:
    """Return ``project_mart`` rows, optionally narrowed by provider and/or id.

    One indexed scan over a small table (one row per project). Both filters
    are applied in SQL — ``project_mart`` is wide enough that pushing them
    down beats iterating in Python — and they AND together when both are
    given.

    ``project_ids`` scopes the read to those projects. ``None`` means "every
    project"; an **empty** sequence means "no projects" and returns ``[]``
    without touching the DB — it is never silently promoted to "all". That
    is the same trap ``queries._scoped_rows`` documents, and it is live, not
    theoretical: the caller that needs the scope (``GET /api/projects``)
    derives it from a page slice, and an offset past the end of the list is
    a legitimate request whose page is empty. It relies on this contract
    instead of branching, so promoting empty to all would hand exactly that
    request the whole mart.

    Bound-parameter budget: the id list becomes one ``?`` each. The largest
    caller-side scope is a project page (``PROJECTS_MAX_LIMIT`` slugs, plus
    provider-duplicates), which stays far under SQLite's 32766-variable
    ceiling, so this does not chunk — same shape as
    :func:`command_day_series` / :func:`command_count_in_window`.
    """
    if not _table_exists(conn, "project_mart"):
        return []
    sql = (
        "SELECT project_id, provider, slug, display_name, first_ts, last_ts, "
        "       total_messages, total_sessions, total_input_tokens, "
        "       total_output_tokens, total_cache_read, total_cache_create, "
        "       total_cost_usd, "
        "       total_user_messages, total_assistant_messages, "
        "       total_tool_use_messages, total_tool_result_messages, "
        "       total_commands, "
        "       total_records, total_errors, errors_by_category, "
        "       total_cache_read_messages, total_commands_followed_by_interruption, "
        "       total_command_tools, total_command_steps "
        "FROM project_mart"
    )
    params: list[Any] = []
    clauses: list[str] = []
    if provider_filter:
        placeholders = ",".join(["?"] * len(provider_filter))
        clauses.append(f"LOWER(provider) IN ({placeholders})")
        params.extend(p.lower() for p in provider_filter)
    if project_ids is not None:
        pids = [int(p) for p in project_ids]
        if not pids:
            return []
        placeholders = ",".join("?" * len(pids))
        clauses.append(f"project_id IN ({placeholders})")
        params.extend(pids)
    if clauses:
        sql += " WHERE " + " AND ".join(clauses)  # noqa: S608 — placeholders are bound
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
        "       total_cost_usd, "
        "       total_user_messages, total_assistant_messages, "
        "       total_tool_use_messages, total_tool_result_messages, "
        "       total_commands, "
        "       total_records, total_errors, errors_by_category, "
        "       total_cache_read_messages, total_commands_followed_by_interruption, "
        "       total_command_tools, total_command_steps "
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
    when filters narrow the daily window). The project-mart row also
    carries the materialised message-type counts (v022), so we surface
    ``message_types`` here — the same dict ``aggregator.summarise`` writes
    into ``overview`` (``user`` / ``assistant`` match its kind counts;
    ``tool_use`` / ``tool_result`` are the derived per-message-flag counts
    the Overview cards read). The daily-only fallback can't carry them, so
    it emits an empty dict (the UI reads each key with ``?? 0``).
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
            "message_types": {
                "user": int(project_mart_row.get("total_user_messages", 0) or 0),
                "assistant": int(
                    project_mart_row.get("total_assistant_messages", 0) or 0
                ),
                "tool_use": int(
                    project_mart_row.get("total_tool_use_messages", 0) or 0
                ),
                "tool_result": int(
                    project_mart_row.get("total_tool_result_messages", 0) or 0
                ),
            },
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
        # Message-type counts only live on ``project_mart`` (v022); the
        # daily-only path can't recover them, so emit an empty dict (UI
        # reads each key with ``?? 0``).
        "message_types": {},
    }


def daily_mart_by_day(
    rows: Sequence[dict[str, Any]],
) -> dict[str, dict[str, Any]]:
    """Group ``daily_mart`` rows by ``day`` → dict keyed by date.

    Output shape matches the legacy aggregator's ``_daily`` so the
    frontend's ``Record<string, DailyData>`` type contract holds:
    ``{messages, sessions, tokens: {input, output, cache_creation,
    cache_read}, cost: {total, by_model}, user_commands,
    interrupted_commands, interruption_rate, errors,
    assistant_messages, error_rate}``.
    """
    by_day: dict[str, dict[str, Any]] = {}
    for r in rows:
        day = r.get("day")
        if not day:
            continue
        bucket = by_day.setdefault(
            day,
            {
                "messages": 0,
                "sessions": 0,
                "tokens": {
                    "input": 0,
                    "output": 0,
                    "cache_creation": 0,
                    "cache_read": 0,
                },
                "cost": {"total": 0.0, "by_model": {}},
                "user_commands": 0,
                "interrupted_commands": 0,
                "interruption_rate": 0.0,
                "errors": 0,
                "assistant_messages": 0,
                "error_rate": 0.0,
            },
        )
        cost = float(r.get("cost_usd", 0.0) or 0.0)
        bucket["cost"]["total"] += cost
        bucket["tokens"]["input"] += int(r.get("input_tokens", 0) or 0)
        bucket["tokens"]["output"] += int(r.get("output_tokens", 0) or 0)
        bucket["tokens"]["cache_read"] += int(r.get("cache_read", 0) or 0)
        bucket["tokens"]["cache_creation"] += int(r.get("cache_create", 0) or 0)
        bucket["messages"] += int(r.get("message_count", 0) or 0)
        bucket["sessions"] += int(r.get("session_count", 0) or 0)
        model = r.get("model") or ""
        if model:
            existing = bucket["cost"]["by_model"].get(model, 0.0)
            bucket["cost"]["by_model"][model] = existing + cost
    return by_day


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


# ── Wave 4A — additional mart reads (compare/yield/optimize/messages-summary) ─


def mart_has_session_rows(conn: sqlite3.Connection) -> bool:
    """Return True iff ``session_mart`` has at least one row.

    Used by the global-scope route migrations (compare, optimize) where
    the mart populated/empty distinction is not project-scoped: if the
    ETL pipeline has ever run, every project's sessions are in the mart;
    if it hasn't, the table is empty and we fall back.
    """
    if not _table_exists(conn, "session_mart"):
        return False
    row = conn.execute("SELECT 1 FROM session_mart LIMIT 1").fetchone()
    return row is not None


def mart_has_model_day_rows(conn: sqlite3.Connection) -> bool:
    """Return True iff ``model_day_mart`` has at least one row.

    Used by ``services.compare`` to gate the model-rollup mart read. Pairs
    with ``mart_has_session_rows`` because compare needs both marts to be
    materialised to produce a full response.
    """
    if not _table_exists(conn, "model_day_mart"):
        return False
    row = conn.execute("SELECT 1 FROM model_day_mart LIMIT 1").fetchone()
    return row is not None


def _iso_to_day(iso_ts: str | None) -> str | None:
    """Extract ``YYYY-MM-DD`` from an ISO-8601 timestamp.

    Returns ``None`` on empty/invalid input so callers can pass through
    optional scope bounds without an extra guard. ``model_day_mart``
    keys on day strings, so we slice the leading 10 characters of the
    ISO timestamp — equivalent to ``date()`` in SQL but done host-side
    so the mart filter pushes a parametric ``BETWEEN`` rather than a
    function expression.
    """
    if not iso_ts or len(iso_ts) < 10:
        return None
    return iso_ts[:10]


# ── model_day_mart reads ────────────────────────────────────────────────────


def model_day_totals(
    conn: sqlite3.Connection,
    *,
    since_iso: str | None = None,
    until_iso: str | None = None,
) -> dict[str, dict[str, Any]]:
    """Aggregate ``model_day_mart`` rows into per-model totals.

    Sums across (day, speed) so the result is keyed by ``model`` only —
    the shape ``services.compare`` consumes for its per-model totals
    (``calls``, tokens, ``total_cost``). ``since_iso`` / ``until_iso``
    are ISO-8601 strings; we slice ``YYYY-MM-DD`` and push it down as
    a ``day BETWEEN ?`` filter so the index on ``model_day_mart`` does
    the work.
    """
    if not _table_exists(conn, "model_day_mart"):
        return {}
    sql = (
        "SELECT model, "
        "       SUM(cost_usd) AS cost_usd, "
        "       SUM(input_tokens) AS input_tokens, "
        "       SUM(output_tokens) AS output_tokens, "
        "       SUM(cache_read) AS cache_read, "
        "       SUM(cache_create) AS cache_create, "
        "       SUM(message_count) AS message_count "
        "FROM model_day_mart WHERE 1=1"
    )
    params: list[Any] = []
    day_from = _iso_to_day(since_iso)
    day_to = _iso_to_day(until_iso)
    if day_from:
        sql += " AND day >= ?"
        params.append(day_from)
    if day_to:
        sql += " AND day <= ?"
        params.append(day_to)
    sql += " GROUP BY model"
    out: dict[str, dict[str, Any]] = {}
    for row in conn.execute(sql, params).fetchall():
        model = row["model"] or ""
        if not model:
            continue
        out[model] = {
            "cost_usd": float(row["cost_usd"] or 0.0),
            "input_tokens": int(row["input_tokens"] or 0),
            "output_tokens": int(row["output_tokens"] or 0),
            "cache_read": int(row["cache_read"] or 0),
            "cache_create": int(row["cache_create"] or 0),
            "message_count": int(row["message_count"] or 0),
        }
    return out


def model_day_series(
    conn: sqlite3.Connection,
    *,
    since_iso: str | None = None,
    until_iso: str | None = None,
) -> list[dict[str, Any]]:
    """Per-(day, model) rows from ``model_day_mart`` for spend-over-time charts.

    Unlike :func:`model_day_totals` (which collapses the day axis to per-model
    totals), this keeps one row per ``(day, model)`` — summed across ``speed``
    — ordered by day, so a caller can build a per-model cost time series.
    ``since_iso`` / ``until_iso`` are ISO-8601 strings sliced to ``YYYY-MM-DD``
    and pushed down as a ``day`` range so the mart's index does the work.
    """
    if not _table_exists(conn, "model_day_mart"):
        return []
    sql = (
        "SELECT day, model, "
        "       SUM(cost_usd) AS cost_usd, "
        "       SUM(input_tokens) AS input_tokens, "
        "       SUM(output_tokens) AS output_tokens, "
        "       SUM(cache_read) AS cache_read, "
        "       SUM(cache_create) AS cache_create, "
        "       SUM(message_count) AS message_count, "
        "       SUM(session_count) AS session_count "
        "FROM model_day_mart WHERE 1=1"
    )
    params: list[Any] = []
    day_from = _iso_to_day(since_iso)
    day_to = _iso_to_day(until_iso)
    if day_from:
        sql += " AND day >= ?"
        params.append(day_from)
    if day_to:
        sql += " AND day <= ?"
        params.append(day_to)
    sql += " GROUP BY day, model ORDER BY day"
    out: list[dict[str, Any]] = []
    for row in conn.execute(sql, params).fetchall():
        model = row["model"] or ""
        if not model:
            continue
        out.append({
            "day": row["day"],
            "model": model,
            "cost_usd": float(row["cost_usd"] or 0.0),
            "input_tokens": int(row["input_tokens"] or 0),
            "output_tokens": int(row["output_tokens"] or 0),
            "cache_read": int(row["cache_read"] or 0),
            "cache_create": int(row["cache_create"] or 0),
            "message_count": int(row["message_count"] or 0),
            "session_count": int(row["session_count"] or 0),
        })
    return out


# ── session_mart reads ──────────────────────────────────────────────────────


def session_mart_rows_for_compare(
    conn: sqlite3.Connection,
    *,
    since_iso: str | None = None,
    until_iso: str | None = None,
    provider_filter: str | None = None,
) -> list[dict[str, Any]]:
    """Return per-session rows ``services.compare`` needs.

    Each row carries ``primary_model``, ``provider``, ``is_one_shot``,
    and ``assistant_message_count`` — enough to compute one-shot %,
    retry rate, and per-session cost attribution. Filter is keyed on
    ``first_ts`` (mart's session start time, ISO-8601) and on the
    optional single-string ``provider_filter`` (matches the existing
    ``compare_models`` argument shape).
    """
    if not _table_exists(conn, "session_mart"):
        return []
    sql = (
        "SELECT session_id, project_id, provider, primary_model, "
        "       first_ts, last_ts, "
        "       message_count, user_message_count, assistant_message_count, "
        "       input_tokens, output_tokens, cache_read, cache_create, "
        "       cost_usd, is_one_shot, cwd "
        "FROM session_mart WHERE 1=1"
    )
    params: list[Any] = []
    if since_iso:
        sql += " AND first_ts >= ?"
        params.append(since_iso)
    if until_iso:
        sql += " AND first_ts <= ?"
        params.append(until_iso)
    if provider_filter:
        sql += " AND LOWER(provider) = ?"
        params.append(provider_filter.lower())
    return [dict(r) for r in conn.execute(sql, params).fetchall()]


def session_mart_rows_for_yield(
    conn: sqlite3.Connection,
    *,
    since_iso: str | None = None,
    until_iso: str | None = None,
    project_slugs: list[str] | None = None,
) -> list[dict[str, Any]]:
    """Return per-session rows ``services.yield_tracker`` needs.

    Joins ``session_mart`` with ``projects`` to surface the project slug
    (yield's project filter speaks slugs, mart speaks ``project_id``).
    Sessions are ordered by ``first_ts`` so the caller's chronological
    iteration over the result is preserved.
    """
    if not _table_exists(conn, "session_mart"):
        return []
    # Join the raw ``sessions`` row in too — yield's cwd lookup needs
    # the integer ``session_fk`` to query ``messages.raw_json`` (cwd is
    # not yet materialised on ``session_mart`` per the v1 spec note).
    sql = (
        "SELECT m.session_id AS session_id, "
        "       p.slug AS project_slug, "
        "       p.provider AS provider, "
        "       m.project_id AS project_id, "
        "       m.first_ts AS first_ts, "
        "       m.primary_model AS primary_model, "
        "       m.cost_usd AS cost_usd, "
        "       sess.id AS session_fk "
        "FROM session_mart m "
        "JOIN projects p ON p.id = m.project_id "
        "LEFT JOIN sessions sess "
        "       ON sess.project_id = m.project_id "
        "      AND sess.session_id = m.session_id "
        "WHERE m.first_ts IS NOT NULL"
    )
    params: list[Any] = []
    if since_iso:
        sql += " AND m.first_ts >= ?"
        params.append(since_iso)
    if until_iso:
        sql += " AND m.first_ts <= ?"
        params.append(until_iso)
    if project_slugs:
        placeholders = ",".join("?" for _ in project_slugs)
        sql += f" AND p.slug IN ({placeholders})"
        params.extend(project_slugs)
    sql += " ORDER BY m.first_ts"
    return [dict(r) for r in conn.execute(sql, params).fetchall()]


def session_mart_cache_overhead(
    conn: sqlite3.Connection,
    *,
    since_iso: str | None = None,
    until_iso: str | None = None,
    ratio_threshold: float,
) -> list[dict[str, Any]]:
    """Return per-session cache-overhead candidates from ``session_mart``.

    Mirrors the legacy ``GROUP BY session_fk`` pass in
    ``reports/optimize._detect_cache_overhead`` but reads from the
    materialised ``session_mart`` rows: ``input_tokens`` and
    ``cache_create`` are pre-summed so the only work left is the ratio
    test. Returns rows shaped to feed straight into the detector's
    finding payload.
    """
    if not _table_exists(conn, "session_mart"):
        return []
    sql = (
        "SELECT session_id, project_id, "
        "       input_tokens AS inp, cache_create AS cache_create "
        "FROM session_mart WHERE 1=1"
    )
    params: list[Any] = []
    if since_iso:
        sql += " AND first_ts >= ?"
        params.append(since_iso)
    if until_iso:
        sql += " AND first_ts <= ?"
        params.append(until_iso)
    bad: list[dict[str, Any]] = []
    for row in conn.execute(sql, params).fetchall():
        inp = int(row["inp"] or 0)
        cache = int(row["cache_create"] or 0)
        if inp == 0 or cache == 0:
            continue
        total_input = inp + cache
        if total_input == 0:
            continue
        ratio = cache / total_input
        if ratio > ratio_threshold:
            bad.append(
                {
                    "session_id": row["session_id"],
                    "project_id": row["project_id"],
                    "cache_create_tokens": cache,
                    "input_tokens": inp,
                    "ratio": round(ratio, 3),
                }
            )
    return bad


# ── tool_mart reads (Wave 5) ────────────────────────────────────────────────


def mart_has_tool_rows(conn: sqlite3.Connection) -> bool:
    """Return True iff ``tool_mart`` has at least one row.

    Same gate pattern as ``mart_has_session_rows`` — used by the
    optimize-pattern detectors to decide whether they can short-circuit
    on empty ``tool_mart`` (no rows ≡ no events, so no findings) or
    must fall through to the aggregator path.
    """
    if not _table_exists(conn, "tool_mart"):
        return False
    row = conn.execute("SELECT 1 FROM tool_mart LIMIT 1").fetchone()
    return row is not None


# Columns ``tool_call_count_in_window`` is allowed to sum. Whitelisted so
# the column name can be interpolated into the SQL skeleton without
# becoming an injection vector.
_TOOL_COUNT_COLUMNS = frozenset({"event_count", "calls_total"})


def tool_call_count_in_window(
    conn: sqlite3.Connection,
    *,
    tool_names: Sequence[str],
    since_iso: str | None = None,
    until_iso: str | None = None,
    project_filter: Sequence[str] | None = None,
    count_column: str = "event_count",
) -> int:
    """SUM of a ``tool_mart`` count column for the named tools in a day window.

    ``count_column`` selects which measure to sum:

    * ``event_count`` (default) — distinct ``(message, tool)`` pairs;
      the "did anyone use this tool" signal.
    * ``calls_total`` — total tool occurrences (a turn that called Read
      3× counts 3); matches the legacy aggregator's ``calls`` semantics.
      Note: on a ``tool_mart`` that predates v012 this column is
      all-zero until a ``--force`` rebuild — callers using it as a
      ``== 0`` short-circuit accept that transient (a stale-zero just
      means "fall through to the full scan", a conservative miss).

    ``tool_names`` is a non-empty sequence (we always pass at least one
    name); empty would match nothing. ``since_iso`` / ``until_iso`` are
    ISO-8601 timestamps — we slice to ``YYYY-MM-DD`` so the index on
    ``tool_mart(tool_name, day)`` does the work.

    Returns ``0`` when the mart is empty or no rows match. Used by the
    optimize detectors as a pre-flight check: "did anyone use this tool
    in window?". When the answer is 0, the detector emits no findings
    and skips the expensive raw-messages scan.

    ``project_filter`` accepts a list of project slugs the route layer
    has narrowed to. When provided, we JOIN ``projects`` so the count
    only spans the requested projects.
    """
    if not tool_names:
        return 0
    if not _table_exists(conn, "tool_mart"):
        return 0
    if count_column not in _TOOL_COUNT_COLUMNS:
        raise ValueError(
            f"count_column must be one of {sorted(_TOOL_COUNT_COLUMNS)}, "
            f"got {count_column!r}"
        )
    # ``placeholders`` is a fixed-length string of ``?`` separators;
    # values are bound parametrically below. ``count_column`` is checked
    # against the whitelist above — no user input lands in the SQL
    # skeleton.
    placeholders = ",".join("?" * len(tool_names))
    sql = (
        f"SELECT COALESCE(SUM({count_column}), 0) AS c "  # noqa: S608
        f"FROM tool_mart WHERE tool_name IN ({placeholders})"
    )
    params: list[Any] = list(tool_names)
    day_from = _iso_to_day(since_iso)
    day_to = _iso_to_day(until_iso)
    if day_from:
        sql += " AND day >= ?"
        params.append(day_from)
    if day_to:
        sql += " AND day <= ?"
        params.append(day_to)
    if project_filter:
        slugs = [s for s in project_filter if s]
        if slugs:
            sql = (
                f"SELECT COALESCE(SUM(t.{count_column}), 0) AS c "  # noqa: S608
                f"FROM tool_mart t "
                f"JOIN projects p ON p.id = t.project_id "
                f"WHERE t.tool_name IN ({placeholders}) "
                f"AND p.slug IN ({','.join('?' * len(slugs))})"
            )
            params = list(tool_names) + slugs
            if day_from:
                sql += " AND t.day >= ?"
                params.append(day_from)
            if day_to:
                sql += " AND t.day <= ?"
                params.append(day_to)
    row = conn.execute(sql, params).fetchone()
    if row is None:
        return 0
    val = row["c"] if hasattr(row, "keys") else row[0]
    return int(val or 0)


def tool_mart_for_project(
    conn: sqlite3.Connection,
    *,
    project_id: int,
    day_from: str | None = None,
    day_to: str | None = None,
) -> dict[str, dict[str, Any]]:
    """Per-project per-tool rollup keyed by ``tool_name``.

    Each value is ``{calls, calls_total, cost, tokens_in, tokens_out,
    cache_read_tokens, cache_creation_tokens, sessions}``.

    Powers the ``/api/cost-data`` ``tool_costs`` block when the mart is
    populated. Aggregates across all (day, provider) combos within the
    window for the requested project, since the legacy aggregator
    output keys only on tool_name.

    ``calls`` is the distinct ``(message, tool)`` pair count
    (``SUM(event_count)`` — the 1/N attribution unit); ``calls_total``
    is the non-distinct occurrence count (``SUM(calls_total)``, added in
    v012). On a store whose ``tool_mart`` predates v012 the ``calls_total``
    column is all-zero until a ``--force`` rebuild — the value just
    mirrors ``calls`` as a floor in that transient state would be nicer
    but isn't worth a CASE; consumers that care should treat ``0`` as
    "not yet rebuilt".

    ``cache_read_tokens`` / ``cache_creation_tokens`` (``SUM`` of the v023
    ``cache_read`` / ``cache_create`` columns) carry the 1/N-attributed
    prompt-cache tokens — they're keyed with the aggregator's
    ``_ToolCostCollector`` field names so the ToolCost block can surface a
    non-zero per-tool cache cost (ui-perf #20). Pre-v023 ``tool_mart`` rows
    read 0 here until a ``--force`` rebuild re-derives them.
    """
    if not _table_exists(conn, "tool_mart"):
        return {}
    sql = (
        "SELECT tool_name, "
        "       SUM(event_count) AS calls, "
        "       SUM(calls_total) AS calls_total, "
        "       SUM(cost_usd) AS cost, "
        "       SUM(tokens_in) AS tokens_in, "
        "       SUM(tokens_out) AS tokens_out, "
        "       SUM(cache_read) AS cache_read_tokens, "
        "       SUM(cache_create) AS cache_creation_tokens, "
        "       MAX(session_count) AS sessions "
        "FROM tool_mart WHERE project_id = ?"
    )
    params: list[Any] = [project_id]
    if day_from:
        sql += " AND day >= ?"
        params.append(day_from)
    if day_to:
        sql += " AND day <= ?"
        params.append(day_to)
    sql += " GROUP BY tool_name"
    out: dict[str, dict[str, Any]] = {}
    for row in conn.execute(sql, params).fetchall():
        name = row["tool_name"] or ""
        if not name:
            continue
        out[name] = {
            "calls": int(row["calls"] or 0),
            "calls_total": int(row["calls_total"] or 0),
            "cost": float(row["cost"] or 0.0),
            "tokens_in": int(row["tokens_in"] or 0),
            "tokens_out": int(row["tokens_out"] or 0),
            "cache_read_tokens": int(row["cache_read_tokens"] or 0),
            "cache_creation_tokens": int(row["cache_creation_tokens"] or 0),
            "sessions": int(row["sessions"] or 0),
        }
    return out


def tool_mart_calls_distribution(
    conn: sqlite3.Connection,
    project_id: int,
    *,
    since: str | None = None,
) -> list[dict[str, Any]]:
    """Per-tool-name distribution for one project, sorted by total calls desc.

    Each row: ``{tool_name, distinct_messages, total_calls, cost_usd}``
    where

    * ``distinct_messages`` — ``SUM(event_count)``: how many assistant
      turns invoked this tool (the 1/N-attribution unit).
    * ``total_calls``       — ``SUM(calls_total)``: how many times the
      tool was actually invoked (a turn that called Read 3× counts 3).
    * ``cost_usd``          — ``SUM(cost_usd)``: the 1/N-attributed cost.

    ``since`` is an inclusive ``YYYY-MM-DD`` lower bound on the mart's
    ``day`` column (the caller slices any timezone offset before
    passing). Returns ``[]`` when ``tool_mart`` doesn't exist or has no
    matching rows.
    """
    if not _table_exists(conn, "tool_mart"):
        return []
    sql = (
        "SELECT tool_name, "
        "       SUM(event_count) AS distinct_messages, "
        "       SUM(calls_total) AS total_calls, "
        "       SUM(cost_usd) AS cost_usd "
        "FROM tool_mart WHERE project_id = ?"
    )
    params: list[Any] = [project_id]
    if since:
        sql += " AND day >= ?"
        params.append(since)
    sql += " GROUP BY tool_name ORDER BY SUM(calls_total) DESC, tool_name"
    out: list[dict[str, Any]] = []
    for row in conn.execute(sql, params).fetchall():
        name = row["tool_name"] or ""
        if not name:
            continue
        out.append(
            {
                "tool_name": name,
                "distinct_messages": int(row["distinct_messages"] or 0),
                "total_calls": int(row["total_calls"] or 0),
                "cost_usd": float(row["cost_usd"] or 0.0),
            }
        )
    return out


# ── command_mart reads (Wave 5) ─────────────────────────────────────────────


def mart_has_command_rows(conn: sqlite3.Connection) -> bool:
    """Return True iff ``command_mart`` has at least one row."""
    if not _table_exists(conn, "command_mart"):
        return False
    row = conn.execute("SELECT 1 FROM command_mart LIMIT 1").fetchone()
    return row is not None


def command_mart_for_project(
    conn: sqlite3.Connection,
    *,
    project_id: int,
    day_from: str | None = None,
    day_to: str | None = None,
) -> list[dict[str, Any]]:
    """Return per-command rollup rows for one project, sorted cost desc.

    Each row: ``{command_name, event_count, cost_usd, tokens_in,
    tokens_out, session_count}``. Powers the per-command rollup
    consumed by ``/api/cost-data`` and the optimize-pattern early-exit
    checks.
    """
    if not _table_exists(conn, "command_mart"):
        return []
    sql = (
        "SELECT command_name, "
        "       SUM(event_count) AS event_count, "
        "       SUM(cost_usd) AS cost_usd, "
        "       SUM(tokens_in) AS tokens_in, "
        "       SUM(tokens_out) AS tokens_out, "
        "       MAX(session_count) AS session_count "
        "FROM command_mart WHERE project_id = ?"
    )
    params: list[Any] = [project_id]
    if day_from:
        sql += " AND day >= ?"
        params.append(day_from)
    if day_to:
        sql += " AND day <= ?"
        params.append(day_to)
    sql += " GROUP BY command_name ORDER BY SUM(cost_usd) DESC"
    return [
        {
            "command_name": r["command_name"] or "",
            "event_count": int(r["event_count"] or 0),
            "cost_usd": float(r["cost_usd"] or 0.0),
            "tokens_in": int(r["tokens_in"] or 0),
            "tokens_out": int(r["tokens_out"] or 0),
            "session_count": int(r["session_count"] or 0),
        }
        for r in conn.execute(sql, params).fetchall()
    ]


# ── command_day_mart reads (per-(day, project) user-command count, v025) ─────
#
# ``command_day_mart`` materialises the windowed Commands KPI (#25): one row per
# (day, project_id) carrying the count of real user command turns — the SAME
# tally ``project_mart.total_commands`` reports lifetime, just bucketed by day.
# Summing ``command_count`` across a project's ids in a day window gives the
# windowed ``user_commands_analyzed``; summing every row gives the lifetime
# total. ``day_from`` / ``day_to`` are inclusive ISO ``YYYY-MM-DD`` strings
# (caller slices any timezone offset before passing — marts store UTC days).


def mart_has_command_day_rows(conn: sqlite3.Connection) -> bool:
    """Return True iff ``command_day_mart`` exists and has ≥1 row.

    The gate the read path uses to decide whether the windowed Commands KPI can
    be sourced from the mart. False (table absent or empty — a store that
    hasn't run the v025 backfill yet) means the caller keeps the lifetime
    ``project_mart.total_commands`` fallback so the KPI never blanks.
    """
    if not _table_exists(conn, "command_day_mart"):
        return False
    return conn.execute("SELECT 1 FROM command_day_mart LIMIT 1").fetchone() is not None


def command_count_in_window(
    conn: sqlite3.Connection,
    *,
    project_ids: Sequence[int],
    day_from: str | None = None,
    day_to: str | None = None,
) -> int:
    """SUM ``command_day_mart.command_count`` for *project_ids* in a day window.

    The windowed analogue of ``project_mart.total_commands`` — one indexed
    aggregate over the tiny per-(day, project) table. Empty ``project_ids`` or
    a missing table returns 0. With no day bounds it equals the lifetime
    command total for those projects (used by the dashboard fast-path).
    """
    pids = [int(p) for p in project_ids]
    if not pids or not _table_exists(conn, "command_day_mart"):
        return 0
    placeholders = ",".join("?" * len(pids))
    sql = (
        f"SELECT COALESCE(SUM(command_count), 0) AS c "  # noqa: S608 — placeholders are bound
        f"FROM command_day_mart WHERE project_id IN ({placeholders})"
    )
    params: list[Any] = list(pids)
    if day_from:
        sql += " AND day >= ?"
        params.append(day_from)
    if day_to:
        sql += " AND day <= ?"
        params.append(day_to)
    row = conn.execute(sql, params).fetchone()
    if row is None:
        return 0
    val = row["c"] if hasattr(row, "keys") else row[0]
    return int(val or 0)


def command_day_series(
    conn: sqlite3.Connection,
    *,
    project_ids: Sequence[int] | None = None,
    day_from: str | None = None,
    day_to: str | None = None,
) -> list[dict[str, Any]]:
    """Per-day command counts as ``[{date, commands}]``, oldest day first.

    Powers the Overview "Commands" KPI's window-aware sum (#25): the frontend
    sums ``commands`` over the days inside its selected date range, exactly as
    it already sums ``daily_token_usage`` / ``daily_costs``. ``project_ids`` is
    ``None`` for the cross-project (global Overview) view or a list to scope to
    one project's ids; counts are summed across projects per day. Empty mart →
    ``[]`` (the caller falls back to the lifetime total).
    """
    if not _table_exists(conn, "command_day_mart"):
        return []
    sql = "SELECT day, SUM(command_count) AS commands FROM command_day_mart WHERE 1=1"
    params: list[Any] = []
    if project_ids is not None:
        pids = [int(p) for p in project_ids]
        if not pids:
            return []
        placeholders = ",".join("?" * len(pids))
        sql += f" AND project_id IN ({placeholders})"  # noqa: S608 — placeholders are bound
        params.extend(pids)
    if day_from:
        sql += " AND day >= ?"
        params.append(day_from)
    if day_to:
        sql += " AND day <= ?"
        params.append(day_to)
    sql += " GROUP BY day ORDER BY day"
    return [
        {"date": r["day"], "commands": int(r["commands"] or 0)}
        for r in conn.execute(sql, params).fetchall()
        if r["day"]
    ]


# ── message_tool_mart reads (per-message-grain mart, v011) ──────────────────
#
# These power the ``reports/optimize.py`` detectors that used to re-parse
# ``messages.raw_json`` on every call. ``since_iso`` / ``until_iso`` are
# ISO-8601 timestamps; we slice ``YYYY-MM-DD`` and push it down as a
# ``day BETWEEN`` filter so the ``(tool_name, day)`` / ``(project_id, day)``
# indexes do the work. ``project_slugs`` narrows to specific projects by
# JOINing ``projects`` (the detectors speak slugs; the mart speaks
# ``project_id``). Empty/``None`` filters == "all".


def mart_has_message_tool_rows(conn: sqlite3.Connection) -> bool:
    """Return True iff ``message_tool_mart`` has at least one row.

    The "is the per-message mart materialised?" gate for the optimize
    detectors — when True they read tool-call detail straight off the
    mart; when False they fall through to the raw ``messages`` scan so
    the empty-mart / fresh-install path keeps working unchanged.
    """
    if not _table_exists(conn, "message_tool_mart"):
        return False
    row = conn.execute("SELECT 1 FROM message_tool_mart LIMIT 1").fetchone()
    return row is not None


def _norm_slugs(project_slugs: Sequence[str] | None) -> list[str]:
    """Drop falsy entries from a slug filter (mirrors the route-layer norm)."""
    return [s for s in (project_slugs or []) if s]


def message_tool_junk_reads(
    conn: sqlite3.Connection,
    *,
    repeat_threshold: int,
    since_iso: str | None = None,
    until_iso: str | None = None,
    project_slugs: Sequence[str] | None = None,
) -> list[dict[str, Any]]:
    """Per-(session, file) Read counts that meet ``repeat_threshold``.

    Returns ``[{session_id, file_path, reads}, ...]`` for files Read at
    least ``repeat_threshold`` times within one session in the window —
    the signal ``optimize._detect_junk_reads`` groups by session into a
    finding. The aggregator-path equivalent is the per-session
    ``per_path`` Counter over ``raw_json``; this is one indexed
    ``GROUP BY ... HAVING``.
    """
    if not _table_exists(conn, "message_tool_mart"):
        return []
    slugs = _norm_slugs(project_slugs)
    # `join` is one of two fixed literals; values are bound parametrically below.
    join = " JOIN projects p ON p.id = mt.project_id" if slugs else ""
    sql = (
        "SELECT mt.session_id AS session_id, mt.file_path AS file_path, "  # noqa: S608
        "       COUNT(*) AS reads "
        f"FROM message_tool_mart mt{join} "
        "WHERE mt.tool_name = 'Read' AND mt.file_path IS NOT NULL"
    )
    params: list[Any] = []
    if slugs:
        sql += f" AND p.slug IN ({','.join('?' * len(slugs))})"  # noqa: S608
        params.extend(slugs)
    sql, params = _push_day_window(sql, params, since_iso, until_iso, col="mt.day")
    sql += " GROUP BY mt.session_id, mt.file_path HAVING COUNT(*) >= ?"
    params.append(int(repeat_threshold))
    return [
        {
            "session_id": r["session_id"],
            "file_path": r["file_path"],
            "reads": int(r["reads"] or 0),
        }
        for r in conn.execute(sql, params).fetchall()
    ]


def message_tool_read_edit_per_session(
    conn: sqlite3.Connection,
    *,
    since_iso: str | None = None,
    until_iso: str | None = None,
    project_slugs: Sequence[str] | None = None,
) -> list[dict[str, Any]]:
    """Per-session ``{session_id, reads, edits}`` counts in the window.

    ``reads`` counts ``Read`` calls; ``edits`` counts the write family
    (``Edit`` / ``Write`` / ``MultiEdit`` / ``NotebookEdit``). Feeds
    ``optimize._detect_low_read_edit_ratio`` — it flags sessions with
    ``reads >= floor AND edits == 0``.
    """
    if not _table_exists(conn, "message_tool_mart"):
        return []
    slugs = _norm_slugs(project_slugs)
    # `join` is one of two fixed literals; values are bound parametrically below.
    join = " JOIN projects p ON p.id = mt.project_id" if slugs else ""
    sql = (
        "SELECT mt.session_id AS session_id, "  # noqa: S608
        "       SUM(CASE WHEN mt.tool_name = 'Read' THEN 1 ELSE 0 END) AS reads, "
        "       SUM(CASE WHEN mt.tool_name IN "
        "           ('Edit', 'Write', 'MultiEdit', 'NotebookEdit') THEN 1 ELSE 0 END) AS edits "
        f"FROM message_tool_mart mt{join} WHERE 1 = 1"
    )
    params: list[Any] = []
    if slugs:
        sql += f" AND p.slug IN ({','.join('?' * len(slugs))})"  # noqa: S608
        params.extend(slugs)
    sql, params = _push_day_window(sql, params, since_iso, until_iso, col="mt.day")
    sql += " GROUP BY mt.session_id"
    return [
        {
            "session_id": r["session_id"],
            "reads": int(r["reads"] or 0),
            "edits": int(r["edits"] or 0),
        }
        for r in conn.execute(sql, params).fetchall()
    ]


def message_tool_oversized(
    conn: sqlite3.Connection,
    *,
    tool_name: str,
    threshold_bytes: int,
    since_iso: str | None = None,
    until_iso: str | None = None,
    project_slugs: Sequence[str] | None = None,
) -> list[dict[str, Any]]:
    """Mart rows for *tool_name* whose ``byte_count`` exceeds *threshold_bytes*.

    Returns ``[{session_id, message_id, byte_count}, ...]`` sorted by
    ``byte_count`` desc. Feeds ``optimize._detect_bash_output_limits``
    (``tool_name='Bash'``) — the mart already carries the tool-result
    size (paired off the following ``tool_result`` block), so the
    detector's two-pass raw scan collapses to this one query.
    """
    if not _table_exists(conn, "message_tool_mart"):
        return []
    slugs = _norm_slugs(project_slugs)
    # `join` is one of two fixed literals; values are bound parametrically below.
    join = " JOIN projects p ON p.id = mt.project_id" if slugs else ""
    sql = (
        "SELECT mt.session_id AS session_id, mt.message_id AS message_id, "  # noqa: S608
        "       mt.byte_count AS byte_count "
        f"FROM message_tool_mart mt{join} "
        "WHERE mt.tool_name = ? AND mt.byte_count IS NOT NULL AND mt.byte_count > ?"
    )
    params: list[Any] = [tool_name, int(threshold_bytes)]
    if slugs:
        sql += f" AND p.slug IN ({','.join('?' * len(slugs))})"  # noqa: S608
        params.extend(slugs)
    sql, params = _push_day_window(sql, params, since_iso, until_iso, col="mt.day")
    sql += " ORDER BY mt.byte_count DESC"
    return [
        {
            "session_id": r["session_id"],
            "message_id": int(r["message_id"]),
            "byte_count": int(r["byte_count"] or 0),
        }
        for r in conn.execute(sql, params).fetchall()
    ]


def message_tool_invoked_agents(
    conn: sqlite3.Connection,
    *,
    since_iso: str | None = None,
    until_iso: str | None = None,
) -> set[str]:
    """Distinct subagent names spawned via ``Task`` in the window.

    The mart stores the ``subagent_type`` of each ``Task`` call in
    ``file_path``, so this is a single ``SELECT DISTINCT`` — replacing
    ``optimize._detect_ghost_agents``'s ``subagent_type=...`` string
    match against every ``messages.raw_json``. ``ghost = registered −
    this set``.
    """
    if not _table_exists(conn, "message_tool_mart"):
        return set()
    sql = (
        "SELECT DISTINCT file_path FROM message_tool_mart "
        "WHERE tool_name = 'Task' AND file_path IS NOT NULL"
    )
    params: list[Any] = []
    sql, params = _push_day_window(sql, params, since_iso, until_iso, col="day")
    return {
        r["file_path"]
        for r in conn.execute(sql, params).fetchall()
        if r["file_path"]
    }


def message_tool_for_project(
    conn: sqlite3.Connection,
    *,
    project_id: int,
    tool_name: str | None = None,
    day_from: str | None = None,
    day_to: str | None = None,
    limit: int | None = None,
) -> list[dict[str, Any]]:
    """Raw ``message_tool_mart`` rows for one project, newest day first.

    A general-purpose reader for future consumers (outcome-aware
    discovery, auto-skill synthesis) that want per-tool-call detail
    without re-deriving it from ``tools_json``. Optionally narrowed to a
    single ``tool_name`` and/or a day window; ``limit`` caps the result.
    """
    if not _table_exists(conn, "message_tool_mart"):
        return []
    sql = (
        "SELECT message_id, project_id, session_id, ts, day, "
        "       tool_name, file_path, byte_count, call_index "
        "FROM message_tool_mart WHERE project_id = ?"
    )
    params: list[Any] = [project_id]
    if tool_name:
        sql += " AND tool_name = ?"
        params.append(tool_name)
    if day_from:
        sql += " AND day >= ?"
        params.append(day_from)
    if day_to:
        sql += " AND day <= ?"
        params.append(day_to)
    sql += " ORDER BY day DESC, message_id DESC, tool_name, call_index"
    if limit is not None and limit > 0:
        sql += " LIMIT ?"
        params.append(int(limit))
    return [dict(r) for r in conn.execute(sql, params).fetchall()]


def tool_call_byte_dist_for_project(
    conn: sqlite3.Connection,
    *,
    project_id: int,
    day_from: str | None = None,
    day_to: str | None = None,
) -> dict[str, dict[str, int]]:
    """Per-tool byte-size distribution for one project.

    Returns ``{tool_name: {calls, calls_with_bytes, total_bytes,
    max_bytes}}`` — a cheap shape for "which tools produce the biggest
    payloads in this project" UI / report rollups.
    """
    if not _table_exists(conn, "message_tool_mart"):
        return {}
    sql = (
        "SELECT tool_name, "
        "       COUNT(*) AS calls, "
        "       COUNT(byte_count) AS calls_with_bytes, "
        "       COALESCE(SUM(byte_count), 0) AS total_bytes, "
        "       COALESCE(MAX(byte_count), 0) AS max_bytes "
        "FROM message_tool_mart WHERE project_id = ?"
    )
    params: list[Any] = [project_id]
    if day_from:
        sql += " AND day >= ?"
        params.append(day_from)
    if day_to:
        sql += " AND day <= ?"
        params.append(day_to)
    sql += " GROUP BY tool_name"
    out: dict[str, dict[str, int]] = {}
    for r in conn.execute(sql, params).fetchall():
        name = r["tool_name"] or ""
        if not name:
            continue
        out[name] = {
            "calls": int(r["calls"] or 0),
            "calls_with_bytes": int(r["calls_with_bytes"] or 0),
            "total_bytes": int(r["total_bytes"] or 0),
            "max_bytes": int(r["max_bytes"] or 0),
        }
    return out


def _push_day_window(
    sql: str,
    params: list[Any],
    since_iso: str | None,
    until_iso: str | None,
    *,
    col: str,
) -> tuple[str, list[Any]]:
    """Append an inclusive ``[since, until]`` day filter on *col* to *sql*.

    ``since_iso`` / ``until_iso`` are ISO-8601 timestamps; we slice the
    leading ``YYYY-MM-DD`` so the day-keyed indexes drive the scan.
    *col* is always a hardcoded column name from this module (``"day"``
    or ``"mt.day"``), never user input. Returns ``(sql, params)`` with
    *params* extended in place.
    """
    day_from = _iso_to_day(since_iso)
    day_to = _iso_to_day(until_iso)
    if day_from:
        sql += f" AND {col} >= ?"  # noqa: S608 — `col` is a module-local literal
        params.append(day_from)
    if day_to:
        sql += f" AND {col} <= ?"  # noqa: S608 — `col` is a module-local literal
        params.append(day_to)
    return sql, params


def tool_mart_distinct_tool_names_in_window(
    conn: sqlite3.Connection,
    *,
    since_iso: str | None = None,
    until_iso: str | None = None,
    name_prefix: str | None = None,
) -> list[str]:
    """Return distinct ``tool_name`` values from ``tool_mart`` in a day window.

    Empty mart → empty list (caller falls back to the raw-messages scan).
    The ``name_prefix`` filter is bound parametrically as an SQL LIKE
    pattern (caller passes ``"mcp__"`` to get only MCP tool calls — the
    convention is ``mcp__<server>__<tool>``).

    Used by ``optimize._detect_unused_mcp_servers`` to skip the ~1.3s
    ``tools_json`` parse on stores with a populated ``tool_mart``.
    """
    if not _table_exists(conn, "tool_mart"):
        return []
    sql = "SELECT DISTINCT tool_name FROM tool_mart WHERE 1 = 1"
    params: list[Any] = []
    if name_prefix:
        sql += " AND tool_name LIKE ?"
        params.append(f"{name_prefix}%")
    sql, params = _push_day_window(sql, params, since_iso, until_iso, col="day")
    return [r[0] for r in conn.execute(sql, params).fetchall() if r[0]]
