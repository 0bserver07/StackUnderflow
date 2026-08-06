-- v012: add `tool_mart.calls_total` — non-distinct tool-call count.
--
-- Closes HANDOFF item #6. `tool_mart.event_count` carries the *distinct*
-- ``(message, tool)`` pair count: a turn that called ``Read`` three times
-- contributes ``event_count += 1`` (one Read bucket per message — the
-- legacy ``_ToolCostCollector`` 1/N attribution contract). The pre-Wave-5
-- aggregator's ``tool_costs`` block reported ``calls`` as the *non-distinct*
-- count (3 Reads in one turn = 3 calls), so the mart overlay was a quiet
-- semantic change.
--
-- ``calls_total`` restores that signal alongside ``event_count`` so
-- consumers get a clean choice:
--
--   * ``event_count``  — distinct (message, tool) pairs (unchanged)
--   * ``calls_total``  — total tool occurrences across all messages (new)
--   * ``cost_usd``     — 1/N attribution per distinct tool (unchanged;
--                        cost must not double for repeated calls in one turn)
--
-- Migration is **additive** — no existing tables touched. Existing
-- ``tool_mart`` rows get ``calls_total = 0`` via the DEFAULT; a
-- ``MartBuilder.rebuild_from_scratch()`` (``stackunderflow etl backfill
-- --force``) re-derives both columns from the raw ``messages.tools_json``.
-- That stale-zero window is acceptable: no consumer needed ``calls_total``
-- before this migration, and the only path that produces stale rows
-- (v007's ``tool_mart`` populated, then v012 applied) is self-healing on
-- the next ``--force`` rebuild.

BEGIN;

ALTER TABLE tool_mart ADD COLUMN calls_total INTEGER NOT NULL DEFAULT 0;

PRAGMA user_version = 12;

COMMIT;
