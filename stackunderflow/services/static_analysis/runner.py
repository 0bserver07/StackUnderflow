"""Coordinator — reconstruct pre/post file states, run analyzers, persist deltas.

The runner is the only module the rest of StackUnderflow imports from
this package. It owns:

* Language detection (``detect_language``).
* Pre/post snapshot reconstruction via Playback v2's
  ``reconstruct_fs_at`` (one snapshot at the session's first message ts,
  one at the session's last message ts).
* Analyzer dispatch — picks the per-language module, writes the file
  bytes to a tmpdir, runs ``analyze``, captures metrics + warnings.
* Persistence into ``static_analysis_findings`` with
  ``INSERT OR REPLACE`` so re-runs are idempotent.
* Backfill — scans recent sessions lacking findings, runs analyses
  with a concurrency cap (defaults to ``min(4, cpu_count)``).
* The ``get_session_quality(session_id)`` reader the meta-agent /
  HTTP route surface.

Edge cases the runner handles explicitly:

* **File created in-session** — ``pre_value=NULL``, ``delta=NULL``,
  ``details_json["reason"] = "file_created_in_session"``.
* **File deleted in-session** — ``post_value=NULL``, ``delta=NULL``,
  ``details_json["reason"] = "file_deleted_in_session"``. (In the v1
  model "deleted" means the post snapshot has no content for the
  path — Playback v2 doesn't track a real ``rm`` event but a path
  that's empty post is treated as deleted.)
* **Tool not available** — the per-metric details capture
  ``{"reason": "tool_not_available", "detail": "..."}`` and no
  pre/post numbers; the row is still written so a re-analysis after
  the user installs the tool will overwrite cleanly via
  ``INSERT OR REPLACE``.
* **Pre snapshot Read before any Edit** — Playback v2 returns the
  same content for both pre and post snapshots when no Edit happened
  in the session for that file. The runner skips writing a row in
  that case (delta=0, no signal).
* **Per-file timeout** — analyzers self-cap at 60s (their own
  subprocess timeouts); the runner wraps each analysis in a try/except
  so one stuck file doesn't fail the whole session.
"""

from __future__ import annotations

import json
import logging
import os
import sqlite3
import tempfile
import threading
from concurrent.futures import ThreadPoolExecutor, as_completed
from dataclasses import asdict, dataclass, field
from datetime import UTC, datetime
from pathlib import Path
from typing import Any

from stackunderflow.services import playback_fs
from stackunderflow.services.static_analysis import (
    go_analyzer,
    python_analyzer,
    typescript_analyzer,
)
from stackunderflow.services.static_analysis.python_analyzer import FileMetrics

__all__ = [
    "AnalysisOutcome",
    "METRIC_KEYS",
    "SUPPORTED_LANGUAGES",
    "SessionQuality",
    "analyze_session",
    "backfill",
    "detect_language",
    "get_session_quality",
]

_LOG = logging.getLogger(__name__)

# Closed enum of metric names the schema accepts. New metrics get a
# named entry here AND a per-analyzer ALL_METRICS update; the runner
# refuses to persist a metric not in this set so a typo can't quietly
# write rubbish into the table.
METRIC_KEYS = ("complexity", "coverage", "lint_count", "type_completeness")

# Language → (analyzer module, file extensions).
_LANGUAGE_TABLE: dict[str, tuple[Any, tuple[str, ...]]] = {
    "python": (python_analyzer, (".py",)),
    "typescript": (typescript_analyzer, (".ts", ".tsx", ".js", ".jsx")),
    "go": (go_analyzer, (".go",)),
}
SUPPORTED_LANGUAGES = tuple(_LANGUAGE_TABLE.keys())

# How big a details_json blob we'll persist per row. The schema accepts
# arbitrary text but a runaway blob would bloat the DB; the analyzer
# details are small to start (top-3 rule ids + counts) so this is just
# a defensive cap.
_DETAILS_JSON_CAP = 4_000

# Backfill's per-session timeout. A healthy session analysis runs in
# well under 30s; the cap protects against a pathological mypy/tsc
# pass on a giant file that itself has hung past its 60s subprocess
# timeout.
_PER_SESSION_TIMEOUT_S = 300


