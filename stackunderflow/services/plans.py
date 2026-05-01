"""Plan budgets — track monthly AI spend against a known plan.

A *plan* is a monthly USD budget the user has signed up for (Claude Pro,
Claude Max, Cursor Pro, etc.) plus a *reset day* — the day of the month
the billing period rolls over (default 1).

The dashboard's raw cost figure answers "how much have I spent?"; this
module answers "how much is left on the plan?" and "am I tracking to go
over?". Projection is intentionally simple linear — see ``project_month_end``
for the assumption.

Public surface (consumed by the CLI command and the HTTP route):

* ``PRESETS``         — the canonical preset name → monthly USD map.
* ``Plan``            — small dataclass: ``name``, ``monthly_usd``, ``reset_day``.
* ``get_active_plan`` — read the active plan from settings; ``None`` if unset.
* ``set_plan``        — persist a preset (or a custom plan) to settings.
* ``reset_plan``      — clear all plan keys from settings.
* ``compute_usage``   — turn (plan, total spend) into a usage payload with
                        status banding (``ok`` / ``warn`` / ``over``).
* ``project_month_end`` — linear extrapolation of month-end spend.

The plan itself lives in three settings keys (``plan_name``,
``plan_monthly_usd``, ``plan_reset_day``); see ``stackunderflow/settings.py``.
"""

from __future__ import annotations

from calendar import monthrange
from dataclasses import dataclass
from datetime import UTC, date, datetime

from stackunderflow.settings import Settings

__all__ = [
    "PRESETS",
    "Plan",
    "compute_usage",
    "get_active_plan",
    "project_month_end",
    "reset_plan",
    "set_plan",
]


# ── presets ──────────────────────────────────────────────────────────────────
#
# Canonical name → monthly USD. ``custom`` is the sentinel that means "the
# user supplied an arbitrary amount"; it cannot resolve a default amount, so
# callers must pass ``monthly_usd`` when ``name == "custom"``.

PRESETS: dict[str, float | None] = {
    "claude-pro": 20.0,
    "claude-max": 200.0,
    "cursor-pro": 20.0,
    "cursor-max": 40.0,
    "custom": None,
}


@dataclass(frozen=True)
class Plan:
    """A monthly budget the user has signed up for.

    ``reset_day`` is the day-of-month the billing window rolls over (1–28
    is universally safe; values up to 31 are accepted but a month with
    fewer days clamps to its last day at usage-compute time).
    """

    name: str
    monthly_usd: float
    reset_day: int = 1


# ── settings I/O ─────────────────────────────────────────────────────────────

_SETTINGS_KEYS = ("plan_name", "plan_monthly_usd", "plan_reset_day")


def get_active_plan() -> Plan | None:
    """Return the active plan from settings, or ``None`` if no plan is set."""
    s = Settings()
    name = s.get("plan_name")
    monthly = s.get("plan_monthly_usd")
    if name is None or monthly is None:
        return None
    reset_day = s.get("plan_reset_day") or 1
    return Plan(name=str(name), monthly_usd=float(monthly), reset_day=int(reset_day))


def set_plan(
    name: str,
    monthly_usd: float | None = None,
    reset_day: int = 1,
) -> Plan:
    """Persist a plan to settings.

    For preset names, ``monthly_usd`` is optional (the preset's amount is
    used). For ``custom``, ``monthly_usd`` is required. ``reset_day`` is
    clamped to ``[1, 31]``; the actual day used at compute time is further
    clamped to the month's last day.

    Returns the resolved ``Plan`` so callers can echo the final shape.
    """
    if name not in PRESETS:
        raise ValueError(
            f"Unknown plan name '{name}'. Valid: {', '.join(sorted(PRESETS))}"
        )

    preset_amount = PRESETS[name]
    if name == "custom":
        if monthly_usd is None:
            raise ValueError("custom plan requires --monthly-usd")
        amount = float(monthly_usd)
    else:
        # An explicit override is allowed (e.g. the user is grandfathered
        # into an old price). Falls back to the preset amount otherwise.
        amount = float(monthly_usd) if monthly_usd is not None else float(preset_amount or 0.0)

    if amount <= 0:
        raise ValueError("monthly_usd must be a positive number")

    if not (1 <= int(reset_day) <= 31):
        raise ValueError("reset_day must be between 1 and 31")

    s = Settings()
    s.persist("plan_name", name)
    s.persist("plan_monthly_usd", amount)
    s.persist("plan_reset_day", int(reset_day))

    return Plan(name=name, monthly_usd=amount, reset_day=int(reset_day))


def reset_plan() -> None:
    """Clear every plan key from settings."""
    s = Settings()
    for key in _SETTINGS_KEYS:
        s.remove(key)


