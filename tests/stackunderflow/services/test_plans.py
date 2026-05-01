"""Unit tests for ``stackunderflow.services.plans``.

Covers:
* Preset resolution (``set_plan`` for each known name).
* Custom plan validation (must supply ``monthly_usd``; positive amount).
* ``compute_usage()`` math under, at, and over budget.
* ``project_month_end()`` linear projection edge cases.
* Reset-day window resolution (mid-month vs. before reset day, and
  end-of-month rollover when ``reset_day=31`` lands on a short month).
"""

from __future__ import annotations

from datetime import UTC, datetime
from pathlib import Path
from unittest.mock import patch

import pytest

from stackunderflow.services import plans as plans_mod
from stackunderflow.services.plans import (
    PRESETS,
    Plan,
    compute_usage,
    get_active_plan,
    project_month_end,
    reset_plan,
    set_plan,
)

# ── settings isolation helper (every test gets a clean config dir) ───────────

def _patch_settings_dir(tmpdir: Path):
    """Redirect settings I/O to ``tmpdir/.stackunderflow``."""
    app_dir = tmpdir / ".stackunderflow"
    app_dir.mkdir(exist_ok=True)
    cfg_file = app_dir / "config.json"
    return (
        patch("stackunderflow.settings._APP_DIR", app_dir),
        patch("stackunderflow.settings._CFG_FILE", cfg_file),
    )


@pytest.fixture()
def isolated_settings(tmp_path):
    p1, p2 = _patch_settings_dir(tmp_path)
    with p1, p2:
        yield


# ── presets ──────────────────────────────────────────────────────────────────

class TestPresetResolution:
    def test_preset_table_contains_expected_names(self):
        assert set(PRESETS) == {
            "claude-pro", "claude-max", "cursor-pro", "cursor-max", "custom",
        }

    def test_set_claude_pro_resolves_to_20_usd(self, isolated_settings):
        plan = set_plan("claude-pro")
        assert plan.name == "claude-pro"
        assert plan.monthly_usd == 20.0
        assert plan.reset_day == 1

    def test_set_claude_max_resolves_to_200_usd(self, isolated_settings):
        plan = set_plan("claude-max")
        assert plan.monthly_usd == 200.0

    def test_set_cursor_pro_resolves_to_20_usd(self, isolated_settings):
        plan = set_plan("cursor-pro")
        assert plan.monthly_usd == 20.0

    def test_set_cursor_max_resolves_to_40_usd(self, isolated_settings):
        plan = set_plan("cursor-max")
        assert plan.monthly_usd == 40.0

    def test_unknown_preset_raises(self, isolated_settings):
        with pytest.raises(ValueError, match="Unknown plan name"):
            set_plan("anthropic-mega")

    def test_preset_amount_can_be_overridden(self, isolated_settings):
        """User on a grandfathered price can override the preset amount."""
        plan = set_plan("claude-pro", monthly_usd=15.0)
        assert plan.monthly_usd == 15.0
        assert plan.name == "claude-pro"


# ── custom plan validation ──────────────────────────────────────────────────

class TestCustomPlan:
    def test_custom_requires_monthly_usd(self, isolated_settings):
        with pytest.raises(ValueError, match="custom plan requires"):
            set_plan("custom")

    def test_custom_with_amount(self, isolated_settings):
        plan = set_plan("custom", monthly_usd=75.0)
        assert plan.name == "custom"
        assert plan.monthly_usd == 75.0

    def test_negative_amount_rejected(self, isolated_settings):
        with pytest.raises(ValueError, match="positive"):
            set_plan("custom", monthly_usd=-5.0)

    def test_zero_amount_rejected(self, isolated_settings):
        with pytest.raises(ValueError, match="positive"):
            set_plan("custom", monthly_usd=0.0)

    def test_reset_day_out_of_range(self, isolated_settings):
        with pytest.raises(ValueError, match="reset_day"):
            set_plan("claude-pro", reset_day=0)
        with pytest.raises(ValueError, match="reset_day"):
            set_plan("claude-pro", reset_day=32)

    def test_reset_day_28_accepted(self, isolated_settings):
        plan = set_plan("claude-pro", reset_day=28)
        assert plan.reset_day == 28