# ── public dataclasses ────────────────────────────────────────────────────


@dataclass(slots=True)
class AnalysisOutcome:
    """Per-session summary returned by ``analyze_session``.

    ``rows_written`` counts the (file, metric) rows the analyzer
    persisted. ``files_analyzed`` is the number of touched files the
    runner looked at (a file with no language match contributes 0
    rows but still counts here so the CLI can say "looked at N files,
    M produced metrics"). ``warnings`` aggregates per-file warnings
    plus per-analyzer "tool not available" notices — useful for the
    CLI ``--verbose`` path and the JSON consumers.
    """

    session_id: str
    files_analyzed: int
    rows_written: int
    languages: list[str]
    warnings: list[str] = field(default_factory=list)
    skipped_files: list[str] = field(default_factory=list)


@dataclass(slots=True)
class SessionQuality:
    """The shape ``get_session_quality`` returns.

    ``findings`` is the raw rows for the session (one per file × metric).
    ``summary`` is the aggregate the meta-agent / UI consumers care
    about: total deltas per metric, plus a flag for "regressed" /
    "improved" sessions ("reduced complexity by 20%" headlines).
    """

    session_id: str
    findings: list[dict[str, Any]]
    summary: dict[str, Any]


# ── language detection ────────────────────────────────────────────────────


def detect_language(file_path: str) -> str | None:
    """Return ``"python"``/``"typescript"``/``"go"`` or ``None``.

    Suffix-based — there's no point shelling out to ``file(1)`` for a
    metric we'd skip anyway. ``None`` ⇒ unsupported language; the
    runner skips the file silently.
    """
    suffix = Path(file_path).suffix.lower()
    for lang, (_module, exts) in _LANGUAGE_TABLE.items():
        if suffix in exts:
            return lang
    return None


def _select_analyzer(language: str) -> Any:
    return _LANGUAGE_TABLE[language][0]


# ── snapshot reconstruction ───────────────────────────────────────────────


def _session_bounds(
    conn: sqlite3.Connection, session_id: str,
) -> tuple[str, str] | None:
    """Return ``(first_ts, last_ts)`` for the session, or ``None``.

    The pre snapshot is taken at ``first_ts`` (session start — the FS
    state before any session edit lands); the post snapshot at
    ``last_ts``. We *don't* derive these from messages on every call —
    the ``sessions`` table already carries them and is indexed.
    """
    row = conn.execute(
        "SELECT first_ts, last_ts FROM sessions WHERE session_id = ? LIMIT 1",
        (session_id,),
    ).fetchone()
    if row is None:
        return None
    first = row["first_ts"] if isinstance(row, sqlite3.Row) else row[0]
    last = row["last_ts"] if isinstance(row, sqlite3.Row) else row[1]
    if not first or not last:
        return None
    return str(first), str(last)


def _reconstruct_snapshots(
    conn: sqlite3.Connection, session_id: str,
) -> tuple[dict[str, str], dict[str, str], list[str]]:
    """Return ``(pre_files, post_files, warnings)``.

    ``pre_files`` and ``post_files`` are ``{path: content}`` maps. The
    pre snapshot only carries the *initial* content the session saw
    (Read results). The post snapshot is the end-of-session
    reconstruction. A path appearing in ``post_files`` but not
    ``pre_files`` ⇒ "file created in session"; a path in ``pre_files``
    but not ``post_files`` is a no-op for the runner (we never write a
    "file deleted" row because Playback v2 doesn't surface deletions).

    ``warnings`` aggregates the Playback v2 reconstruction warnings
    so the runner can pass them up to the caller (mostly "Edit
    substitution skipped" — the analysis of that file will be on the
    pre-edit content, which the consumer wants to know about).
    """
    bounds = _session_bounds(conn, session_id)
    if bounds is None:
        return {}, {}, [f"session has no first_ts/last_ts: {session_id}"]
    first_ts, last_ts = bounds

    # Pre snapshot: cutoff at first_ts means we capture only the initial
    # state of files the session Read on its very first message — anything
    # touched before the first message is, by definition, the pre state.
    # In practice Playback v2 walks "events with ts <= cutoff" so the very
    # first Read is included. We use the first message's exact ts so we
    # see the file as the session first saw it.
    try:
        pre = playback_fs.reconstruct_fs_at(
            conn, session_id, at=first_ts, include_content=True,
        )
        post = playback_fs.reconstruct_fs_at(
            conn, session_id, at=last_ts, include_content=True,
        )
    except playback_fs.UnknownSession as e:
        return {}, {}, [str(e)]
    except playback_fs.FsReconstructionError as e:
        return {}, {}, [f"playback_fs error: {e}"]

    pre_files = {
        p: f.get("content", "")
        for p, f in (pre.get("files") or {}).items()
        if isinstance(p, str)
    }
    post_files = {
        p: f.get("content", "")
        for p, f in (post.get("files") or {}).items()
        if isinstance(p, str)
    }
    warnings = list(post.get("warnings") or [])
    return pre_files, post_files, warnings


