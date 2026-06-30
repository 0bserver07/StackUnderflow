"""Unit tests for ``stackunderflow.services.budgets``.

Covers:
* get/set/clear round-trips through the descriptor-based settings.
* Independent legs (set monthly without clobbering daily, and vice versa).
* Positivity validation (a $0 / negative ceiling is rejected).
* Corrupt-config tolerance (a non-numeric persisted value reads as unset).
* ``compute_status`` banding (under / approaching / over) for each leg.
* The month-end projection + overrun flag wiring.
"""

from __future__ import annotations

from pathlib import Path
from unittest.mock import patch

import pytest

from stackunderflow.services import budgets as budgets_mod
from stackunderflow.services.budgets import (
    APPROACHING_PCT,
    Budget,
    clear_budget,
    compute_status,
    get_budget,
    set_budget,
)


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


# ── persistence ──────────────────────────────────────────────────────────────


class TestPersistence:
    def test_default_is_unset(self, isolated_settings):
        b = get_budget()
        assert b.monthly_usd is None
        assert b.daily_usd is None
        assert b.is_set is False

    def test_set_monthly_only(self, isolated_settings):
        b = set_budget(monthly_usd=150.0)
        assert b.monthly_usd == 150.0
        assert b.daily_usd is None
        assert b.is_set is True
        # Survives a fresh read.
        assert get_budget().monthly_usd == 150.0

    def test_set_daily_only(self, isolated_settings):
        b = set_budget(daily_usd=10.0)
        assert b.daily_usd == 10.0
        assert b.monthly_usd is None
        assert get_budget().daily_usd == 10.0

    def test_set_both(self, isolated_settings):
        b = set_budget(monthly_usd=200.0, daily_usd=15.0)
        assert b.monthly_usd == 200.0
        assert b.daily_usd == 15.0

    def test_legs_are_independent(self, isolated_settings):
        """Setting one leg to None clears it without touching the other."""
        set_budget(monthly_usd=200.0, daily_usd=15.0)
        # Clear only the daily leg.
        b = set_budget(monthly_usd=200.0, daily_usd=None)
        assert b.monthly_usd == 200.0
        assert b.daily_usd is None

    def test_clear_removes_both(self, isolated_settings):
        set_budget(monthly_usd=200.0, daily_usd=15.0)
        clear_budget()
        b = get_budget()
        assert b.monthly_usd is None
        assert b.daily_usd is None

    def test_zero_is_rejected(self, isolated_settings):
        with pytest.raises(ValueError, match="positive"):
            set_budget(monthly_usd=0.0)

    def test_negative_is_rejected(self, isolated_settings):
        with pytest.raises(ValueError, match="positive"):
            set_budget(daily_usd=-5.0)

    def test_corrupt_config_reads_as_unset(self, isolated_settings):
        """A hand-edited non-numeric value must not wedge ``get_budget``."""
        from stackunderflow.settings import Settings

        Settings().persist("budget_monthly_usd", "not-a-number")
        b = get_budget()
        assert b.monthly_usd is None  # tolerated, not raised


# ── status banding ───────────────────────────────────────────────────────────


class TestComputeStatus:
    def _budget(self, monthly=None, daily=None) -> Budget:
        return Budget(monthly_usd=monthly, daily_usd=daily)

    def test_unset_budget_yields_null_legs(self):
        out = compute_status(
            self._budget(),
            month_spend=50.0,
            today_spend=5.0,
            days_so_far=10,
            days_in_period=30,
        )
        assert out["monthly"] is None
        assert out["daily"] is None
        assert out["projected_month_end"] is None
        assert out["projection_overruns"] is None

    def test_monthly_under(self):
        out = compute_status(
            self._budget(monthly=200.0),
            month_spend=50.0,  # 25%
            today_spend=0.0,
            days_so_far=10,
            days_in_period=30,
        )
        m = out["monthly"]
        assert m["status"] == "under"
        assert m["budget"] == 200.0
        assert m["used"] == 50.0
        assert m["remaining"] == 150.0
        assert m["pct"] == pytest.approx(25.0)

    def test_monthly_approaching_at_threshold(self):
        out = compute_status(
            self._budget(monthly=100.0),
            month_spend=APPROACHING_PCT,  # exactly 80% of 100
            today_spend=0.0,
            days_so_far=20,
            days_in_period=30,
        )
        assert out["monthly"]["status"] == "approaching"

    def test_monthly_over(self):
        out = compute_status(
            self._budget(monthly=100.0),
            month_spend=120.0,  # 120%
            today_spend=0.0,
            days_so_far=25,
            days_in_period=30,
        )
        m = out["monthly"]
        assert m["status"] == "over"
        assert m["remaining"] < 0

    def test_daily_banding(self):
        out = compute_status(
            self._budget(daily=10.0),
            month_spend=0.0,
            today_spend=9.5,  # 95% → approaching
            days_so_far=5,
            days_in_period=30,
        )
        assert out["daily"]["status"] == "approaching"
        assert out["monthly"] is None  # monthly leg unset

    def test_projection_overruns_flag(self):
        """A burn rate that exceeds the ceiling by month-end flags overrun."""
        # $50 spent over 5 days = $10/day. 25 days left → +$250 → $300 total.
        out = compute_status(
            self._budget(monthly=200.0),
            month_spend=50.0,
            today_spend=0.0,
            days_so_far=5,
            days_in_period=30,
        )
        assert out["projected_month_end"] == pytest.approx(300.0)
        assert out["projection_overruns"] is True

    def test_projection_within_budget_does_not_overrun(self):
        # $10 spent over 5 days = $2/day. 25 days left → +$50 → $60 total < $200.
        out = compute_status(
            self._budget(monthly=200.0),
            month_spend=10.0,
            today_spend=0.0,
            days_so_far=5,
            days_in_period=30,
        )
        assert out["projected_month_end"] == pytest.approx(60.0)
        assert out["projection_overruns"] is False

    def test_last_day_projection_is_just_used(self):
        """On the final day there are no days left, so projection == used."""
        out = compute_status(
            self._budget(monthly=200.0),
            month_spend=90.0,
            today_spend=0.0,
            days_so_far=30,
            days_in_period=30,
        )
        assert out["projected_month_end"] == pytest.approx(90.0)
        assert out["projection_overruns"] is False


def test_module_exports():
    """Public surface is stable."""
    for name in ("Budget", "get_budget", "set_budget", "clear_budget", "compute_status"):
        assert hasattr(budgets_mod, name)
