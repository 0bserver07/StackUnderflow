"""Typed query helpers.

All SQL the app runs against the store lives here. Callers import
helpers, not raw SQL. If a helper gets hot enough to warrant caching
later, it can add an @lru_cache without changing any call site.
"""

from __future__ import annotations

import sqlite3

from .types import MessageRow, ProjectRow, SessionRow


def list_projects(conn: sqlite3.Connection) -> list[ProjectRow]:
    rows = conn.execute(
        "SELECT id, provider, slug, path, display_name, first_seen, last_modified "
        "FROM projects ORDER BY last_modified DESC"
    ).fetchall()
    return [ProjectRow(**dict(r)) for r in rows]


def bulk_session_counts(conn: sqlite3.Connection) -> dict[int, int]:
    """Return ``{project_id: session_count}`` in one query.

    Replaces N+1 ``list_sessions(conn, project_id=…)`` loops over the
    full project list. ~30ms for ~1000 sessions vs N×10ms otherwise.
    """
    rows = conn.execute(
        "SELECT project_id, COUNT(*) FROM sessions GROUP BY project_id"
    ).fetchall()
    return {int(r[0]): int(r[1]) for r in rows}


def bulk_project_lite_stats(conn: sqlite3.Connection) -> dict[int, dict]:
    """Return ``{project_id: {tokens, cost-driving counts, dates}}`` in one query.

    Lite stats fill the project-list cards on the dashboard without
    running the per-project aggregator pipeline (which is single-pass
    over every message and takes ~100ms × N projects on real data).
    Fields not derivable from a single GROUP BY (avg_steps_per_command,
    compact_summary_count, etc.) default to 0/None so the UI shape stays
    backwards-compatible — those are only meaningful in the per-project
    detail view, which still runs the full aggregator on demand.
    """
    rows = conn.execute(
        "SELECT s.project_id, "
        "       SUM(m.input_tokens), SUM(m.output_tokens), "
        "       SUM(m.cache_read_tokens), SUM(m.cache_create_tokens), "
        "       MIN(m.timestamp), MAX(m.timestamp), "
        "       SUM(CASE WHEN m.role = 'user' THEN 1 ELSE 0 END) AS user_msgs, "
        "       COUNT(*) AS total_msgs "
        "FROM messages m "
        "JOIN sessions s ON s.id = m.session_fk "
        "WHERE m.model IS NULL OR m.model != '<synthetic>' "
        "GROUP BY s.project_id"
    ).fetchall()
    out: dict[int, dict] = {}
    for r in rows:
        pid = int(r[0])
        out[pid] = {
            "total_input_tokens": int(r[1] or 0),
            "total_output_tokens": int(r[2] or 0),
            "total_cache_read": int(r[3] or 0),
            "total_cache_write": int(r[4] or 0),
            "first_message_date": r[5],
            "last_message_date": r[6],
            "total_commands": int(r[7] or 0),
            "total_messages": int(r[8] or 0),
            # Filled by route layer using the cost helpers + currency
            "total_cost": 0.0,
            # Aggregator-only fields default to 0/None for the list view
            "avg_tokens_per_command": 0,
            "avg_steps_per_command": 0,
            "compact_summary_count": 0,
        }
    return out