# ── per-file analysis ─────────────────────────────────────────────────────


_TMPDIR_LOCK = threading.Lock()


def _write_temp(content: str, suffix: str) -> Path:
    """Write ``content`` to a tmp file with the given ``suffix``.

    The lock here is a defensive synchronisation around ``mkstemp`` —
    the system call itself is thread-safe but the surrounding write +
    cleanup pattern (in the backfill executor) was easier to keep
    correct with one critical section than to reason about per-call.
    """
    with _TMPDIR_LOCK:
        fd, path = tempfile.mkstemp(suffix=suffix, prefix="sa_")
        try:
            with os.fdopen(fd, "w", encoding="utf-8") as fh:
                fh.write(content)
        except Exception:
            os.unlink(path)
            raise
    return Path(path)


def _analyze_file_content(language: str, file_path: str, content: str) -> FileMetrics:
    """Write ``content`` to a tmpdir and run the per-language analyzer.

    Always cleans up the tmp file. A crash in the analyzer (which
    shouldn't happen — they catch their own subprocess errors) still
    runs the cleanup via try/finally.
    """
    analyzer = _select_analyzer(language)
    suffix = Path(file_path).suffix or ".tmp"
    tmp_path = _write_temp(content, suffix)
    try:
        return analyzer.analyze(tmp_path, content)
    finally:
        try:
            tmp_path.unlink(missing_ok=True)
        except OSError:
            # Tmpdir cleanup is best-effort; the OS reaps /tmp eventually.
            pass


def _build_finding_rows(
    *,
    session_id: str,
    file_path: str,
    language: str,
    pre: FileMetrics | None,
    post: FileMetrics | None,
    pre_missing_reason: str | None,
    post_missing_reason: str | None,
) -> list[dict[str, Any]]:
    """Produce the finding row dicts for one file, one (pre, post) pair.

    Emits one row per metric the analyzer produced for *either* side.
    A metric only present on one side ⇒ ``delta = NULL`` and the row
    carries the per-side value with the missing-side flagged in
    ``details_json["reason"]``.
    """
    metrics_seen: set[str] = set()
    if pre is not None:
        metrics_seen.update(pre.metrics.keys())
    if post is not None:
        metrics_seen.update(post.metrics.keys())

    ts_now = datetime.now(UTC).isoformat()
    rows: list[dict[str, Any]] = []

    # Pre/post-missing-reason fallback: if neither side produced any
    # metrics, emit a single placeholder row so the consumer can still
    # see "we tried, here's why we got nothing". The placeholder uses
    # ``lint_count`` as the metric (the cheapest one to compute) with
    # both pre/post NULL. This is the documented "tool not available"
    # row shape.
    if not metrics_seen and (pre_missing_reason or post_missing_reason):
        details: dict[str, Any] = {
            "reason": "no_metrics_produced",
            "pre_reason": pre_missing_reason,
            "post_reason": post_missing_reason,
        }
        rows.append({
            "session_id": session_id,
            "file_path": file_path,
            "language": language,
            "ts": ts_now,
            "metric": "lint_count",
            "pre_value": None,
            "post_value": None,
            "delta": None,
            "details_json": _safe_json_dumps(details),
        })
        return rows

    for metric in sorted(metrics_seen):
        if metric not in METRIC_KEYS:
            continue
        pre_val = pre.metrics.get(metric) if pre is not None else None
        post_val = post.metrics.get(metric) if post is not None else None
        delta: float | None
        if pre_val is not None and post_val is not None:
            delta = round(post_val - pre_val, 6)
        else:
            delta = None
        details: dict[str, Any] = {}
        if pre is not None and metric in pre.details:
            details["pre"] = pre.details[metric]
        if post is not None and metric in post.details:
            details["post"] = post.details[metric]
        if pre_val is None and pre_missing_reason:
            details["pre_reason"] = pre_missing_reason
        if post_val is None and post_missing_reason:
            details["post_reason"] = post_missing_reason
        if pre_val is None and pre_missing_reason is None and post_val is not None:
            # Pre side ran but the metric wasn't produced — best-effort
            # explanation so the consumer doesn't think the tool failed.
            details["pre_reason"] = "metric_not_produced_for_pre_state"
        if post_val is None and post_missing_reason is None and pre_val is not None:
            details["post_reason"] = "metric_not_produced_for_post_state"

        rows.append({
            "session_id": session_id,
            "file_path": file_path,
            "language": language,
            "ts": ts_now,
            "metric": metric,
            "pre_value": pre_val,
            "post_value": post_val,
            "delta": delta,
            "details_json": _safe_json_dumps(details) if details else None,
        })
    return rows


