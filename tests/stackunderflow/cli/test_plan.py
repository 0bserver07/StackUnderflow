"""CLI tests for ``stackunderflow plan {show,set,reset}``.

Exercises the round-trip through the Click runner:
* ``plan set`` accepts every preset name and the ``custom`` shape with
  ``--monthly-usd``.
* ``plan show`` renders a human-readable table and a JSON payload, and
  prints the no-plan message when nothing is set.
* ``plan reset`` clears every plan key.
* Validation errors (unknown preset, custom without amount, bad
  reset-day) surface as Click ``BadParameter`` exits.

The cost rollup (``stackunderflow.reports.aggregate.build_report``) is
patched to return a fixed total — these tests are about the CLI
plumbing, not the SQL aggregation.
"""

from __future__ import annotations

import json
from pathlib import Path
from unittest.mock import patch

from click.testing import CliRunner

from stackunderflow.cli import cli


def _patch_settings_dir(tmpdir: Path):
    app_dir = tmpdir / ".stackunderflow"
    app_dir.mkdir(exist_ok=True)
    cfg_file = app_dir / "config.json"
    return (
        patch("stackunderflow.settings._APP_DIR", app_dir),
        patch("stackunderflow.settings._CFG_FILE", cfg_file),
    )


def _patch_spend(total_usd: float):
    """Patch the CLI's spend-rollup helper to return a fixed number."""
    return patch("stackunderflow.cli._resolve_period_spend", return_value=total_usd)


def _patch_daily(daily: list[float]):
    """Patch the CLI's per-day cost helper for burn-projector v2."""
    return patch("stackunderflow.cli._resolve_period_daily_costs", return_value=daily)


# ── plan set ────────────────────────────────────────────────────────────────

class TestPlanSet:
    def test_set_claude_pro_writes_settings(self):
        runner = CliRunner()
        with runner.isolated_filesystem() as td:
            p1, p2 = _patch_settings_dir(Path(td))
            with p1, p2:
                result = runner.invoke(cli, ["plan", "set", "claude-pro"])
                assert result.exit_code == 0, result.output
                assert "claude-pro" in result.output
                assert "$20.00" in result.output

                cfg_file = Path(td) / ".stackunderflow" / "config.json"
                data = json.loads(cfg_file.read_text())
                assert data["plan_name"] == "claude-pro"
                assert data["plan_monthly_usd"] == 20.0
                assert data["plan_reset_day"] == 1

    def test_set_claude_max(self):
        runner = CliRunner()
        with runner.isolated_filesystem() as td:
            p1, p2 = _patch_settings_dir(Path(td))
            with p1, p2:
                result = runner.invoke(cli, ["plan", "set", "claude-max"])
                assert result.exit_code == 0, result.output
                assert "$200.00" in result.output

    def test_set_cursor_pro(self):
        runner = CliRunner()
        with runner.isolated_filesystem() as td:
            p1, p2 = _patch_settings_dir(Path(td))
            with p1, p2:
                result = runner.invoke(cli, ["plan", "set", "cursor-pro"])
                assert result.exit_code == 0, result.output
                assert "cursor-pro" in result.output
                assert "$20.00" in result.output

    def test_set_cursor_max(self):
        runner = CliRunner()
        with runner.isolated_filesystem() as td:
            p1, p2 = _patch_settings_dir(Path(td))
            with p1, p2:
                result = runner.invoke(cli, ["plan", "set", "cursor-max"])
                assert result.exit_code == 0, result.output
                assert "$40.00" in result.output

    def test_set_custom_requires_monthly_usd(self):
        runner = CliRunner()
        with runner.isolated_filesystem() as td:
            p1, p2 = _patch_settings_dir(Path(td))
            with p1, p2:
                result = runner.invoke(cli, ["plan", "set", "custom"])
                assert result.exit_code != 0
                assert "custom plan requires" in result.output

    def test_set_custom_with_amount(self):
        runner = CliRunner()
        with runner.isolated_filesystem() as td:
            p1, p2 = _patch_settings_dir(Path(td))
            with p1, p2:
                result = runner.invoke(
                    cli, ["plan", "set", "custom", "--monthly-usd", "75"]
                )
                assert result.exit_code == 0, result.output
                assert "$75.00" in result.output

    def test_set_unknown_preset(self):
        runner = CliRunner()
        with runner.isolated_filesystem() as td:
            p1, p2 = _patch_settings_dir(Path(td))
            with p1, p2:
                result = runner.invoke(cli, ["plan", "set", "anthropic-mega"])
                assert result.exit_code != 0
                assert "Unknown plan name" in result.output

    def test_set_with_reset_day(self):
        runner = CliRunner()
        with runner.isolated_filesystem() as td:
            p1, p2 = _patch_settings_dir(Path(td))
            with p1, p2:
                result = runner.invoke(
                    cli, ["plan", "set", "claude-pro", "--reset-day", "15"]
                )
                assert result.exit_code == 0, result.output
                assert "day 15" in result.output

    def test_set_invalid_reset_day(self):
        runner = CliRunner()
        with runner.isolated_filesystem() as td:
            p1, p2 = _patch_settings_dir(Path(td))
            with p1, p2:
                result = runner.invoke(
                    cli, ["plan", "set", "claude-pro", "--reset-day", "0"]
                )
                assert result.exit_code != 0


