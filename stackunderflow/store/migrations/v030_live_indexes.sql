-- v030: read-path indexes for the live tab — mart time window + slug lookup.
--
-- Two indexes, both pure read-path. No table is created, altered or dropped.
--
--   * ``idx_message_tool_mart_ts`` — ``(ts, message_id, tool_name)``.
--     ``services.live._latency_samples`` opens every ``/api/live/stats`` poll
--     with ``SELECT message_id, tool_name, session_id FROM message_tool_mart
--     WHERE ts >= ?``. The mart's existing indexes are keyed on
--     ``session_id`` / ``(project_id, day)`` / ``file_path`` /
--     ``(tool_name, day)`` — none of them leads with ``ts``, so that predicate
--     full-scanned the mart on every poll. Leading on ``ts`` turns it into a
--     range seek; carrying ``message_id`` + ``tool_name`` in the index makes
--     the two hot columns index-only. (``session_id`` is deliberately NOT in
--     the key: it would widen every entry for one extra column the latency
--     query reads once per row, and the ``ts`` seek already bounds the
--     rowid lookups.) Measured on the live latency query, 24h window over a
--     252K-message / 61.5K-mart-row store (median of 5): 13.0ms without this
--     index, 9.1ms with it — ``EXPLAIN QUERY PLAN`` flips from
--     ``SCAN message_tool_mart`` to
--     ``SEARCH message_tool_mart USING INDEX idx_message_tool_mart_ts (ts>?)``.
--
--   * ``idx_projects_slug`` — ``projects(slug)``. ``projects`` has
--     ``UNIQUE (provider, slug)``, which only serves lookups that supply
--     ``provider``; the many ``WHERE slug = ?`` / ``WHERE p.slug IN (…)``
--     call sites (mart_queries' slug scoping, the per-project routes) fell
--     back to a table scan. Small table, but it is scanned on nearly every
--     project-scoped request.
--
-- Migration is **index-only and idempotent** — both statements are
-- ``CREATE INDEX IF NOT EXISTS``, so re-running is a no-op and no
-- ``_ADD_COLUMN_GUARDS`` entry is needed (there is no column to guard and no
-- partial-application state to recover from).
--
-- Survives a mart rebuild: every mart builder clears rows with ``DELETE FROM``
-- (``etl/marts/base.py``, ``etl/marts/message_tool.py``), never ``DROP TABLE``,
-- so ``rebuild_from_scratch`` leaves both indexes in place.

BEGIN;

CREATE INDEX IF NOT EXISTS idx_message_tool_mart_ts
    ON message_tool_mart(ts, message_id, tool_name);

CREATE INDEX IF NOT EXISTS idx_projects_slug
    ON projects(slug);

PRAGMA user_version = 30;

COMMIT;
