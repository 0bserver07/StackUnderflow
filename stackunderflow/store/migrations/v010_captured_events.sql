-- v010: `captured_events` — opt-in hybrid-capture hook sink.
--
-- See ``.notes/specs/05-hybrid-capture-hooks.md`` and ``docs/hooks.md``.
--
-- Claude Code lifecycle hooks (``PostToolUse``, ``UserPromptSubmit``,
-- ``Stop``, ``PreCompact``) — installed *only* when the user runs
-- ``stackunderflow hooks install`` — write one row here per interesting
-- event:
--
--   * ``failure``    — a Bash tool call exited non-zero (PostToolUse)
--   * ``correction`` — a user prompt matched the correction heuristic
--                      (UserPromptSubmit); the prompt text is NOT stored
--   * ``boundary``   — Claude finished a turn (Stop); session-totals snapshot
--   * ``snapshot``   — pre-compaction snapshot (PreCompact)
--
-- ``payload_json`` is sanitised by default (hook metadata + tool name +
-- exit code only, never raw prompt / stdout / stderr). Users who pass
-- ``stackunderflow hooks install --capture-content`` get the full payload.
--
-- Outcome-aware discovery (spec 01) reads this table for deterministic
-- failure/correction outcomes, falling back to its transcript heuristic
-- when the table is empty (hook-less installs). No producer dependency
-- between the two specs.
--
-- ``CREATE TABLE IF NOT EXISTS`` so the migration coexists with the
-- handler's own ``ensure_captured_events_table`` safety net (a user can
-- install hooks and start capturing before the dashboard ever runs
-- ``schema.apply``; the handler creates the table on first fire without
-- touching ``user_version`` or any other table). Both paths create the
-- identical shape.
--
-- Migration is **additive** — no existing tables touched.

BEGIN;

CREATE TABLE IF NOT EXISTS captured_events (
    id              INTEGER PRIMARY KEY,
    ts              TEXT NOT NULL,          -- ISO 8601 UTC, sub-second
    project_id      INTEGER,                -- nullable: best-effort cwd→slug match
    session_id      TEXT,                   -- from the hook payload, if present
    hook_id         TEXT NOT NULL,          -- e.g. 'stackunderflow-post-tool-use'
    event_kind      TEXT NOT NULL,          -- 'failure' | 'correction' | 'boundary' | 'snapshot'
    payload_json    TEXT NOT NULL,          -- sanitised hook payload (or full, with --capture-content)
    UNIQUE (ts, hook_id, session_id)
);
CREATE INDEX IF NOT EXISTS idx_captured_events_session ON captured_events(session_id);
CREATE INDEX IF NOT EXISTS idx_captured_events_kind    ON captured_events(event_kind, ts);

PRAGMA user_version = 10;

COMMIT;
