"""Tests for cross-project aggregation."""

import os
import sys
import unittest
from unittest.mock import patch

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))))

from stackunderflow.reports.aggregate import build_report
from stackunderflow.reports.scope import Scope


def _fake_project(name: str) -> dict:
    return {
        "dir_name": name,
        "log_path": f"/fake/{name}",
    }


def _fake_stats(daily: dict, total_cost: float = 0.0) -> dict:
    """Shape matches what stackunderflow.pipeline.aggregator.summarise() returns."""
    return {
        "overview": {
            "project_name": "x",
            "log_dir_name": "x",
            "project_path": "/fake/x",
            "total_messages": sum(d["messages"] for d in daily.values()),
            "date_range": {"start": None, "end": None},
            "sessions": 1,
            "message_types": {},
            "total_tokens": {"input": 100, "output": 50},
            "total_cost": total_cost,
        },
        "tools": {"usage_counts": {}, "error_counts": {}, "error_rates": {}},
        "sessions": {"count": 1, "average_duration_seconds": 0, "average_messages": 0, "sessions_with_errors": 0},
        "daily_stats": daily,
        "hourly_pattern": {"messages": {}, "tokens": {}},
        "errors": {"total": 0, "rate": 0, "by_type": {}, "by_category": {}, "error_details": [], "assistant_details": []},  # noqa: E501
        "models": {},
        "user_interactions": {},
        "cache": {"total_created": 0, "total_read": 0, "hit_rate": 0},
    }


class TestBuildReport(unittest.TestCase):
    """build_report sums across projects within scope."""

    def _process_stub(self, log_path: str):
        data = {
            "/fake/proj-a": _fake_stats({
                "2026-04-15": {"messages": 10, "sessions": 2, "tokens": {"input": 100}, "cost": {"total": 1.00, "by_model": {}}, "user_commands": 5, "interrupted_commands": 0, "interruption_rate": 0, "errors": 0, "assistant_messages": 5, "error_rate": 0},  # noqa: E501
                "2026-04-16": {"messages": 20, "sessions": 3, "tokens": {"input": 200}, "cost": {"total": 2.00, "by_model": {}}, "user_commands": 10, "interrupted_commands": 0, "interruption_rate": 0, "errors": 0, "assistant_messages": 10, "error_rate": 0},  # noqa: E501
            }, total_cost=3.00),
            "/fake/proj-b": _fake_stats({
                "2026-04-16": {"messages": 5, "sessions": 1, "tokens": {"input": 50}, "cost": {"total": 0.50, "by_model": {}}, "user_commands": 3, "interrupted_commands": 0, "interruption_rate": 0, "errors": 0, "assistant_messages": 3, "error_rate": 0},  # noqa: E501
            }, total_cost=0.50),
        }
        # Return (messages, stats) — messages unused here
        return ([], data[log_path])

    def test_all_time_scope_sums_everything(self):
        projects = [_fake_project("proj-a"), _fake_project("proj-b")]
        scope = Scope(since=None, until=None, label="all time")
        with patch("stackunderflow.reports.aggregate._run_pipeline", side_effect=self._process_stub):
            report = build_report(projects, scope=scope, include=None, exclude=None)
        self.assertAlmostEqual(report["total_cost"], 3.50)
        self.assertEqual(report["total_messages"], 35)
        self.assertEqual(report["total_sessions"], 6)
        self.assertEqual(len(report["by_project"]), 2)

    def test_today_scope_only_counts_today(self):
        projects = [_fake_project("proj-a"), _fake_project("proj-b")]
        scope = Scope(
            since="2026-04-16T00:00:00+00:00",
            until="2026-04-16T23:59:59+00:00",
            label="today",
        )
        with patch("stackunderflow.reports.aggregate._run_pipeline", side_effect=self._process_stub):
            report = build_report(projects, scope=scope, include=None, exclude=None)
        # proj-a: 2026-04-16 → 20 messages, $2.00; proj-b: 2026-04-16 → 5 messages, $0.50.
        # 2026-04-15 is excluded.
        self.assertAlmostEqual(report["total_cost"], 2.50)
        self.assertEqual(report["total_messages"], 25)

    def test_include_filter(self):
        projects = [_fake_project("proj-a"), _fake_project("proj-b")]
        scope = Scope(since=None, until=None, label="all")
        with patch("stackunderflow.reports.aggregate._run_pipeline", side_effect=self._process_stub):
            report = build_report(projects, scope=scope, include=["proj-a"], exclude=None)
        self.assertEqual(len(report["by_project"]), 1)
        self.assertEqual(report["by_project"][0]["name"], "proj-a")
        self.assertAlmostEqual(report["total_cost"], 3.00)

    def test_exclude_filter(self):
        projects = [_fake_project("proj-a"), _fake_project("proj-b")]
        scope = Scope(since=None, until=None, label="all")
        with patch("stackunderflow.reports.aggregate._run_pipeline", side_effect=self._process_stub):
            report = build_report(projects, scope=scope, include=None, exclude=["proj-b"])
        self.assertEqual(len(report["by_project"]), 1)
        self.assertEqual(report["by_project"][0]["name"], "proj-a")

    def test_per_project_rankings_sorted_by_cost_desc(self):
        projects = [_fake_project("proj-a"), _fake_project("proj-b")]
        scope = Scope(since=None, until=None, label="all")
        with patch("stackunderflow.reports.aggregate._run_pipeline", side_effect=self._process_stub):
            report = build_report(projects, scope=scope, include=None, exclude=None)
        costs = [p["cost"] for p in report["by_project"]]
        self.assertEqual(costs, sorted(costs, reverse=True))


if __name__ == "__main__":
    unittest.main()
