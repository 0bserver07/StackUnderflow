-- v028: opt-in multi-device sync — device identity + push outbox (Phase 1 MVP).
--
-- Foundation tables for ``docs/specs/multi-device-sync.md`` Phase 1 (one-way,
-- client-side-encrypted backup of the analytics aggregates to the user's own
-- bucket). Only two tables ship in the MVP:
--
--   * ``sync_identity`` — single row (CHECK id = 1): this device's random UUID,
--     the encryption-key FINGERPRINT (never the secret — that lives in the
--     keychain / a 0600 file / an env var), and the destination bucket config.
--   * ``sync_outbox``   — per-shard PUSH watermark: the last content-hash we
--     uploaded for each ``(mart family, month)`` shard, so an unchanged shard is
--     skipped (idempotent push). ``sync_cursors`` / ``sync_remote_devices`` and
--     the ``_remote`` landing tables are Phase 2 (pull/merge) and NOT created here.
--
-- Migration is **additive** — no existing table is touched, so a store with
-- sync disabled (no ``sync_identity`` row) is byte-for-byte unchanged and every
-- existing query behaves exactly as before. Both CREATEs are ``IF NOT EXISTS``
-- and the loader's ``_ADD_COLUMN_GUARDS`` entry ``("sync_identity", "device_uuid")``
-- makes a partial prior run (table present, ``user_version`` behind) bump the
-- version without re-executing the body.

BEGIN;

-- Device identity + bucket config. Single row (id = 1).
CREATE TABLE IF NOT EXISTS sync_identity (
    id               INTEGER PRIMARY KEY CHECK (id = 1),
    device_uuid      TEXT NOT NULL,          -- random, minted at `sync init`
    key_fingerprint  TEXT NOT NULL,          -- fingerprint ONLY; secret never here
    bucket_url       TEXT NOT NULL,
    endpoint_url     TEXT,                   -- NULL = AWS default; set for R2/B2/MinIO
    layout_version   INTEGER NOT NULL DEFAULT 1,
    created_at       TEXT NOT NULL
);

-- Push watermark: what this device owes the bucket, keyed by logical shard.
CREATE TABLE IF NOT EXISTS sync_outbox (
    shard_key        TEXT PRIMARY KEY,       -- "daily_mart.2026-07"
    content_hash     TEXT,                   -- current local plaintext hash
    generation       INTEGER NOT NULL DEFAULT 0,
    dirty            INTEGER NOT NULL DEFAULT 1,
    last_pushed_hash TEXT,
    last_pushed_ts   TEXT
);

PRAGMA user_version = 28;

COMMIT;
