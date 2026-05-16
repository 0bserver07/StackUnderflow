"""Unit tests for ``stackunderflow.services.burn`` — burn projector v2.

Pure-function tests over synthetic daily-cost arrays. The store-coupled
plumbing lives in ``tests/stackunderflow/routes/test_plan.py`` and the
CLI tests; the math itself doesn't need a database.
"""

from __future__ import annotations

import pytest

from stackunderflow.services import burn
from stackunderflow.services.burn import (
    DEFAULT_THRESHOLDS,
    DEFAULT_WEIGHTED_WINDOW,
    build_projection,
    crossed_thresholds,
    days_to_limit,
    linear_projection,
    pick_projection_method,
    weighted_projection,
)

# ── linear ──────────────────────────────────────────────────────────────────


class TestLinearProjection:
    def test_empty_returns_zero(self):
        assert linear_projection([]) == 0.0

    def test_single_day(self):
        assert linear_projection([4.0]) == pytest.approx(4.0)

    def test_average_across_days(self):
        # 3 days at $2 / $4 / $6 → mean $4.
        assert linear_projection([2.0, 4.0, 6.0]) == pytest.approx(4.0)

    def test_zeroes_dilute_the_mean(self):
        # A quiet weekend in the middle still drags the average down.
        assert linear_projection([10.0, 0.0, 0.0, 10.0]) == pytest.approx(5.0)

    def test_negative_values_clamped(self):
        # A refund or normalisation glitch must not subtract from the forecast.
        assert linear_projection([10.0, -10.0]) == pytest.approx(5.0)


# ── weighted ────────────────────────────────────────────────────────────────


class TestWeightedProjection:
    def test_empty_returns_zero(self):
        assert weighted_projection([]) == 0.0

    def test_single_day_equals_value(self):
        # Only one observation → its weight is 1.0 → answer is the value.
        assert weighted_projection([7.5]) == pytest.approx(7.5)

    def test_flat_input_collapses_to_value(self):
        # If every day is the same, any weighted average must equal that
        # number regardless of decay.
        assert weighted_projection([4.0] * 7) == pytest.approx(4.0)

    def test_recent_day_dominates(self):
        # Yesterday = $20, today = $0. Linear says $10. Weighted ought to be
        # *less* than the linear midpoint because today's zero has weight 1
        # and yesterday's $20 has weight 0.85.
        weighted = weighted_projection([20.0, 0.0])
        linear = linear_projection([20.0, 0.0])
        assert weighted < linear
        # Hand math: (0*1 + 20*0.85) / (1 + 0.85) ≈ 9.189.
        assert weighted == pytest.approx(17.0 / 1.85, rel=1e-6)

    def test_default_window_caps_history(self):
        # Provide 14 days but the default window is 7 → earlier 7 are ignored.
        history = [100.0] * 7 + [1.0] * 7
        # If we honoured all 14, the mean would be 50.5; cropping to last 7
        # makes it 1.0 (all the same), so the result is 1.0.
        assert weighted_projection(history) == pytest.approx(1.0)

    def test_decay_one_is_simple_mean_over_window(self):
        # decay=1.0 means every day inside the window weights 1.0 →
        # the weighted average degrades to a plain mean.
        history = [2.0, 4.0, 6.0]
        assert weighted_projection(history, decay=1.0) == pytest.approx(4.0)

    def test_window_zero_falls_back_to_default(self):
        # Defensive: a caller passing window=0 should still get a sane answer.
        assert weighted_projection([3.0], window=0) == pytest.approx(3.0)

    def test_default_window_constant(self):
        # Sanity: the documented constant matches what the function uses.
        assert DEFAULT_WEIGHTED_WINDOW == 7


# ── method picker ───────────────────────────────────────────────────────────


class TestPickProjectionMethod:
    def test_empty_history_is_linear(self):
        assert pick_projection_method([]) == "linear"

    def test_one_day_is_linear(self):
        assert pick_projection_method([5.0]) == "linear"

    def test_two_days_is_linear(self):
        # 2 non-zero samples is still below the threshold.
        assert pick_projection_method([3.0, 5.0]) == "linear"

    def test_three_non_zero_days_is_weighted(self):
        assert pick_projection_method([1.0, 2.0, 3.0]) == "weighted-7d"

    def test_zeroes_dont_count_toward_threshold(self):
        # A long string of zero days doesn't unlock weighted-7d — we want
        # actual activity samples for the decay to do anything.
        assert pick_projection_method([0.0] * 10) == "linear"
        assert pick_projection_method([0.0, 0.0, 0.0, 4.0]) == "linear"
        assert pick_projection_method([0.0, 4.0, 5.0, 6.0]) == "weighted-7d"


# ── days_to_limit ───────────────────────────────────────────────────────────