def bulk_project_cost(conn: sqlite3.Connection) -> dict[int, float]:
    """Return ``{project_id: total_cost_usd}`` keyed by aggregated tokens
    × ``compute_cost`` per (model, speed) bucket.

    One pass: gather per-(project_id, model, speed) totals, fold to USD
    using the current rate card. Replaces the per-project aggregator
    pipeline for the project-list view's cost field.
    """
    from stackunderflow.infra.costs import compute_cost

    rows = conn.execute(
        "SELECT s.project_id, "
        "       COALESCE(m.model, ''), "
        "       COALESCE(m.speed, 'standard'), "
        "       SUM(m.input_tokens), SUM(m.output_tokens), "
        "       SUM(m.cache_read_tokens), SUM(m.cache_create_tokens) "
        "FROM messages m "
        "JOIN sessions s ON s.id = m.session_fk "
        "WHERE m.model IS NOT NULL AND m.model != '' AND m.model != '<synthetic>' "
        "GROUP BY s.project_id, m.model, m.speed"
    ).fetchall()
    cost_by_pid: dict[int, float] = {}
    for r in rows:
        pid = int(r[0])
        model = r[1] or ""
        speed = r[2] or "standard"
        tokens = {
            "input": int(r[3] or 0),
            "output": int(r[4] or 0),
            "cache_read": int(r[5] or 0),
            "cache_creation": int(r[6] or 0),
        }
        breakdown = compute_cost(tokens, model, speed=speed) if model else None
        usd = float(breakdown["total_cost"]) if breakdown else 0.0
        cost_by_pid[pid] = cost_by_pid.get(pid, 0.0) + usd
    return cost_by_pid


def get_project(conn: sqlite3.Connection, *, slug: str) -> ProjectRow | None:
    row = conn.execute(
        "SELECT id, provider, slug, path, display_name, first_seen, last_modified "
        "FROM projects WHERE slug = ?",
        (slug,),
    ).fetchone()
    return ProjectRow(**dict(row)) if row else None


def list_sessions(conn: sqlite3.Connection, *, project_id: int) -> list[SessionRow]:
    rows = conn.execute(
        "SELECT id, project_id, session_id, first_ts, last_ts, message_count "
        "FROM sessions WHERE project_id = ? ORDER BY last_ts DESC",
        (project_id,),
    ).fetchall()
    return [SessionRow(**dict(r)) for r in rows]


def get_messages(
    conn: sqlite3.Connection,
    *,
    session_fk: int,
    limit: int,
    offset: int = 0,
) -> list[MessageRow]:
    rows = conn.execute(
        "SELECT id, session_fk, seq, timestamp, role, model, "
        "       input_tokens, output_tokens, cache_create_tokens, cache_read_tokens, "
        "       content_text, tools_json, raw_json, is_sidechain, uuid, parent_uuid, "
        "       speed "
        "FROM messages WHERE session_fk = ? "
        "ORDER BY seq LIMIT ? OFFSET ?",
        (session_fk, limit, offset),
    ).fetchall()
    return [
        MessageRow(**{**dict(r), "is_sidechain": bool(r["is_sidechain"])})
        for r in rows
    ]


def get_session_messages(conn: sqlite3.Connection, *, session_fk: int) -> list[MessageRow]:
    rows = conn.execute(
        "SELECT id, session_fk, seq, timestamp, role, model, "
        "       input_tokens, output_tokens, cache_create_tokens, cache_read_tokens, "
        "       content_text, tools_json, raw_json, is_sidechain, uuid, parent_uuid, "
        "       speed "
        "FROM messages WHERE session_fk = ? ORDER BY seq",
        (session_fk,),
    ).fetchall()
    return [
        MessageRow(**{**dict(r), "is_sidechain": bool(r["is_sidechain"])})
        for r in rows
    ]


def get_session_stats(conn: sqlite3.Connection, *, session_fk: int) -> dict:
    row = conn.execute(
        "SELECT "
        "  SUM(CASE WHEN role = 'user' THEN 1 ELSE 0 END) AS user_messages, "
        "  SUM(CASE WHEN role = 'assistant' THEN 1 ELSE 0 END) AS assistant_messages, "
        "  COALESCE(SUM(input_tokens), 0) AS input_tokens, "
        "  COALESCE(SUM(output_tokens), 0) AS output_tokens, "
        "  MAX(CASE WHEN model IS NOT NULL AND model != '' THEN model END) AS model, "
        "  COALESCE(SUM(json_array_length(tools_json)), 0) AS tool_calls "
        "FROM messages WHERE session_fk = ?",
        (session_fk,),
    ).fetchone()
    return {
        "user_messages": row["user_messages"] or 0,
        "assistant_messages": row["assistant_messages"] or 0,
        "input_tokens": row["input_tokens"] or 0,
        "output_tokens": row["output_tokens"] or 0,
        "model": row["model"],
        "tool_calls": row["tool_calls"] or 0,
    }


