-- v001: initial schema
BEGIN;

CREATE TABLE projects (
  id             INTEGER PRIMARY KEY,
  provider       TEXT NOT NULL,
  slug           TEXT NOT NULL,
  path           TEXT,
  display_name   TEXT NOT NULL,
  first_seen     REAL NOT NULL,
  last_modified  REAL NOT NULL,
  UNIQUE (provider, slug)
);

CREATE TABLE sessions (
  id             INTEGER PRIMARY KEY,
  project_id     INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
  session_id     TEXT NOT NULL,
  first_ts       TEXT,
  last_ts        TEXT,
  message_count  INTEGER NOT NULL DEFAULT 0,
  UNIQUE (project_id, session_id)
);

CREATE TABLE messages (
  id                    INTEGER PRIMARY KEY,
  session_fk            INTEGER NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
  seq                   INTEGER NOT NULL,
  timestamp             TEXT NOT NULL,
  role                  TEXT NOT NULL,
  model                 TEXT,
  input_tokens          INTEGER NOT NULL DEFAULT 0,
  output_tokens         INTEGER NOT NULL DEFAULT 0,
  cache_create_tokens   INTEGER NOT NULL DEFAULT 0,
  cache_read_tokens     INTEGER NOT NULL DEFAULT 0,
  content_text          TEXT NOT NULL DEFAULT '',
  tools_json            TEXT NOT NULL DEFAULT '[]',
  raw_json              TEXT NOT NULL,
  is_sidechain          INTEGER NOT NULL DEFAULT 0,
  uuid                  TEXT,
  parent_uuid           TEXT,
  UNIQUE (session_fk, seq)
);

CREATE TABLE ingest_log (
  file_path          TEXT PRIMARY KEY,
  provider           TEXT NOT NULL,
  mtime              REAL NOT NULL,
  size               INTEGER NOT NULL,
  processed_offset   INTEGER NOT NULL,
  last_ingest_ts     REAL NOT NULL
);

CREATE INDEX idx_messages_session_seq  ON messages(session_fk, seq);
CREATE INDEX idx_messages_timestamp    ON messages(timestamp);
CREATE INDEX idx_messages_model        ON messages(model);
CREATE INDEX idx_sessions_project      ON sessions(project_id);
CREATE INDEX idx_sessions_last_ts      ON sessions(last_ts);

PRAGMA user_version = 1;

COMMIT;
