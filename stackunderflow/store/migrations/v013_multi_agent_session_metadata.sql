-- v013: multi-agent session metadata (Claude Code agent teams).
--
-- See ``.notes/specs/09-multi-agent-fs-recognition.md`` for the design
-- rationale. Round 2 reconstructed the agent-team graph at the service
-- layer by scanning ``messages.is_sidechain`` + parsing ``raw_json`` for
-- ``teamName`` / ``agentId`` on every dashboard render. That works but is
-- slow (raw_json parsing on the hot path), lossy (the on-disk team
-- metadata under ``~/.claude/teams/`` and the task assignments under
-- ``~/.claude/tasks/`` were never ingested), and fragile (cross-file
-- ``parent_uuid`` resolution is heuristic).
--
-- This migration materialises the graph in the schema so the service
-- layer can JOIN instead of scan:
--
--   * four additive nullable columns on ``sessions`` carrying the
--     per-session team affiliation (``team_id``), the spawning session
--     (``spawned_by_session_id``), the original spawn prompt
--     (``spawn_prompt`` — richer than the sub-agent's own first user
--     message), and the role within the team (``agent_role``: ``lead`` |
--     ``subagent`` | NULL for a regular non-team session).
--   * a new ``agent_teams`` table with one row per Claude Code team,
--     keyed on the team name, carrying the project, creation timestamp,
--     description, lead session, and the raw ``config.json`` blob.
--
-- Adapters other than Claude leave the new ``sessions`` columns NULL —
-- Codex / Cursor / Cline have no equivalent team primitive.
--
-- Sessions ingested before this migration runs keep NULL team metadata
-- until the next ingest cycle re-materialises it (the ingest pass calls
-- ``adapters.claude_teams.materialize_team_metadata``); we do NOT
-- auto-backfill here.
--
-- Migration is **additive** — no existing tables touched. The four
-- ``ALTER TABLE`` statements are idempotency-guarded in ``schema.py`` via
-- the ``_ADD_COLUMN_GUARDS`` ``("sessions", "team_id")`` entry.

BEGIN;

-- ── sessions: per-session team affiliation ──────────────────────────────────
ALTER TABLE sessions ADD COLUMN team_id               TEXT;
ALTER TABLE sessions ADD COLUMN spawned_by_session_id TEXT;
ALTER TABLE sessions ADD COLUMN spawn_prompt          TEXT;
ALTER TABLE sessions ADD COLUMN agent_role            TEXT;

CREATE INDEX idx_sessions_team       ON sessions(team_id)
    WHERE team_id IS NOT NULL;
CREATE INDEX idx_sessions_spawned_by ON sessions(spawned_by_session_id)
    WHERE spawned_by_session_id IS NOT NULL;

-- ── agent_teams: one row per Claude Code team ───────────────────────────────
--
-- ``team_id`` is Claude Code's team name (the directory name under
-- ``~/.claude/teams/``). ``config_json`` is the verbatim ``config.json``
-- blob so downstream consumers (outcome-aware discovery, auto-skill
-- synthesis) can mine member prompts / models without re-reading the
-- filesystem. ``lead_session_id`` is the team config's ``leadSessionId``
-- — nullable because a team whose lead transcript hasn't been ingested
-- yet still gets a row (the lead links up on a later ingest pass).
CREATE TABLE agent_teams (
    team_id          TEXT PRIMARY KEY,
    project_id       INTEGER NOT NULL REFERENCES projects(id),
    created_ts       TEXT NOT NULL,
    description      TEXT,
    lead_session_id  TEXT,
    config_json      TEXT NOT NULL
);
CREATE INDEX idx_agent_teams_project ON agent_teams(project_id);

PRAGMA user_version = 13;

COMMIT;
