-- v021: Stop trusting fabricated fallback grades.
--
-- Earlier code persisted a synthetic overall_score=5.0 grade when the local
-- Ollama instance was offline — indistinguishable from a real grade and
-- silently polluting aggregations/dashboards. services/grading.py now raises
-- (callers surface HTTP 503 / CLI skip) instead of writing on failure. Purge
-- any fakes already written so those sessions re-grade (or report
-- unavailable) on next access. Identified by their distinctive rationale.

BEGIN;

DELETE FROM session_quality_metrics
 WHERE rationale LIKE 'Fallback grade generated because%';

PRAGMA user_version = 21;

COMMIT;
