"""Burn projector v2 — month-end forecast + alert thresholds.

The original linear projection in :mod:`stackunderflow.services.plans`
flat-lines whatever the average daily burn happens to be (``used / days_so_far``)
and extrapolates it. That under- or over-shoots whenever the user's actual
recent rhythm differs from the running average — a single $80 spike on day 1
of a $100 plan reads as "you'll spend $2400 this month" and never decays.

This module adds two refinements:

1. ``weighted_projection`` — exponentially-weighted average over the last
   ``window`` days (default 7). Recent days dominate; older days fade with
   ``decay`` (default 0.85). Smoother and more current than the linear
   running mean once the user has at least a handful of days of history.

2. ``days_to_limit`` — answers "given today's burn rate, how many more days
   until I hit my plan limit?". Returns ``None`` when the plan can't be
   exhausted at the current rate (zero burn, already over, or limit
   already reached).

3. ``crossed_thresholds`` — given a current pct-used and a list of
   threshold percentages (e.g. ``[50, 75, 90]``), returns the highest
   threshold that the user has met or crossed. Returns ``None`` when none
   of the thresholds have been hit. Used by the CLI / route / MCP /
   meta-agent surfaces to render an alert line.

Public surface
--------------

* :func:`linear_projection` — the v1 simple per-day-average extrapolation.
* :func:`weighted_projection` — exponentially-weighted recent average.
* :func:`days_to_limit` — calendar-days until the plan limit is hit.
* :func:`crossed_thresholds` — highest threshold met, or ``None``.
* :func:`pick_projection_method` — heuristic that returns ``"weighted-7d"``
  when there are at least 3 daily samples, else ``"linear"``.
* :func:`build_projection` — orchestrates the above into the JSON-safe dict
  the routes / CLI / MCP all emit.
* :data:`DEFAULT_THRESHOLDS` — the 50/75/90 default if the user hasn't
  set their own via ``stackunderflow plan thresholds set``.

The module has no SQL knowledge; the caller is responsible for slicing the
daily-cost array out of the store. See ``stackunderflow.routes.plan`` for
the wire-up.
"""

from __future__ import annotations

from collections.abc import Iterable, Sequence
from typing import Literal

__all__ = [
    "DEFAULT_THRESHOLDS",
    "DEFAULT_WEIGHTED_DECAY",
    "DEFAULT_WEIGHTED_WINDOW",
    "ProjectionMethod",
    "build_projection",
    "crossed_thresholds",
    "days_to_limit",
    "linear_projection",
    "pick_projection_method",
    "weighted_projection",
]


# Number of days we look back for the weighted average. 7 lines up with a
# typical work week and is small enough to react quickly to a Monday-morning
# burst, large enough to smooth weekends out.
DEFAULT_WEIGHTED_WINDOW = 7

# Geometric decay applied to the weighted average. 0.85 means yesterday weighs
# 85% of today, the day before weighs ~72%, etc. Picked to put roughly 70%
# of the weight on the most recent 4 days.
DEFAULT_WEIGHTED_DECAY = 0.85

# Built-in alert ladder. The CLI / route / MCP all default to these when the
# user hasn't configured their own via ``plan thresholds set``.
DEFAULT_THRESHOLDS: tuple[int, ...] = (50, 75, 90)

# Below this many daily samples we fall back to the v1 linear projection —
# a 7-day weighted average over <3 days is dominated by whichever day we
# started recording, which produces wild swings on day 1 / day 2 of a billing
# period. Three days is the smallest window where the geometric weights
# meaningfully damp the most-recent observation.
_MIN_SAMPLES_FOR_WEIGHTED = 3

ProjectionMethod = Literal["linear", "weighted-7d"]


def linear_projection(daily_costs: Sequence[float]) -> float:
    """Average per-day cost across the supplied window — the v1 baseline.

    Empty or all-zero input returns ``0.0``. Negative values are clamped to
    zero (a refund or a normalisation glitch should not subtract from the
    forecast). The number is the *daily burn rate* the caller can multiply
    by ``days_left`` to get a forecast tail.
    """
    cleaned = [max(0.0, float(c)) for c in daily_costs]
    if not cleaned:
        return 0.0
    return sum(cleaned) / len(cleaned)


