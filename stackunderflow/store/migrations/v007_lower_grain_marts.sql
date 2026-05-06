-- v007: Wave 5 lower-grain marts — `tool_mart` + `command_mart`.
--
-- See ``docs/specs/etl-architecture.md`` and HANDOFF §"What I'd do next"
-- item 1 for context. v006 shipped the foundational marts (daily,
-- session, project, provider_day, model_day) but deliberately deferred
-- the per-tool and per-command rollups so they could land as their own
-- migration once the foundation was proven.
--
-- These marts unblock:
--
--   * `/api/cost-data` `tool_costs` block — currently rebuilt from
--     `messages.tools_json` on every request via the aggregator path.
--   * `/api/optimize` patterns that today scan raw `messages` for
--     tool-call signals (bash_output_limits, junk_reads,
--     low_read_edit_ratio, ghost_agents). With `tool_mart` we can
--     short-circuit detectors when the implicated tool was never
--     called in the period.
--
-- The two marts are kept additive ON CONFLICT — same pattern as
-- daily_mart / provider_day_mart. `session_count` is the additive-mart
-- DISTINCT-count trap (see HANDOFF §"`session_count` correctness across
-- windows"); we follow the same recompute-for-affected-keys solution.
--
-- Migration is **additive** — no existing tables touched.

BEGIN;

-- ── tool_mart ────────────────────────────────────────────────────────────────
--
-- One row per (day, project_id, provider, tool_name). A single billable
-- event (assistant message in `usage_events`) may have used 0..N tools
-- — the mart builder fans the event out across the message's
-- `tools_json` so a Read+Edit message contributes one row to "Read" and
-- one row to "Edit". Cost is attributed 1/N across the distinct tool
-- names used by that message (mirroring `_ToolCostCollector` §1.3 in
-- `stats/aggregator.py`).
CREATE TABLE tool_mart (
    day             TEXT NOT NULL,
    project_id      INTEGER NOT NULL,
    provider        TEXT NOT NULL,
    tool_name       TEXT NOT NULL,
    event_count     INTEGER NOT NULL DEFAULT 0,
    cost_usd        REAL NOT NULL DEFAULT 0.0,
    tokens_in       INTEGER NOT NULL DEFAULT 0,
    tokens_out      INTEGER NOT NULL DEFAULT 0,
    session_count   INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (day, project_id, provider, tool_name)
);
CREATE INDEX idx_tool_mart_project ON tool_mart(project_id, day);
CREATE INDEX idx_tool_mart_tool    ON tool_mart(tool_name, day);

-- ── command_mart ────────────────────────────────────────────────────────────
--
-- One row per (day, project_id, command_name). `command_name` is the
-- leading slash-command of the user prompt that triggered the assistant
-- turn (e.g. `/init`, `/review`, `/help`), or the literal string
-- `freeform` for non-slash prompts. Cost / tokens are attributed to the
-- command via the chain "assistant event → its source message → walk
-- back through the same session to the most recent user message in
-- `messages`".
--
-- Note: user prompts themselves are NOT in `usage_events` (only
-- billable assistant rows are). The mart builder therefore JOINs
-- `usage_events` to `messages` and uses `seq` ordering on the same
-- session to attach each event to its parent prompt.
CREATE TABLE command_mart (
    day             TEXT NOT NULL,
    project_id      INTEGER NOT NULL,
    command_name    TEXT NOT NULL,
    event_count     INTEGER NOT NULL DEFAULT 0,
    cost_usd        REAL NOT NULL DEFAULT 0.0,
    tokens_in       INTEGER NOT NULL DEFAULT 0,
    tokens_out      INTEGER NOT NULL DEFAULT 0,
    session_count   INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (day, project_id, command_name)
);
CREATE INDEX idx_command_mart_project ON command_mart(project_id, day);
CREATE INDEX idx_command_mart_name    ON command_mart(command_name, day);

PRAGMA user_version = 7;

COMMIT;
