"""``GET /api/yield`` — productive vs reverted vs abandoned session breakdown.

Thin HTTP wrapper around ``services.yield_tracker.compute_yield``. Costs are
returned in the user's active currency (same convention as every other cost
endpoint), and the body always carries a ``warning`` field with the
heuristic caveat so frontend consumers can render it inline.
"""

from __future__ import annotations

from fastapi import APIRouter, HTTPException, Query

import stackunderflow.deps as deps
from stackunderflow.infra.currency import active_currency_payload
from stackunderflow.services.yield_tracker import (
    compute_yield,
    to_dicts,
    yield_summary,
)
from stackunderflow.store import db

router = APIRouter()

# Periods we accept here are a friendly superset — ``week`` is mapped to
# ``7days`` inside the tracker. ``all`` is intentionally excluded from the
# default UI surface but still works.
_VALID_PERIODS = ("today", "week", "month", "all", "7days", "30days")

# Module-level Query singletons — Click-style defaults computed at import,
# so the function signature stays clean and ruff B008 stays happy.
_PERIOD_QUERY = Query("month", description="today | week | month | all")
_PROJECT_QUERY = Query(None, description="Filter by project slug (repeatable)")

# The heuristic disclaimer lives next to the data so consumers (CLI / UI)
# never display the breakdown without it.
_WARNING = (
    "Yield is correlated by time, not by content. A commit that lands within "
    "24h of a session is credited to that session even if it was about something "
    "else. Treat the breakdown as a smoke signal, not a verdict."
)

# Fields that hold per-entry dollar amounts and need converting alongside
# the summary totals. Keeping the list explicit prevents surprising the
# frontend with double-converted fields.
_ENTRY_COST_FIELDS: tuple[str, ...] = ("cost_usd",)
_SUMMARY_COST_FIELDS: tuple[str, ...] = (
    "productive_cost",
    "reverted_cost",
    "abandoned_cost",
    "no_repo_cost",
    "total_cost",
)


@router.get("/api/yield")
async def get_yield(
    period: str = _PERIOD_QUERY,
    project: list[str] | None = _PROJECT_QUERY,
):
    """Return ``{period, summary, entries, currency, warning}``.

    ``summary`` carries productive/reverted/abandoned/no_repo counts and
    USD-priced cost totals (converted to the active currency before send).
    ``entries`` is the per-session list, sorted by start time.
    """
    if period not in _VALID_PERIODS:
        raise HTTPException(
            status_code=400,
            detail=(
                f"Invalid period '{period}'. "
                f"Valid: {', '.join(_VALID_PERIODS)}"
            ),
        )

    # When the route is invoked directly (tests, not via FastAPI's DI), the
    # ``Query(None)`` default leaks through as a Query sentinel — coerce
    # anything that isn't a real list into None so the service sees what
    # it expects. Same pattern ``routes/compare.py`` already uses.
    project_filter = list(project) if isinstance(project, list) else None

    conn = db.connect(deps.store_path)
    try:
        entries = compute_yield(conn, period=period, project_filter=project_filter)
        # Sort by cost desc so the UI's table renders meaningfully without
        # needing client-side reordering.
        sorted_entries = sorted(entries, key=lambda e: e.cost_usd, reverse=True)
        body_entries = to_dicts(sorted_entries)

        from stackunderflow.services.outcome_attribution import get_outcomes_for_session
        for e in body_entries:
            outcomes = get_outcomes_for_session(conn, e["session_id"])
            e["pr"] = outcomes["prs"]
            e["ci_runs"] = outcomes["ci_runs"]
    finally:
        conn.close()

    summary = yield_summary(entries)

    currency = active_currency_payload()
    rate = currency["rate_from_usd"]
    if rate != 1.0:
        for e in body_entries:
            for k in _ENTRY_COST_FIELDS:
                e[k] = float(e[k]) * rate
        for k in _SUMMARY_COST_FIELDS:
            summary[k] = float(summary[k]) * rate

    return {
        "period": period,
        "summary": summary,
        "entries": body_entries,
        "currency": currency,
        "warning": _WARNING,
    }
