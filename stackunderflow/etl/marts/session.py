"""session_mart — one row per session.

Replace-from-scratch-for-affected-keys pattern. New events for an
existing session invalidate the prior aggregate, so we recompute the
session row in full from all of its events. ``INSERT OR REPLACE`` on
the ``session_id`` PRIMARY KEY does the swap atomically.

``primary_model`` is the assistant model with the most messages in the
session — chosen via a correlated subquery so a session that switches
models mid-conversation still gets a stable label. Ties resolve by SQL
ORDER BY's natural row order (deterministic but unspecified — fine for
a tie-breaker on a "primarily X" label).

``cwd`` is left ``NULL`` in v1 — pulling it from ``messages.raw_json``
needs the messages table joined in, and the v0.7 routes that consume
session_mart don't read ``cwd`` yet. Wave 3 can add a join when needed.
"""

from __future__ import annotations

import sqlite3

from .base import MartBuilder


class SessionMartBuilder(MartBuilder):
    """Per-session lifetime aggregates."""

    name = "session"

    def refresh(self, conn: sqlite3.Connection, since_event_id: int) -> int:
        max_id = _max_event_id(conn)
        if max_id <= since_event_id:
            return since_event_id

        conn.execute(
            """
            INSERT OR REPLACE INTO session_mart (
                session_id, project_id, provider, primary_model,
                first_ts, last_ts,
                message_count, user_message_count, assistant_message_count,
                input_tokens, output_tokens, cache_read, cache_create,
                cost_usd, is_one_shot, cwd
            )
            SELECT
                e.session_id,
                MIN(e.project_id),
                MIN(e.provider),
                (
                    SELECT e2.model
                    FROM usage_events e2
                    WHERE e2.session_id = e.session_id
                      AND e2.role = 'assistant'
                      AND e2.model <> ''
                    GROUP BY e2.model
                    ORDER BY COUNT(*) DESC
                    LIMIT 1
                ) AS primary_model,
                MIN(e.ts),
                MAX(e.ts),
                COUNT(*),
                SUM(CASE WHEN e.role = 'user' THEN 1 ELSE 0 END),
                SUM(CASE WHEN e.role = 'assistant' THEN 1 ELSE 0 END),
                SUM(e.input_tokens),
                SUM(e.output_tokens),
                SUM(e.cache_read_tokens),
                SUM(e.cache_create_tokens),
                SUM(e.cost_usd),
                CASE
                    WHEN SUM(CASE WHEN e.role = 'user' THEN 1 ELSE 0 END) = 1
                     AND SUM(CASE WHEN e.role = 'assistant' THEN 1 ELSE 0 END) = 1
                    THEN 1
                    ELSE 0
                END,
                NULL  -- cwd: deferred to a future wave
            FROM usage_events e
            WHERE e.session_id IN (
                SELECT DISTINCT session_id
                FROM usage_events
                WHERE id > ? AND id <= ?
            )
            GROUP BY e.session_id
            """,
            (since_event_id, max_id),
        )

        return max_id

    def rebuild_from_scratch(self, conn: sqlite3.Connection) -> None:
        conn.execute("DELETE FROM session_mart")
        self.refresh(conn, since_event_id=0)


def _max_event_id(conn: sqlite3.Connection) -> int:
    row = conn.execute("SELECT MAX(id) AS m FROM usage_events").fetchone()
    if row is None:
        return 0
    val = row["m"] if hasattr(row, "keys") else row[0]
    return int(val) if val is not None else 0
