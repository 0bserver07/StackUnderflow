"""Spend-budget routes — get / set / clear a user budget + read its status.

Three endpoints over :mod:`stackunderflow.services.budgets`:

* ``GET    /api/budgets`` — the active budget *and* its current status
  (month-to-date vs the monthly ceiling, today vs the daily ceiling, plus a
  linear month-end projection). When no budget is set, ``status`` legs are
  ``null`` so the UI can render an "add a budget" CTA without field-guessing.
* ``PUT    /api/budgets`` — persist a budget. Body: ``{monthly_usd?, daily_usd?}``
  (USD). ``null`` for a leg clears that ceiling; omitting a leg leaves it
  untouched. Returns the same shape as ``GET``.
* ``DELETE /api/budgets`` — clear both ceilings.

The budget itself is persisted through the descriptor-based settings
(``budget_monthly_usd`` / ``budget_daily_usd`` in ``stackunderflow/settings.py``
→ ``~/.stackunderflow/config.json``) — no DB migration. Spend is summed from
the ``usage_events`` cost mart, scoped to the **whole store** (a budget is a
spend cap across everything the user does, not per-project), bucketed on the
caller's local timezone so the month/today boundaries line up with the Cost and
Live tabs.

Every dollar field inside ``status`` is pre-converted to the active currency —
same contract as ``/api/plan`` and ``/api/cost-data`` so a single
``formatCost(amount, currency)`` callsite renders correctly. ``pct`` and the
``status`` strings are dimensionless and stay as-is.
"""

from __future__ import annotations

import calendar
import sqlite3
from datetime import UTC, datetime, timedelta

from fastapi import APIRouter
from pydantic import BaseModel

import stackunderflow.deps as deps
from stackunderflow.infra.currency import active_currency_payload
from stackunderflow.services import budgets as budgets_mod
from stackunderflow.store import db

router = APIRouter()

# Dollar fields inside each status leg that need FX conversion before send.
# ``pct`` / ``status`` are dimensionless.
_LEG_COST_FIELDS: tuple[str, ...] = ("budget", "used", "remaining")


class BudgetBody(BaseModel):
    """Request body for ``PUT /api/budgets``.

    Both fields are optional and ``null``-able. The semantics (set / clear /
    leave-untouched) are resolved in :func:`put_budget` because FastAPI/pydantic
    can't by itself distinguish "field omitted" from "field set to null".
    """

    monthly_usd: float | None = None
    daily_usd: float | None = None


def _table_exists(conn: sqlite3.Connection, name: str) -> bool:
    row = conn.execute(
        "SELECT 1 FROM sqlite_master WHERE type='table' AND name = ?",
        (name,),
    ).fetchone()
    return row is not None


def _month_window(now_local: datetime) -> tuple[int, int]:
    """Return ``(days_so_far, days_in_month)`` for the local calendar month."""
    days_in_month = calendar.monthrange(now_local.year, now_local.month)[1]
    return now_local.day, days_in_month


def _spend_scalars(
    conn: sqlite3.Connection, *, tz_offset: int
) -> tuple[float, float, list[str], int, int]:
    """Sum month-to-date + today spend across the whole store (USD).

    ``tz_offset`` is minutes east of UTC (``aggregator._local_day``'s
    convention). Month-start / today-start are the *local* boundaries expressed
    as UTC instants so they compare directly against the stored ISO-8601 UTC
    ``ts``. Returns ``(month_spend, today_spend, models_used, days_so_far,
    days_in_month)``.

    A fresh store with no ``usage_events`` table returns zeros and an empty
    model list — the route never crashes on a cold install.
    """
    now_utc = datetime.now(UTC)
    now_local = now_utc + timedelta(minutes=tz_offset)
    local_today = now_local.replace(hour=0, minute=0, second=0, microsecond=0)
    local_month = local_today.replace(day=1)
    today_cutoff = (local_today - timedelta(minutes=tz_offset)).isoformat()
    month_cutoff = (local_month - timedelta(minutes=tz_offset)).isoformat()
    days_so_far, days_in_month = _month_window(now_local)

    if not _table_exists(conn, "usage_events"):
        return 0.0, 0.0, [], days_so_far, days_in_month

    row = conn.execute(
        "SELECT "
        "  COALESCE(SUM(CASE WHEN ts >= ? THEN cost_usd ELSE 0 END), 0.0) AS month_cost, "
        "  COALESCE(SUM(CASE WHEN ts >= ? THEN cost_usd ELSE 0 END), 0.0) AS today_cost "
        "FROM usage_events WHERE ts >= ?",
        (month_cutoff, today_cutoff, month_cutoff),
    ).fetchone()
    month_cost = float(row[0] or 0.0)
    today_cost = float(row[1] or 0.0)

    model_rows = conn.execute(
        "SELECT DISTINCT model FROM usage_events "
        "WHERE ts >= ? AND model <> ''",
        (month_cutoff,),
    ).fetchall()
    models = [r[0] for r in model_rows if r[0]]

    return month_cost, today_cost, models, days_so_far, days_in_month


