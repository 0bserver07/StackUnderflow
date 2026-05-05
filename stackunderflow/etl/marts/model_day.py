"""model_day mart — (day, model, speed) rollup for the compare-across-agents view.

Additive over SUM/COUNT(*) columns. ``session_count`` is recomputed for
affected keys after the additive upsert (option (a) from the spec —
``COUNT(DISTINCT session_id)`` is not additive across refresh windows).
"""

from __future__ import annotations

import sqlite3

from .base import MartBuilder


class ModelDayMartBuilder(MartBuilder):
    """Per-(day, model, speed) rollup across all providers + projects."""

    name = "model_day"

    def refresh(self, conn: sqlite3.Connection, since_event_id: int) -> int:
        max_id = _max_event_id(conn)
        if max_id <= since_event_id:
            return since_event_id

        conn.execute(
            """
            INSERT INTO model_day_mart (
                day, model, speed,
                cost_usd, input_tokens, output_tokens,
                cache_read, cache_create,
                message_count, session_count
            )
            SELECT
                day, model, speed,
                SUM(cost_usd),
                SUM(input_tokens),
                SUM(output_tokens),
                SUM(cache_read_tokens),
                SUM(cache_create_tokens),
                COUNT(*),
                COUNT(DISTINCT session_id)
            FROM usage_events
            WHERE id > ? AND id <= ?
            GROUP BY day, model, speed
            ON CONFLICT (day, model, speed) DO UPDATE SET
                cost_usd      = cost_usd      + excluded.cost_usd,
                input_tokens  = input_tokens  + excluded.input_tokens,
                output_tokens = output_tokens + excluded.output_tokens,
                cache_read    = cache_read    + excluded.cache_read,
                cache_create  = cache_create  + excluded.cache_create,
                message_count = message_count + excluded.message_count,
                session_count = session_count + excluded.session_count
            """,
            (since_event_id, max_id),
        )

        conn.execute(
            """
            WITH affected AS (
                SELECT DISTINCT day, model, speed
                FROM usage_events
                WHERE id > ? AND id <= ?
            ),
            recomputed AS (
                SELECT
                    e.day, e.model, e.speed,
                    COUNT(DISTINCT e.session_id) AS sc
                FROM usage_events e
                JOIN affected a USING (day, model, speed)
                GROUP BY e.day, e.model, e.speed
            )
            UPDATE model_day_mart
               SET session_count = (
                   SELECT sc FROM recomputed r
                   WHERE r.day = model_day_mart.day
                     AND r.model = model_day_mart.model
                     AND r.speed = model_day_mart.speed
               )
             WHERE (day, model, speed) IN (
                   SELECT day, model, speed FROM affected
               )
            """,
            (since_event_id, max_id),
        )

        return max_id

    def rebuild_from_scratch(self, conn: sqlite3.Connection) -> None:
        conn.execute("DELETE FROM model_day_mart")
        self.refresh(conn, since_event_id=0)


def _max_event_id(conn: sqlite3.Connection) -> int:
    row = conn.execute("SELECT MAX(id) AS m FROM usage_events").fetchone()
    if row is None:
        return 0
    val = row["m"] if hasattr(row, "keys") else row[0]
    return int(val) if val is not None else 0