# ── settings round-trip ──────────────────────────────────────────────────────

class TestSettingsRoundTrip:
    def test_get_active_plan_when_unset(self, isolated_settings):
        assert get_active_plan() is None

    def test_set_then_get(self, isolated_settings):
        set_plan("claude-pro", reset_day=15)
        plan = get_active_plan()
        assert plan is not None
        assert plan.name == "claude-pro"
        assert plan.monthly_usd == 20.0
        assert plan.reset_day == 15

    def test_reset_clears_plan(self, isolated_settings):
        set_plan("claude-max")
        assert get_active_plan() is not None
        reset_plan()
        assert get_active_plan() is None


# ── projection ───────────────────────────────────────────────────────────────

class TestProjectMonthEnd:
    def test_zero_burn_returns_zero(self):
        assert project_month_end(0.0, 10) == 0.0

    def test_zero_days_left_returns_zero(self):
        assert project_month_end(5.0, 0) == 0.0

    def test_negative_days_returns_zero(self):
        # Defensive: never project negative spend
        assert project_month_end(5.0, -3) == 0.0

    def test_simple_linear(self):
        assert project_month_end(10.0, 5) == 50.0

    def test_fractional_burn(self):
        assert project_month_end(2.5, 4) == 10.0


# ── compute_usage math ───────────────────────────────────────────────────────

class TestComputeUsageStatus:
    """Status banding: ok < 80% ≤ warn ≤ 100% < over."""

    def test_under_budget_is_ok(self):
        plan = Plan(name="claude-pro", monthly_usd=20.0, reset_day=1)
        usage = compute_usage(plan, 5.0, now=datetime(2026, 4, 15, tzinfo=UTC))
        assert usage["used"] == 5.0
        assert usage["budget"] == 20.0
        assert usage["remaining"] == 15.0
        assert usage["pct"] == 25.0
        assert usage["status"] == "ok"

    def test_at_80_percent_is_warn(self):
        plan = Plan(name="claude-pro", monthly_usd=20.0, reset_day=1)
        usage = compute_usage(plan, 16.0, now=datetime(2026, 4, 15, tzinfo=UTC))
        assert usage["pct"] == 80.0
        assert usage["status"] == "warn"

    def test_at_100_percent_is_warn(self):
        plan = Plan(name="claude-pro", monthly_usd=20.0, reset_day=1)
        usage = compute_usage(plan, 20.0, now=datetime(2026, 4, 15, tzinfo=UTC))
        assert usage["pct"] == 100.0
        assert usage["status"] == "warn"

    def test_over_100_is_over(self):
        plan = Plan(name="claude-pro", monthly_usd=20.0, reset_day=1)
        usage = compute_usage(plan, 25.0, now=datetime(2026, 4, 15, tzinfo=UTC))
        assert usage["pct"] == 125.0
        assert usage["remaining"] == -5.0
        assert usage["status"] == "over"

    def test_just_under_80_is_ok(self):
        plan = Plan(name="claude-pro", monthly_usd=100.0, reset_day=1)
        usage = compute_usage(plan, 79.0, now=datetime(2026, 4, 15, tzinfo=UTC))
        assert usage["status"] == "ok"