def _safe_json_dumps(obj: Any) -> str:
    text = json.dumps(obj, default=str)
    if len(text) > _DETAILS_JSON_CAP:
        return text[: _DETAILS_JSON_CAP - 16] + "...[truncated]\"}"
    return text


# ── persistence ───────────────────────────────────────────────────────────


def _persist_rows(conn: sqlite3.Connection, rows: list[dict[str, Any]]) -> int:
    """Write rows with INSERT OR REPLACE — idempotent on (session, file, metric)."""
    if not rows:
        return 0
    conn.executemany(
        "INSERT OR REPLACE INTO static_analysis_findings "
        "  (session_id, file_path, language, ts, metric, "
        "   pre_value, post_value, delta, details_json) "
        "VALUES (:session_id, :file_path, :language, :ts, :metric, "
        "        :pre_value, :post_value, :delta, :details_json)",
        rows,
    )
    conn.commit()
    return len(rows)


# ── public: analyze_session ───────────────────────────────────────────────


def analyze_session(
    conn: sqlite3.Connection,
    session_id: str,
    *,
    only_languages: tuple[str, ...] | None = None,
) -> AnalysisOutcome:
    """Analyze every file the session touched; persist deltas.

    Parameters
    ----------
    conn:
        Open store connection — must already have the v018 migration
        applied (the routes apply schema lazily; CLI / MCP /
        meta-agent surfaces all do this).
    session_id:
        The session UUID (matches ``sessions.session_id``).
    only_languages:
        Restrict to a subset of supported languages. ``None`` ⇒ every
        supported language. Used by tests + the CLI's ``--language``
        filter.

    Returns
    -------
    :class:`AnalysisOutcome`. The dataclass is JSON-safe for the CLI's
    ``--format json`` output via :func:`dataclasses.asdict`.
    """
    if not session_id or not session_id.strip():
        raise ValueError("session_id must be non-empty")

    pre_files, post_files, warnings = _reconstruct_snapshots(conn, session_id)

    # Files we attempt: union of pre + post that exists in the post
    # snapshot. A path that only exists in pre (with no post entry) is
    # treated as "not edited" (Playback v2 only includes edited files
    # in the post snapshot).
    candidate_paths = sorted(set(pre_files) | set(post_files))
    languages_seen: set[str] = set()
    rows_to_write: list[dict[str, Any]] = []
    skipped: list[str] = []
    files_analyzed = 0

    for path in candidate_paths:
        language = detect_language(path)
        if language is None:
            skipped.append(f"{path}: unsupported language")
            continue
        if only_languages is not None and language not in only_languages:
            continue
        analyzer = _select_analyzer(language)
        avail, why = analyzer.available()
        if not avail:
            warnings.append(f"{language}: skipped — {why}")
            skipped.append(f"{path}: {language} analyzer unavailable")
            continue
        files_analyzed += 1
        languages_seen.add(language)

        pre_content = pre_files.get(path)
        post_content = post_files.get(path)

        # Skip the no-op case (file Read but never edited): pre == post
        # exactly, no signal to extract.
        if (
            pre_content is not None
            and post_content is not None
            and pre_content == post_content
        ):
            continue

        pre_metrics: FileMetrics | None = None
        post_metrics: FileMetrics | None = None
        pre_missing_reason: str | None = None
        post_missing_reason: str | None = None

        if pre_content is None:
            pre_missing_reason = "file_created_in_session"
        else:
            try:
                pre_metrics = _analyze_file_content(language, path, pre_content)
            except Exception as e:  # noqa: BLE001 — never fail the runner on one file
                pre_missing_reason = f"analyzer_error: {type(e).__name__}: {e}"
                _LOG.warning("pre-analysis failed for %s: %s", path, e)

        if post_content is None:
            post_missing_reason = "file_deleted_in_session"
        else:
            try:
                post_metrics = _analyze_file_content(language, path, post_content)
            except Exception as e:  # noqa: BLE001
                post_missing_reason = f"analyzer_error: {type(e).__name__}: {e}"
                _LOG.warning("post-analysis failed for %s: %s", path, e)

        rows_to_write.extend(_build_finding_rows(
            session_id=session_id,
            file_path=path,
            language=language,
            pre=pre_metrics,
            post=post_metrics,
            pre_missing_reason=pre_missing_reason,
            post_missing_reason=post_missing_reason,
        ))
        if pre_metrics is not None:
            warnings.extend(pre_metrics.warnings)
        if post_metrics is not None:
            warnings.extend(post_metrics.warnings)

    rows_written = _persist_rows(conn, rows_to_write)

    return AnalysisOutcome(
        session_id=session_id,
        files_analyzed=files_analyzed,
        rows_written=rows_written,
        languages=sorted(languages_seen),
        warnings=warnings,
        skipped_files=skipped,
    )


