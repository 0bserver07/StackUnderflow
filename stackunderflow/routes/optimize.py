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

Campaign #7 adds the **prescriptions** surface — findings turned into
concrete, previewable actions:

GET ``/api/optimize/prescriptions?project=<slug>&period=30days`` returns:
    {
        "scope": "last 30 days",
        "project": "<slug>" | null,
        "routing": {recommendations, models, observed_days, ...},
        "claudemd_previews": [{preview_diff, rationale, ...}, ...],
        "currency": {...},
    }

POST ``/api/optimize/claudemd-preview`` (body: ``{"text": "..."}``) returns
``{"preview": {...}, "currency": {...}}`` for a CLAUDE.md supplied by the
client.

**Filesystem contract (documented decision).** ``/api/optimize`` has always
read the user's CLAUDE.md files *read-only* from the well-known config
locations (``~/.claude/CLAUDE.md``, ``~/.claude/projects/<slug>/CLAUDE.md``)
— that is how the ``bloated_claude_md`` detector works. The GET
prescriptions endpoint reuses exactly that discovery (via
``find_claudemd_bloat``) and adds **no new filesystem surface**. For any
CLAUDE.md living elsewhere (e.g. inside a repo checkout) the server does
NOT accept a path — the client POSTs the *text* to
``/api/optimize/claudemd-preview`` instead, so the server never reads
arbitrary user files. Neither endpoint ever writes a file: the preview
generator is a pure function and "apply" is copy/download in the client.
"""

from __future__ import annotations

import threading
import time
from typing import Annotated, Any

from fastapi import APIRouter, HTTPException, Query
from pydantic import BaseModel, Field

import stackunderflow.deps as deps
from stackunderflow.infra.currency import active_currency_payload
from stackunderflow.reports.anomaly import find_cost_anomalies
from stackunderflow.reports.optimize import find_claudemd_bloat, find_patterns, find_waste
from stackunderflow.reports.prescribe import (
    build_routing_recommendations,
    generate_claudemd_preview,
)
from stackunderflow.reports.scope import parse_period
from stackunderflow.services.context_budget import DEFAULT_SESSIONS_PER_MONTH
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


# ── campaign #7 — prescriptions ──────────────────────────────────────────────

# Upper bound on a POSTed CLAUDE.md body. Real CLAUDE.md files are a few KB;
# 2 MB is two orders of magnitude of headroom while keeping the pure-python
# diff/parse work bounded.
MAX_CLAUDEMD_BYTES = 2_000_000

# Dollar fields converted into the active currency, listed explicitly (same
# convention as routes/forks.py) so a payload change can't silently
# double-convert or skip a field.
_REC_COST_FIELDS = (
    "window_cost_usd",
    "candidate_window_cost_usd",
    "window_delta_usd",
    "estimated_monthly_delta_usd",
)
_PREVIEW_COST_FIELDS = (
    "estimated_savings_usd_per_session",
    "estimated_savings_usd_monthly",
)


class ClaudeMdPreviewBody(BaseModel):
    """POST body for ``/api/optimize/claudemd-preview``.

    Carries the CLAUDE.md *text* — never a path. The server computes the
    slim preview purely from this body and touches no files.
    """

    text: str
    file_label: str = "CLAUDE.md"
    sessions_per_month: int = Field(
        default=DEFAULT_SESSIONS_PER_MONTH, ge=1, le=100_000,
    )


def _convert_routing(routing: dict, rate: float) -> dict:
    if rate == 1.0:
        return routing
    for rec in routing.get("recommendations", []):
        for f in _REC_COST_FIELDS:
            if isinstance(rec.get(f), int | float):
                rec[f] = float(rec[f]) * rate
    for m in routing.get("models", []):
        if isinstance(m.get("window_cost_usd"), int | float):
            m["window_cost_usd"] = float(m["window_cost_usd"]) * rate
    return routing


def _convert_preview(preview: dict, rate: float) -> dict:
    if rate == 1.0:
        return preview
    for f in _PREVIEW_COST_FIELDS:
        if isinstance(preview.get(f), int | float):
            preview[f] = float(preview[f]) * rate
    for entry in preview.get("rationale", []):
        for f in _PREVIEW_COST_FIELDS:
            if isinstance(entry.get(f), int | float):
                entry[f] = float(entry[f]) * rate
    return preview


def _slug_for_prescriptions(project: str | None) -> str | None:
    """Resolve the prescription scope's slug: explicit param, else the
    active project (``deps.current_log_path`` basename), else whole store."""
    if project:
        return project
    path = deps.current_log_path
    if path:
        from pathlib import Path

        return Path(path).name
    return None


def _project_ids_for_slug(conn, slug: str) -> list[int]:
    """``projects.id`` list for *slug* — empty when unknown (advisory scope,
    never a 500; an unknown slug yields empty results, not the whole store)."""
    try:
        rows = conn.execute("SELECT id FROM projects WHERE slug = ?", (slug,)).fetchall()
    except Exception:  # noqa: BLE001 — advisory route, never 500 on a bad store
        return []
    return [int(r["id"]) for r in rows]


def _read_text_defensive(path: str) -> str | None:
    """Read one of the CLAUDE.md files the bloat detector just scanned.

    Read-only, and only ever called with paths produced by
    ``find_claudemd_bloat`` (the established ``~/.claude`` discovery) —
    see the module docstring for the filesystem contract.
    """
    from pathlib import Path

    try:
        return Path(path).read_text(encoding="utf-8", errors="replace")
    except OSError:
        return None


@router.get("/api/optimize/prescriptions")
async def get_prescriptions(
    period: str = "30days",
    project: Annotated[str | None, Query()] = None,
):
    """Prescriptive-cost payload: routing recommendations + CLAUDE.md previews.

    Args:
        period: ``today | 7days | 30days | month | all`` (same set as
            ``/api/optimize``).
        project: Optional project slug. Absent → falls back to the active
            project (``deps.current_log_path``), else spans the whole store.

    Everything is advisory and read-only. Dollar fields are pre-converted
    into the active currency (list in ``_REC_COST_FIELDS`` /
    ``_PREVIEW_COST_FIELDS``).
    """
    if period not in _VALID_PERIODS:
        raise HTTPException(
            status_code=400,
            detail=f"Unknown period '{period}'. Valid: {', '.join(sorted(_VALID_PERIODS))}",
        )
    scope = parse_period(period)
    # Direct-call tolerance (tests invoke the coroutine without FastAPI DI,
    # where the Query default leaks through as a sentinel object).
    project_str = project if isinstance(project, str) else None
    slug = _slug_for_prescriptions(project_str)

    conn = db.connect(deps.store_path)
    try:
        schema.apply(conn)
        project_ids = _project_ids_for_slug(conn, slug) if slug else None
        routing = build_routing_recommendations(conn, scope=scope, project_ids=project_ids)
        bloat_findings = find_claudemd_bloat(
            conn, project_filter=[slug] if slug else None,
        )
    finally:
        conn.close()

    previews: list[dict[str, Any]] = []
    for finding in bloat_findings:
        finding_dict = finding.to_dict()
        for entry in finding.details.get("files", []):
            path = entry.get("path")
            if not path:
                continue
            text = _read_text_defensive(path)
            if text is None:
                continue
            preview = generate_claudemd_preview(
                text, findings=[finding_dict], file_label=path,
            )
            if preview["changed"]:
                preview["source_path"] = path
                previews.append(preview)

    currency = active_currency_payload()
    rate = float(currency.get("rate_from_usd") or 1.0)
    routing = _convert_routing(routing, rate)
    previews = [_convert_preview(p, rate) for p in previews]

    return {
        "scope": scope.label,
        "project": slug,
        "routing": routing,
        "claudemd_previews": previews,
        "currency": currency,
    }


@router.post("/api/optimize/claudemd-preview")
async def post_claudemd_preview(body: ClaudeMdPreviewBody):
    """Slim-preview a CLAUDE.md supplied as text by the client.

    Exists for CLAUDE.md files outside the locations ``/api/optimize``
    already scans (e.g. a repo-local CLAUDE.md): the client sends the
    *text*, the server computes the preview purely from the request body —
    no filesystem read, no write, ever.
    """
    if len(body.text.encode("utf-8", errors="replace")) > MAX_CLAUDEMD_BYTES:
        raise HTTPException(
            status_code=413,
            detail=f"CLAUDE.md text exceeds {MAX_CLAUDEMD_BYTES} bytes",
        )
    preview = generate_claudemd_preview(
        body.text,
        file_label=body.file_label,
        sessions_per_month=body.sessions_per_month,
    )
    currency = active_currency_payload()
    rate = float(currency.get("rate_from_usd") or 1.0)
    return {
        "preview": _convert_preview(preview, rate),
        "currency": currency,
    }
