-- v019: Link commits to session (outcome attribution v2)
--
-- One row per (session_id, commit_sha) pair.
-- Maps which commits were generated during a session.
-- Combined with pr_outcomes and ci_runs, this enables querying the outcomes
-- of changes authored by AI coding sessions.

BEGIN;

CREATE TABLE IF NOT EXISTS commit_session_link (
    id              INTEGER PRIMARY KEY,
    session_id      TEXT    NOT NULL,
    commit_sha      TEXT    NOT NULL,
    repo_slug       TEXT    NOT NULL,
    committed_at    TEXT    NOT NULL,
    UNIQUE (session_id, commit_sha)
);

CREATE INDEX IF NOT EXISTS idx_commit_session_sha
    ON commit_session_link(commit_sha);

CREATE INDEX IF NOT EXISTS idx_commit_session_id
    ON commit_session_link(session_id);

PRAGMA user_version = 19;

COMMIT;