def weighted_projection(
    daily_costs: Sequence[float],
    *,
    window: int = DEFAULT_WEIGHTED_WINDOW,
    decay: float = DEFAULT_WEIGHTED_DECAY,
) -> float:
    """Exponentially-weighted average of the most-recent ``window`` days.

    The input is interpreted as oldest-first (``daily_costs[-1]`` is *today*,
    same orientation as ``daily_costs`` rows from
    :mod:`stackunderflow.store.queries.get_global_stats`). Days outside the
    window are ignored. The decay weight at offset ``k`` from the most-recent
    day is ``decay ** k``; the result is the weighted sum divided by the
    weight sum so the answer stays a per-day USD number, not a tally.

    Falls back to a plain mean when ``decay`` is 1.0 (no decay applied).
    """
    cleaned = [max(0.0, float(c)) for c in daily_costs]
    if not cleaned:
        return 0.0

    if window <= 0:
        window = DEFAULT_WEIGHTED_WINDOW
    tail = cleaned[-window:]
    if not (0.0 < decay <= 1.0):
        decay = DEFAULT_WEIGHTED_DECAY

    # Walk newest → oldest so weight 1.0 lands on today's spend.
    total_weight = 0.0
    total = 0.0
    weight = 1.0
    for value in reversed(tail):
        total += value * weight
        total_weight += weight
        weight *= decay
    if total_weight == 0.0:
        return 0.0
    return total / total_weight


def pick_projection_method(daily_costs: Sequence[float]) -> ProjectionMethod:
    """Choose the projection method based on the supplied daily-cost history.

    With at least ``_MIN_SAMPLES_FOR_WEIGHTED`` non-trivial samples, the
    weighted-7d variant gives a more current signal and avoids the
    "first-day spike never decays" artefact of the linear running mean.
    Below that we fall back to ``"linear"`` because the weighted average
    over a 1-2 day window collapses to "today's number" which is too
    jumpy on the first days of a billing period.
    """
    if len([c for c in daily_costs if c > 0]) >= _MIN_SAMPLES_FOR_WEIGHTED:
        return "weighted-7d"
    return "linear"