def build_enriched_dataset(
    conn: sqlite3.Connection,
    *,
    project_id: int,
):
    """Reconstruct an ``EnrichedDataset`` for a project from the store.

    Shared by ``get_project_stats`` (for the full stats dict) and the
    ``/api/interaction/{id}`` route (which needs the raw Interaction chain).
    Returns ``(dataset, log_dir)`` or ``(None, "")`` if the project is missing.
    """
    import json as _json
    from pathlib import Path

    from stackunderflow.stats import classifier, enricher
    from stackunderflow.stats.classifier import RawEntry

    row = conn.execute(
        "SELECT path, slug, provider FROM projects WHERE id = ?", (project_id,)
    ).fetchone()
    if row is None:
        return None, ""

    log_dir = row["path"] or str(Path.home() / ".claude" / "projects" / row["slug"])
    provider = row["provider"] or "anthropic"

    rows = conn.execute(
        "SELECT m.raw_json, s.session_id, m.timestamp "
        "FROM messages m "
        "JOIN sessions s ON s.id = m.session_fk "
        "WHERE s.project_id = ? "
        "ORDER BY m.timestamp",
        (project_id,),
    ).fetchall()

    raw_entries = []
    for r in rows:
        payload = _json.loads(r["raw_json"])
        # Authoritative clean timestamp lives in the column; raw_json may hold
        # epoch-millis ints from non-Claude adapters that the downstream
        # aggregator's string-ts assumption can't handle.
        if r["timestamp"]:
            payload["timestamp"] = r["timestamp"]
        raw_entries.append(
            RawEntry(
                payload=payload,
                session_id=r["session_id"],
                origin=r["session_id"],
                provider=provider,
            )
        )

    tagged = classifier.tag(raw_entries)
    dataset = enricher.build(tagged, log_dir)
    return dataset, log_dir


def get_project_stats(
    conn: sqlite3.Connection,
    *,
    project_id: int,
    tz_offset: int = 0,
) -> tuple[list[dict], dict]:
    """Run the pipeline on stored messages and return (messages, statistics).

    Reconstructs pipeline RawEntry objects from raw_json stored in the messages
    table, then runs the full dedup → classify → enrich → aggregate chain.
    The return shape is identical to pipeline.process(log_dir).
    """
    from stackunderflow.stats import aggregator, formatter

    dataset, log_dir = build_enriched_dataset(conn, project_id=project_id)
    if dataset is None:
        return [], {}

    messages = formatter.to_dicts(dataset)
    stats = aggregator.summarise(dataset, log_dir, tz_offset=tz_offset)
    return messages, stats


def get_project_messages(
    conn: sqlite3.Connection,
    *,
    project_id: int,
    limit: int | None = None,
) -> list[dict]:
    """Return pipeline-formatted messages for a project, ordered by timestamp."""
    messages, _ = get_project_stats(conn, project_id=project_id)
    if limit is not None:
        return messages[:limit]
    return messages


