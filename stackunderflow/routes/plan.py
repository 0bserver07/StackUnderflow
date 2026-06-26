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
        "projection": {
            "projected_month_end_usd", "projection_method",
            "daily_burn_usd", "days_to_limit",
            "thresholds", "crossed_threshold", "alert",
        } | null,
        "currency": {"code", "symbol", "rate_from_usd"},
    }

When no plan is set, both ``plan`` and ``usage`` are ``null`` so the
frontend can render an "add a plan" CTA without parsing fields. The
``currency`` block is always present (every cost-bearing endpoint stamps
it) and the dollar fields inside ``usage`` are pre-converted to that
currency — same convention as ``/api/cost-data`` and ``/api/yield`` so a
single ``formatCost(amount, currency)`` callsite renders correctly.

The ``projection`` block (added in burn-projector v2) is the structured
shape ``stackunderflow.services.burn.build_projection`` emits — the
dollar fields ``projected_month_end_usd`` and ``daily_burn_usd`` are
pre-converted to the active currency just like ``usage`` so the UI can
``formatCost`` them with the same helper.
"""

from __future__ import annotations

import threading
from datetime import date, datetime, timedelta
from pathlib import Path

from fastapi import APIRouter

import stackunderflow.deps as deps
from stackunderflow.infra.currency import active_currency_payload
from stackunderflow.reports.aggregate import build_report
from stackunderflow.reports.scope import Scope
from stackunderflow.services import burn
from stackunderflow.services import plans as plans_mod
from stackunderflow.settings import Settings
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


def _spend_daily_window(
    period_start: str,
    period_end: str,
    *,
    store_path: Path | None = None,
) -> list[float]:
    """Per-day USD cost across every project, oldest-first.

    Returns a list whose values align with the calendar days in
    ``[period_start, period_end]``. Days with no recorded spend are
    represented as ``0.0`` so the burn projector sees the *true* shape
    of the user's activity (a quiet weekend should drag the weighted
    average down, not be silently elided).

    The query reuses the v0.7.2 ``usage_events`` mart whose
    ``cost_usd`` is already attributed and rate-aware. We intentionally
    don't fall back to the legacy ``messages`` path here — when the mart
    is empty the route still returns ``[]`` and the burn projector
    naturally degrades to "no data → linear projection of 0".
    """
    start_d = date.fromisoformat(period_start)
    end_d = date.fromisoformat(period_end)
    since = datetime.combine(start_d, datetime.min.time()).isoformat()
    until = datetime.combine(end_d + timedelta(days=1), datetime.min.time()).isoformat()

    path = store_path if store_path is not None else deps.store_path
    conn = db.connect(path)
    try:
        schema.apply(conn)
        # Probe — when the mart is empty the SUM returns no rows; we want
        # an explicit zero-day list of length ``days_so_far`` so the
        # weighted projection gets the right denominator.
        rows = conn.execute(
            "SELECT substr(ts, 1, 10) AS day, SUM(cost_usd) AS cost "
            "FROM usage_events WHERE ts >= ? AND ts < ? "
            "GROUP BY day ORDER BY day",
            (since, until),
        ).fetchall()
    finally:
        conn.close()

    # Build a date-keyed map so we can fill missing days with zero in
    # one pass. Walk start_d → today (inclusive) so the list is
    # oldest-first and aligned with ``daily_costs[-1]`` == today.
    today = date.today()
    last_day = min(end_d, today)
    by_day = {r["day"] if hasattr(r, "keys") else r[0]: float(
        (r["cost"] if hasattr(r, "keys") else r[1]) or 0.0
    ) for r in rows}
    out: list[float] = []
    cursor = start_d
    while cursor <= last_day:
        out.append(by_day.get(cursor.isoformat(), 0.0))
        cursor = date.fromordinal(cursor.toordinal() + 1)
    return out


# ── spend-rollup memo (#27) ──────────────────────────────────────────────────
#
# ``/api/plan`` is hit on every Overview render *and* on a poll timer, but the
# spend rollup it needs costs ~0.6s: ``build_report`` sums every project's cost
# across the billing window and ``_spend_daily_window`` walks the per-day series
# — plus each opens a connection and runs ``schema.apply``. Both inputs only
# move when a new event is ingested, which bumps ``store.db``'s mtime, so we
# memoise the USD-denominated ``(used, daily_costs)`` pair keyed on
# ``(store_path, period_start, period_end)`` and validate it against the store
# mtime (the same self-evicting pattern as the optimize-route cache). On a hit
# we never open a connection — ``build_report`` and ``schema.apply`` are
# skipped. Currency conversion and plan banding run per-request off these cached
# USD numbers, so a currency switch is a cheap rescale and the cache never has
# to be flushed on a settings write.
_SPEND_CACHE: dict[tuple[str, str, str], tuple[int, tuple[float, list[float]]]] = {}
_SPEND_CACHE_LOCK = threading.Lock()
_SPEND_CACHE_MAX = 8


def _store_mtime_ns() -> int:
    """Return ``store.db`` mtime in nanoseconds, or 0 when missing."""
    try:
        return deps.store_path.stat().st_mtime_ns
    except (OSError, AttributeError):
        return 0


def invalidate_plan_cache() -> None:
    """Drop every memoised spend rollup. Cheap — the cache is tiny."""
    with _SPEND_CACHE_LOCK:
        _SPEND_CACHE.clear()


def _spend_for_window(period_start: str, period_end: str) -> tuple[float, list[float]]:
    """Return the memoised ``(used_usd, daily_costs_usd)`` for the window.

    Both halves are read from one ``store.db`` revision — the scalar total
    from :func:`_spend_in_window`, the per-day series from
    :func:`_spend_daily_window` — and cached against the store mtime. Repeat
    polls inside one revision skip the ~0.6s ``build_report`` pass (and the
    per-request ``schema.apply``); a fresh ingest bumps the mtime and the
    entry drifts out naturally.
    """
    key = (str(deps.store_path), period_start, period_end)
    mtime = _store_mtime_ns()
    with _SPEND_CACHE_LOCK:
        hit = _SPEND_CACHE.get(key)
    if hit is not None and hit[0] == mtime:
        return hit[1]

    used = _spend_in_window(period_start, period_end)
    daily = _spend_daily_window(period_start, period_end)

    with _SPEND_CACHE_LOCK:
        if key not in _SPEND_CACHE and len(_SPEND_CACHE) >= _SPEND_CACHE_MAX:
            # Tiny FIFO trim — the key space is one entry per active window.
            oldest = next(iter(_SPEND_CACHE), None)
            if oldest is not None:
                _SPEND_CACHE.pop(oldest, None)
        _SPEND_CACHE[key] = (mtime, (used, daily))
    return used, daily


@router.get("/api/plan")
async def get_plan_status() -> dict:
    """Return the active plan and current usage, or ``{plan: null, usage: null}``."""
    currency = active_currency_payload()

    plan = plans_mod.get_active_plan()
    if plan is None:
        return {
            "plan": None,
            "usage": None,
            "projection": None,
            "currency": currency,
        }

    # First call resolves the period window; the ``used=0`` argument is a
    # throwaway. The window depends only on the plan + today (not on spend),
    # so it's stable enough to key the spend memo on.
    window = plans_mod.compute_usage(plan, 0.0)
    # Both the scalar spend and the per-day series come from one memoised
    # store revision (#27) — repeat polls skip the ~0.6s build_report pass.
    # ``build_projection`` (below) is pure and picks linear vs weighted-7d
    # from the per-day sample count, so it runs fresh off the cached series.
    used, daily = _spend_for_window(window["period_start"], window["period_end"])
    usage = plans_mod.compute_usage(plan, used)

    thresholds = Settings().get("plan_alert_thresholds") or list(burn.DEFAULT_THRESHOLDS)
    projection_usd = burn.build_projection(
        daily_costs=daily,
        used=usage["used"],
        budget=plan.monthly_usd,
        days_so_far=usage["days_so_far"],
        days_in_period=usage["days_in_period"],
        thresholds=thresholds,
    )

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

    # Pre-convert the projection's dollar fields the same way usage is
    # converted — UI calls ``formatCost(amount, currency)`` once and the
    # right symbol falls out. ``thresholds`` / ``crossed_threshold`` /
    # ``days_to_limit`` / ``projection_method`` are dimensionless.
    projection_block = {
        "projected_month_end_usd": projection_usd["projected_month_end_usd"] * rate,
        "projection_method": projection_usd["projection_method"],
        "daily_burn_usd": projection_usd["daily_burn_usd"] * rate,
        "days_to_limit": projection_usd["days_to_limit"],
        "thresholds": projection_usd["thresholds"],
        "crossed_threshold": projection_usd["crossed_threshold"],
        "alert": projection_usd["alert"],
    }

    return {
        "plan": {
            "name": plan.name,
            "monthly_usd": plan.monthly_usd,
            "reset_day": plan.reset_day,
        },
        "usage": usage_block,
        "projection": projection_block,
        "currency": currency,
    }
