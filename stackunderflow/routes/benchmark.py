"""``GET /api/benchmark`` — comparative "which model wins for your work" verdict.

Thin HTTP wrapper around :func:`stackunderflow.reports.benchmark.analyze_benchmark`.
An observational benchmark over the user's own history (spec 26): per-task-type
verdicts with the full statistical-honesty machinery, or — just as often and
just as valuable — an honest "insufficient evidence".

Mirrors ``routes/forks.py`` exactly:

* **200 ms analytical tier** (the ``/api/yield`` / ``/api/optimize`` tier, not
  the 100 ms mart tier) — it is a cross-session statistical composite, so it is
  wrapped in the same read-through cache forks uses (keyed on store + scope +
  project ids, self-invalidated by a sessions signature that moves on ingest).
* **Currency contract** — every dollar figure is pre-converted to the active
  currency before send, applied to a deep copy outside the cache so an FX
  change is picked up without recompute.
* A ``warning`` field carries the natural-experiment caveat inline.
"""

from __future__ import annotations

import copy
import threading
from pathlib import Path
from typing import Any

from fastapi import APIRouter, HTTPException, Query

import stackunderflow.deps as deps
from stackunderflow.infra.currency import active_currency_payload
from stackunderflow.reports.benchmark import analyze_benchmark, recommend_from_history
from stackunderflow.reports.scope import parse_period
from stackunderflow.store import db

router = APIRouter()

# Read-through cache: the analysis walks every scoped session and re-derives
# intent per session, so it isn't free. Keyed on (store, scope, ids) plus a
# sessions signature (max last_ts, summed message_count) that any ingest bumps,
# so a stale entry can't outlive a refresh — the same contract forks/cost use.
# Currency conversion stays OUTSIDE the cache (applied to a deep copy).
_BENCH_CACHE: dict[
    tuple[str, str, tuple[int, ...] | None, str | None], tuple[tuple[str | None, int], dict]
] = {}
_BENCH_CACHE_LOCK = threading.Lock()


def _bench_signature(conn: Any, project_ids: list[int] | None) -> tuple[str | None, int]:
    """(max last_ts, summed message_count) over the scoped sessions.

    ``project_ids is None`` = whole store. Any ingest that writes a message
    bumps this signature and forces a recompute. Advisory: a bad store returns
    a sentinel that simply misses the cache rather than raising.
    """
    try:
        if project_ids is None:
            row = conn.execute(
                "SELECT MAX(last_ts) AS max_ts, "
                "COALESCE(SUM(message_count), 0) AS n FROM sessions"
            ).fetchone()
        elif not project_ids:
            return (None, 0)
        else:
            placeholders = ",".join("?" for _ in project_ids)
            row = conn.execute(
                "SELECT MAX(last_ts) AS max_ts, "  # noqa: S608 — placeholders are only ? marks
                "COALESCE(SUM(message_count), 0) AS n "
                f"FROM sessions WHERE project_id IN ({placeholders})",
                tuple(project_ids),
            ).fetchone()
    except Exception:  # noqa: BLE001 — advisory: a bad store just misses cache
        return (None, -1)
    if row is None:
        return (None, 0)
    return (row["max_ts"], int(row["n"] or 0))


def _analyze_benchmark_cached(
    conn: Any, *, scope: Any, project_ids: list[int] | None, intent: str | None
) -> dict:
    """Read-through cache around :func:`analyze_benchmark` (returns USD report)."""
    sig = _bench_signature(conn, project_ids)
    key = (
        str(deps.store_path),
        scope.label,
        tuple(sorted(project_ids)) if project_ids is not None else None,
        intent,
    )
    with _BENCH_CACHE_LOCK:
        cached = _BENCH_CACHE.get(key)
    if cached is not None and cached[0] == sig:
        return copy.deepcopy(cached[1])
    report = analyze_benchmark(conn, scope=scope, project_ids=project_ids, intent=intent)
    with _BENCH_CACHE_LOCK:
        _BENCH_CACHE[key] = (sig, report)
    return copy.deepcopy(report)


# Friendly period superset — ``week`` maps to ``7days`` inside ``parse_period``.
# Mirrors forks / yield so all three beta surfaces accept the same selector.
_PERIOD_ALIASES = {
    "today": "today",
    "week": "7days",
    "7days": "7days",
    "month": "month",
    "30days": "30days",
    "all": "all",
}

_PERIOD_QUERY = Query("all", description="today | week | month | all")
_LOG_PATH_QUERY = Query(None, description="Project log path; omit for whole-store")
_INTENT_QUERY = Query(None, description="Filter to one intent stratum (build/fix/…)")
_SIZE_QUERY = Query(None, description="Task size band (tiny/small/med/large)")
_LANG_QUERY = Query(None, description="Dominant language hint")