# ── plan show ───────────────────────────────────────────────────────────────

class TestPlanShow:
    def test_show_with_no_plan_set(self):
        runner = CliRunner()
        with runner.isolated_filesystem() as td:
            p1, p2 = _patch_settings_dir(Path(td))
            with p1, p2:
                result = runner.invoke(cli, ["plan", "show"])
                assert result.exit_code == 0
                assert "No plan set" in result.output

    def test_show_with_no_plan_set_json(self):
        runner = CliRunner()
        with runner.isolated_filesystem() as td:
            p1, p2 = _patch_settings_dir(Path(td))
            with p1, p2:
                result = runner.invoke(cli, ["plan", "show", "--format", "json"])
                assert result.exit_code == 0
                data = json.loads(result.output)
                assert data == {"plan": None, "usage": None}

    def test_show_text_under_budget(self):
        runner = CliRunner()
        with runner.isolated_filesystem() as td:
            p1, p2 = _patch_settings_dir(Path(td))
            with p1, p2, _patch_spend(5.0), _patch_daily([5.0]):
                runner.invoke(cli, ["plan", "set", "claude-pro"])
                result = runner.invoke(cli, ["plan", "show"])
                assert result.exit_code == 0, result.output
                assert "Plan:" in result.output
                assert "claude-pro" in result.output
                assert "Used:" in result.output
                assert "$5.00" in result.output
                # Status: ok (under 80% on a $20 budget)
                assert "ok" in result.output

    def test_show_text_warn(self):
        runner = CliRunner()
        with runner.isolated_filesystem() as td:
            p1, p2 = _patch_settings_dir(Path(td))
            with p1, p2, _patch_spend(18.0), _patch_daily([18.0]):
                runner.invoke(cli, ["plan", "set", "claude-pro"])
                result = runner.invoke(cli, ["plan", "show"])
                assert result.exit_code == 0, result.output
                assert "warn" in result.output

    def test_show_text_over(self):
        runner = CliRunner()
        with runner.isolated_filesystem() as td:
            p1, p2 = _patch_settings_dir(Path(td))
            with p1, p2, _patch_spend(50.0), _patch_daily([50.0]):
                runner.invoke(cli, ["plan", "set", "claude-pro"])
                result = runner.invoke(cli, ["plan", "show"])
                assert result.exit_code == 0, result.output
                assert "over" in result.output

    def test_show_json_payload_shape(self):
        runner = CliRunner()
        with runner.isolated_filesystem() as td:
            p1, p2 = _patch_settings_dir(Path(td))
            with p1, p2, _patch_spend(8.0), _patch_daily([8.0]):
                runner.invoke(cli, ["plan", "set", "claude-pro"])
                result = runner.invoke(cli, ["plan", "show", "--format", "json"])
                assert result.exit_code == 0, result.output
                data = json.loads(result.output)
                assert data["plan"] == {
                    "name": "claude-pro",
                    "monthly_usd": 20.0,
                    "reset_day": 1,
                }
                u = data["usage"]
                assert u["used"] == 8.0
                assert u["budget"] == 20.0
                assert u["remaining"] == 12.0
                assert u["pct"] == 40.0
                assert u["status"] == "ok"
                assert "projected_month_end" in u
                assert "period_start" in u
                assert "period_end" in u
                # Burn-projector v2 — projection block must round-trip.
                p = data["projection"]
                assert "projected_month_end_usd" in p
                assert "projection_method" in p
                assert p["projection_method"] in ("linear", "weighted-7d")
                assert "daily_burn_usd" in p
                assert "thresholds" in p
                assert p["thresholds"] == [50, 75, 90]
                assert "crossed_threshold" in p
                assert "alert" in p

    def test_show_text_includes_projection_method(self):
        runner = CliRunner()
        with runner.isolated_filesystem() as td:
            p1, p2 = _patch_settings_dir(Path(td))
            with p1, p2, _patch_spend(6.0), _patch_daily([1.0, 2.0, 3.0]):
                runner.invoke(cli, ["plan", "set", "claude-max"])
                result = runner.invoke(cli, ["plan", "show"])
                assert result.exit_code == 0, result.output
                # 3 non-zero days unlocks the weighted-7d method
                assert "weighted-7d" in result.output
                assert "burn" in result.output

    def test_show_text_alert_when_threshold_crossed(self):
        runner = CliRunner()
        with runner.isolated_filesystem() as td:
            p1, p2 = _patch_settings_dir(Path(td))
            # 11/20 = 55% > 50% threshold → alert line surfaces. Project
            # the spend over a single non-zero day so the linear forecast
            # is bounded and the threshold-crossed alert wins (vs. the
            # "overrun forecast" banner).
            with (
                p1,
                p2,
                _patch_spend(11.0),
                _patch_daily([0.0] * 29 + [11.0]),
            ):
                runner.invoke(cli, ["plan", "set", "claude-pro"])
                result = runner.invoke(cli, ["plan", "show"])
                assert result.exit_code == 0, result.output
                assert "Alert:" in result.output


