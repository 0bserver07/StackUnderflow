-- The mart + sync schema, extracted VERBATIM from the maintainer's live
-- store (schema v030) plus the v028/v029 sync migrations. Not hand-written:
-- a fixture whose DDL drifts from the real one proves nothing about the real
-- one. Regenerate with:
--   sqlite3 "file:$STORE?mode=ro" '.schema projects' '.schema daily_mart' ...
-- and the two migration files under stackunderflow/store/migrations/.

CREATE TABLE projects (
  id             INTEGER PRIMARY KEY,
  provider       TEXT NOT NULL,
  slug           TEXT NOT NULL,
  path           TEXT,
  display_name   TEXT NOT NULL,
  first_seen     REAL NOT NULL,
  last_modified  REAL NOT NULL, worktree_of TEXT,
  UNIQUE (provider, slug)
);
CREATE INDEX idx_projects_slug
    ON projects(slug);
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
, total_user_messages        INTEGER NOT NULL DEFAULT 0, total_assistant_messages   INTEGER NOT NULL DEFAULT 0, total_tool_use_messages    INTEGER NOT NULL DEFAULT 0, total_tool_result_messages INTEGER NOT NULL DEFAULT 0, total_commands             INTEGER NOT NULL DEFAULT 0, total_records                           INTEGER NOT NULL DEFAULT 0, total_errors                            INTEGER NOT NULL DEFAULT 0, errors_by_category                      TEXT    NOT NULL DEFAULT '{}', total_cache_read_messages               INTEGER NOT NULL DEFAULT 0, total_commands_followed_by_interruption INTEGER NOT NULL DEFAULT 0, total_command_tools                     INTEGER NOT NULL DEFAULT 0, total_command_steps                     INTEGER NOT NULL DEFAULT 0);
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



CREATE TABLE IF NOT EXISTS sync_identity (
    id               INTEGER PRIMARY KEY CHECK (id = 1),
    device_uuid      TEXT NOT NULL,          -- random, minted at `sync init`
    key_fingerprint  TEXT NOT NULL,          -- fingerprint ONLY; secret never here
    bucket_url       TEXT NOT NULL,
    endpoint_url     TEXT,                   -- NULL = AWS default; set for R2/B2/MinIO
    layout_version   INTEGER NOT NULL DEFAULT 1,
    created_at       TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS sync_outbox (
    shard_key        TEXT PRIMARY KEY,       -- "daily_mart.2026-07"
    content_hash     TEXT,                   -- current local plaintext hash
    generation       INTEGER NOT NULL DEFAULT 0,
    dirty            INTEGER NOT NULL DEFAULT 1,
    last_pushed_hash TEXT,
    last_pushed_ts   TEXT
);





CREATE TABLE IF NOT EXISTS sync_cursors (
    remote_device_uuid  TEXT NOT NULL,
    shard_key           TEXT NOT NULL,          -- "daily_mart.2026-07"
    remote_content_hash TEXT NOT NULL,
    pulled_at           TEXT NOT NULL,
    PRIMARY KEY (remote_device_uuid, shard_key)
);

CREATE TABLE IF NOT EXISTS sync_remote_devices (
    remote_device_uuid TEXT PRIMARY KEY,
    alias              TEXT,
    key_fingerprint    TEXT,
    first_seen         TEXT NOT NULL,
    last_seen          TEXT NOT NULL,
    last_generation    INTEGER NOT NULL DEFAULT 0
);


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

CREATE INDEX IF NOT EXISTS idx_daily_mart_remote_grain
    ON daily_mart_remote(provider, slug, day);
CREATE INDEX IF NOT EXISTS idx_session_mart_remote_session
    ON session_mart_remote(session_id);


