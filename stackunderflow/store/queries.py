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
        "       content_text, tools_json, raw_json, is_sidechain, uuid, parent_uuid "
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
        "       content_text, tools_json, raw_json, is_sidechain, uuid, parent_uuid "
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
    import json as _json
    from pathlib import Path

    from stackunderflow.pipeline import aggregator, classifier, dedup, enricher, formatter
    from stackunderflow.pipeline.reader import RawEntry

    row = conn.execute(
        "SELECT path, slug FROM projects WHERE id = ?", (project_id,)
    ).fetchone()
    if row is None:
        return [], {}

    log_dir = row["path"] or str(Path.home() / ".claude" / "projects" / row["slug"])

    rows = conn.execute(
        "SELECT m.raw_json, s.session_id "
        "FROM messages m "
        "JOIN sessions s ON s.id = m.session_fk "
        "WHERE s.project_id = ? "
        "ORDER BY m.timestamp",
        (project_id,),
    ).fetchall()

    raw_entries = [
        RawEntry(
            payload=_json.loads(r["raw_json"]),
            session_id=r["session_id"],
            origin=r["session_id"],
        )
        for r in rows
    ]

    merged = dedup.collapse(raw_entries)
    tagged = classifier.tag(merged)
    dataset = enricher.build(tagged, log_dir)
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


def cross_project_daily_totals(
    conn: sqlite3.Connection,
    *,
    since: str | None = None,
    until: str | None = None,
) -> list[tuple]:
    """Per-(project_slug, day, model) token rollups within [since, until]."""
    sql = (
        "SELECT projects.slug AS slug, "
        "       substr(messages.timestamp, 1, 10) AS day, "
        "       COALESCE(messages.model, '') AS model, "
        "       SUM(messages.input_tokens) AS input_tokens, "
        "       SUM(messages.output_tokens) AS output_tokens, "
        "       COUNT(*) AS messages "
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
    sql += "GROUP BY slug, day, model ORDER BY day"
    return [tuple(row) for row in conn.execute(sql, params).fetchall()]
