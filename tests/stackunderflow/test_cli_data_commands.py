"""End-to-end CLI tests for data-facing commands."""

import json
import os
import sys
import unittest
from unittest.mock import patch

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

from click.testing import CliRunner

from stackunderflow.cli import cli


def _fake_projects():
    return [
        {"dir_name": "alpha", "log_path": "/fake/alpha"},
        {"dir_name": "beta", "log_path": "/fake/beta"},
    ]


def _fake_report():
    return {
        "scope_label": "last 7 days",
        "total_cost": 5.00,
        "total_messages": 500,
        "total_sessions": 20,
        "by_project": [
            {"name": "alpha", "cost": 3.00, "messages": 300, "sessions": 12},
            {"name": "beta", "cost": 2.00, "messages": 200, "sessions": 8},
        ],
    }


class TestReportCommand(unittest.TestCase):
    def setUp(self):
        self.runner = CliRunner()

    def test_report_default_period_is_7days(self):
        with patch("stackunderflow.cli.list_projects", return_value=_fake_projects()), \
             patch("stackunderflow.cli.build_report", return_value=_fake_report()) as mock_build:
            result = self.runner.invoke(cli, ["report"])
        self.assertEqual(result.exit_code, 0, msg=result.output)
        self.assertIn("last 7 days", result.output)
        # Verify scope passed was the 7day window
        _args, kwargs = mock_build.call_args
        self.assertEqual(kwargs["scope"].label, "last 7 days")

    def test_report_format_json(self):
        with patch("stackunderflow.cli.list_projects", return_value=_fake_projects()), \
             patch("stackunderflow.cli.build_report", return_value=_fake_report()):
            result = self.runner.invoke(cli, ["report", "--format", "json"])
        self.assertEqual(result.exit_code, 0, msg=result.output)
        parsed = json.loads(result.output)
        self.assertEqual(parsed["total_cost"], 5.00)

    def test_report_period_30days(self):
        with patch("stackunderflow.cli.list_projects", return_value=_fake_projects()), \
             patch("stackunderflow.cli.build_report", return_value=_fake_report()) as mock_build:
            self.runner.invoke(cli, ["report", "-p", "30days"])
        _args, kwargs = mock_build.call_args
        self.assertEqual(kwargs["scope"].label, "last 30 days")

    def test_unknown_period_exits_nonzero(self):
        with patch("stackunderflow.cli.list_projects", return_value=_fake_projects()):
            result = self.runner.invoke(cli, ["report", "-p", "bogus"])
        self.assertNotEqual(result.exit_code, 0)
        self.assertIn("Unknown period", result.output)


class TestTodayMonthCommands(unittest.TestCase):
    def setUp(self):
        self.runner = CliRunner()

    def test_today_passes_today_scope(self):
        with patch("stackunderflow.cli.list_projects", return_value=_fake_projects()), \
             patch("stackunderflow.cli.build_report", return_value=_fake_report()) as mock_build:
            self.runner.invoke(cli, ["today"])
        _args, kwargs = mock_build.call_args
        self.assertEqual(kwargs["scope"].label, "today")

    def test_month_passes_month_scope(self):
        with patch("stackunderflow.cli.list_projects", return_value=_fake_projects()), \
             patch("stackunderflow.cli.build_report", return_value=_fake_report()) as mock_build:
            self.runner.invoke(cli, ["month"])
        _args, kwargs = mock_build.call_args
        self.assertIn("this month", kwargs["scope"].label)


class TestStatusCommand(unittest.TestCase):
    def setUp(self):
        self.runner = CliRunner()

    def test_status_outputs_one_line(self):
        def build(_projects, *, scope, include, exclude):
            return {
                "scope_label": scope.label,
                "total_cost": 1.23 if scope.label == "today" else 45.67,
                "total_messages": 10 if scope.label == "today" else 500,
                "total_sessions": 2,
                "by_project": [],
            }
        with patch("stackunderflow.cli.list_projects", return_value=_fake_projects()), \
             patch("stackunderflow.cli.build_report", side_effect=build):
            result = self.runner.invoke(cli, ["status"])
        self.assertEqual(result.exit_code, 0, msg=result.output)
        lines = [ln for ln in result.output.strip().split("\n") if ln.strip()]
        self.assertEqual(len(lines), 1)
        self.assertIn("today: $1.23", lines[0])
        self.assertIn("month: $45.67", lines[0])

    def test_status_format_json(self):
        def build(_projects, *, scope, include, exclude):
            return {
                "scope_label": scope.label,
                "total_cost": 1.0,
                "total_messages": 10,
                "total_sessions": 2,
                "by_project": [],
            }
        with patch("stackunderflow.cli.list_projects", return_value=_fake_projects()), \
             patch("stackunderflow.cli.build_report", side_effect=build):
            result = self.runner.invoke(cli, ["status", "--format", "json"])
        self.assertEqual(result.exit_code, 0, msg=result.output)
        parsed = json.loads(result.output)
        self.assertIn("today", parsed)
        self.assertIn("month", parsed)
        self.assertEqual(parsed["today"]["total_cost"], 1.0)


class TestExportCommand(unittest.TestCase):
    def setUp(self):
        self.runner = CliRunner()

    def test_export_csv_default(self):
        with patch("stackunderflow.cli.list_projects", return_value=_fake_projects()), \
             patch("stackunderflow.cli.build_report", return_value=_fake_report()):
            result = self.runner.invoke(cli, ["export"])
        self.assertEqual(result.exit_code, 0, msg=result.output)
        self.assertIn("project,cost,messages,sessions", result.output)
        self.assertIn("alpha", result.output)

    def test_export_json(self):
        with patch("stackunderflow.cli.list_projects", return_value=_fake_projects()), \
             patch("stackunderflow.cli.build_report", return_value=_fake_report()):
            result = self.runner.invoke(cli, ["export", "-f", "json"])
        self.assertEqual(result.exit_code, 0, msg=result.output)
        parsed = json.loads(result.output)
        self.assertEqual(parsed["total_messages"], 500)


if __name__ == "__main__":
    unittest.main()