# ── plan thresholds ─────────────────────────────────────────────────────────


class TestPlanThresholds:
    def test_show_default_thresholds(self):
        runner = CliRunner()
        with runner.isolated_filesystem() as td:
            p1, p2 = _patch_settings_dir(Path(td))
            with p1, p2:
                result = runner.invoke(cli, ["plan", "thresholds", "show"])
                assert result.exit_code == 0, result.output
                assert "50%" in result.output
                assert "75%" in result.output
                assert "90%" in result.output

    def test_show_default_thresholds_json(self):
        runner = CliRunner()
        with runner.isolated_filesystem() as td:
            p1, p2 = _patch_settings_dir(Path(td))
            with p1, p2:
                result = runner.invoke(
                    cli, ["plan", "thresholds", "show", "--format", "json"]
                )
                assert result.exit_code == 0, result.output
                data = json.loads(result.output)
                assert data == {"thresholds": [50, 75, 90]}

    def test_set_three_thresholds(self):
        runner = CliRunner()
        with runner.isolated_filesystem() as td:
            p1, p2 = _patch_settings_dir(Path(td))
            with p1, p2:
                result = runner.invoke(
                    cli, ["plan", "thresholds", "set", "60", "85", "95"]
                )
                assert result.exit_code == 0, result.output
                assert "60%" in result.output
                assert "85%" in result.output
                assert "95%" in result.output

                # Round-trip via show.
                result = runner.invoke(
                    cli, ["plan", "thresholds", "show", "--format", "json"]
                )
                assert json.loads(result.output) == {"thresholds": [60, 85, 95]}

    def test_set_thresholds_dedupes_and_sorts(self):
        runner = CliRunner()
        with runner.isolated_filesystem() as td:
            p1, p2 = _patch_settings_dir(Path(td))
            with p1, p2:
                result = runner.invoke(
                    cli, ["plan", "thresholds", "set", "90", "50", "50", "75"]
                )
                assert result.exit_code == 0, result.output

                result = runner.invoke(
                    cli, ["plan", "thresholds", "show", "--format", "json"]
                )
                # Deduped ↑ sorted; settings persistence uses the canonical form.
                assert json.loads(result.output) == {"thresholds": [50, 75, 90]}

    def test_set_threshold_out_of_range_rejected(self):
        runner = CliRunner()
        with runner.isolated_filesystem() as td:
            p1, p2 = _patch_settings_dir(Path(td))
            with p1, p2:
                result = runner.invoke(cli, ["plan", "thresholds", "set", "0"])
                assert result.exit_code != 0
                result = runner.invoke(cli, ["plan", "thresholds", "set", "201"])
                assert result.exit_code != 0

    def test_set_requires_at_least_one_value(self):
        runner = CliRunner()
        with runner.isolated_filesystem() as td:
            p1, p2 = _patch_settings_dir(Path(td))
            with p1, p2:
                result = runner.invoke(cli, ["plan", "thresholds", "set"])
                assert result.exit_code != 0

    def test_reset_restores_default(self):
        runner = CliRunner()
        with runner.isolated_filesystem() as td:
            p1, p2 = _patch_settings_dir(Path(td))
            with p1, p2:
                runner.invoke(cli, ["plan", "thresholds", "set", "10", "20"])
                result = runner.invoke(cli, ["plan", "thresholds", "reset"])
                assert result.exit_code == 0, result.output
                assert "default" in result.output
                # After reset, show emits the built-in [50, 75, 90].
                result = runner.invoke(
                    cli, ["plan", "thresholds", "show", "--format", "json"]
                )
                assert json.loads(result.output) == {"thresholds": [50, 75, 90]}

    def test_cfg_set_plan_alert_thresholds_rejected(self):
        """``cfg set plan_alert_thresholds`` must steer users to the dedicated cmd."""
        runner = CliRunner()
        with runner.isolated_filesystem() as td:
            p1, p2 = _patch_settings_dir(Path(td))
            with p1, p2:
                result = runner.invoke(
                    cli, ["cfg", "set", "plan_alert_thresholds", "50"]
                )
                assert result.exit_code != 0
                assert "plan thresholds set" in result.output


