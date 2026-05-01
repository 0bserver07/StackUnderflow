"""Read-only store accessors used by the MCP server.

The MCP tools call into this module instead of walking JSONL files
directly. Reading from the unified SQLite store at
``~/.stackunderflow/store.db`` means a single MCP query sees every
provider StackUnderflow has ingested (claude, codex, cursor, cline,
droid, kiro, openclaw, pi, copilot, …) without each adapter needing to
be re-implemented in the MCP path.

Every public helper accepts an optional ``conn`` argument so tests can
inject a synthetic store without touching the user's real DB. When
``conn`` is omitted, the helper opens the default store at
``deps.store_path`` and closes it again before returning. If the store
does not exist (fresh install with no ingest yet) the helpers all
return ``None`` / empty lists rather than raising — the MCP server
falls back to the legacy JSONL walk in that case.

This module is **read-only**. It never writes to the store and never
runs migrations.
"""

from __future__ import annotations

import json
import logging
import sqlite3
from collections.abc import Callable, Iterable
from contextlib import contextmanager
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Literal

from stackunderflow import deps
from stackunderflow.store import db

_log = logging.getLogger(__name__)


@dataclass(frozen=True, slots=True)
class StoredSession:
    """Flat session record returned to the MCP layer.

    Carries enough provenance for an MCP client to either show a list
    or pivot into ``session_query`` for the full event stream.
    """

    session_id: str
    provider: str
    project_slug: str
    project_display_name: str
    started_at: str | None
    last_ts: str | None
    message_count: int
    cost_usd: float
    _session_fk: int = 0  # internal — stable PK so callers can re-query


@dataclass(frozen=True, slots=True)
class StoredProject:
    """Flat project record returned to the MCP layer."""

    slug: str
    provider: str
    display_name: str
    first_seen: str | None
    last_modified: str | None
    path: str | None


# ── connection management ───────────────────────────────────────────────────


@contextmanager
def _maybe_conn(conn: sqlite3.Connection | None) -> Iterable[sqlite3.Connection | None]:
    """Yield ``conn`` if provided; otherwise open the default store.

    If the store DB does not exist yet, yields ``None`` so callers can
    short-circuit to a fallback path instead of forcing a fresh DB to be
    created (which would happen for free with ``db.connect``).
    """
    if conn is not None:
        yield conn
        return
    path: Path = deps.store_path
    if not path.exists():
        yield None
        return
    opened = db.connect(path)
    try:
        yield opened
    finally:
        opened.close()


# ── helpers ─────────────────────────────────────────────────────────────────


def _epoch_to_iso(value: float | None) -> str | None:
    """Convert a Unix epoch float to an ISO-8601 UTC string."""
    if value is None:
        return None
    try:
        from datetime import UTC, datetime

        return datetime.fromtimestamp(float(value), tz=UTC).isoformat()
    except (TypeError, ValueError, OSError):
        return None


def _safe_compute_cost(input_tokens: int, output_tokens: int, model: str | None) -> float:
    """Cost for a session row. Returns 0.0 on any pricer failure.

    Imported lazily to avoid pulling the cost module into the MCP
    process unless an MCP tool actually asks for cost.
    """
    if not model or (not input_tokens and not output_tokens):
        return 0.0
    try:
        from stackunderflow.infra.costs import compute_cost

        cost = compute_cost(
            {"input": int(input_tokens), "output": int(output_tokens)},
            model,
        )
        return float(cost.get("total_cost", 0.0))
    except Exception as exc:  # never propagate from a read-only path
        _log.debug("cost compute failed for model=%r: %s", model, exc)
        return 0.0


def _row_to_session(row: sqlite3.Row, *, with_cost: bool) -> StoredSession:
    cost = (
        _safe_compute_cost(row["input_tokens"], row["output_tokens"], row["model"])
        if with_cost
        else 0.0
    )
    return StoredSession(
        session_id=row["session_id"],
        provider=row["provider"],
        project_slug=row["slug"],
        project_display_name=row["display_name"],
        started_at=row["first_ts"],
        last_ts=row["last_ts"],
        message_count=int(row["message_count"] or 0),
        cost_usd=cost,
        _session_fk=int(row["session_fk"]),
    )


_SESSION_BASE_SQL = """
SELECT
    s.id              AS session_fk,
    s.session_id      AS session_id,
    s.first_ts        AS first_ts,
    s.last_ts         AS last_ts,
    s.message_count   AS message_count,
    p.provider        AS provider,
    p.slug            AS slug,
    p.display_name    AS display_name,
    COALESCE(SUM(m.input_tokens),  0) AS input_tokens,
    COALESCE(SUM(m.output_tokens), 0) AS output_tokens,
    MAX(CASE WHEN m.model IS NOT NULL AND m.model != '' THEN m.model END) AS model
FROM sessions s
JOIN projects p ON p.id = s.project_id
LEFT JOIN messages m ON m.session_fk = s.id
"""


# ── public API ──────────────────────────────────────────────────────────────


def find_session(
    session_id: str,
    conn: sqlite3.Connection | None = None,
) -> StoredSession | None:
    """Look up a single session by its public ``session_id``.

    Returns ``None`` if the session is not in the store (e.g. the user
    has not yet ingested it, or the DB does not exist).
    """
    with _maybe_conn(conn) as c:
        if c is None:
            return None
        row = c.execute(
            _SESSION_BASE_SQL
            + "WHERE s.session_id = ? GROUP BY s.id LIMIT 1",
            (session_id,),
        ).fetchone()
        return _row_to_session(row, with_cost=True) if row else None


