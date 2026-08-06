-- v023: materialise the remaining "Overview shows 0 on the mart path" dims.
--
-- The mart fast-path (``routes/data._stats_from_marts``) serves the Overview /
-- Cost dashboard from mart reads in <100ms instead of the ~3.1s
-- ``get_project_stats`` pipeline. v022 closed the message-type + command-count
-- gap; four signals still read 0/empty on the mart path because NO mart carries
-- them (ui-perf-audit #20, plus the cache/interruption/error blocks):
--
--   * ``cache.hit_rate``        — messages_with_cache_read / assistant_messages
--   * ``interruption_rate``     — commands_followed_by_interruption / commands
--   * Steps/Cmd, Tools/Cmd      — total_assistant_steps / commands,
--                                 total_tools_used / commands
--   * ``errors`` total + rate + ``errors.by_category``
--   * per-tool cache cost (#20) — ``tool_mart`` had no cache-token columns, so
--                                 ToolCost's cache attribution was structurally 0
--
-- Like v022 these are computed once at mart-build time (``ProjectMartBuilder``
-- / ``ToolMartBuilder``) by running the SAME classifier → enricher → aggregator
-- logic ``get_project_stats`` uses over the project's ``messages.raw_json``,
-- then read back as indexed mart lookups. We store the COUNTS (numerators) and
-- derive the rates at read time from the already-materialised denominators so
-- the per-provider rows of a slug stay additive (``_merge_project_mart_rows``):
--
--   project_mart (per project_id):
--     total_records                            -- len(EnrichedDataset.records),
--                                                 every kind; errors.rate denom.
--                                                 (NOT total_messages, which is
--                                                 the billable-event count)
--     total_errors                             -- _ErrorsCollector._total
--     errors_by_category                       -- JSON {category: count}, the
--                                                 _ErrorsCollector by_category map
--     total_cache_read_messages                -- _CacheCollector.w_read
--                                                 (assistant rows w/ cache_read>0);
--                                                 hit_rate = / total_assistant_messages
--     total_commands_followed_by_interruption  -- _command_analysis int_followed;
--                                                 interruption_rate = / total_commands
--     total_command_tools                      -- _command_analysis total_tools_used;
--                                                 avg_tools_per_command = / total_commands
--     total_command_steps                      -- _command_analysis total_assistant_steps;
--                                                 avg_steps_per_command = / total_commands
--
--   tool_mart (per (day, project_id, provider, tool_name)):
--     cache_read   -- 1/N-attributed cache-read tokens   (mirrors tokens_in)
--     cache_create -- 1/N-attributed cache-create tokens (#20: ToolCost cache)
--
-- ``errors.by_category`` IS materialised (not skipped): ``classifier._detect_error``
-- runs from ``raw_json`` alone, so the category map is reproducible at build
-- time and stored as a small JSON blob. It's the one non-additive column here;
-- the read site (``_stats_from_marts``) merges the per-provider category maps
-- directly from the unmerged rows rather than through the additive-sum merge.
--
-- Multi-provider note: ``total_records``/``total_errors``/``total_cache_read_messages``
-- and the command counts are summed across a slug's per-provider mart rows at
-- read time. The message-grain counts (records, errors, cache-read messages) are
-- order-independent and sum EXACTLY to the combined-pipeline value. The
-- command-span counts (followed-by-interruption / tools / steps) are computed
-- per provider; they equal the combined ``get_project_stats([id,...])`` value
-- except in the rare case where two providers' messages interleave WITHIN a
-- single command's response span. Single-provider (the common case) is exact.
--
-- Migration is **additive** — no existing table touched. Existing rows get the
-- DEFAULT (0 / '{}') until the next mart refresh (or ``etl backfill --force``)
-- re-derives them. The ADD COLUMN statements are idempotency-guarded in
-- ``schema.py`` via the ``_ADD_COLUMN_GUARDS`` ``("project_mart",
-- "total_records")`` entry (the .sql runs in one transaction, so the columns
-- land all-or-nothing — guarding the first added column is sufficient).

BEGIN;

ALTER TABLE project_mart ADD COLUMN total_records                           INTEGER NOT NULL DEFAULT 0;
ALTER TABLE project_mart ADD COLUMN total_errors                            INTEGER NOT NULL DEFAULT 0;
ALTER TABLE project_mart ADD COLUMN errors_by_category                      TEXT    NOT NULL DEFAULT '{}';
ALTER TABLE project_mart ADD COLUMN total_cache_read_messages               INTEGER NOT NULL DEFAULT 0;
ALTER TABLE project_mart ADD COLUMN total_commands_followed_by_interruption INTEGER NOT NULL DEFAULT 0;
ALTER TABLE project_mart ADD COLUMN total_command_tools                     INTEGER NOT NULL DEFAULT 0;
ALTER TABLE project_mart ADD COLUMN total_command_steps                     INTEGER NOT NULL DEFAULT 0;

ALTER TABLE tool_mart ADD COLUMN cache_read   INTEGER NOT NULL DEFAULT 0;
ALTER TABLE tool_mart ADD COLUMN cache_create INTEGER NOT NULL DEFAULT 0;

PRAGMA user_version = 23;

COMMIT;
