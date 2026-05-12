-- v011: per-message-grain mart — `message_tool_mart`.
--
-- See ``.notes/specs/07-per-message-marts.md`` (and HANDOFF follow-up #2).
-- The seven marts shipped to date (daily, session, project, provider_day,
-- model_day, tool, command) are all *aggregate*-grain — they roll the
-- per-message `tools_json` up to `(day, project)` / `(session)` keys. The
-- ``optimize.py`` detectors (`junk_reads`, `bash_output_limits`,
-- `low_read_edit_ratio`, `ghost_agents`) need finer signal — file paths
-- read/written, byte counts, per-tool-call sequences — and currently
-- re-parse `messages.raw_json` directly. Post-v008 (`messages` is a
-- UNION-ALL view over monthly partitions) that scan fans out across every
-- partition on each call; ``message_tool_mart`` replaces it with a single
-- indexed lookup.
--
-- Grain: one row per `(message, tool_name, call_index)` triple. A single
-- assistant message that calls Read three times and Edit once produces
-- four rows: Read#0, Read#1, Read#2, Edit#0 — `call_index` is 0-based
-- *within the message, per tool name* (so `UNIQUE(message_id, tool_name,
-- call_index)` is the dedup key the builder's `INSERT OR IGNORE` relies on).
--
-- Pattern: per-entity (each `usage_events` row → 0..N mart rows). The
-- builder watermarks on `usage_events.id` (not `messages` — that's a view
-- post-v008 and can't be watermarked directly), JOINs each event back to
-- its source message, parses `raw_json` for `tool_use` blocks, and emits
-- one row per call. `byte_count` is the size of the write payload for
-- write-family tools (Write→content, Edit→new_string, MultiEdit→Σ
-- new_string, NotebookEdit→new_source) and the size of the tool *result*
-- — pulled from the immediately-following message's `tool_result` block,
-- matched on `tool_use_id` — for output-producing tools (Bash, Read,
-- Grep, ...). `file_path` is the obvious input key (`file_path` / `path`
-- / `notebook_path`); for `Task` it carries the `subagent_type` so the
-- ghost-agent detector can read invoked agents straight off the mart.
--
-- Migration is **additive** — no existing tables touched.
--
-- Numbering note: the spec assumes v009 + v010 land first (specs 04 + 05,
-- built in parallel). On a store that applies migrations in order the gap
-- is harmless — `schema.apply` runs every migration whose number exceeds
-- `PRAGMA user_version`, so v009/v010/v011 chain correctly whatever order
-- they merge in.

BEGIN;

CREATE TABLE IF NOT EXISTS message_tool_mart (
    id              INTEGER PRIMARY KEY,
    message_id      INTEGER NOT NULL,            -- references messages.id (no FK; messages is a view post-v008)
    project_id      INTEGER NOT NULL REFERENCES projects(id),
    session_id      TEXT    NOT NULL,
    ts              TEXT    NOT NULL,
    day             TEXT    NOT NULL,            -- YYYY-MM-DD for partition affinity
    tool_name       TEXT    NOT NULL,            -- "Read" | "Edit" | "Write" | "Bash" | "Task" | ...
    file_path       TEXT,                         -- NULL when not applicable (e.g. Bash without a path arg)
    byte_count      INTEGER,                      -- write payload size, or tool-result size for output tools; nullable
    call_index      INTEGER NOT NULL,            -- 0-based index of this call within the message, per tool_name
    UNIQUE (message_id, tool_name, call_index)
);
CREATE INDEX IF NOT EXISTS idx_message_tool_mart_session    ON message_tool_mart(session_id);
CREATE INDEX IF NOT EXISTS idx_message_tool_mart_project    ON message_tool_mart(project_id, day);
CREATE INDEX IF NOT EXISTS idx_message_tool_mart_file       ON message_tool_mart(file_path);
CREATE INDEX IF NOT EXISTS idx_message_tool_mart_tool_day   ON message_tool_mart(tool_name, day);

PRAGMA user_version = 11;

COMMIT;
