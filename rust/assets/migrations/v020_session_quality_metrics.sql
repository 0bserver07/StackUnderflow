-- v020: Session quality metrics (LLM-graded session quality)
--
-- One row per session_id. Carries the overall quality score, sub-grades JSON,
-- structured rationale, and suggestions JSON.

BEGIN;

CREATE TABLE IF NOT EXISTS session_quality_metrics (
    id                INTEGER PRIMARY KEY,
    session_id        TEXT    NOT NULL UNIQUE,
    overall_score     REAL    NOT NULL,
    grades_json       TEXT    NOT NULL,  -- sub-grades: {"goal_clarity": X, "execution_efficiency": Y, "success": Z}
    rationale         TEXT    NOT NULL,
    suggestions_json  TEXT    NOT NULL,  -- list of strings: ["suggestion 1", ...]
    graded_at         TEXT    NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_sq_session_id
    ON session_quality_metrics(session_id);

PRAGMA user_version = 20;

COMMIT;
