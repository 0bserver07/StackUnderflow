-- v014: Opt-in semantic search for discovery — embedding cache table.
--
-- Closes HANDOFF follow-up #10. Adds an opt-in semantic-search mode to
-- ``search-past-decisions`` (and its MCP counterpart) via local
-- sentence-transformers embeddings. The substring filter still runs first;
-- when ``--use-embeddings`` is passed the candidate set is re-ranked by
-- cosine similarity against the query embedding. This table is a
-- pull-through cache for the per-message vectors so a second invocation
-- against the same candidate set is just a SELECT, not a recompute.
--
-- Storage shape
-- -------------
-- One row per ``(session_id, message_id, model_name)``. ``embedding`` is
-- a raw ``numpy.float32`` byte buffer — ``np.frombuffer(blob, np.float32)``
-- gives back the vector. ``embedding_dim`` is recorded explicitly so a
-- corrupt blob is caught at read time. ``model_name`` keys vectors by the
-- sentence-transformers model that produced them so changing the model
-- (or running both side-by-side) doesn't silently mix incompatible
-- vectors. ``created_ts`` is ISO 8601 UTC for cache-age inspection.
--
-- Why ``session_id`` (TEXT) instead of just a ``session_fk`` foreign key:
-- the table is meant to survive store rebuilds (re-ingest churns
-- ``sessions.id`` but ``session_id`` is stable across rebuilds). The
-- columns are denormalised on purpose so an embedding cache can outlive a
-- single ingest cycle.
--
-- No FK on ``message_id`` either. After v008 ``messages`` is a UNION view
-- across monthly partitions; the underlying ids come from
-- ``_messages_id_seq``. A foreign key on a view isn't enforceable.
-- Garbage-collection of orphaned rows is a future sweep — not in scope
-- for this migration.
--
-- Privacy / footprint
-- -------------------
-- Vectors are derived from message text; they leak less than the raw
-- ``content_text`` but are still derived data. The table lives in
-- ``~/.stackunderflow/store.db`` like everything else — never leaves the
-- machine. For the default model (``all-MiniLM-L6-v2``, 384 dims × 4
-- bytes = 1,536 bytes per row) a 100k-message store would cost ~150 MB
-- if every message were embedded; in practice only the candidate set
-- behind a ``search-past-decisions`` query is ever embedded, so the table
-- grows pull-through with usage.
--
-- Migration is **additive** — no existing tables touched. ``IF NOT
-- EXISTS`` guards make it safe to re-run after a crash that created the
-- table but didn't bump ``PRAGMA user_version``.

BEGIN;

CREATE TABLE IF NOT EXISTS discovery_embeddings (
    session_id      TEXT    NOT NULL,
    message_id      INTEGER NOT NULL,
    model_name      TEXT    NOT NULL,
    embedding       BLOB    NOT NULL,
    embedding_dim   INTEGER NOT NULL,
    created_ts      TEXT    NOT NULL,
    PRIMARY KEY (session_id, message_id, model_name)
);
CREATE INDEX IF NOT EXISTS idx_discovery_embeddings_session
    ON discovery_embeddings(session_id);
CREATE INDEX IF NOT EXISTS idx_discovery_embeddings_message
    ON discovery_embeddings(message_id, model_name);

PRAGMA user_version = 14;

COMMIT;
