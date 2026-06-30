"""Settings-descriptor coverage for the budget keys (audit #7 part 2).

The budget ceilings persist through the same descriptor-based ``Settings``
chain (env → file → default) as every other setting. These tests pin the
contract the Budgets route relies on: file-only (no env leg), default ``None``,
and a clean persist → read → remove round-trip via ``~/.stackunderflow``.
"""

from __future__ import annotations

from pathlib import Path
from unittest.mock import patch

import pytest

from stackunderflow.settings import Settings


def _patch_settings_dir(tmpdir: Path):
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


class TestBudgetSettings:
    def test_budget_keys_are_declared(self):
        keys = Settings._keys()
        assert "budget_monthly_usd" in keys
        assert "budget_daily_usd" in keys

    def test_defaults_are_none(self):
        assert Settings.DEFAULTS["budget_monthly_usd"] is None
        assert Settings.DEFAULTS["budget_daily_usd"] is None

    def test_budget_keys_are_file_only(self):
        """No env-var leg — these are managed via the route, not the shell."""
        assert Settings.ENV_MAPPINGS["budget_monthly_usd"] is None
        assert Settings.ENV_MAPPINGS["budget_daily_usd"] is None

    def test_persist_read_remove_round_trip(self, isolated_settings):
        s = Settings()
        assert s.get("budget_monthly_usd") is None  # default

        s.persist("budget_monthly_usd", 150.0)
        s.persist("budget_daily_usd", 10.0)
        assert s.get("budget_monthly_usd") == 150.0
        assert s.get("budget_daily_usd") == 10.0

        s.remove("budget_monthly_usd")
        assert s.get("budget_monthly_usd") is None
        # The other leg is untouched.
        assert s.get("budget_daily_usd") == 10.0
