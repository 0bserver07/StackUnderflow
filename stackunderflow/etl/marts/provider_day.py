"""provider_day mart — (day, provider) rollup for the by-provider chart.

Additive over cost + message_count, with the same session_count and
project_count caveat as :mod:`daily` (DISTINCT counts aren't additive
across refresh windows). We follow option (a): recompute
``session_count`` and ``project_count`` for the (day, provider) keys
touched by this refresh.
"""

from __future__ import annotations

import sqlite3

from .base import MartBuilder


class ProviderDayMartBuilder(MartBuilder):
    """Per-(day, provider) cost + count rollup."""

    name = "provider_day"

    def refresh(self, conn: sqlite3.Connection, since_event_id: int) -> int:
        max_id = _max_event_id(conn)
        if max_id <= since_event_id:
            return since_event_id

        conn.execute(
            """
            INSERT INTO provider_day_mart (
                day, provider, cost_usd, message_count,
                session_count, project_count
            )
            SELECT
                day, provider,
                SUM(cost_usd),
                COUNT(*),
                COUNT(DISTINCT session_id),
                COUNT(DISTINCT project_id)
            FROM usage_events
            WHERE id > ? AND id <= ?
            GROUP BY day, provider
            ON CONFLICT (day, provider) DO UPDATE SET
                cost_usd      = cost_usd      + excluded.cost_usd,
                message_count = message_count + excluded.message_count,
                session_count = session_count + excluded.session_count,
                project_count = project_count + excluded.project_count
            """,
            (since_event_id, max_id),
        )

        # Recompute the two DISTINCT-count columns for affected keys.
        conn.execute(
            """
            WITH affected AS (
                SELECT DISTINCT day, provider
                FROM usage_events
                WHERE id > ? AND id <= ?
            ),
            recomputed AS (
                SELECT
                    e.day, e.provider,
                    COUNT(DISTINCT e.session_id) AS sc,
                    COUNT(DISTINCT e.project_id) AS pc
                FROM usage_events e
                JOIN affected a USING (day, provider)
                GROUP BY e.day, e.provider
            )
            UPDATE provider_day_mart
               SET session_count = (
                       SELECT sc FROM recomputed r
                       WHERE r.day = provider_day_mart.day
                         AND r.provider = provider_day_mart.provider
                   ),
                   project_count = (
                       SELECT pc FROM recomputed r
                       WHERE r.day = provider_day_mart.day
                         AND r.provider = provider_day_mart.provider
                   )
             WHERE (day, provider) IN (
                   SELECT day, provider FROM affected
               )
            """,
            (since_event_id, max_id),
        )

        return max_id

    def rebuild_from_scratch(self, conn: sqlite3.Connection) -> None:
        conn.execute("DELETE FROM provider_day_mart")
        self.refresh(conn, since_event_id=0)


def _max_event_id(conn: sqlite3.Connection) -> int:
    row = conn.execute("SELECT MAX(id) AS m FROM usage_events").fetchone()
    if row is None:
        return 0
    val = row["m"] if hasattr(row, "keys") else row[0]
    return int(val) if val is not None else 0