# ── projection + usage math ──────────────────────────────────────────────────

def project_month_end(daily_burn: float, days_left: int) -> float:
    """Extrapolate the month-end spend assuming today's burn rate continues.

    This is intentionally a *simple linear* projection — total spend so far
    plus ``daily_burn × days_left``. It does not account for weekends,
    holidays, project ramps, or week-of-month seasonality. The number is
    a directional signal, not a forecast.

    The caller is expected to add the current spend to this delta:

        projected_total = used + project_month_end(daily_burn, days_left)
    """
    if daily_burn <= 0 or days_left <= 0:
        return 0.0
    return float(daily_burn) * float(days_left)


def _period_window(plan: Plan, *, now: datetime) -> tuple[date, date, int, int]:
    """Resolve the current billing window for ``plan`` relative to ``now``.

    Returns ``(period_start, period_end, days_so_far, days_in_period)``:

    * ``period_start`` — first calendar date of the active window
    * ``period_end``   — last calendar date of the active window (inclusive)
    * ``days_so_far``  — days from ``period_start`` through ``now`` (inclusive,
                        floor 1 so we never divide by zero on the reset day)
    * ``days_in_period`` — total days in the window

    The window is anchored on ``plan.reset_day``; if the day exceeds the
    month length (e.g. day 31 in February), we clamp to the last day of
    the month. So a Jan 31 reset-day on a Feb 28 month rolls Feb 28 → Mar 31.
    """
    today = now.date()
    year, month = today.year, today.month

    last_day_this_month = monthrange(year, month)[1]
    reset_clamped = min(plan.reset_day, last_day_this_month)

    if today.day >= reset_clamped:
        # We're inside the window that started on this month's reset day.
        period_start = date(year, month, reset_clamped)
        # End is the day before next month's reset day (clamped to next
        # month's length), inclusive.
        n_year, n_month = (year + 1, 1) if month == 12 else (year, month + 1)
        last_day_next_month = monthrange(n_year, n_month)[1]
        next_reset_clamped = min(plan.reset_day, last_day_next_month)
        next_reset = date(n_year, n_month, next_reset_clamped)
        # period_end is one day before the next reset.
        period_end = date.fromordinal(next_reset.toordinal() - 1)
    else:
        # We're inside the window that started on last month's reset day.
        p_year, p_month = (year - 1, 12) if month == 1 else (year, month - 1)
        last_day_prev_month = monthrange(p_year, p_month)[1]
        prev_reset_clamped = min(plan.reset_day, last_day_prev_month)
        period_start = date(p_year, p_month, prev_reset_clamped)
        period_end = date.fromordinal(date(year, month, reset_clamped).toordinal() - 1)

    days_in_period = (period_end - period_start).days + 1
    days_so_far = max(1, (today - period_start).days + 1)
    return period_start, period_end, days_so_far, days_in_period


def compute_usage(
    plan: Plan,
    total_usd_this_period: float,
    *,
    now: datetime | None = None,
) -> dict:
    """Turn a plan + period total into a structured usage payload.

    Returned dict shape (all numeric fields are USD floats):

        {
            "used":         total_usd_this_period,
            "budget":       plan.monthly_usd,
            "remaining":    budget - used        # may be negative,
            "pct":          100 * used / budget  # >100 if over,
            "projected_month_end": projected total at end of period,
            "status":       "ok"   if pct < 80
                             "warn" if 80 <= pct <= 100
                             "over" if pct > 100,
            "period_start": ISO date string,
            "period_end":   ISO date string,
            "days_so_far":  int,
            "days_in_period": int,
        }

    The ``ok``/``warn``/``over`` thresholds match the HTTP route contract.
    """
    now = now or datetime.now(UTC)
    used = float(total_usd_this_period)
    budget = float(plan.monthly_usd)

    period_start, period_end, days_so_far, days_in_period = _period_window(plan, now=now)

    pct = 100.0 * used / budget if budget > 0 else 0.0
    daily_burn = used / days_so_far if days_so_far > 0 else 0.0
    days_left = max(0, days_in_period - days_so_far)
    projected_delta = project_month_end(daily_burn, days_left)
    projected = used + projected_delta

    if pct > 100:
        status = "over"
    elif pct >= 80:
        status = "warn"
    else:
        status = "ok"

    return {
        "used": used,
        "budget": budget,
        "remaining": budget - used,
        "pct": pct,
        "projected_month_end": projected,
        "status": status,
        "period_start": period_start.isoformat(),
        "period_end": period_end.isoformat(),
        "days_so_far": days_so_far,
        "days_in_period": days_in_period,
    }