def list_recent_sessions(
    limit: int = 50,
    provider: str | None = None,
    since: str | None = None,
    conn: sqlite3.Connection | None = None,
) -> list[StoredSession]:
    """Return the most recently active sessions in the store.

    Args:
        limit: Max sessions to return. Negative / zero → empty list.
        provider: If set, only sessions whose project has this provider.
        since: ISO-8601 timestamp lower bound on ``last_ts`` (inclusive).
        conn: Test/inject a sqlite3 connection. Default opens the store.
    """
    if limit <= 0:
        return []

    where: list[str] = []
    params: list[Any] = []
    if provider is not None:
        where.append("p.provider = ?")
        params.append(provider)
    if since is not None:
        where.append("s.last_ts >= ?")
        params.append(since)
    where_sql = ("WHERE " + " AND ".join(where) + " ") if where else ""

    sql = (
        _SESSION_BASE_SQL
        + where_sql
        + "GROUP BY s.id ORDER BY COALESCE(s.last_ts, '') DESC LIMIT ?"
    )
    params.append(int(limit))

    with _maybe_conn(conn) as c:
        if c is None:
            return []
        rows = c.execute(sql, params).fetchall()
        return [_row_to_session(r, with_cost=True) for r in rows]


def list_stored_projects(
    provider: str | None = None,
    conn: sqlite3.Connection | None = None,
) -> list[StoredProject]:
    """Return projects in the store, optionally filtered by provider."""
    sql = (
        "SELECT provider, slug, path, display_name, first_seen, last_modified "
        "FROM projects "
    )
    params: list[Any] = []
    if provider is not None:
        sql += "WHERE provider = ? "
        params.append(provider)
    sql += "ORDER BY last_modified DESC"

    with _maybe_conn(conn) as c:
        if c is None:
            return []
        rows = c.execute(sql, params).fetchall()
        return [
            StoredProject(
                slug=r["slug"],
                provider=r["provider"],
                display_name=r["display_name"],
                first_seen=_epoch_to_iso(r["first_seen"]),
                last_modified=_epoch_to_iso(r["last_modified"]),
                path=r["path"],
            )
            for r in rows
        ]


_KIND = Literal["all", "tool_calls", "errors"]


def get_session_messages(
    session_id: str,
    kind: _KIND = "all",
    limit: int = 100,
    conn: sqlite3.Connection | None = None,
    *,
    is_error: Callable[[dict], bool] | None = None,
) -> list[dict]:
    """Return messages for ``session_id`` filtered by ``kind``.

    Each returned dict has the shape the MCP ``session_query`` tool
    surfaces (agent / project_slug / session_id / timestamp / role / …).
    Returns an empty list if the session is not in the store.

    Filtering semantics match the JSONL fallback path:

    * ``"all"``       — every message, in seq order, capped at ``limit``.
    * ``"tool_calls"`` — only assistant messages with one or more
      ``tool_use`` blocks.
    * ``"errors"``    — only messages whose payload contains a
      ``tool_result`` flagged ``is_error`` or with error-like text.

    The ``is_error`` callable lets the server inject its existing
    detection helper without creating an import cycle.
    """
    if limit <= 0:
        return []

    with _maybe_conn(conn) as c:
        if c is None:
            return []
        # Identify the session row + project context in one go.
        sess_row = c.execute(
            "SELECT s.id AS session_fk, p.provider AS provider, p.slug AS slug "
            "FROM sessions s JOIN projects p ON p.id = s.project_id "
            "WHERE s.session_id = ? LIMIT 1",
            (session_id,),
        ).fetchone()
        if sess_row is None:
            return []

        msg_rows = c.execute(
            "SELECT seq, timestamp, role, model, content_text, tools_json, "
            "       raw_json, is_sidechain, uuid "
            "FROM messages WHERE session_fk = ? ORDER BY seq",
            (sess_row["session_fk"],),
        ).fetchall()

    out: list[dict] = []
    for r in msg_rows:
        try:
            raw = json.loads(r["raw_json"]) if r["raw_json"] else {}
        except (json.JSONDecodeError, TypeError):
            raw = {}

        try:
            tools = json.loads(r["tools_json"]) if r["tools_json"] else []
        except (json.JSONDecodeError, TypeError):
            tools = []
        if not isinstance(tools, list):
            tools = []

        if kind == "tool_calls" and not tools:
            continue
        if kind == "errors":
            if not is_error or not is_error(raw):
                continue

        out.append(
            {
                "agent": sess_row["provider"],
                "project_slug": sess_row["slug"],
                "session_id": session_id,
                "timestamp": r["timestamp"],
                "role": r["role"],
                "model": r["model"],
                "tools": list(tools),
                "content_preview": (r["content_text"] or "")[:200]
                + ("…" if r["content_text"] and len(r["content_text"]) > 200 else ""),
                "is_sidechain": bool(r["is_sidechain"]),
                "uuid": r["uuid"],
                "raw": raw,
            }
        )
        if len(out) >= limit:
            break

    return out


def store_available(conn: sqlite3.Connection | None = None) -> bool:
    """Cheap probe: True if the store DB exists and the schema is set up.

    Used by the MCP server to decide whether to take the store path or
    the legacy JSONL fallback. Never raises.
    """
    if conn is not None:
        try:
            conn.execute("SELECT 1 FROM projects LIMIT 1").fetchone()
            return True
        except sqlite3.Error:
            return False
    path: Path = deps.store_path
    if not path.exists():
        return False
    try:
        c = db.connect(path)
        try:
            c.execute("SELECT 1 FROM projects LIMIT 1").fetchone()
            return True
        except sqlite3.Error:
            return False
        finally:
            c.close()
    except sqlite3.Error:
        return False
