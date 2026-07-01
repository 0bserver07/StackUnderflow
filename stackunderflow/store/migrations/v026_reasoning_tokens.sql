-- v026: separate reasoning-token attribution on ``usage_events``.
--
-- Reasoning / "thinking" tokens are, for every provider we ingest, BILLED AS
-- OUTPUT — Anthropic thinking blocks, OpenAI ``reasoning_output_tokens``, and
-- Droid ``thinkingTokens`` are all already summed into ``output_tokens`` so the
-- stored ``cost_usd`` is correct. That folding, though, DESTROYS the
-- attribution: once reasoning is inside ``output_tokens`` you can no longer ask
-- "what share of my output spend was reasoning?".
--
-- This column restores that attribution WITHOUT changing any cost total.
-- ``reasoning_tokens`` is an ADDITIVE-metadata SUBSET of ``output_tokens`` (it
-- is NOT summed into cost — the pricer only ever reads the canonical four token
-- columns), so:
--
--   * ``cost_usd`` is untouched by this migration and by the capture that
--     populates the column — reasoning was, and stays, folded into output for
--     billing.
--   * ``reasoning_tokens <= output_tokens`` for every row that carries a real
--     count; providers with no measurable reasoning (Grok — encrypted; Claude —
--     no separate wire count) leave it at 0.
--
-- Migration is **additive** — a single ``ALTER TABLE ADD COLUMN`` with a
-- ``DEFAULT 0`` so every existing row backfills to "no reasoning attributed
-- yet" (the honest state — the historical rows were normalised before capture
-- existed). New events populate it going forward via the normalizers /
-- ``ingest.writer``. Idempotency-guarded by ``schema.py``'s
-- ``_ADD_COLUMN_GUARDS`` ``("usage_events", "reasoning_tokens")`` entry so a
-- partial prior run (column added, ``user_version`` not bumped) re-applies
-- cleanly instead of erroring on "duplicate column".

BEGIN;

ALTER TABLE usage_events ADD COLUMN reasoning_tokens INTEGER NOT NULL DEFAULT 0;

PRAGMA user_version = 26;

COMMIT;
