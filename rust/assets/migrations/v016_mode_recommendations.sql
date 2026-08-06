-- v016: Mode-recommender cache table (Spec 18 — heuristic v1).
--
-- Closes GitHub issue #88. The mode recommender pattern-matches an
-- incoming prompt's feature shape against the user's own past sessions
-- and suggests the cheapest model that historically solved similar
-- tasks. The recommendation itself is cheap to compute (a few SELECTs
-- against ``sessions`` + ``usage_events``) but pointless to recompute on
-- every call when the same prompt-shape is asked back-to-back during a
-- session — this table is a 24h pull-through cache keyed on the hash of
-- the extracted feature dict.
--
-- Storage shape
-- -------------
-- One row per ``task_pattern_hash``. ``recommended_model`` is the model
-- the recommender picked (cheapest model whose past sessions matched
-- the pattern). ``confidence`` is in [0, 1] — see
-- ``services.mode_recommender._compute_confidence`` for the scoring.
-- ``evidence_session_ids`` is a JSON-encoded list of the
-- ``sessions.session_id`` values that contributed to the choice (the
-- "why" the user can drill into via ``session_query``). ``created_ts``
-- and ``last_used_ts`` are ISO-8601 UTC; the 24h TTL is computed off
-- ``created_ts`` and ``last_used_ts`` is bumped on every cache hit so
-- frequently-asked patterns can be surfaced for analytics later.
--
-- Why ``task_pattern_hash`` (TEXT, md5) instead of a feature-tuple PK:
-- the feature dict is open-ended (intent + language hints + counts) and
-- adding a new feature must not require an ALTER TABLE. md5 of the
-- normalised feature JSON keeps the key narrow + lookup-fast and
-- naturally invalidates whenever the feature extractor changes shape
-- (cache misses, recomputes, refills — no manual migration needed).
--
-- Privacy / footprint
-- -------------------
-- The hash is derived from the prompt's feature shape, never the prompt
-- itself. ``evidence_session_ids`` are the user's own session IDs from
-- the local store. Nothing leaves the machine. At ~50 bytes per row
-- a 10k-cache-entry store costs ~500 KB.
--
-- Migration is **additive** — no existing tables touched. ``IF NOT
-- EXISTS`` guards make it safe to re-run after a crash that created the
-- table but didn't bump ``PRAGMA user_version``.

BEGIN;

CREATE TABLE IF NOT EXISTS mode_recommendations (
    id                    INTEGER PRIMARY KEY,
    task_pattern_hash     TEXT    NOT NULL,
    recommended_model     TEXT    NOT NULL,
    confidence            REAL    NOT NULL DEFAULT 0.0,
    evidence_session_ids  TEXT    NOT NULL DEFAULT '[]',
    created_ts            TEXT    NOT NULL,
    last_used_ts          TEXT    NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_mode_recommendations_hash
    ON mode_recommendations(task_pattern_hash);

PRAGMA user_version = 16;

COMMIT;
