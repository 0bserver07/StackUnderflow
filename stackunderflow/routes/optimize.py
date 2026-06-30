"""Optimize / waste-detection routes.

Surfaces both the legacy looped-Q&A waste view and the structural
pattern findings (CLAUDE.md bloat, unused MCP, ghost agents, junk
reads, cache thrash, oversized bash output, exploration-only sessions).

GET ``/api/optimize?period=30days`` returns:
    {
        "scope": "last 30 days",
        "waste": [...],            # legacy find_waste()
        "patterns": [Finding,...], # each carries estimated_waste_usd
        "total_waste_usd": 12.34,  # Σ priced waste across patterns
        "anomalies": {...},        # cost outlier days/sessions (anomaly.py)
        "warnings": [...],         # mart-backfill hints, optional
        "cache": "hit|miss"        # diagnostic
    }
"""

from __future__ import annotations

import threading
import time
from typing import Annotated

from fastapi import APIRouter, HTTPException, Query

import stackunderflow.deps as deps
from stackunderflow.reports.anomaly import find_cost_anomalies
from stackunderflow.reports.optimize import find_patterns, find_waste
from stackunderflow.reports.scope import parse_period
from stackunderflow.store import db, mart_queries, schema

router = APIRouter()


_VALID_PERIODS = {"today", "7days", "30days", "month", "all"}


# In-process response cache keyed by (period, project-tuple, exclude-tuple,
# store_mtime_ns). Optimize is read-heavy / write-rare: identical args within
# the same store revision return identical findings, so we memoise the dict
# until the SQLite file's mtime moves. ``/api/refresh`` doesn't have to
# evict — the next ingest pass bumps mtime and the key drifts naturally.
_OPTIMIZE_CACHE: dict[tuple, tuple[int, dict]] = {}
_OPTIMIZE_CACHE_LOCK = threading.Lock()
_OPTIMIZE_CACHE_MAX = 16  # tiny LRU — params space is small in practice


def _store_mtime_ns() -> int:
    """Return ``store.db`` mtime in nanoseconds, or 0 when missing."""
    try:
        return deps.store_path.stat().st_mtime_ns
    except (OSError, AttributeError):
        return 0


def _cache_get(key: tuple, mtime: int) -> dict | None:
    with _OPTIMIZE_CACHE_LOCK:
        hit = _OPTIMIZE_CACHE.get(key)
    if hit is None or hit[0] != mtime:
        return None
    return hit[1]


def _cache_put(key: tuple, mtime: int, payload: dict) -> None:
    with _OPTIMIZE_CACHE_LOCK:
        if len(_OPTIMIZE_CACHE) >= _OPTIMIZE_CACHE_MAX:
            # Trim the oldest entry — tiny cache, FIFO is fine.
            try:
                first = next(iter(_OPTIMIZE_CACHE))
                _OPTIMIZE_CACHE.pop(first, None)
            except StopIteration:
                pass
        _OPTIMIZE_CACHE[key] = (mtime, payload)


def invalidate_optimize_cache() -> None:
    """Drop every cached optimize payload. Cheap — the cache is tiny."""
    with _OPTIMIZE_CACHE_LOCK:
        _OPTIMIZE_CACHE.clear()


@router.get("/api/optimize")
async def get_optimize_report(
    period: str = "30days",
    project: Annotated[list[str] | None, Query()] = None,
    exclude: Annotated[list[str] | None, Query()] = None,
    force: bool = False,
):
    """Run waste + structural-pattern detection over *period*.

    Args:
        period: ``today | 7days | 30days | month | all``.
        project: Optional repeated query param to narrow project scope.
        exclude: Optional repeated query param to drop projects.
        force: Bypass the in-process cache for this call. The result is
            still written back to the cache so subsequent calls benefit.
    """
    if period not in _VALID_PERIODS:
        raise HTTPException(
            status_code=400,
            detail=f"Unknown period '{period}'. Valid: {', '.join(sorted(_VALID_PERIODS))}",
        )

    t0 = time.time()
    key = (
        period,
        tuple(sorted(project)) if project else (),
        tuple(sorted(exclude)) if exclude else (),
    )
    mtime = _store_mtime_ns()
    if not force:
        cached = _cache_get(key, mtime)
        if cached is not None:
            cached = dict(cached)
            cached["cache"] = "hit"
            deps.logger.debug(f"optimize [hit] {(time.time()-t0)*1000:.0f}ms")
            return cached

    scope = parse_period(period)

    conn = db.connect(deps.store_path)
    try:
        schema.apply(conn)
        warnings: list[dict] = []
        # Mart backfill hint — when ``message_tool_mart`` is empty the
        # detectors fall back to the raw ``messages`` scan, which is
        # slow on large stores. Surface this so the UI can prompt the
        # user to run an ETL backfill.
        if not mart_queries.mart_has_message_tool_rows(conn):
            warnings.append({
                "code": "mart_empty",
                "level": "info",
                "message": (
                    "message_tool_mart is empty — optimize detectors are "
                    "running on the raw messages table and will be slower. "
                    "Backfill via the ETL pipeline for the fast path."
                ),
            })
        waste = find_waste(
            conn,
            scope=scope,
            include=project,
            exclude=exclude,
        )
        patterns = find_patterns(
            conn,
            scope=scope,
            project_filter=project,
        )
        anomalies = find_cost_anomalies(conn, scope=scope)
    finally:
        conn.close()

    pattern_dicts = [p.to_dict() for p in patterns]
    # Aggregate priced waste across detectors that carry a dollar figure —
    # the UI shows this as the headline "$X identified as waste" number.
    total_waste_usd = round(
        sum(p.get("estimated_waste_usd") or 0.0 for p in pattern_dicts), 4
    )

    payload: dict = {
        "scope": scope.label,
        "waste": waste,
        "patterns": pattern_dicts,
        "total_waste_usd": total_waste_usd,
        "anomalies": anomalies,
        "warnings": warnings,
        "cache": "miss",
    }
    _cache_put(key, mtime, payload)
    deps.logger.debug(f"optimize [miss] {(time.time()-t0)*1000:.0f}ms")
    return payload
