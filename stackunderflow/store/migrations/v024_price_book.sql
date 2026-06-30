-- v024: unify pricing into a single effective-dated ``price_book`` table.
--
-- Until now model rates lived in THREE places with an implicit resolution
-- order (``docs`` / audit #2):
--
--   1. ``infra/costs.py``                 — the ``RATE_CARD`` dict + ``_CANONICAL_IDS``
--   2. ``data/models.toml`` (manifest)    — effective-dated rows, the real source
--   3. ``services/pricing_service.py``    — the LiteLLM "live" overlay (JSON cache)
--
-- This migration adds the persistent home that the manifest + rate card back-fill
-- into and the live overlay APPENDS dated snapshots into. The lookup
-- (``infra/model_manifest.price_book_lookup``) reads it with the SAME precedence
-- as before (live > rate_card/manifest) and falls back to the in-code manifest
-- when the book is empty (a fresh store), so cost numbers are unchanged.
--
-- One row per (provider, model, effective_from, source) effective-dated rate.
-- Rates are stored in the manifest's $/M-tokens unit (NOT per-token), matching
-- ``model_manifest.rates_for`` so a book hit is byte-for-byte the in-code value.
--
--   provider         pricer key ("anthropic", "openai", ...) — the value
--                    ``etl/normalize/base._provider_for`` resolves to.
--   model            the concrete model id we price under (e.g. ``claude-opus-4-8``);
--                    for manifest families also the family-representative ids.
--   effective_from   ISO ``YYYY-MM-DD`` the rate took effect, or '' for
--                    always-current (NULL would break the UNIQUE dedup since
--                    SQLite treats NULLs as distinct).
--   effective_until  ISO ``YYYY-MM-DD`` the rate stopped applying (exclusive),
--                    or '' for open-ended / still-current.
--   input/output/cache_write/cache_read  $/M tokens.
--   source           'manifest' | 'rate_card' | 'live' — provenance + precedence
--                    tiebreak (live wins).
--   updated_at       unix epoch seconds of the last write (live snapshots
--                    overwrite their same-key row).
--
-- Migration is **additive** — no existing table is touched, and an empty
-- ``price_book`` is the fresh-store state the lookup falls back through. The
-- CREATE is idempotency-guarded by ``IF NOT EXISTS`` AND by ``schema.py``'s
-- ``_ADD_COLUMN_GUARDS`` ``("price_book", "model")`` entry so a partial prior
-- run (table created, ``user_version`` not bumped) re-applies cleanly.

BEGIN;

CREATE TABLE IF NOT EXISTS price_book (
    id              INTEGER PRIMARY KEY,
    provider        TEXT    NOT NULL,
    model           TEXT    NOT NULL,
    effective_from  TEXT    NOT NULL DEFAULT '',
    effective_until TEXT    NOT NULL DEFAULT '',
    input           REAL    NOT NULL DEFAULT 0.0,
    output          REAL    NOT NULL DEFAULT 0.0,
    cache_write     REAL    NOT NULL DEFAULT 0.0,
    cache_read      REAL    NOT NULL DEFAULT 0.0,
    source          TEXT    NOT NULL DEFAULT 'manifest',
    updated_at      REAL    NOT NULL DEFAULT 0.0,
    UNIQUE (provider, model, effective_from, source)
);

-- Lookup index: resolve (model, provider) then pick the effective row by date.
CREATE INDEX IF NOT EXISTS idx_price_book_lookup
    ON price_book(provider, model, effective_from);

PRAGMA user_version = 24;

COMMIT;