def _project_ids_for(conn: Any, path: str) -> list[int]:
    """Resolve a log path to the ``projects.id`` list for its slug (own resolver)."""
    slug = Path(path).name
    try:
        rows = conn.execute("SELECT id FROM projects WHERE slug = ?", (slug,)).fetchall()
    except Exception:  # noqa: BLE001 — advisory route, never 500 on a bad store
        return []
    return [int(r["id"]) for r in rows]


def _convert_report_costs(report: dict, rate: float) -> None:
    """Convert every dollar figure in the report to the active currency in place.

    Explicit walk (never a blanket multiply) so a schema change can't silently
    double-convert a non-cost field. ``cost_usd`` originates in ``session_mart``
    and is only *displayed* in another currency — the invariant is untouched.
    """
    if rate == 1.0:
        return
    verdict = report.get("verdict") or {}
    if verdict.get("cost_per_outcome_usd") is not None:
        verdict["cost_per_outcome_usd"] = float(verdict["cost_per_outcome_usd"]) * rate
    for stratum in report.get("strata") or []:
        for m in stratum.get("models") or []:
            _convert_cost_block(m.get("cost_per_outcome"), rate)
            _convert_cost_block(m.get("median_cost"), rate)


def _convert_cost_block(block: Any, rate: float) -> None:
    """Scale a ``{"point": x, "ci": [lo, hi]}`` cost block by ``rate`` in place."""
    if not isinstance(block, dict):
        return
    if block.get("point") is not None:
        block["point"] = float(block["point"]) * rate
    ci = block.get("ci")
    if isinstance(ci, list):
        block["ci"] = [float(x) * rate for x in ci]


@router.get("/api/benchmark")
async def get_benchmark(
    period: str = _PERIOD_QUERY,
    log_path: str | None = _LOG_PATH_QUERY,
    intent: str | None = _INTENT_QUERY,
):
    """Return ``{period, scope, report, currency, warning}``.

    ``report`` is the full benchmark verdict with every dollar figure already
    converted to the active currency. Scoped to a project when ``log_path`` (or
    the active ``deps.current_log_path``) resolves to one, else the whole store.
    """
    period = period if isinstance(period, str) else "all"
    spec = _PERIOD_ALIASES.get(period)
    if spec is None:
        raise HTTPException(
            status_code=400,
            detail=f"Invalid period '{period}'. Valid: {', '.join(_PERIOD_ALIASES)}",
        )
    scope = parse_period(spec)

    log_path_str = log_path if isinstance(log_path, str) else None
    intent_str = intent if isinstance(intent, str) else None
    path = log_path_str or deps.current_log_path

    conn = db.connect(deps.store_path)
    try:
        project_ids = _project_ids_for(conn, path) if path else None
        report = _analyze_benchmark_cached(
            conn, scope=scope, project_ids=project_ids, intent=intent_str
        )
    finally:
        conn.close()

    currency = active_currency_payload()
    _convert_report_costs(report, currency["rate_from_usd"])

    return {
        "period": period,
        "scope": scope.label,
        "report": report,
        "currency": currency,
        "warning": report.get("warning"),
    }


@router.get("/api/benchmark/recommend")
async def get_benchmark_recommend(
    intent: str = Query(..., description="Task intent (build/fix/explore/refactor/test/ops)"),
    size: str | None = _SIZE_QUERY,
    language: str | None = _LANG_QUERY,
    log_path: str | None = _LOG_PATH_QUERY,
    period: str = _PERIOD_QUERY,
):
    """Return the outcome-aware model recommendation for a described task."""
    period = period if isinstance(period, str) else "all"
    spec = _PERIOD_ALIASES.get(period)
    if spec is None:
        raise HTTPException(
            status_code=400,
            detail=f"Invalid period '{period}'. Valid: {', '.join(_PERIOD_ALIASES)}",
        )
    scope = parse_period(spec)
    if not isinstance(intent, str) or not intent.strip():
        raise HTTPException(status_code=400, detail="intent is required")

    size_str = size if isinstance(size, str) else None
    lang_str = language if isinstance(language, str) else None
    log_path_str = log_path if isinstance(log_path, str) else None
    path = log_path_str or deps.current_log_path

    conn = db.connect(deps.store_path)
    try:
        project_ids = _project_ids_for(conn, path) if path else None
        rec = recommend_from_history(
            conn, intent=intent, size=size_str, language=lang_str,
            scope=scope, project_ids=project_ids,
        )
    finally:
        conn.close()

    currency = active_currency_payload()
    rate = currency["rate_from_usd"]
    if rate != 1.0 and isinstance(rec.get("evidence"), dict):
        _convert_cost_block(rec["evidence"].get("cost_per_outcome"), rate)
        _convert_cost_block(rec["evidence"].get("median_cost"), rate)

    return {"period": period, "scope": scope.label, "recommendation": rec, "currency": currency}
