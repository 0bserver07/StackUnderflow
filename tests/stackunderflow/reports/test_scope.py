"""Tests for report scope / period parsing."""

import os
import sys
import unittest
from datetime import UTC, datetime, timedelta

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))))

from stackunderflow.reports.scope import Scope, parse_period


class TestParsePeriod(unittest.TestCase):
    """parse_period(spec) returns a (since, until) pair in UTC ISO-8601."""

    def _now(self) -> datetime:
        # Use a fixed "now" so tests are deterministic.
        return datetime(2026, 4, 16, 12, 0, 0, tzinfo=UTC)

    def test_today(self):
        s = parse_period("today", now=self._now())
        self.assertEqual(s.since, "2026-04-16T00:00:00+00:00")
        self.assertEqual(s.until, "2026-04-16T23:59:59+00:00")
        self.assertEqual(s.label, "today")

    def test_7days(self):
        s = parse_period("7days", now=self._now())
        # Rolling 7-day window ends at now() and starts 7 days back at the same instant.
        expected_since = (self._now() - timedelta(days=7)).isoformat()
        self.assertEqual(s.since, expected_since)
        self.assertEqual(s.until, self._now().isoformat())
        self.assertEqual(s.label, "last 7 days")

    def test_30days(self):
        s = parse_period("30days", now=self._now())
        expected_since = (self._now() - timedelta(days=30)).isoformat()
        self.assertEqual(s.since, expected_since)
        self.assertEqual(s.label, "last 30 days")

    def test_month(self):
        s = parse_period("month", now=self._now())
        self.assertEqual(s.since, "2026-04-01T00:00:00+00:00")
        self.assertEqual(s.until, "2026-04-30T23:59:59+00:00")
        self.assertEqual(s.label, "this month (April 2026)")

    def test_all(self):
        s = parse_period("all", now=self._now())
        self.assertIsNone(s.since)
        self.assertIsNone(s.until)
        self.assertEqual(s.label, "all time")

    def test_unknown_period_raises(self):
        with self.assertRaises(ValueError) as ctx:
            parse_period("bogus", now=self._now())
        self.assertIn("Unknown period", str(ctx.exception))


class TestScopeContains(unittest.TestCase):
    """Scope.contains(timestamp) is the filter used by aggregation."""

    def test_all_contains_anything(self):
        s = Scope(since=None, until=None, label="all")
        self.assertTrue(s.contains("2020-01-01T00:00:00+00:00"))
        self.assertTrue(s.contains("2099-12-31T23:59:59+00:00"))
        self.assertFalse(s.contains(""))  # empty timestamp never included
        self.assertFalse(s.contains(None))

    def test_bounded_scope(self):
        s = Scope(since="2026-04-16T00:00:00+00:00", until="2026-04-16T23:59:59+00:00", label="today")
        self.assertTrue(s.contains("2026-04-16T10:30:00+00:00"))
        self.assertFalse(s.contains("2026-04-15T23:59:59+00:00"))
        self.assertFalse(s.contains("2026-04-17T00:00:00+00:00"))

    def test_malformed_timestamp_excluded(self):
        s = Scope(since="2026-04-16T00:00:00+00:00", until="2026-04-16T23:59:59+00:00", label="today")
        self.assertFalse(s.contains("not-a-date"))


if __name__ == "__main__":
    unittest.main()
