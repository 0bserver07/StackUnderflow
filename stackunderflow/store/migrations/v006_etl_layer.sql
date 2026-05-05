-- v006: ETL foundation — usage_events fact table + 5 marts + watermark.
--
-- See ``docs/specs/etl-architecture.md`` for the full design. Migration
-- is **additive**: it does not touch existing ``messages`` / ``sessions`` /
-- ``projects`` / ``ingest_log`` tables. Existing routes and aggregator
-- code keep working unchanged. Wave 1 only lays the schema; Waves 2 and
-- 3 fill in the normalizers, mart builders, watcher and route migrations.
--
-- Note on numbering: the spec refers to this as ``v004_etl_layer.sql``,
-- but two migrations (v004 synthetic-models cleanup, v005 cursor-workspace
-- redistribute) shipped between the spec being written and Wave 1 landing.
-- The migration is therefore wired in as v006 — the spec doc is updated
-- to match.

BEGIN;

-- ── canonical fact table ────────────────────────────────────────────────────
--
-- One row per billable event. ``source_message_fk`` is the dedup key —
-- re-running normalization for an already-converted ``messages`` row is
-- a no-op (UNIQUE constraint + ON CONFLICT handling in the normalizer).
CREATE TABLE usage_events (
    id                  INTEGER PRIMARY KEY,
    -- provenance
    source_message_fk   INTEGER NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
    provider            TEXT    NOT NULL,
    account             TEXT    NOT NULL DEFAULT 'default',
    project_id          INTEGER NOT NULL REFERENCES projects(id),
    session_id          TEXT    NOT NULL,
    -- temporal
    ts                  TEXT    NOT NULL,
    day                 TEXT    NOT NULL,
    -- model + tier
    model               TEXT    NOT NULL DEFAULT '',
    speed               TEXT    NOT NULL DEFAULT 'standard',
    -- canonical 4-token shape (Anthropic-style)
    input_tokens        INTEGER NOT NULL DEFAULT 0,
    output_tokens       INTEGER NOT NULL DEFAULT 0,
    cache_read_tokens   INTEGER NOT NULL DEFAULT 0,
    cache_create_tokens INTEGER NOT NULL DEFAULT 0,
    -- cost (computed during normalization, stored)
    cost_usd            REAL    NOT NULL DEFAULT 0.0,
    cost_source         TEXT    NOT NULL DEFAULT 'rate_card',
    -- structural
    role                TEXT    NOT NULL,
    -- extensibility — JSON; provider-specific fields preserved verbatim
    raw_extras          TEXT
);

CREATE INDEX idx_events_day        ON usage_events(day);
CREATE INDEX idx_events_project    ON usage_events(project_id, day);
CREATE INDEX idx_events_provider   ON usage_events(provider, day);
CREATE INDEX idx_events_session    ON usage_events(session_id);
CREATE INDEX idx_events_model      ON usage_events(model, day);
CREATE UNIQUE INDEX uniq_events_msg ON usage_events(source_message_fk);

-- ── marts ───────────────────────────────────────────────────────────────────
--
-- Each mart owns its rebuild SQL. No mart depends on another. Watermarks
-- are tracked separately in ``mart_watermark`` so each can refresh
-- independently.

CREATE TABLE daily_mart (
    day               TEXT NOT NULL,
    project_id        INTEGER NOT NULL,
    provider          TEXT NOT NULL,
    model             TEXT NOT NULL DEFAULT '',
    speed             TEXT NOT NULL DEFAULT 'standard',
    input_tokens      INTEGER NOT NULL DEFAULT 0,
    output_tokens     INTEGER NOT NULL DEFAULT 0,
    cache_read        INTEGER NOT NULL DEFAULT 0,
    cache_create      INTEGER NOT NULL DEFAULT 0,
    message_count     INTEGER NOT NULL DEFAULT 0,
    session_count     INTEGER NOT NULL DEFAULT 0,
    cost_usd          REAL NOT NULL DEFAULT 0.0,
    PRIMARY KEY (day, project_id, provider, model, speed)
);
CREATE INDEX idx_daily_mart_project ON daily_mart(project_id, day);

CREATE TABLE session_mart (
    session_id              TEXT PRIMARY KEY,
    project_id              INTEGER NOT NULL,
    provider                TEXT NOT NULL,
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
    cwd                     TEXT
);
CREATE INDEX idx_session_mart_project ON session_mart(project_id);
CREATE INDEX idx_session_mart_first   ON session_mart(first_ts);

CREATE TABLE project_mart (
    project_id           INTEGER PRIMARY KEY,
    provider             TEXT NOT NULL,
    slug                 TEXT NOT NULL,
    display_name         TEXT NOT NULL,
    first_ts             TEXT,
    last_ts              TEXT,
    total_messages       INTEGER NOT NULL DEFAULT 0,
    total_sessions       INTEGER NOT NULL DEFAULT 0,
    total_input_tokens   INTEGER NOT NULL DEFAULT 0,
    total_output_tokens  INTEGER NOT NULL DEFAULT 0,
    total_cache_read     INTEGER NOT NULL DEFAULT 0,
    total_cache_create   INTEGER NOT NULL DEFAULT 0,
    total_cost_usd       REAL NOT NULL DEFAULT 0.0
);

CREATE TABLE provider_day_mart (
    day             TEXT NOT NULL,
    provider        TEXT NOT NULL,
    cost_usd        REAL NOT NULL DEFAULT 0.0,
    message_count   INTEGER NOT NULL DEFAULT 0,
    session_count   INTEGER NOT NULL DEFAULT 0,
    project_count   INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (day, provider)
);
CREATE INDEX idx_provider_day_mart_day ON provider_day_mart(day);

CREATE TABLE model_day_mart (
    day             TEXT NOT NULL,
    model           TEXT NOT NULL,
    speed           TEXT NOT NULL DEFAULT 'standard',
    cost_usd        REAL NOT NULL DEFAULT 0.0,
    input_tokens    INTEGER NOT NULL DEFAULT 0,
    output_tokens   INTEGER NOT NULL DEFAULT 0,
    cache_read      INTEGER NOT NULL DEFAULT 0,
    cache_create    INTEGER NOT NULL DEFAULT 0,
    message_count   INTEGER NOT NULL DEFAULT 0,
    session_count   INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (day, model, speed)
);

CREATE TABLE mart_watermark (
    mart_name        TEXT PRIMARY KEY,
    last_event_id    INTEGER NOT NULL DEFAULT 0,
    last_refresh_ts  TEXT NOT NULL
);

PRAGMA user_version = 6;

COMMIT;