# ── public: backfill ──────────────────────────────────────────────────────


def _sessions_lacking_findings(
    conn: sqlite3.Connection, *, since: str | None, limit: int | None,
) -> list[str]:
    """List session_ids whose ``static_analysis_findings`` is empty.

    ``since`` is an ISO-8601 lower bound on ``sessions.last_ts``;
    ``None`` means no bound. ``limit`` caps the candidate set.
    """
    where = ["NOT EXISTS ("
             "  SELECT 1 FROM static_analysis_findings f "
             "  WHERE f.session_id = s.session_id"
             ")"]
    params: list[Any] = []
    if since:
        where.append("s.last_ts >= ?")
        params.append(since)
    sql = (
        # `where` is built from fixed clauses + parameter placeholders only.
        "SELECT s.session_id FROM sessions s WHERE "  # noqa: S608
        + " AND ".join(where)
        + " ORDER BY s.last_ts DESC"
    )
    if limit is not None and limit > 0:
        sql += " LIMIT ?"
        params.append(limit)
    rows = conn.execute(sql, params).fetchall()
    return [
        (r["session_id"] if isinstance(r, sqlite3.Row) else r[0])
        for r in rows
    ]


def _default_concurrency() -> int:
    """``min(4, cpu_count)`` per the spec — analyzers fork subprocesses."""
    cpu = os.cpu_count() or 1
    return max(1, min(4, cpu))