class TestComputeUsageProjection:
    def test_projection_extrapolates_linearly(self):
        plan = Plan(name="claude-pro", monthly_usd=100.0, reset_day=1)
        # Day 10 of a 30-day April; spent $10 → daily burn $1 → projected $30 total.
        usage = compute_usage(plan, 10.0, now=datetime(2026, 4, 10, tzinfo=UTC))
        assert usage["days_so_far"] == 10
        assert usage["days_in_period"] == 30
        assert usage["projected_month_end"] == pytest.approx(30.0, rel=1e-6)

    def test_projection_at_period_start(self):
        plan = Plan(name="claude-pro", monthly_usd=100.0, reset_day=1)
        # Day 1: 1 day so far, 30 in period → 29 days left.
        usage = compute_usage(plan, 5.0, now=datetime(2026, 4, 1, tzinfo=UTC))
        # daily burn 5/1 = 5; remaining days = 29; projected 5 + 5*29 = 150
        assert usage["projected_month_end"] == pytest.approx(150.0, rel=1e-6)

    def test_projection_at_period_end(self):
        plan = Plan(name="claude-pro", monthly_usd=100.0, reset_day=1)
        # Last day: zero days left, projection equals current spend.
        usage = compute_usage(plan, 90.0, now=datetime(2026, 4, 30, tzinfo=UTC))
        assert usage["projected_month_end"] == pytest.approx(90.0, rel=1e-6)


class TestPeriodWindow:
    """Verify the billing window resolves correctly for varied reset days."""

    def test_default_reset_day_1_april(self):
        plan = Plan(name="claude-pro", monthly_usd=20.0, reset_day=1)
        usage = compute_usage(plan, 0.0, now=datetime(2026, 4, 15, tzinfo=UTC))
        assert usage["period_start"] == "2026-04-01"
        assert usage["period_end"] == "2026-04-30"
        assert usage["days_in_period"] == 30
        assert usage["days_so_far"] == 15

    def test_reset_day_15_when_today_after(self):
        plan = Plan(name="claude-pro", monthly_usd=20.0, reset_day=15)
        # April 20: window is April 15 → May 14 (30-day window).
        usage = compute_usage(plan, 0.0, now=datetime(2026, 4, 20, tzinfo=UTC))
        assert usage["period_start"] == "2026-04-15"
        assert usage["period_end"] == "2026-05-14"
        assert usage["days_so_far"] == 6

    def test_reset_day_15_when_today_before(self):
        plan = Plan(name="claude-pro", monthly_usd=20.0, reset_day=15)
        # April 10: window is March 15 → April 14 (31-day window).
        usage = compute_usage(plan, 0.0, now=datetime(2026, 4, 10, tzinfo=UTC))
        assert usage["period_start"] == "2026-03-15"
        assert usage["period_end"] == "2026-04-14"

    def test_reset_day_31_clamps_to_short_month(self):
        """Reset day 31 in February clamps to Feb 28 (or 29 in leap years)."""
        plan = Plan(name="claude-pro", monthly_usd=20.0, reset_day=31)
        # Feb 15, 2026 (non-leap): window started Jan 31 (31 exists in Jan),
        # ends Feb 27 (day before next reset, which clamps to Feb 28).
        usage = compute_usage(plan, 0.0, now=datetime(2026, 2, 15, tzinfo=UTC))
        assert usage["period_start"] == "2026-01-31"
        assert usage["period_end"] == "2026-02-27"

    def test_reset_day_one_year_boundary(self):
        plan = Plan(name="claude-pro", monthly_usd=20.0, reset_day=1)
        # Jan 5: window started Jan 1, ends Jan 31.
        usage = compute_usage(plan, 0.0, now=datetime(2026, 1, 5, tzinfo=UTC))
        assert usage["period_start"] == "2026-01-01"
        assert usage["period_end"] == "2026-01-31"

    def test_reset_day_crosses_year_boundary(self):
        plan = Plan(name="claude-pro", monthly_usd=20.0, reset_day=15)
        # Jan 5: window is Dec 15 of prior year through Jan 14.
        usage = compute_usage(plan, 0.0, now=datetime(2026, 1, 5, tzinfo=UTC))
        assert usage["period_start"] == "2025-12-15"
        assert usage["period_end"] == "2026-01-14"


# ── module sanity ───────────────────────────────────────────────────────────

def test_module_exports_public_api():
    """Guard the public surface so refactors don't accidentally hide names."""
    public = {
        "PRESETS", "Plan", "compute_usage", "get_active_plan",
        "project_month_end", "reset_plan", "set_plan",
    }
    assert public.issubset(set(plans_mod.__all__))
