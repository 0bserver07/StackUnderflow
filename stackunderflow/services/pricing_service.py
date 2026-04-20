"""
Dynamic pricing service that fetches and caches model pricing from LiteLLM.
"""

import json
import logging
import urllib.error
import urllib.request
from datetime import UTC, datetime, timedelta
from pathlib import Path
from typing import Any

from ..infra.costs import RATE_CARD as DEFAULT_CLAUDE_PRICING

logger = logging.getLogger(__name__)


class PricingService:
    """Service for managing dynamic model pricing with caching."""

    # Pricing older than this is considered stale even if the last refresh
    # didn't explicitly fail. 7 days is long enough to absorb intermittent
    # LiteLLM outages but short enough that a real stagnation is visible.
    STALE_THRESHOLD = timedelta(days=7)

    def __init__(self):
        self.cache_dir = Path.home() / ".stackunderflow" / "cache"
        self.pricing_cache_file = self.cache_dir / "pricing.json"
        self.litellm_url = "https://raw.githubusercontent.com/BerriAI/litellm/main/model_prices_and_context_window.json"
        self.cache_duration = timedelta(hours=24)

        # Ensure cache directory exists
        self.cache_dir.mkdir(parents=True, exist_ok=True)

    def get_pricing(self) -> dict[str, Any]:
        """
        Get pricing with intelligent cache and fallback logic.

        Returns:
            Dict with keys:
            - pricing: Dict of model prices
            - source: 'cache', 'litellm', or 'default'
            - timestamp: When prices were fetched
            - is_stale: Boolean indicating if cache is expired
        """
        # Check if cache exists and load it
        cache_data = self._load_cache()

        if cache_data:
            is_valid = self._is_cache_valid(cache_data.get("timestamp"))

            if is_valid:
                # Cache is fresh, use it
                return {
                    "pricing": cache_data["pricing"],
                    "source": "cache",
                    "timestamp": cache_data["timestamp"],
                    "is_stale": self._is_beyond_stale_threshold(cache_data.get("timestamp")),
                }
            else:
                # Cache is stale, try to refresh
                fresh_data = self._fetch_from_litellm()

                if fresh_data:
                    # Successfully fetched fresh data
                    self._save_to_cache(fresh_data)
                    return {
                        "pricing": fresh_data,
                        "source": "litellm",
                        "timestamp": datetime.now(UTC).isoformat(),
                        "is_stale": False,
                    }
                else:
                    # Failed to fetch - surface staleness (refresh failed OR
                    # cached data exceeds STALE_THRESHOLD, whichever applies).
                    return {
                        "pricing": cache_data["pricing"],
                        "source": "cache",
                        "timestamp": cache_data["timestamp"],
                        "is_stale": True,
                    }
        else:
            # No cache exists, try to fetch
            fresh_data = self._fetch_from_litellm()

            if fresh_data:
                # Successfully fetched data
                self._save_to_cache(fresh_data)
                return {
                    "pricing": fresh_data,
                    "source": "litellm",
                    "timestamp": datetime.now(UTC).isoformat(),
                    "is_stale": False,
                }
            else:
                # No cache and can't fetch - use hardcoded defaults.
                # These can drift from Anthropic's real rates, so flag stale.
                return {
                    "pricing": DEFAULT_CLAUDE_PRICING,
                    "source": "default",
                    "timestamp": datetime.now(UTC).isoformat(),
                    "is_stale": True,
                }

    def force_refresh(self) -> bool:
        """Force refresh pricing from LiteLLM."""
        fresh_data = self._fetch_from_litellm()
        if fresh_data:
            self._save_to_cache(fresh_data)
            return True
        return False

    def _load_cache(self) -> dict | None:
        """Load pricing data from cache file."""
        if not self.pricing_cache_file.exists():
            return None

        try:
            with open(self.pricing_cache_file) as f:
                return json.load(f)
        except (OSError, json.JSONDecodeError) as e:
            logger.info(f"Error loading pricing cache: {e}")
            return None

    def _save_to_cache(self, pricing_data: dict):
        """Save pricing data to cache with timestamp."""
        cache_data = {
            "timestamp": datetime.now(UTC).isoformat(),
            "source": "litellm",
            "version": "1.0",
            "pricing": pricing_data,
        }

        try:
            # Cold-cache cleanup at startup may have removed the parent dir;
            # re-create it defensively so pricing refresh survives.
            self.cache_dir.mkdir(parents=True, exist_ok=True)
            with open(self.pricing_cache_file, "w") as f:
                json.dump(cache_data, f, indent=2)
        except OSError as e:
            logger.info(f"Error saving pricing cache: {e}")

    def _is_cache_valid(self, timestamp_str: str | None) -> bool:
        """Check if cache timestamp is within valid duration."""
        if not timestamp_str:
            return False

        try:
            cache_time = datetime.fromisoformat(timestamp_str.replace("Z", "+00:00"))
            age = datetime.now(UTC) - cache_time
            return age < self.cache_duration
        except (ValueError, AttributeError):
            return False

    def _is_beyond_stale_threshold(self, timestamp_str: str | None) -> bool:
        """True if the cached timestamp is older than STALE_THRESHOLD.

        A missing/unparseable timestamp is treated as stale — we can't prove
        freshness, so we warn.
        """
        if not timestamp_str:
            return True
        try:
            cache_time = datetime.fromisoformat(timestamp_str.replace("Z", "+00:00"))
        except (ValueError, AttributeError):
            return True
        return (datetime.now(UTC) - cache_time) >= self.STALE_THRESHOLD

    def _fetch_from_litellm(self) -> dict | None:
        """Fetch latest pricing from LiteLLM GitHub.

        A failure here (network, parse, schema) means downstream callers will
        fall back to a cached or hardcoded rate card. We emit a WARNING so
        operators can notice rather than silently serving stale numbers.
        """
        try:
            # Set a timeout for the request
            with urllib.request.urlopen(self.litellm_url, timeout=10) as response:
                litellm_data = json.loads(response.read().decode("utf-8"))

            # Transform to our format
            return self._transform_litellm_to_claude(litellm_data)

        except (urllib.error.URLError, urllib.error.HTTPError, json.JSONDecodeError, Exception) as e:
            logger.warning(
                "Failed to refresh pricing from LiteLLM (%s); pricing may be stale: %s",
                self.litellm_url,
                e,
            )
            return None

    def _transform_litellm_to_claude(self, litellm_data: dict) -> dict:
        """Transform LiteLLM format to our Claude pricing format."""
        result = {}

        for model_name, model_data in litellm_data.items():
            # Only process Anthropic models
            if not isinstance(model_data, dict):
                continue

            provider = model_data.get("litellm_provider", "")
            if provider != "anthropic":
                continue

            # Skip if no pricing data
            if "input_cost_per_token" not in model_data:
                continue

            # Extract base costs
            input_cost = float(model_data.get("input_cost_per_token", 0))
            output_cost = float(model_data.get("output_cost_per_token", 0))

            # Calculate cache costs if not explicitly provided
            # LiteLLM might have these fields: cache_creation_input_token_cost, cache_read_input_token_cost
            cache_creation = float(model_data.get("cache_creation_input_token_cost", input_cost * 1.25))
            cache_read = float(model_data.get("cache_read_input_token_cost", input_cost * 0.10))

            result[model_name] = {
                "input_cost_per_token": input_cost,
                "output_cost_per_token": output_cost,
                "cache_creation_cost_per_token": cache_creation,
                "cache_read_cost_per_token": cache_read,
            }

        # If no Anthropic models found, return our defaults
        if not result:
            return DEFAULT_CLAUDE_PRICING

        return result
