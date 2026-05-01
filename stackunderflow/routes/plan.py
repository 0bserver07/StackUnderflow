"""``GET /api/plan`` — current plan + usage payload.

Mirrors the ``stackunderflow plan show`` CLI command so the dashboard can
render a plan-budget widget without scraping the CLI. The cost rollup
reuses ``build_report`` with a Scope pinned to the active billing window
so the math matches whatever ``stackunderflow month`` shows.

Response shape::

    {
        "plan": {"name", "monthly_usd", "reset_day"} | null,
        "usage": {
            "used", "budget", "remaining", "pct",
            "projected": ...,
            "status": "ok" | "warn" | "over",
            "period_start", "period_end",
            "days_so_far", "days_in_period",
        } | null,
        "currency": {"code", "symbol", "rate_from_usd"},
    }

When no plan is set, both ``plan`` and ``usage`` are ``null`` so the
frontend can render an "add a plan" CTA without parsing fields. The
``currency`` block is always present (every cost-bearing endpoint stamps
it) and the dollar fields inside ``usage`` are pre-converted to that
currency — same convention as ``/api/cost-data`` and ``/api/yield`` so a
single ``formatCost(amount, currency)`` callsite renders correctly.
"""

from __future__ import annotations

from datetime import date, datetime, timedelta

from fastapi import APIRouter

import stackunderflow.deps as deps
from stackunderflow.infra.currency import active_currency_payload
from stackunderflow.reports.aggregate import build_report
from stackunderflow.reports.scope import Scope
from stackunderflow.services import plans as plans_mod
from stackunderflow.store import db, schema

router = APIRouter()

# Fields inside the ``usage`` block that hold dollar amounts and need to be
# converted to the active currency before send. ``pct`` / ``days_*`` /
# ``period_*`` are dimensionless or date strings and stay as-is.
_USAGE_COST_FIELDS: tuple[str, ...] = ("used", "budget", "remaining", "projected")


def _spend_in_window(period_start: str, period_end: str) -> float:
    """Sum cost across every project for the inclusive ``[start, end]`` window."""
    start_d = date.fromisoformat(period_start)
    end_d = date.fromisoformat(period_end)
    since = datetime.combine(start_d, datetime.min.time()).isoformat()
    until = datetime.combine(end_d + timedelta(days=1), datetime.min.time()).isoformat()
    scope = Scope(since=since, until=until, label="plan-period")

    conn = db.connect(deps.store_path)
    try:
        schema.apply(conn)
        report = build_report(conn, scope=scope, include=None, exclude=None)
    finally:
        conn.close()
    return float(report["total_cost"])


@router.get("/api/plan")
async def get_plan_status() -> dict:
    """Return the active plan and current usage, or ``{plan: null, usage: null}``."""
    currency = active_currency_payload()

    plan = plans_mod.get_active_plan()
    if plan is None:
        return {"plan": None, "usage": None, "currency": currency}

    # First call resolves the period window; the ``used=0`` argument is a
    # throwaway — we re-call with the real number once we know which dates
    # to sum over.
    window = plans_mod.compute_usage(plan, 0.0)
    used = _spend_in_window(window["period_start"], window["period_end"])
    usage = plans_mod.compute_usage(plan, used)

    # ``plan.monthly_usd`` is the canonical USD amount the user signed up for;
    # we emit it under the original key so callers and tests that pin to USD
    # values keep working. The active-currency mirror lives under
    # ``monthly`` so the dashboard can render the same number with the right
    # symbol when the user runs in a non-USD locale.
    rate = float(currency.get("rate_from_usd") or 1.0)
    usage_block = {
        "used": usage["used"] * rate,
        "budget": usage["budget"] * rate,
        "remaining": usage["remaining"] * rate,
        "pct": usage["pct"],
        # The HTTP route exposes the projected month-end *total* under
        # the shorter ``projected`` key so the dashboard doesn't have
        # to know about the underlying ``project_month_end`` helper.
        "projected": usage["projected_month_end"] * rate,
        "status": usage["status"],
        "period_start": usage["period_start"],
        "period_end": usage["period_end"],
        "days_so_far": usage["days_so_far"],
        "days_in_period": usage["days_in_period"],
    }

    return {
        "plan": {
            "name": plan.name,
            "monthly_usd": plan.monthly_usd,
            "reset_day": plan.reset_day,
        },
        "usage": usage_block,
        "currency": currency,
    }