def days_to_limit(
    spent: float,
    daily_avg: float,
    limit: float,
) -> int | None:
    """Calendar days until cumulative spend hits ``limit`` at ``daily_avg``.

    Returns ``None`` when the answer is undefined or unreachable:

    * ``daily_avg <= 0`` — zero burn means the limit is never hit.
    * ``spent >= limit`` — already overrun, surface ``0`` would be misleading.
    * ``limit <= 0``     — no plan, no projection.

    Otherwise returns the *integer floor* of remaining-budget / daily-burn
    so a "days left" callout doesn't promise a fraction of a day. The math
    does not constrain the answer to the current billing window — that's
    the caller's responsibility (compare against ``days_in_period -
    days_so_far``).
    """
    spent_f = float(spent)
    daily_f = float(daily_avg)
    limit_f = float(limit)

    if limit_f <= 0 or daily_f <= 0 or spent_f >= limit_f:
        return None
    remaining = limit_f - spent_f
    return int(remaining // daily_f)


def crossed_thresholds(
    pct: float,
    thresholds: Iterable[int] = DEFAULT_THRESHOLDS,
) -> int | None:
    """Highest threshold (as an int percentage) met or exceeded by ``pct``.

    Returns ``None`` when none of the thresholds have been crossed. The
    intent is "show one alert line, not three", so we surface only the
    most-severe threshold the user has tripped; the frontend / CLI uses
    this to colour-band the row.
    """
    pct_f = float(pct)
    crossed = [int(t) for t in thresholds if pct_f >= float(t)]
    if not crossed:
        return None
    return max(crossed)


def build_projection(
    *,
    daily_costs: Sequence[float],
    used: float,
    budget: float,
    days_so_far: int,
    days_in_period: int,
    thresholds: Iterable[int] | None = None,
    method: ProjectionMethod | None = None,
) -> dict:
    """Compose the projection block the routes / CLI / MCP all emit.

    Returned dict shape::

        {
            "projected_month_end_usd": float,   # full-month forecast incl. used
            "projection_method":       "linear" | "weighted-7d",
            "daily_burn_usd":          float,   # the rate the projection used
            "days_to_limit":           int | None,
            "thresholds":              [int, ...],
            "crossed_threshold":       int | None,  # highest met or null
            "alert":                   str | None,  # human-readable banner
        }

    The function is pure — it does no SQL, no settings I/O. Wiring is the
    caller's job (see :mod:`stackunderflow.routes.plan` and
    :mod:`stackunderflow.cli`).
    """
    threshold_list = sorted({int(t) for t in (thresholds or DEFAULT_THRESHOLDS)})
    chosen = method or pick_projection_method(daily_costs)
    if chosen == "weighted-7d":
        daily_burn = weighted_projection(daily_costs)
        # Stale-store fallback — when the last 7 days are all zero but the
        # period as a whole had real activity, the weighted-7d figure
        # silently collapses to $0 and we'd forecast a $0 month-end. That's
        # *technically* correct (your last 7 days really are quiet) but
        # misleading when the cause is a stale store, not an actual quiet
        # week. Falling back to the linear average over the whole period
        # gives a more useful number; the caller-facing
        # ``projection_method`` switches to ``"linear"`` so the user can
        # see what happened.
        if daily_burn == 0.0:
            linear_burn = linear_projection(daily_costs)
            if linear_burn > 0.0:
                daily_burn = linear_burn
                chosen = "linear"
    else:
        daily_burn = linear_projection(daily_costs)

    days_left = max(0, int(days_in_period) - int(days_so_far))
    projected = float(used) + daily_burn * days_left

    pct = (100.0 * float(used) / float(budget)) if float(budget) > 0 else 0.0
    crossed = crossed_thresholds(pct, threshold_list)
    dtl = days_to_limit(used, daily_burn, budget)

    alert = _alert_message(
        crossed=crossed,
        days_to_limit_value=dtl,
        days_left=days_left,
        budget=float(budget),
        projected=projected,
    )

    return {
        "projected_month_end_usd": projected,
        "projection_method": chosen,
        "daily_burn_usd": daily_burn,
        "days_to_limit": dtl,
        "thresholds": threshold_list,
        "crossed_threshold": crossed,
        "alert": alert,
    }


def _alert_message(
    *,
    crossed: int | None,
    days_to_limit_value: int | None,
    days_left: int,
    budget: float,
    projected: float,
) -> str | None:
    """Produce a single human-readable alert string, or ``None``.

    Priority:
      1. Already over budget.
      2. Projected to overrun before the period ends, with a "by day N" note
         when ``days_to_limit_value`` lands inside the remaining window.
      3. Crossed a configured threshold (50 / 75 / 90 by default).

    Returning ``None`` means the CLI / UI should suppress the alert line.
    """
    if budget > 0 and projected > budget * 1.0001:  # tiny epsilon — ignore rounding
        # Already-over check supersedes "projected to overrun" — the user
        # doesn't need a forecast when the limit is behind them.
        if days_to_limit_value is None:
            return f"Projected to exceed plan: ${projected:,.2f} vs ${budget:,.2f}"
        if 0 <= days_to_limit_value <= days_left:
            return (
                f"Projected to hit plan limit in ~{days_to_limit_value} day"
                f"{'s' if days_to_limit_value != 1 else ''} "
                f"(forecast ${projected:,.2f})"
            )
        return f"Projected to exceed plan: ${projected:,.2f} vs ${budget:,.2f}"

    if crossed is not None:
        return f"Crossed {crossed}% of plan budget"

    return None
