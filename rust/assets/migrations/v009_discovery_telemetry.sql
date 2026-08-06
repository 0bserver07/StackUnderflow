-- v009: Citation-feedback loop on discovery — telemetry table.
--
-- See ``.notes/specs/04-citation-feedback-loop.md`` for the full design.
--
-- The three discovery commands (``find-sessions-in-path``,
-- ``find-sessions-touching-file``, ``search-past-decisions``) rank
-- surfaced sessions purely on metadata (recency, cost) — there's no
-- signal from "did this result actually help". This table closes the
-- loop:
--
--   * every time a session is surfaced by a discovery command we bump
--     ``loaded_count`` (see ``services.discovery_telemetry.record_loaded``);
--   * every time an agent looks one up via the ``session_query`` MCP tool
--     (or a future ``stackunderflow sessions show``) we bump ``cited_count``
--     (``record_cited``).
--
-- ``cite_rate = cited_count / loaded_count`` then feeds the
-- token-budgeted ranking (spec 03's ``pack_within_budget``) so sessions
-- that consistently earn citations climb and uncited noise sinks. The
-- companion ``stackunderflow discovery demote-uncited`` sweep flags
-- sessions surfaced N+ times over M+ days that were never cited.
--
-- Two columns extend the spec sketch so the spec's own ``demote_candidates``
-- function and the ``demote-uncited`` CLI are actually functional:
--
--   * ``first_loaded_ts`` — when the session first entered the discovery
--     surface, needed to express "N loads over M+ days".
--   * ``demoted`` — sticky flag the ``demote-uncited`` sweep sets; the
--     ranking term zeroes the cite contribution for demoted sessions so
--     they drop out of default ranking but stay reachable via direct
--     lookup.
--
-- Privacy: this table stores session ids + counters only — no transcript
-- content. It stays local (it's in ``~/.stackunderflow/store.db`` like
-- everything else). Telemetry writes are gated at the call site behind
-- ``STACKUNDERFLOW_DISCOVERY_TELEMETRY`` (default on; set to ``0`` to
-- disable for ephemeral / scripted use).
--
-- Migration is **additive** — no existing tables touched. ``IF NOT
-- EXISTS`` guards make it safe to re-run after a crash that created the
-- table but didn't bump ``PRAGMA user_version``.

BEGIN;

CREATE TABLE IF NOT EXISTS discovery_telemetry (
    command         TEXT    NOT NULL,   -- 'find_sessions_in_path' | 'find_sessions_touching_file' | 'search_past_decisions'
    session_id      TEXT    NOT NULL,
    loaded_count    INTEGER NOT NULL DEFAULT 0,
    cited_count     INTEGER NOT NULL DEFAULT 0,
    first_loaded_ts TEXT,                -- ISO 8601 UTC — first time this (command, session) was surfaced
    last_loaded_ts  TEXT,                -- ISO 8601 UTC
    last_cited_ts   TEXT,                -- ISO 8601 UTC
    demoted         INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (command, session_id)
);
CREATE INDEX IF NOT EXISTS idx_discovery_telemetry_session ON discovery_telemetry(session_id);

PRAGMA user_version = 9;

COMMIT;
