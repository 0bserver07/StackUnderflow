"""project_mart — one row per project, lifetime totals.

Replace-from-scratch-for-affected-keys pattern. New events for an
existing project invalidate the prior aggregate (``total_sessions``
especially is a DISTINCT count that can't be summed across windows),
so we recompute the project row from all of its events. ``INSERT OR
REPLACE`` on the ``project_id`` PRIMARY KEY does the swap atomically.

``provider``, ``slug``, ``display_name`` come from the ``projects``
table, joined in by id.
"""

from __future__ import annotations

import sqlite3

from .base import MartBuilder


class ProjectMartBuilder(MartBuilder):
    """Per-project lifetime aggregates."""

    name = "project"

    def refresh(self, conn: sqlite3.Connection, since_event_id: int) -> int:
        max_id = _max_event_id(conn)
        if max_id <= since_event_id:
            return since_event_id

        conn.execute(
            """
            INSERT OR REPLACE INTO project_mart (
                project_id, provider, slug, display_name,
                first_ts, last_ts,
                total_messages, total_sessions,
                total_input_tokens, total_output_tokens,
                total_cache_read, total_cache_create,
                total_cost_usd
            )
            SELECT
                e.project_id,
                p.provider,
                p.slug,
                p.display_name,
                MIN(e.ts),
                MAX(e.ts),
                COUNT(*),
                COUNT(DISTINCT e.session_id),
                SUM(e.input_tokens),
                SUM(e.output_tokens),
                SUM(e.cache_read_tokens),
                SUM(e.cache_create_tokens),
                SUM(e.cost_usd)
            FROM usage_events e
            JOIN projects p ON p.id = e.project_id
            WHERE e.project_id IN (
                SELECT DISTINCT project_id
                FROM usage_events
                WHERE id > ? AND id <= ?
            )
            GROUP BY e.project_id, p.provider, p.slug, p.display_name
            """,
            (since_event_id, max_id),
        )

        return max_id

    def rebuild_from_scratch(self, conn: sqlite3.Connection) -> None:
        conn.execute("DELETE FROM project_mart")
        self.refresh(conn, since_event_id=0)


def _max_event_id(conn: sqlite3.Connection) -> int:
    row = conn.execute("SELECT MAX(id) AS m FROM usage_events").fetchone()
    if row is None:
        return 0
    val = row["m"] if hasattr(row, "keys") else row[0]
    return int(val) if val is not None else 0
