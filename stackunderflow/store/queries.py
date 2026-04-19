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
