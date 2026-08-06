-- v029: opt-in multi-device sync — pull cursors + remote landing tables (Phase 2).
--
-- Phase 2 of ``docs/specs/multi-device-sync.md`` (two-way, multi-device READ):
-- ``sync pull`` fetches every *other* device's encrypted aggregate shards, and
-- the ``sync/merge.py`` union overlay surfaces them behind ``?scope=all-devices``.
-- This migration lays the tables that pull writes and merge reads:
--
--   * ``sync_cursors``        — PULL watermark: per (remote device, shard) the
--     content-hash we last ingested, so an unchanged remote shard is skipped
--     (zero downloads — the mirror of ``sync_outbox`` on the push side).
--   * ``sync_remote_devices`` — known peers: alias, key fingerprint, first/last
--     seen, and the highest manifest ``generation`` we have accepted (the
--     monotonic-generation replay guard, §3.4).
--   * ``<mart>_remote``       — per-mart landing tables for the Overview/Cost
--     core (daily / provider_day / model_day / project / session). Each mirrors
--     its local mart's columns BUT replaces the machine-local ``project_id`` with
--     the stable ``(provider, slug)`` identity and adds a ``device_uuid``
--     provenance column. Pull REPLACEs a device's rows for each changed shard;
--     merge UNIONs local + remote and SUMs at the stable grain (§5.1).
--
-- Migration is **additive** — no existing table is touched, so a store with sync
-- disabled (no ``sync_identity`` row, no peers pulled) is byte-for-byte unchanged
-- and every existing query behaves exactly as before. Every CREATE is
-- ``IF NOT EXISTS``; the loader's ``_ADD_COLUMN_GUARDS`` entry
-- ``(29, ("sync_cursors", "remote_device_uuid"))`` makes a partial prior run
-- (tables present, ``user_version`` behind) bump the version without re-running
-- the body. The landing-table column order MUST match the serialized shard
-- columns (``sync/serialize.py`` ``_SPECS``); ``tests/.../sync`` pins that.

BEGIN;

-- ── pull watermark ────────────────────────────────────────────────────────────

-- Per remote device, per shard: the content-hash we last decrypted + landed.
-- Unchanged manifest hash ⇒ the shard download is skipped (idempotent pull).
CREATE TABLE IF NOT EXISTS sync_cursors (
    remote_device_uuid  TEXT NOT NULL,
    shard_key           TEXT NOT NULL,          -- "daily_mart.2026-07"
    remote_content_hash TEXT NOT NULL,
    pulled_at           TEXT NOT NULL,
    PRIMARY KEY (remote_device_uuid, shard_key)
);

-- Known peer devices + human aliases ("work-mac", "dev-box"). ``last_generation``
-- is the monotonic replay guard: a manifest whose generation is lower than this
-- is rejected (§3.4). Additive-only — a brand-new table, so adding the guard
-- column here alters nothing existing.
CREATE TABLE IF NOT EXISTS sync_remote_devices (
    remote_device_uuid TEXT PRIMARY KEY,
    alias              TEXT,
    key_fingerprint    TEXT,
    first_seen         TEXT NOT NULL,
    last_seen          TEXT NOT NULL,
    last_generation    INTEGER NOT NULL DEFAULT 0
);

-- ── remote landing tables (Overview/Cost core) ────────────────────────────────
--
-- Columns = ``device_uuid`` provenance + the re-keyed shard columns (local
-- ``project_id`` replaced by stable ``(provider, slug)``; ``session_mart.cwd``
-- dropped at serialize time — never on the wire). A remote device's rows are
-- REPLACE-on-pull per changed shard; the merge overlay reads these UNION ALL the
-- local mart and SUMs at the stable grain.

