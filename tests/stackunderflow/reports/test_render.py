"""Tests for report renderers."""

import io
import json
import os
import sys
import unittest

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))))

from stackunderflow.reports.render import (
    render_csv,
    render_json,
    render_status_line,
    render_text,
)


def _sample() -> dict:
    return {
        "scope_label": "last 7 days",
        "total_cost": 12.345,
        "total_messages": 1234,
        "total_sessions": 56,
        "by_project": [
            {"name": "proj-a", "cost": 10.00, "messages": 1000, "sessions": 40},
            {"name": "proj-b", "cost": 2.345, "messages": 234, "sessions": 16},
        ],
    }


class TestRenderJSON(unittest.TestCase):
    def test_roundtrip_is_stable(self):
        out = render_json(_sample())
        parsed = json.loads(out)
        self.assertEqual(parsed["total_cost"], 12.345)
        self.assertEqual(len(parsed["by_project"]), 2)
        self.assertEqual(parsed["scope_label"], "last 7 days")


class TestRenderText(unittest.TestCase):
    def test_text_contains_cost_and_project_names(self):
        buf = io.StringIO()
        render_text(_sample(), stream=buf)
        out = buf.getvalue()
        self.assertIn("last 7 days", out)
        self.assertIn("proj-a", out)
        self.assertIn("proj-b", out)
        # Rich formats floats; at minimum the integer portions appear.
        # Note: f"{12.345:.2f}" rounds to "12.35" in CPython, not "12.34".
        self.assertIn("12.35", out)  # total cost
        self.assertIn("1,234", out)  # total messages with thousands separator

    def test_empty_report_renders_without_error(self):
        buf = io.StringIO()
        render_text({
            "scope_label": "today",
            "total_cost": 0.0,
            "total_messages": 0,
            "total_sessions": 0,
            "by_project": [],
        }, stream=buf)
        out = buf.getvalue()
        self.assertIn("today", out)
        self.assertIn("No activity", out)


class TestRenderStatusLine(unittest.TestCase):
    def test_one_liner_shape(self):
        today = {"total_cost": 0.50, "total_messages": 10, "total_sessions": 2, "scope_label": "today", "by_project": []}
        month = {"total_cost": 15.25, "total_messages": 500, "total_sessions": 30, "scope_label": "month", "by_project": []}
        line = render_status_line(today=today, month=month)
        self.assertIn("today", line)
        self.assertIn("month", line)
        self.assertIn("$0.50", line)
        self.assertIn("$15.25", line)


class TestRenderCSV(unittest.TestCase):
    def test_csv_has_header_and_rows(self):
        out = render_csv(_sample())
        lines = out.strip().split("\n")
        self.assertEqual(lines[0], "project,cost,messages,sessions")
        self.assertEqual(len(lines), 3)  # header + 2 projects
        self.assertIn("proj-a", lines[1])
        self.assertIn("10.00", lines[1])


if __name__ == "__main__":
    unittest.main()
