"""``GET /api/forks`` — fork / sidechain economics for a project (or the store).

Thin HTTP wrapper around :func:`stackunderflow.reports.forks.analyze_forks`.
Surfaces the cost/token share that went to Claude subagent (sidechain)
messages, plus the fork points where the conversation branched and one path was
abandoned — the DAG that ``is_sidechain`` + ``parent_uuid`` already capture but
that nothing had priced.

Currency contract matches every other cost endpoint: dollar figures are
pre-converted into the active currency before send, so the frontend never
multiplies by an FX rate. A ``warning`` field carries the heuristic caveat so
UI consumers can render it inline.
"""

from __future__ import annotations

from pathlib import Path
from typing import Any

from fastapi import APIRouter, HTTPException, Query

import stackunderflow.deps as deps
from stackunderflow.infra.currency import active_currency_payload
from stackunderflow.reports.forks import analyze_forks
from stackunderflow.reports.scope import parse_period
from stackunderflow.store import db

router = APIRouter()

# Friendly period superset — ``week`` maps to ``7days`` inside ``parse_period``
# via the alias table below. Mirrors ``routes/yield_route.py``'s contract so the
# two beta tabs accept the same selector values.
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

# Dollar fields on the top-level report that need converting to the active
# currency. Kept explicit so a schema change can't silently double-convert.
_SUMMARY_COST_FIELDS: tuple[str, ...] = (
    "sidechain_cost_usd",
    "total_cost_usd",
    "abandoned_cost_usd",
)

_WARNING = (
    "Branch abandonment is inferred from the message DAG (parent_uuid): a fork "
    "whose branch stops before the session's last activity is read as dropped. "
    "Edits, retries, and tool re-runs all look like branches, so treat the "
    "abandoned-branch list as a signal to review, not a verdict."
)


def _project_ids_for(conn: Any, path: str) -> list[int]:
    """Resolve a log path to the ``projects.id`` list for its slug.

    Own resolver (no ``store/queries.py`` dependency) — a plain slug lookup
    guarded so a missing project yields an empty scope rather than a 500. Same
    slug-from-basename convention the cost route uses.
    """
    slug = Path(path).name
    try:
        rows = conn.execute(
            "SELECT id FROM projects WHERE slug = ?", (slug,)
        ).fetchall()
    except Exception:  # noqa: BLE001 — advisory route, never 500 on a bad store
        return []
    return [int(r["id"]) for r in rows]


@router.get("/api/forks")
async def get_forks(
    period: str = _PERIOD_QUERY,
    log_path: str | None = _LOG_PATH_QUERY,
):
    """Return ``{period, scope, report, currency, warning}``.

    ``report`` is :meth:`ForkReport.to_dict` with every dollar figure already
    converted to the active currency. When ``log_path`` (or the active
    ``deps.current_log_path``) resolves to a project the analysis is scoped to
    THAT project's sessions; with no project it spans the whole store.
    """
    spec = _PERIOD_ALIASES.get(period)
    if spec is None:
        raise HTTPException(
            status_code=400,
            detail=f"Invalid period '{period}'. Valid: {', '.join(_PERIOD_ALIASES)}",
        )
    scope = parse_period(spec)

    # When invoked directly (tests, not via FastAPI's DI) the ``Query(None)``
    # default leaks through as a Query sentinel — coerce anything that isn't a
    # real string into None so the resolver never sees a non-path object. Same
    # pattern ``routes/yield_route.py`` uses for its list default.
    log_path_str = log_path if isinstance(log_path, str) else None
    path = log_path_str or deps.current_log_path

    conn = db.connect(deps.store_path)
    try:
        project_ids = _project_ids_for(conn, path) if path else None
        report = analyze_forks(conn, scope=scope, project_ids=project_ids)
    finally:
        conn.close()

    currency = active_currency_payload()
    rate = currency["rate_from_usd"]
    if rate != 1.0:
        for k in _SUMMARY_COST_FIELDS:
            if k in report:
                report[k] = float(report[k]) * rate
        for branch in report.get("abandoned_branches", []):
            branch["cost_usd"] = float(branch.get("cost_usd", 0.0)) * rate

    return {
        "period": period,
        "scope": scope.label,
        "report": report,
        "currency": currency,
        "warning": _WARNING,
    }
