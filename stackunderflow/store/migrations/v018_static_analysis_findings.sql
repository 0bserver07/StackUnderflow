-- v018: Per-session static analysis pass (Spec 21 — issue #93).
--
-- One row per (session, file, metric) — captures the pre/post snapshot of
-- a static-analysis metric (cyclomatic complexity, lint count, type
-- completeness, coverage) for every file the session touched. The deltas
-- power outcome attribution v2 ("session X reduced complexity by 20%")
-- and the comparative benchmark ("agent A is better than agent B on YOUR
-- code on metric M").
--
-- Storage shape
-- -------------
-- ``session_id`` is the stable string id (matches ``sessions.session_id``,
-- not the integer FK that churns on re-ingest); ``file_path`` is the
-- absolute path the analyzer ran on. ``language`` records the analyzer
-- bucket (``python``/``typescript``/``go``) so a future v2 metric set can
-- join on it without re-deriving the language. ``metric`` is one of
-- ``complexity``/``coverage``/``lint_count``/``type_completeness`` —
-- explicitly a closed enum on the consumer side; new metrics get added
-- via ALTER on the consumer code, never by a schema migration.
--
-- ``pre_value``/``post_value`` are nullable: a file created in-session has
-- no pre-state (``pre_value=NULL``, ``delta=NULL``, ``details_json``
-- carries ``{"reason": "file_created_in_session"}``). A file deleted
-- in-session has the inverse (``post_value=NULL``). When both sides are
-- observable ``delta = post_value - pre_value`` (post-minus-pre — a
-- *negative* delta means the metric dropped, which is "better" for
-- complexity/lint and "worse" for coverage/type-completeness; the
-- consumer interprets sign per metric).
--
-- ``details_json`` is a per-metric extras blob (lint rule ids, the radon
-- per-function complexity table, mypy untyped-symbol list). Capped to a
-- few KB by the runner — see ``services/static_analysis/runner.py``.
--
-- The ``UNIQUE (session_id, file_path, metric)`` constraint keys the
-- "is this session already analyzed?" idempotency check the backfill
-- relies on; the runner uses ``INSERT OR REPLACE`` so a re-analysis
-- updates in place rather than accumulating duplicate rows.
--
-- Indexes
-- -------
-- ``idx_sa_session`` — the dominant access pattern is "what did the
-- analyzer say about session X?" (one row-set per session, fan-out 0 to
-- N files × M metrics). ``idx_sa_file`` — the cross-session aggregate
-- ("how often does file Y see complexity reductions?") drives the
-- comparative-benchmark queries.
--
-- Privacy / footprint
-- -------------------
-- Analyzer output (lint rule ids, function-level complexity numbers)
-- never leaves the machine. ``details_json`` does not store source
-- code. At ~250 bytes per row a 10k-session store with three metrics
-- per file × ~5 touched files would cost ~37 MB.
--
-- Migration is **additive** — no existing tables touched. ``IF NOT
-- EXISTS`` guards make it safe to re-run after a crash that created the
-- table but didn't bump ``PRAGMA user_version``.

BEGIN;

CREATE TABLE IF NOT EXISTS static_analysis_findings (
    id              INTEGER PRIMARY KEY,
    session_id      TEXT    NOT NULL,
    file_path       TEXT    NOT NULL,
    language        TEXT    NOT NULL,
    ts              TEXT    NOT NULL,
    metric          TEXT    NOT NULL,
    pre_value       REAL,
    post_value      REAL,
    delta           REAL,
    details_json    TEXT,
    UNIQUE (session_id, file_path, metric)
);
CREATE INDEX IF NOT EXISTS idx_sa_session
    ON static_analysis_findings(session_id);
CREATE INDEX IF NOT EXISTS idx_sa_file
    ON static_analysis_findings(file_path);

PRAGMA user_version = 18;

COMMIT;