def _convert_status(status: dict, rate: float) -> dict:
    """Pre-convert the USD dollar fields inside a status payload by ``rate``."""
    if rate == 1.0:
        return status
    for leg_key in ("monthly", "daily"):
        leg = status.get(leg_key)
        if isinstance(leg, dict):
            for f in _LEG_COST_FIELDS:
                if isinstance(leg.get(f), (int, float)):
                    leg[f] = leg[f] * rate
    if isinstance(status.get("projected_month_end"), (int, float)):
        status["projected_month_end"] = status["projected_month_end"] * rate
    return status


def _build_payload(*, timezone_offset: int) -> dict:
    """Assemble the ``{budget, status, currency}`` payload for the active budget."""
    currency = active_currency_payload()
    budget = budgets_mod.get_budget()

    conn = db.connect(deps.store_path)
    try:
        month_spend, today_spend, models, days_so_far, days_in_period = _spend_scalars(
            conn, tz_offset=timezone_offset
        )
    finally:
        conn.close()

    status = budgets_mod.compute_status(
        budget,
        month_spend=month_spend,
        today_spend=today_spend,
        days_so_far=days_so_far,
        days_in_period=days_in_period,
    )
    rate = float(currency.get("rate_from_usd") or 1.0)
    status = _convert_status(status, rate)
    # Surface the models that drove the spend so the UI can caption the status.
    status["models"] = sorted(models)

    return {
        "budget": {
            "monthly_usd": budget.monthly_usd,
            "daily_usd": budget.daily_usd,
        },
        "status": status if budget.is_set else None,
        "currency": currency,
    }


@router.get("/api/budgets")
async def get_budget_status(timezone_offset: int = 0) -> dict:
    """Return the active spend budget and its current status.

    ``timezone_offset`` (minutes east of UTC) buckets the month/today spend on
    the caller's local day so the figures line up with the Cost / Live tabs.
    When no budget is set, ``status`` is ``null`` and ``budget`` carries two
    ``null`` legs — the UI renders an "add a budget" CTA.
    """
    return _build_payload(timezone_offset=timezone_offset)


@router.put("/api/budgets")
async def put_budget(body: BudgetBody, timezone_offset: int = 0) -> dict:
    """Persist a spend budget and return the refreshed status.

    Each leg is independent: a positive number sets that ceiling, an explicit
    ``null`` clears it, and an omitted field leaves it untouched. A non-positive
    amount is a 422 (validation error) raised from the service layer.
    """
    fields_set = body.model_fields_set
    current = budgets_mod.get_budget()

    # Resolve each leg: present-in-body → use the body value (incl. explicit
    # null = clear); absent → preserve whatever is already persisted.
    monthly = body.monthly_usd if "monthly_usd" in fields_set else current.monthly_usd
    daily = body.daily_usd if "daily_usd" in fields_set else current.daily_usd

    try:
        budgets_mod.set_budget(monthly_usd=monthly, daily_usd=daily)
    except ValueError as exc:
        from fastapi import HTTPException

        raise HTTPException(status_code=422, detail=str(exc)) from exc

    return _build_payload(timezone_offset=timezone_offset)


@router.delete("/api/budgets")
async def delete_budget(timezone_offset: int = 0) -> dict:
    """Clear both budget ceilings and return the now-empty status."""
    budgets_mod.clear_budget()
    return _build_payload(timezone_offset=timezone_offset)


__all__ = ["router"]