# ── plan reset ──────────────────────────────────────────────────────────────

class TestPlanReset:
    def test_reset_clears_plan(self):
        runner = CliRunner()
        with runner.isolated_filesystem() as td:
            p1, p2 = _patch_settings_dir(Path(td))
            with p1, p2:
                runner.invoke(cli, ["plan", "set", "claude-pro"])
                cfg_file = Path(td) / ".stackunderflow" / "config.json"
                assert "plan_name" in json.loads(cfg_file.read_text())

                result = runner.invoke(cli, ["plan", "reset"])
                assert result.exit_code == 0, result.output
                assert "cleared" in result.output

                data = json.loads(cfg_file.read_text())
                assert "plan_name" not in data
                assert "plan_monthly_usd" not in data
                assert "plan_reset_day" not in data

    def test_reset_when_no_plan_set_is_idempotent(self):
        runner = CliRunner()
        with runner.isolated_filesystem() as td:
            p1, p2 = _patch_settings_dir(Path(td))
            with p1, p2:
                result = runner.invoke(cli, ["plan", "reset"])
                assert result.exit_code == 0, result.output


# ── cfg set guard ───────────────────────────────────────────────────────────

class TestCfgSetGuardForPlanKeys:
    """Plan settings have inter-key invariants — ``cfg set`` must reject them."""

    def test_cfg_set_plan_name_rejected(self):
        runner = CliRunner()
        with runner.isolated_filesystem() as td:
            p1, p2 = _patch_settings_dir(Path(td))
            with p1, p2:
                result = runner.invoke(cli, ["cfg", "set", "plan_name", "claude-pro"])
                assert result.exit_code != 0
                assert "stackunderflow plan set" in result.output
