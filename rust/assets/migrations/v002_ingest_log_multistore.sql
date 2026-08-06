-- v002: extend ingest_log to support per-session, database-backed sources.
--
-- Old shape used file_path as a primary key with a single processed_offset
-- (byte position into a JSONL file). The multi-provider work introduces
-- vscdb / SQLite-backed sources that resume by rowid and need to track
-- progress per (file_path, session_id) so a single .vscdb can host many
-- conversations. See docs/specs/multi-provider/spec.md §1.2.
--
-- Migration strategy: ALTER-via-rebuild. Existing rows preserve their
-- file path / mtime / size / processed_offset and get session_id=NULL,
-- storage_kind='file', last_rowid=NULL.

BEGIN;

CREATE TABLE ingest_log_new (
    id                 INTEGER PRIMARY KEY,
    file_path          TEXT NOT NULL,
    provider           TEXT NOT NULL,
    session_id         TEXT,
    storage_kind       TEXT NOT NULL DEFAULT 'file'
        CHECK (storage_kind IN ('file', 'database')),
    mtime              REAL NOT NULL,
    size               INTEGER NOT NULL,
    processed_offset   INTEGER,
    last_rowid         INTEGER,
    last_ingest_ts     REAL,
    UNIQUE (file_path, session_id)
);

INSERT INTO ingest_log_new (
    file_path, provider, session_id, storage_kind,
    mtime, size, processed_offset, last_rowid, last_ingest_ts
)
SELECT
    file_path, provider, NULL, 'file',
    mtime, size, processed_offset, NULL, last_ingest_ts
FROM ingest_log;

DROP TABLE ingest_log;
ALTER TABLE ingest_log_new RENAME TO ingest_log;

-- SQLite treats NULL as distinct in UNIQUE constraints, so the table-level
-- UNIQUE(file_path, session_id) does NOT prevent duplicate file-mode rows
-- (where session_id IS NULL). Two partial unique indexes give us the
-- enforcement we actually want:
--   * file-mode  → one row per file_path
--   * database-mode → one row per (file_path, session_id)
CREATE UNIQUE INDEX idx_ingest_log_file_unique
    ON ingest_log(file_path)
    WHERE session_id IS NULL;

CREATE UNIQUE INDEX idx_ingest_log_db_unique
    ON ingest_log(file_path, session_id)
    WHERE session_id IS NOT NULL;

PRAGMA user_version = 2;

COMMIT;