CREATE TABLE IF NOT EXISTS daily_mart_remote (
    device_uuid   TEXT NOT NULL,
    day           TEXT NOT NULL,
    provider      TEXT NOT NULL,
    slug          TEXT NOT NULL,              -- stable identity, NOT local project_id
    model         TEXT NOT NULL DEFAULT '',
    speed         TEXT NOT NULL DEFAULT 'standard',
    input_tokens  INTEGER NOT NULL DEFAULT 0,
    output_tokens INTEGER NOT NULL DEFAULT 0,
    cache_read    INTEGER NOT NULL DEFAULT 0,
    cache_create  INTEGER NOT NULL DEFAULT 0,
    message_count INTEGER NOT NULL DEFAULT 0,
    session_count INTEGER NOT NULL DEFAULT 0,
    cost_usd      REAL NOT NULL DEFAULT 0.0,
    PRIMARY KEY (device_uuid, provider, slug, day, model, speed)
);

CREATE TABLE IF NOT EXISTS provider_day_mart_remote (
    device_uuid   TEXT NOT NULL,
    day           TEXT NOT NULL,
    provider      TEXT NOT NULL,
    cost_usd      REAL NOT NULL DEFAULT 0.0,
    message_count INTEGER NOT NULL DEFAULT 0,
    session_count INTEGER NOT NULL DEFAULT 0,
    project_count INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (device_uuid, day, provider)
);

CREATE TABLE IF NOT EXISTS model_day_mart_remote (
    device_uuid   TEXT NOT NULL,
    day           TEXT NOT NULL,
    model         TEXT NOT NULL,
    speed         TEXT NOT NULL DEFAULT 'standard',
    cost_usd      REAL NOT NULL DEFAULT 0.0,
    input_tokens  INTEGER NOT NULL DEFAULT 0,
    output_tokens INTEGER NOT NULL DEFAULT 0,
    cache_read    INTEGER NOT NULL DEFAULT 0,
    cache_create  INTEGER NOT NULL DEFAULT 0,
    message_count INTEGER NOT NULL DEFAULT 0,
    session_count INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (device_uuid, day, model, speed)
);

CREATE TABLE IF NOT EXISTS project_mart_remote (
    device_uuid         TEXT NOT NULL,
    provider            TEXT NOT NULL,
    slug                TEXT NOT NULL,
    display_name        TEXT NOT NULL DEFAULT '',
    first_ts            TEXT,
    last_ts             TEXT,
    total_messages      INTEGER NOT NULL DEFAULT 0,
    total_sessions      INTEGER NOT NULL DEFAULT 0,
    total_input_tokens  INTEGER NOT NULL DEFAULT 0,
    total_output_tokens INTEGER NOT NULL DEFAULT 0,
    total_cache_read    INTEGER NOT NULL DEFAULT 0,
    total_cache_create  INTEGER NOT NULL DEFAULT 0,
    total_cost_usd      REAL NOT NULL DEFAULT 0.0,
    PRIMARY KEY (device_uuid, provider, slug)
);

CREATE TABLE IF NOT EXISTS session_mart_remote (
    device_uuid             TEXT NOT NULL,
    session_id              TEXT NOT NULL,      -- globally-unique UUID (no re-key)
    provider                TEXT NOT NULL,
    slug                    TEXT NOT NULL,
    primary_model           TEXT,
    first_ts                TEXT NOT NULL,
    last_ts                 TEXT NOT NULL,
    message_count           INTEGER NOT NULL DEFAULT 0,
    user_message_count      INTEGER NOT NULL DEFAULT 0,
    assistant_message_count INTEGER NOT NULL DEFAULT 0,
    input_tokens            INTEGER NOT NULL DEFAULT 0,
    output_tokens           INTEGER NOT NULL DEFAULT 0,
    cache_read              INTEGER NOT NULL DEFAULT 0,
    cache_create            INTEGER NOT NULL DEFAULT 0,
    cost_usd                REAL NOT NULL DEFAULT 0.0,
    is_one_shot             INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (device_uuid, session_id)
);

-- Merge-read helper indexes (stable-grain lookups for the union overlay).
CREATE INDEX IF NOT EXISTS idx_daily_mart_remote_grain
    ON daily_mart_remote(provider, slug, day);
CREATE INDEX IF NOT EXISTS idx_session_mart_remote_session
    ON session_mart_remote(session_id);

PRAGMA user_version = 29;

COMMIT;
