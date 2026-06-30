-- v025: per-(day, project_id) user-command count — windows the Overview
-- "Commands" KPI (ui-perf-audit #25).
--
-- The Overview "Commands" KPI shows ``user_commands_analyzed`` — the count of
-- real user command turns (kind=='user', not a tool_result, not an
-- interruption). Lifetime, that value lives on ``project_mart.total_commands``
-- (v022). The other Overview headline figures (tokens, cost) are WINDOWED by
-- the dashboard's date-range selector because they're summed from per-day mart
-- rows; the Commands KPI alone read the lifetime total and so ignored the
-- window (#25).
--
-- No EXISTING mart carries a per-day user-command count we could sum within the
-- window:
--
--   * ``daily_mart`` is built from ``usage_events`` (assistant-only — the
--     normalizers skip non-billable rows), so it can't see user turns at all.
--   * ``command_mart`` is event-grain: it attributes each billable assistant
--     event to the most-recent-preceding user message (which may be an
--     interruption) and groups by the parsed slash-command. Summing its
--     ``event_count`` counts assistant turns, NOT user commands, and a command
--     that produced no billable event (e.g. ``/clear``) leaves no row. It
--     therefore cannot reconstruct ``total_commands`` (proven by the v022
--     interruption-exclusion equivalence test).
--
-- So we materialise the missing dimension directly: one row per (day,
-- project_id) carrying ``command_count`` — the SAME "real user command" tally
-- ``project_mart.total_commands`` uses (``ProjectMartBuilder._count_message_dims``),
-- just bucketed by the message's UTC day. Summed over a day window for a
-- project's ids it equals the windowed ``user_commands_analyzed``; summed over
-- ALL rows for a project it equals the lifetime ``total_commands`` (the read
-- path asserts this reconciliation in tests).
--
-- Built by the existing ``CommandMartBuilder`` (it already scans the affected
-- projects to attribute commands) in a second pass over ``messages.raw_json``,
-- mirroring ``project.py``'s ``_refresh_message_dims``. The single registered
-- "command" mart owns both ``command_mart`` and ``command_day_mart``, so no new
-- builder registration is needed; its overridden ``rebuild_from_scratch``
-- clears both tables.
--
-- Migration is **additive** — no existing table touched. A store that hasn't
-- re-run the refresh/backfill has an empty ``command_day_mart``; the read path
-- treats "no rows" as "fall back to the lifetime total" so the KPI keeps
-- working (just un-windowed) until the next refresh materialises the per-day
-- rows. The CREATE is idempotency-guarded by ``IF NOT EXISTS`` AND by
-- ``schema.py``'s ``_ADD_COLUMN_GUARDS`` ``("command_day_mart", "command_count")``
-- entry so a partial prior run (table created, ``user_version`` not bumped)
-- re-applies cleanly.

BEGIN;

CREATE TABLE IF NOT EXISTS command_day_mart (
    day            TEXT    NOT NULL,
    project_id     INTEGER NOT NULL,
    command_count  INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (day, project_id)
);

CREATE INDEX IF NOT EXISTS idx_command_day_mart_project
    ON command_day_mart(project_id, day);

PRAGMA user_version = 25;

COMMIT;