def backfill(
    conn: sqlite3.Connection,
    *,
    since: str | None = None,
    limit: int | None = None,
    concurrency: int | None = None,
    open_conn_factory: Any = None,
) -> dict[str, Any]:
    """Analyze every session whose ``static_analysis_findings`` is empty.

    Parameters
    ----------
    conn:
        The store connection used for *reading* the candidate set and
        writing results back. When ``concurrency > 1`` the per-session
        analyses each open their own connection via
        ``open_conn_factory()`` (the spec calls this out — sqlite
        connections aren't safe to share across threads). The supplied
        ``conn`` is used by the *driving* thread for the candidate
        scan and the final aggregate count.
    since:
        Optional ISO-8601 lower bound on ``sessions.last_ts``. The
        spec's ``--since 30d`` shape is the caller's job to convert.
    limit:
        Cap on candidates to analyze.
    concurrency:
        Worker count. ``None`` ⇒ ``min(4, cpu_count)``.
    open_conn_factory:
        ``() -> sqlite3.Connection`` — used to open per-thread
        connections. ``None`` ⇒ each worker uses the same ``conn``
        passed in (which is fine when ``concurrency=1``; sqlite isn't
        safe for >1 thread sharing the same connection).

    Returns
    -------
    Dict ``{candidates, analyzed, rows_written, warnings_count}``.
    """
    if concurrency is None:
        concurrency = _default_concurrency()
    concurrency = max(1, int(concurrency))
    candidates = _sessions_lacking_findings(conn, since=since, limit=limit)
    if not candidates:
        return {
            "candidates": 0,
            "analyzed": 0,
            "rows_written": 0,
            "warnings_count": 0,
        }

    # Single-threaded path: simpler, used in tests + when no factory
    # was provided (caller is responsible for thread safety).
    if concurrency == 1 or open_conn_factory is None:
        analyzed = 0
        rows_total = 0
        warn_total = 0
        for sid in candidates:
            try:
                outcome = analyze_session(conn, sid)
            except Exception as e:  # noqa: BLE001
                _LOG.warning("backfill: analyze_session(%s) failed: %s", sid, e)
                continue
            analyzed += 1
            rows_total += outcome.rows_written
            warn_total += len(outcome.warnings)
        return {
            "candidates": len(candidates),
            "analyzed": analyzed,
            "rows_written": rows_total,
            "warnings_count": warn_total,
        }

    # Concurrent path: each worker opens its own connection. The
    # ``analyze_session`` call writes results immediately (committed
    # inside the per-row persist). The driving thread aggregates the
    # per-session outcomes when the futures complete.
    def _one(sid: str) -> AnalysisOutcome | None:
        worker_conn = open_conn_factory()
        try:
            return analyze_session(worker_conn, sid)
        except Exception as e:  # noqa: BLE001
            _LOG.warning("backfill worker: analyze_session(%s) failed: %s", sid, e)
            return None
        finally:
            try:
                worker_conn.close()
            except Exception:  # noqa: BLE001, S110 — connection cleanup is best-effort
                pass

    analyzed = 0
    rows_total = 0
    warn_total = 0
    with ThreadPoolExecutor(max_workers=concurrency) as ex:
        futures = {ex.submit(_one, sid): sid for sid in candidates}
        for fut in as_completed(futures):
            try:
                outcome = fut.result(timeout=_PER_SESSION_TIMEOUT_S)
            except Exception as e:  # noqa: BLE001
                _LOG.warning("backfill future raised: %s", e)
                continue
            if outcome is None:
                continue
            analyzed += 1
            rows_total += outcome.rows_written
            warn_total += len(outcome.warnings)

    return {
        "candidates": len(candidates),
        "analyzed": analyzed,
        "rows_written": rows_total,
        "warnings_count": warn_total,
    }


# ── public: get_session_quality ───────────────────────────────────────────


# Threshold (post-pre / pre) for marking a metric as "improved" vs
# "regressed". Spec headline: "agent reduced complexity by 20%".
_SIGNIFICANT_DELTA_PCT = 0.20

# Per-metric semantics: ``True`` = lower is better (complexity, lint),
# ``False`` = higher is better (coverage, type completeness).
_LOWER_IS_BETTER: dict[str, bool] = {
    "complexity": True,
    "lint_count": True,
    "coverage": False,
    "type_completeness": False,
}


def _classify_delta(metric: str, pre: float | None, post: float | None) -> str:
    """Return ``"improved"`` / ``"regressed"`` / ``"neutral"`` / ``"unknown"``.

    A delta with one side missing returns ``"unknown"``; below the
    significance threshold ``"neutral"``.
    """
    if pre is None or post is None:
        return "unknown"
    if pre == 0:
        # Avoid div-by-zero. A 0→N change is "regressed" for
        # lower-is-better metrics, "improved" for higher-is-better.
        if post == 0:
            return "neutral"
        return "regressed" if _LOWER_IS_BETTER.get(metric, True) else "improved"
    pct = (post - pre) / abs(pre)
    if abs(pct) < _SIGNIFICANT_DELTA_PCT:
        return "neutral"
    if _LOWER_IS_BETTER.get(metric, True):
        return "improved" if pct < 0 else "regressed"
    return "improved" if pct > 0 else "regressed"


