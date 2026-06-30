"""Spend budgets — user-set monthly / daily USD ceilings (audit #7 part 2).

A *budget* is a ceiling the user picks for themselves — "don't let me spend
more than $150 this month" or "$10 a day". It is deliberately distinct from a
*plan* (:mod:`stackunderflow.services.plans`), which models a *known*
subscription (Claude Max, Cursor Pro) the user already pays for. A user can
have both: the plan answers "how much of my subscription have I burned?" and a
budget answers "am I about to blow past the cap I set for myself?".

Either ceiling may be unset (``None``). Setting only a monthly budget, only a
daily budget, or both are all valid.

This module is pure: it owns the settings I/O and the status math, but knows
nothing about SQL. The route (``stackunderflow.routes.budgets``) slices the
month-to-date and today spend out of the store and hands the scalars here.

Public surface (consumed by the HTTP route):

* :class:`Budget`        — ``monthly_usd`` / ``daily_usd``, both optional.
* :func:`get_budget`     — read the active budget from settings.
* :func:`set_budget`     — persist (validates positivity; ``None`` clears a leg).
* :func:`clear_budget`   — remove both budget keys from settings.
* :func:`compute_status` — turn a budget + spend scalars into a status payload
                           with ``under`` / ``approaching`` / ``over`` banding.

The budget lives in two settings keys (``budget_monthly_usd``,
``budget_daily_usd``); see ``stackunderflow/settings.py``.
"""

from __future__ import annotations

from dataclasses import dataclass

from stackunderflow.services.plans import project_month_end
from stackunderflow.settings import Settings

__all__ = [
    "APPROACHING_PCT",
    "Budget",
    "clear_budget",
    "compute_status",
    "get_budget",
    "set_budget",
]

# A budget leg is "approaching" once spend reaches this percentage of the
# ceiling (and still <= 100). Matches the plan tab's ``warn`` band so the two
# cost surfaces use one mental model for the amber state.
APPROACHING_PCT: float = 80.0

_SETTINGS_KEYS = ("budget_monthly_usd", "budget_daily_usd")


@dataclass(frozen=True)
class Budget:
    """User-set spend ceilings. Either leg may be ``None`` (unset)."""

    monthly_usd: float | None = None
    daily_usd: float | None = None

    @property
    def is_set(self) -> bool:
        """True when at least one ceiling is configured."""
        return self.monthly_usd is not None or self.daily_usd is not None


# ── settings I/O ─────────────────────────────────────────────────────────────


def get_budget() -> Budget:
    """Return the active budget from settings.

    A budget is always returned (never ``None``); an all-unset budget has both
    legs ``None`` and ``Budget.is_set is False``. Defensive: a non-numeric or
    non-positive persisted value is treated as unset rather than raising, so a
    hand-edited ``config.json`` can't wedge the route.
    """
    s = Settings()
    return Budget(
        monthly_usd=_coerce_positive(s.get("budget_monthly_usd")),
        daily_usd=_coerce_positive(s.get("budget_daily_usd")),
    )


def set_budget(
    monthly_usd: float | None = None,
    daily_usd: float | None = None,
) -> Budget:
    """Persist a budget to settings and return the resolved shape.

    Each leg is independent:

    * a positive number sets that ceiling,
    * ``None`` clears that ceiling (removes the key),

    so ``set_budget(monthly_usd=150)`` leaves any existing daily budget intact,
    while ``set_budget(monthly_usd=None)`` clears the monthly leg. A
    non-positive amount raises ``ValueError`` — a $0 or negative ceiling is
    never a meaningful budget.
    """
    s = Settings()
    _apply_leg(s, "budget_monthly_usd", monthly_usd)
    _apply_leg(s, "budget_daily_usd", daily_usd)
    return get_budget()


def clear_budget() -> None:
    """Remove both budget keys from settings."""
    s = Settings()
    for key in _SETTINGS_KEYS:
        s.remove(key)


def _apply_leg(s: Settings, key: str, value: float | None) -> None:
    if value is None:
        s.remove(key)
        return
    amount = float(value)
    if amount <= 0:
        raise ValueError(f"{key} must be a positive number")
    s.persist(key, amount)


def _coerce_positive(raw: object) -> float | None:
    """Return a positive float, or ``None`` for unset / invalid / non-positive."""
    if raw is None:
        return None
    try:
        amount = float(raw)  # type: ignore[arg-type]
    except (TypeError, ValueError):
        return None
    return amount if amount > 0 else None


# ── status math ──────────────────────────────────────────────────────────────


def _band(used: float, limit: float) -> str:
    """Map ``used`` against ``limit`` to ``under`` / ``approaching`` / ``over``."""
    if limit <= 0:
        return "under"
    pct = 100.0 * used / limit
    if pct > 100.0:
        return "over"
    if pct >= APPROACHING_PCT:
        return "approaching"
    return "under"


def _leg_status(used: float, limit: float) -> dict:
    """Build one budget leg's status block (USD)."""
    pct = 100.0 * used / limit if limit > 0 else 0.0
    return {
        "budget": limit,
        "used": used,
        "remaining": limit - used,
        "pct": pct,
        "status": _band(used, limit),
    }


def compute_status(
    budget: Budget,
    *,
    month_spend: float,
    today_spend: float,
    days_so_far: int,
    days_in_period: int,
) -> dict:
    """Turn a budget + spend scalars into a structured status payload.

    The caller supplies the spend already summed out of the store:

    * ``month_spend`` — USD spent in the current billing/calendar month so far.
    * ``today_spend`` — USD spent today (since local midnight).
    * ``days_so_far`` / ``days_in_period`` — the month window the projection
      extrapolates over (e.g. day 12 of a 30-day month).

    Returns ``{"monthly": <leg|null>, "daily": <leg|null>,
    "projected_month_end": <usd|null>, "projection_overruns": <bool|null>}``.
    A leg is ``null`` when its ceiling is unset. Each leg block carries
    ``budget`` / ``used`` / ``remaining`` / ``pct`` / ``status`` (all dollar
    fields USD; the route converts to the active currency).

    The month-end projection reuses
    :func:`stackunderflow.services.plans.project_month_end` — the same simple
    linear "today's daily burn × days left" extrapolation the plan tab uses —
    so the two surfaces agree on the forecast. ``projection_overruns`` flags
    whether that projected total exceeds the monthly ceiling, which lets the UI
    warn *before* the month is actually over budget. Both projection fields are
    ``null`` when no monthly budget is set (nothing to project against).
    """
    monthly_block = None
    daily_block = None
    projected = None
    projection_overruns = None

    if budget.monthly_usd is not None:
        monthly_block = _leg_status(month_spend, budget.monthly_usd)
        days_left = max(0, int(days_in_period) - int(days_so_far))
        daily_burn = month_spend / days_so_far if days_so_far > 0 else 0.0
        projected = month_spend + project_month_end(daily_burn, days_left)
        projection_overruns = projected > budget.monthly_usd

    if budget.daily_usd is not None:
        daily_block = _leg_status(today_spend, budget.daily_usd)

    return {
        "monthly": monthly_block,
        "daily": daily_block,
        "projected_month_end": projected,
        "projection_overruns": projection_overruns,
    }
