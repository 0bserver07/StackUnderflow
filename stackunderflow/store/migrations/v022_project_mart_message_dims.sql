-- v022: materialise per-project message-type + command counts on project_mart.
--
-- Closes the remaining half of the "Overview shows 0" data bug (ui-perf-audit
-- #7/#26). The mart fast-path serves overview tokens/cost/daily/models, but the
-- Overview's message-type cards (User / Assistant / Tool-Use / Tool-Results) and
-- the Commands KPI read 0 on a mart-backed project: no mart carries those dims.
-- ``usage_events`` is assistant-only (the normalizers skip non-billable rows),
-- so ``session_mart.user_message_count`` is structurally 0 and ``project_mart``
-- never counted users/commands at all. Falling through to ``get_project_stats``
-- to recover them reintroduces the ~3.1s full ``messages`` scan the <100ms perf
-- test forbids.
--
-- These five columns are computed once at mart-build time
-- (``ProjectMartBuilder.refresh``) by running the SAME classifier/enricher logic
-- ``get_project_stats`` uses over the project's ``messages.raw_json``, then read
-- back as a single indexed ``project_mart`` lookup:
--
--   * total_user_messages        — Counter(kind)['user']      (overview.message_types)
--   * total_assistant_messages   — Counter(kind)['assistant']  (overview.message_types)
--   * total_tool_use_messages    — assistant records carrying tool_use blocks
--   * total_tool_result_messages — records carrying a tool_result block
--   * total_commands             — user_interactions.user_commands_analyzed
--                                  (kind=='user', not a tool_result, not an interruption)
--
-- Migration is **additive** — no existing tables touched. Existing
-- ``project_mart`` rows get ``0`` via the DEFAULT until the next mart refresh
-- (or a ``stackunderflow etl backfill --force`` rebuild) re-derives them. The
-- four ``ALTER TABLE`` statements are idempotency-guarded in ``schema.py`` via
-- the ``_ADD_COLUMN_GUARDS`` ``("project_mart", "total_user_messages")`` entry.

BEGIN;

ALTER TABLE project_mart ADD COLUMN total_user_messages        INTEGER NOT NULL DEFAULT 0;
ALTER TABLE project_mart ADD COLUMN total_assistant_messages   INTEGER NOT NULL DEFAULT 0;
ALTER TABLE project_mart ADD COLUMN total_tool_use_messages    INTEGER NOT NULL DEFAULT 0;
ALTER TABLE project_mart ADD COLUMN total_tool_result_messages INTEGER NOT NULL DEFAULT 0;
ALTER TABLE project_mart ADD COLUMN total_commands             INTEGER NOT NULL DEFAULT 0;

PRAGMA user_version = 22;

COMMIT;