def get_global_stats(conn: sqlite3.Connection) -> dict:
    """Return the cross-project stats shape the Overview page expects.

    Keys: first_use_date, last_use_date, daily_token_usage, daily_costs,
    models, total_cache_read_tokens, total_cache_write_tokens.
    """
    from stackunderflow.infra.costs import compute_cost

    row = conn.execute(
        "SELECT MIN(timestamp) AS first_ts, MAX(timestamp) AS last_ts, "
        "       SUM(cache_read_tokens)   AS cache_read, "
        "       SUM(cache_create_tokens) AS cache_write "
        "FROM messages"
    ).fetchone()
    first_ts = (row["first_ts"] or "")[:10]
    last_ts = (row["last_ts"] or "")[:10]

    daily_tokens = [
        {"date": r["day"], "input": r["inp"], "output": r["out"]}
        for r in conn.execute(
            "SELECT substr(timestamp,1,10) AS day, "
            "       SUM(input_tokens) AS inp, SUM(output_tokens) AS out "
            "FROM messages GROUP BY day ORDER BY day"
        )
    ]

    # per-(day, model, speed) rollup feeding both daily_costs and the models map.
    # Grouping by ``speed`` lets ``compute_cost`` apply the Anthropic Opus
    # priority/fast 6× multiplier to the right subset of tokens — without
    # this dimension, every priority record was silently re-billed at the
    # standard rate (the bug PR #44 left in the SQL path). The top-level
    # ``models[model]`` dict still aggregates across speeds because the
    # public API contract doesn't expose the speed dimension yet — frontend
    # update will follow.
    per_day_model = conn.execute(
        "SELECT substr(timestamp,1,10) AS day, "
        "       COALESCE(model,'') AS model, "
        "       COALESCE(speed,'standard') AS speed, "
        "       SUM(input_tokens) AS inp, SUM(output_tokens) AS out, "
        "       SUM(cache_create_tokens) AS cache_create, "
        "       SUM(cache_read_tokens) AS cache_read, "
        "       COUNT(*) AS n "
        "FROM messages GROUP BY day, model, speed ORDER BY day"
    ).fetchall()

    daily_costs_map: dict[str, dict] = {}
    models: dict[str, dict] = {}
    for r in per_day_model:
        day, model, speed = r["day"], r["model"], r["speed"]
        tokens = {
            "input": r["inp"] or 0,
            "output": r["out"] or 0,
            "cache_creation": r["cache_create"] or 0,
            "cache_read": r["cache_read"] or 0,
        }
        cost = compute_cost(tokens, model, speed=speed)["total_cost"] if model else 0.0
        bucket = daily_costs_map.setdefault(day, {"date": day, "cost": 0.0, "by_model": {}})
        bucket["cost"] += cost
        if model:
            bucket["by_model"][model] = bucket["by_model"].get(model, 0.0) + cost
            m = models.setdefault(model, {"count": 0, "cost": 0.0})
            m["count"] += r["n"]
            m["cost"] += cost

    return {
        "first_use_date": first_ts,
        "last_use_date": last_ts,
        "daily_token_usage": daily_tokens,
        "daily_costs": list(daily_costs_map.values()),
        "models": models,
        "total_cache_read_tokens": int(row["cache_read"] or 0),
        "total_cache_write_tokens": int(row["cache_write"] or 0),
    }


def cross_project_daily_totals(
    conn: sqlite3.Connection,
    *,
    since: str | None = None,
    until: str | None = None,
) -> list[tuple]:
    """Per-(project_slug, day, model, speed) token rollups within [since, until].

    Tuple shape is ``(slug, day, model, input_tokens, output_tokens,
    messages, speed)``. ``speed`` is appended at the end so existing
    callers that index the leading columns positionally keep working
    while new callers (e.g. ``reports.aggregate``) can read the speed
    flag for tier-aware cost computation.
    """
    sql = (
        "SELECT projects.slug AS slug, "
        "       substr(messages.timestamp, 1, 10) AS day, "
        "       COALESCE(messages.model, '') AS model, "
        "       SUM(messages.input_tokens) AS input_tokens, "
        "       SUM(messages.output_tokens) AS output_tokens, "
        "       COUNT(*) AS messages, "
        "       COALESCE(messages.speed, 'standard') AS speed "
        "FROM messages "
        "JOIN sessions ON sessions.id = messages.session_fk "
        "JOIN projects ON projects.id = sessions.project_id "
        "WHERE 1=1 "
    )
    params: list[str] = []
    if since:
        sql += "AND messages.timestamp >= ? "
        params.append(since)
    if until:
        sql += "AND messages.timestamp < ? "
        params.append(until)
    sql += "GROUP BY slug, day, model, speed ORDER BY day"
    return [tuple(row) for row in conn.execute(sql, params).fetchall()]
