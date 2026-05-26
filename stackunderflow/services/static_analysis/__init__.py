"""Per-session static analysis pass — Spec 21 (issue #93).

Computes pre/post static-analysis deltas for every file a session touched:
cyclomatic complexity, lint count, type completeness, (coverage is a
deferred sub-task — see ``runner.METRIC_KEYS``).

Public entry points (re-exported here so callers don't need to know which
sub-module each lives in):

* :func:`runner.analyze_session` — analyze one session, persist findings.
* :func:`runner.backfill` — scan recent sessions lacking findings.
* :func:`runner.get_session_quality` — read findings + summary for a
  session (the meta-agent's tool calls in here).
* :data:`runner.METRIC_KEYS` — closed enum of metric names the schema
  uses (``complexity`` / ``coverage`` / ``lint_count`` /
  ``type_completeness``).

Languages supported v1: Python, TypeScript, Go. Everything else is
skipped (the runner returns ``language=None`` for unrecognised
extensions and no findings are written for the file). Adding a new
language is a per-language adapter — see ``python_analyzer.py`` for the
shape an analyzer must implement (``analyze(path, content) -> Metrics``,
``available() -> tuple[bool, str]``).

Optional dependencies
=====================
The Python analyzer needs ``radon`` (complexity) and ``mypy`` (type
completeness); the TypeScript analyzer needs ``tsc`` and ``eslint`` on
PATH; the Go analyzer needs ``go`` + ``gocyclo`` on PATH. Each analyzer
exposes ``available()`` which returns ``(bool, reason)`` — if a tool is
missing the analyzer skips that metric cleanly and the runner records
``details_json={"reason": "tool_not_available", "detail": "..."}`` rather
than crashing.

Privacy
=======
Source files are reconstructed via Playback v2 and written to a tmpdir
that the runner deletes after analysis. Analyzer subprocess output (lint
rule ids, function-level complexity numbers) is parsed and the parsed
metric values + a small ``details_json`` blob are persisted; the raw
source is never stored.
"""

from __future__ import annotations

from stackunderflow.services.static_analysis.runner import (
    METRIC_KEYS,
    SUPPORTED_LANGUAGES,
    AnalysisOutcome,
    SessionQuality,
    analyze_session,
    backfill,
    detect_language,
    get_session_quality,
)

__all__ = [
    "METRIC_KEYS",
    "SUPPORTED_LANGUAGES",
    "AnalysisOutcome",
    "SessionQuality",
    "analyze_session",
    "backfill",
    "detect_language",
    "get_session_quality",
]
