-- v003: persist Anthropic priority/fast tier flag on the messages table.
--
-- PR #44 added Record.speed (in-process) and the (model, speed) cost path
-- through compute_cost(), but the SQLite store had no column for it. Every
-- consumer that recomputes cost from messages.* (the dashboard's
-- get_global_stats, services/compare.py, reports/export.py, anything that
-- reuses build_enriched_dataset) silently re-billed fast records at the
-- standard 1× rate. This column closes that gap.
--
-- Backfill is a no-op: every existing row gets 'standard' via the DEFAULT.
-- That's the conservative direction — under-charging a priority record at
-- standard rates is the bug we're fixing; the inverse (charging a standard
-- record at 6×) would be worse, so unknown rows stay at 'standard' until
-- a re-ingest pulls service_tier from raw_json.

BEGIN;

ALTER TABLE messages ADD COLUMN speed TEXT NOT NULL DEFAULT 'standard';

PRAGMA user_version = 3;

COMMIT;
