"""Tests for the stale-pricing signal in PricingService.

These tests avoid hitting the network by monkey-patching _fetch_from_litellm
and by redirecting the on-disk cache into a tempdir.
"""

import json
import os
import sys
import tempfile
import unittest
from datetime import UTC, datetime, timedelta
from pathlib import Path
from unittest.mock import patch

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__)))))

from stackunderflow.services.pricing_service import PricingService


class TestPricingStaleness(unittest.TestCase):
    """Verify is_stale is True whenever pricing data is untrustworthy."""

    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.tmp_path = Path(self._tmp.name)

        # Redirect the service onto a throwaway cache dir so tests don't touch
        # the user's real ~/.stackunderflow cache.
        self.svc = PricingService()
        self.svc.cache_dir = self.tmp_path
        self.svc.pricing_cache_file = self.tmp_path / "pricing.json"
        self.tmp_path.mkdir(parents=True, exist_ok=True)

    def tearDown(self):
        self._tmp.cleanup()

    def _write_cache(self, timestamp: datetime, pricing: dict | None = None) -> None:
        payload = {
            "timestamp": timestamp.isoformat(),
            "source": "litellm",
            "version": "1.0",
            "pricing": pricing or {"claude-test": {"input_cost_per_token": 1e-6}},
        }
        self.svc.pricing_cache_file.write_text(json.dumps(payload))

    def test_fresh_cache_is_not_stale(self):
        """Cache written seconds ago is within the 7-day threshold."""
        self._write_cache(datetime.now(UTC))
        result = self.svc.get_pricing()
        self.assertFalse(result["is_stale"])
        self.assertEqual(result["source"], "cache")

    def test_cache_older_than_threshold_is_stale_even_when_fetch_fails(self):
        """Cache older than STALE_THRESHOLD must flag is_stale=True."""
        old_ts = datetime.now(UTC) - (PricingService.STALE_THRESHOLD + timedelta(days=1))
        self._write_cache(old_ts)

        # _is_cache_valid returns False (>24h), so service tries a refresh.
        # Simulate a refresh failure — stale cache should be returned with
        # is_stale=True.
        with patch.object(PricingService, "_fetch_from_litellm", return_value=None):
            result = self.svc.get_pricing()

        self.assertTrue(result["is_stale"])
        self.assertEqual(result["source"], "cache")

    def test_failed_refresh_marks_stale_even_if_cache_recent(self):
        """If the 24h validity window just expired, a failed refresh still
        surfaces as stale so users know the data is not fresh."""
        # 25h old: past cache_duration but well under STALE_THRESHOLD.
        ts = datetime.now(UTC) - timedelta(hours=25)
        self._write_cache(ts)

        with patch.object(PricingService, "_fetch_from_litellm", return_value=None):
            result = self.svc.get_pricing()

        self.assertTrue(result["is_stale"])

    def test_no_cache_and_no_network_falls_back_to_defaults_stale(self):
        """When no cache exists and LiteLLM is unreachable, the hardcoded
        defaults are returned but flagged stale — they drift over time."""
        self.assertFalse(self.svc.pricing_cache_file.exists())

        with patch.object(PricingService, "_fetch_from_litellm", return_value=None):
            result = self.svc.get_pricing()

        self.assertEqual(result["source"], "default")
        self.assertTrue(result["is_stale"])

    def test_successful_refresh_clears_stale_flag(self):
        """A good fetch returns is_stale=False regardless of prior cache age."""
        old_ts = datetime.now(UTC) - (PricingService.STALE_THRESHOLD + timedelta(days=2))
        self._write_cache(old_ts)

        fresh = {"claude-test-fresh": {"input_cost_per_token": 2e-6}}
        with patch.object(PricingService, "_fetch_from_litellm", return_value=fresh):
            result = self.svc.get_pricing()

        self.assertFalse(result["is_stale"])
        self.assertEqual(result["source"], "litellm")
        self.assertIn("claude-test-fresh", result["pricing"])


if __name__ == "__main__":
    unittest.main()