def get_session_quality(
    conn: sqlite3.Connection, session_id: str,
) -> SessionQuality:
    """Fetch findings + summary for ``session_id``.

    Returns an empty ``SessionQuality`` (no findings, summary with
    zero counts) when the session has no rows in the table — including
    when it's never been analyzed. Callers can distinguish "analyzed
    and no signal" from "not analyzed yet" by checking whether the
    session has any post-rows in ``static_analysis_findings``.
    """
    rows = conn.execute(
        "SELECT file_path, language, ts, metric, pre_value, post_value, "
        "       delta, details_json "
        "FROM static_analysis_findings "
        "WHERE session_id = ? "
        "ORDER BY file_path, metric",
        (session_id,),
    ).fetchall()
    findings: list[dict[str, Any]] = []
    by_metric: dict[str, list[tuple[float | None, float | None, float | None]]] = {}
    languages: set[str] = set()
    for r in rows:
        f = {
            "file_path": r["file_path"],
            "language": r["language"],
            "ts": r["ts"],
            "metric": r["metric"],
            "pre_value": r["pre_value"],
            "post_value": r["post_value"],
            "delta": r["delta"],
            "details_json": r["details_json"],
        }
        findings.append(f)
        languages.add(str(r["language"]))
        by_metric.setdefault(str(r["metric"]), []).append(
            (r["pre_value"], r["post_value"], r["delta"]),
        )

    metric_summary: dict[str, dict[str, Any]] = {}
    for metric, triples in by_metric.items():
        observed = [t for t in triples if t[2] is not None]
        avg_delta = (
            sum(t[2] for t in observed) / len(observed)
            if observed else None
        )
        improved = sum(
            1 for t in triples
            if _classify_delta(metric, t[0], t[1]) == "improved"
        )
        regressed = sum(
            1 for t in triples
            if _classify_delta(metric, t[0], t[1]) == "regressed"
        )
        neutral = sum(
            1 for t in triples
            if _classify_delta(metric, t[0], t[1]) == "neutral"
        )
        metric_summary[metric] = {
            "files": len(triples),
            "avg_delta": (
                round(avg_delta, 4) if avg_delta is not None else None
            ),
            "improved": improved,
            "regressed": regressed,
            "neutral": neutral,
        }

    summary: dict[str, Any] = {
        "files": len({f["file_path"] for f in findings}),
        "languages": sorted(languages),
        "metrics": metric_summary,
        "headline": _build_headline(metric_summary),
    }
    return SessionQuality(
        session_id=session_id, findings=findings, summary=summary,
    )


def _build_headline(metric_summary: dict[str, dict[str, Any]]) -> str:
    """Pick the most-changed metric and emit a one-line plaintext summary.

    "Reduced complexity by 0.7 across 3 files" / "Increased lint_count
    by 5 across 2 files" / "No significant changes detected".
    """
    if not metric_summary:
        return "No metrics produced."
    best: tuple[str, float, dict[str, Any]] | None = None  # (metric, |avg_delta|, summary)
    for metric, sm in metric_summary.items():
        avg = sm.get("avg_delta")
        if not isinstance(avg, int | float):
            continue
        magnitude = abs(avg)
        if best is None or magnitude > best[1]:
            best = (metric, magnitude, sm)
    if best is None:
        return "No comparable pre/post deltas (analyzer ran but no metric had both sides)."
    metric, _, sm = best
    avg = sm["avg_delta"]
    files = sm["files"]
    direction_word = (
        "Reduced" if (avg < 0 and _LOWER_IS_BETTER.get(metric, True)) or
                     (avg > 0 and not _LOWER_IS_BETTER.get(metric, True))
        else "Increased"
    )
    return (
        f"{direction_word} {metric} by {abs(avg):.3g} on average "
        f"across {files} file{'s' if files != 1 else ''}."
    )


# ── helper: serialise outcome for CLI / route JSON ────────────────────────


def outcome_to_dict(outcome: AnalysisOutcome) -> dict[str, Any]:
    return asdict(outcome)


def quality_to_dict(quality: SessionQuality) -> dict[str, Any]:
    return asdict(quality)
