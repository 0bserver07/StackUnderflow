-- v017: PR / CI webhook ingest tables (Spec 20 — issue #92).
--
-- Two additive tables let the local store hold "PR Z merged at T, CI ran
-- W passed against commit Y". Combined with the session-to-commit link
-- (downstream — Spec 22), this is the data plumbing for "did session X
-- ship code that actually held up?". The heuristic that joins sessions
-- to commits is intentionally NOT in this migration — that's outcome
-- attribution v2 and lives in Spec 22.
--
-- Storage shape
-- -------------
-- ``pr_outcomes`` carries one row per (provider, repo_slug, pr_number).
-- ``state`` is the GitHub / GitLab PR lifecycle ('open' | 'merged' |
-- 'closed') as it stood at the time of the most recent webhook /
-- backfill. ``merged_at`` and ``reverted_at`` are ISO-8601 UTC; the
-- revert detection lives downstream (a "Revert ..." commit pointing at
-- this PR's merge commit, surfaced by Spec 22). ``raw_json`` carries
-- the full payload from the source so future schema changes can be
-- back-filled without re-fetching.
--
-- ``ci_runs`` carries one row per (provider, run_id). ``commit_sha`` is
-- the join key against the eventual session-to-commit map. ``status``
-- is normalised to a small enum ('success' | 'failure' | 'cancelled' |
-- 'in_progress' | 'pending' | 'skipped') so a reader doesn't have to
-- branch on provider-specific strings. The full payload lives in
-- ``raw_json``.
--
-- Why ``raw_json`` instead of more columns
-- ----------------------------------------
-- The webhook payloads are large and provider-specific. Materialising
-- a fixed column set now would lock us into one provider's vocabulary;
-- keeping the raw payload alongside the indexed fields lets follow-up
-- migrations promote new fields (PR labels, CI step durations, etc.)
-- without re-fetching the data from GitHub / GitLab.
--
-- Privacy / footprint
-- -------------------
-- Webhook payloads can carry committer emails + PR descriptions. Same
-- privacy posture as the rest of the store: ``~/.stackunderflow/store.db``
-- is local, never leaves the machine. A typical PR payload is ~6 KB so
-- 1000 PRs costs ~6 MB. CI payloads are smaller (~2 KB) — 5000 runs
-- ~10 MB.
--
-- Tokens are NEVER stored here. ``ingest github`` reads the GitHub PAT
-- from ``$STACKUNDERFLOW_GITHUB_TOKEN`` (or ``$GITHUB_TOKEN``) and the
-- webhook secret from ``$STACKUNDERFLOW_GITHUB_WEBHOOK_SECRET`` /
-- ``$STACKUNDERFLOW_GITLAB_WEBHOOK_SECRET``. Encrypted-at-rest token
-- storage is deferred to Spec 28.
--
-- Migration is **additive** — no existing tables touched. ``IF NOT
-- EXISTS`` guards make it safe to re-run after a crash that created
-- the tables but didn't bump ``PRAGMA user_version``.

BEGIN;

CREATE TABLE IF NOT EXISTS pr_outcomes (
    id           INTEGER PRIMARY KEY,
    provider     TEXT    NOT NULL,
    repo_slug    TEXT    NOT NULL,
    pr_number    INTEGER NOT NULL,
    title        TEXT,
    state        TEXT    NOT NULL,
    merged_at    TEXT,
    reverted_at  TEXT,
    author       TEXT,
    raw_json     TEXT    NOT NULL,
    UNIQUE (provider, repo_slug, pr_number)
);

CREATE TABLE IF NOT EXISTS ci_runs (
    id             INTEGER PRIMARY KEY,
    provider       TEXT    NOT NULL,
    repo_slug      TEXT    NOT NULL,
    run_id         TEXT    NOT NULL,
    commit_sha     TEXT    NOT NULL,
    status         TEXT    NOT NULL,
    workflow_name  TEXT,
    started_ts     TEXT,
    completed_ts   TEXT,
    raw_json       TEXT    NOT NULL,
    UNIQUE (provider, run_id)
);

CREATE INDEX IF NOT EXISTS idx_pr_outcomes_repo ON pr_outcomes(repo_slug, state);
CREATE INDEX IF NOT EXISTS idx_ci_runs_commit ON ci_runs(commit_sha);

PRAGMA user_version = 17;

COMMIT;