class TestDaysToLimit:
    def test_simple_case(self):
        # $20 budget, $5 spent, $1 / day → ~15 more days.
        assert days_to_limit(spent=5.0, daily_avg=1.0, limit=20.0) == 15

    def test_zero_burn_returns_none(self):
        # If no money is going out, the limit is never reached.
        assert days_to_limit(spent=5.0, daily_avg=0.0, limit=20.0) is None

    def test_already_over_returns_none(self):
        # Overrun is already history — surfacing a "days left" of 0 would
        # imply the user has a future runway, which is wrong.
        assert days_to_limit(spent=25.0, daily_avg=1.0, limit=20.0) is None

    def test_zero_budget_returns_none(self):
        assert days_to_limit(spent=0.0, daily_avg=1.0, limit=0.0) is None

    def test_floor_not_round(self):
        # $10 remaining at $3 / day = 3.33 days → floor to 3 so the agent
        # doesn't promise time the budget can't actually cover.
        assert days_to_limit(spent=0.0, daily_avg=3.0, limit=10.0) == 3

    def test_negative_daily_returns_none(self):
        assert days_to_limit(spent=0.0, daily_avg=-1.0, limit=10.0) is None


# ── crossed_thresholds ──────────────────────────────────────────────────────


class TestCrossedThresholds:
    def test_below_all(self):
        assert crossed_thresholds(40.0) is None

    def test_at_first(self):
        # Exactly hitting a threshold counts.
        assert crossed_thresholds(50.0) == 50

    def test_between_first_and_second(self):
        assert crossed_thresholds(60.0) == 50

    def test_at_second(self):
        assert crossed_thresholds(75.0) == 75

    def test_above_third(self):
        # Above 90 still returns 90 — the route / UI surfaces "over budget"
        # separately when pct > 100.
        assert crossed_thresholds(95.0) == 90
        assert crossed_thresholds(200.0) == 90

    def test_custom_thresholds(self):
        # User-defined ladder via ``stackunderflow plan thresholds set``.
        assert crossed_thresholds(70.0, thresholds=[60, 80, 95]) == 60
        assert crossed_thresholds(85.0, thresholds=[60, 80, 95]) == 80

    def test_default_thresholds_constant(self):
        # The shipped default matches what the spec calls out.
        assert DEFAULT_THRESHOLDS == (50, 75, 90)


# ── build_projection ────────────────────────────────────────────────────────


