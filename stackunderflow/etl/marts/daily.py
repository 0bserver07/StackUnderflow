"""Daily mart — (day, project_id, provider, model, speed) rollup.

Additive incremental refresh: tokens, message_count, cost are summed
into the existing row via ``ON CONFLICT DO UPDATE``. The watermark
guarantees the same ``event_id`` is never processed twice, so adding
``excluded.<col>`` to the existing column is safe for SUM/COUNT(*)
aggregates.

session_count is the special case
==================================

``COUNT(DISTINCT session_id)`` is **not** additive across refresh
windows. If session ``S`` produces events on day ``D`` in two separate
refresh windows, naively adding the per-window distinct count would
double-count it (1 + 1 = 2 instead of 1).

We follow option (a) from the spec: after the additive upsert, we
recompute ``session_count`` for the ``(day, project_id, provider, model,
speed)`` keys touched by this refresh. The recompute reads the full
``usage_events`` table (filtered to the affected day/project/provider
combos) and overwrites ``session_count`` with the correct
``COUNT(DISTINCT session_id)``.

This costs us one extra SELECT per refresh, but it's bounded by the
number of distinct (day, project, provider, model, speed) keys in the
window — typically O(1)..O(few dozen) — so the cost is negligible.
"""

from __future__ import annotations

import sqlite3

from .base import MartBuilder


class DailyMartBuilder(MartBuilder):
    """Per-(day, project, provider, model, speed) cost + token rollup."""

    name = "daily"

    def refresh(self, conn: sqlite3.Connection, since_event_id: int) -> int:
        max_id = _max_event_id(conn)
        if max_id <= since_event_id:
            return since_event_id

        # ── additive upsert for SUM/COUNT(*) columns ──────────────────────
        conn.execute(
            """
            INSERT INTO daily_mart (
                day, project_id, provider, model, speed,
                input_tokens, output_tokens, cache_read, cache_create,
                message_count, session_count, cost_usd
            )
            SELECT
                day, project_id, provider, model, speed,
                SUM(input_tokens),
                SUM(output_tokens),
                SUM(cache_read_tokens),
                SUM(cache_create_tokens),
                COUNT(*),
                COUNT(DISTINCT session_id),
                SUM(cost_usd)
            FROM usage_events
            WHERE id > ? AND id <= ?
            GROUP BY day, project_id, provider, model, speed
            ON CONFLICT (day, project_id, provider, model, speed) DO UPDATE SET
                input_tokens  = input_tokens  + excluded.input_tokens,
                output_tokens = output_tokens + excluded.output_tokens,
                cache_read    = cache_read    + excluded.cache_read,
                cache_create  = cache_create  + excluded.cache_create,
                message_count = message_count + excluded.message_count,
                session_count = session_count + excluded.session_count,
                cost_usd      = cost_usd      + excluded.cost_usd
            """,
            (since_event_id, max_id),
        )

        # ── recompute session_count for affected keys ─────────────────────
        # Why: COUNT(DISTINCT session_id) is not additive — if the same
        # session produces events on the same day in two refresh windows,
        # naive addition double-counts. We overwrite with the correct
        # DISTINCT count from the full events table for keys touched here.
        conn.execute(
            """
            WITH affected AS (
                SELECT DISTINCT day, project_id, provider, model, speed
                FROM usage_events
                WHERE id > ? AND id <= ?
            ),
            recomputed AS (
                SELECT
                    e.day, e.project_id, e.provider, e.model, e.speed,
                    COUNT(DISTINCT e.session_id) AS sc
                FROM usage_events e
                JOIN affected a USING (day, project_id, provider, model, speed)
                GROUP BY e.day, e.project_id, e.provider, e.model, e.speed
            )
            UPDATE daily_mart
               SET session_count = (
                   SELECT sc FROM recomputed r
                   WHERE r.day = daily_mart.day
                     AND r.project_id = daily_mart.project_id
                     AND r.provider = daily_mart.provider
                     AND r.model = daily_mart.model
                     AND r.speed = daily_mart.speed
               )
             WHERE (day, project_id, provider, model, speed) IN (
                   SELECT day, project_id, provider, model, speed FROM affected
               )
            """,
            (since_event_id, max_id),
        )

        return max_id

    def rebuild_from_scratch(self, conn: sqlite3.Connection) -> None:
        conn.execute("DELETE FROM daily_mart")
        self.refresh(conn, since_event_id=0)


def _max_event_id(conn: sqlite3.Connection) -> int:
    row = conn.execute("SELECT MAX(id) AS m FROM usage_events").fetchone()
    if row is None:
        return 0
    val = row["m"] if hasattr(row, "keys") else row[0]
    return int(val) if val is not None else 0