class TestBuildProjection:
    def test_empty_store_returns_zero_burn(self):
        # No spend yet on day 1 of a $20 plan.
        result = build_projection(
            daily_costs=[],
            used=0.0,
            budget=20.0,
            days_so_far=1,
            days_in_period=30,
        )
        assert result["projected_month_end_usd"] == pytest.approx(0.0)
        assert result["projection_method"] == "linear"
        assert result["daily_burn_usd"] == pytest.approx(0.0)
        assert result["days_to_limit"] is None
        assert result["crossed_threshold"] is None
        assert result["alert"] is None

    def test_three_day_history_unlocks_weighted(self):
        result = build_projection(
            daily_costs=[1.0, 2.0, 3.0],
            used=6.0,
            budget=100.0,
            days_so_far=3,
            days_in_period=30,
        )
        # 3 non-zero samples → weighted picked automatically.
        assert result["projection_method"] == "weighted-7d"
        # Weighted average of [1, 2, 3] with decay 0.85:
        # (3*1 + 2*0.85 + 1*0.85^2) / (1 + 0.85 + 0.7225)
        expected = (3.0 + 1.7 + 0.7225) / (1 + 0.85 + 0.7225)
        assert result["daily_burn_usd"] == pytest.approx(expected, rel=1e-6)
        # 27 days left × daily_burn + 6 used.
        assert result["projected_month_end_usd"] == pytest.approx(
            6.0 + expected * 27, rel=1e-6
        )

    def test_threshold_alert_at_50_pct(self):
        # $10 of $20 spent on the *last* day of the period → exactly 50% →
        # first threshold crossed, and no projected overrun (no days left).
        result = build_projection(
            daily_costs=[10.0],
            used=10.0,
            budget=20.0,
            days_so_far=30,
            days_in_period=30,
        )
        assert result["crossed_threshold"] == 50
        assert result["alert"] == "Crossed 50% of plan budget"

    def test_threshold_alert_at_75_pct(self):
        result = build_projection(
            daily_costs=[15.0],
            used=15.0,
            budget=20.0,
            days_so_far=30,
            days_in_period=30,
        )
        # 75% exactly → highest threshold met.
        assert result["crossed_threshold"] == 75
        assert result["alert"] == "Crossed 75% of plan budget"

    def test_threshold_alert_at_90_pct(self):
        result = build_projection(
            daily_costs=[18.0],
            used=18.0,
            budget=20.0,
            days_so_far=30,
            days_in_period=30,
        )
        # 90% exactly.
        assert result["crossed_threshold"] == 90
        assert result["alert"] == "Crossed 90% of plan budget"

    def test_overrun_forecast_supersedes_threshold(self):
        # Crossing a threshold AND projected to overrun → the overrun
        # message wins (it's the more actionable signal).
        result = build_projection(
            daily_costs=[10.0],
            used=10.0,
            budget=20.0,
            days_so_far=1,
            days_in_period=30,
        )
        # 50% pct is crossed, but projection (10 + 10*29 = $300) > $20 →
        # alert is the overrun forecast, not the threshold notice.
        assert result["crossed_threshold"] == 50
        assert "Projected to hit plan limit" in (result["alert"] or "")

    def test_overrun_already_happened(self):
        # Already 120% of plan → alert reflects the overrun, not a forecast.
        result = build_projection(
            daily_costs=[24.0],
            used=24.0,
            budget=20.0,
            days_so_far=1,
            days_in_period=30,
        )
        # days_to_limit is None because we're already over → alert says so.
        assert result["days_to_limit"] is None
        assert "Projected to exceed plan" in (result["alert"] or "")
        assert result["crossed_threshold"] == 90

    def test_projected_to_overrun_before_period_ends(self):
        # Day 5, $50 spent on a $100 plan over 30 days, daily burn $10/day.
        # Linear projection picks the cumulative average; weighted with
        # 5 equal days = 10/day. Expected month-end: 50 + 10*25 = $300.
        # Days-to-limit: (100-50)/10 = 5 days (fits in remaining 25-day window).
        result = build_projection(
            daily_costs=[10.0] * 5,
            used=50.0,
            budget=100.0,
            days_so_far=5,
            days_in_period=30,
        )
        assert result["days_to_limit"] == 5
        assert "5 days" in (result["alert"] or "")
        assert "Projected to hit plan limit" in (result["alert"] or "")

    def test_custom_thresholds_propagate(self):
        result = build_projection(
            daily_costs=[12.0],
            used=12.0,
            budget=20.0,
            days_so_far=1,
            days_in_period=30,
            thresholds=[60, 80],
        )
        # 60% of $20 → exactly the first user-defined threshold.
        assert result["thresholds"] == [60, 80]
        assert result["crossed_threshold"] == 60

    def test_thresholds_are_deduped_and_sorted(self):
        result = build_projection(
            daily_costs=[],
            used=0.0,
            budget=20.0,
            days_so_far=1,
            days_in_period=30,
            thresholds=[90, 50, 50, 75],
        )
        assert result["thresholds"] == [50, 75, 90]

    def test_method_override_forces_linear(self):
        # An explicit caller can pin the method (e.g. for tests / debugging).
        result = build_projection(
            daily_costs=[1.0, 2.0, 3.0],
            used=6.0,
            budget=100.0,
            days_so_far=3,
            days_in_period=30,
            method="linear",
        )
        assert result["projection_method"] == "linear"
        # Linear over [1, 2, 3] = 2.0 daily burn.
        assert result["daily_burn_usd"] == pytest.approx(2.0)

    def test_stale_store_falls_back_to_linear(self):
        """When the last 7 days are quiet but the period has real activity.

        The weighted-7d window over zeroes collapses to $0; that's
        technically correct ("the last week was silent") but produces a
        misleading $0 month-end forecast when the actual cause is a
        stale store, not a quiet week. The picker degrades gracefully
        to the linear running mean and surfaces ``method == "linear"``
        so the user sees what happened.
        """
        # 5 days of $1000 then 8 days of $0 (e.g. ingest stopped 8 days ago).
        daily = [1000.0] * 5 + [0.0] * 8
        result = build_projection(
            daily_costs=daily,
            used=5000.0,
            budget=200.0,
            days_so_far=13,
            days_in_period=30,
        )
        # Auto-pick would have started with weighted-7d, but the fallback
        # kicks in because the recent 7-day window is all zero.
        assert result["projection_method"] == "linear"
        # Linear mean of [1000]*5 + [0]*8 = 5000/13 ≈ 384.6/day.
        assert result["daily_burn_usd"] == pytest.approx(5000.0 / 13)

    def test_no_fallback_when_last_7_days_have_activity(self):
        """The fallback only kicks in when weighted-7d collapses to zero."""
        # Continuous activity — weighted-7d returns a positive number → no fallback.
        daily = [1.0] * 10
        result = build_projection(
            daily_costs=daily,
            used=10.0,
            budget=200.0,
            days_so_far=10,
            days_in_period=30,
        )
        assert result["projection_method"] == "weighted-7d"

    def test_zero_budget_no_division_by_zero(self):
        # Edge case: a misconfigured plan with $0 budget shouldn't crash.
        result = build_projection(
            daily_costs=[1.0],
            used=1.0,
            budget=0.0,
            days_so_far=1,
            days_in_period=30,
        )
        # pct is 0 by definition when budget is 0; no threshold crossed.
        assert result["crossed_threshold"] is None
        assert result["days_to_limit"] is None


# ── module surface ──────────────────────────────────────────────────────────


def test_public_api_exports():
    """Lock the public surface so refactors don't accidentally hide names."""
    expected = {
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
    }
    assert expected.issubset(set(burn.__all__))
