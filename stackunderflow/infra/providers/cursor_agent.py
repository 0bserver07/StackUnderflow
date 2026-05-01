"""Cursor Agent provider pricer.

Cursor Agent transcripts don't carry explicit token counts — the adapter
estimates from character length / 4 and stamps
``record.raw["cost_source"] = "estimated"``. Pricing strategy is exactly
the same as ``ClinePricer`` and ``OpenCodePricer``: parse the model id
attributed by the optional SQLite tracking DB and delegate to the real
upstream pricer.

Recognised prefixes (case-insensitive):
- bare ``claude-*`` → ``AnthropicPricer``
- bare ``gpt-*`` → ``OpenAIPricer``

The literal ``cursor-agent`` fallback (used when the SQLite DB is missing
or has no row for a session) returns ``None`` — the cost layer treats
that as "no rate", consistent with the estimated-tokens flag.

``supports_per_message_tokens()`` returns ``False`` because every Record
is built from a length-based estimate, so the aggregator must skip
per-message cost for Cursor Agent records and rely on session totals.

``normalize_tokens`` is a no-op — the adapter already emits the canonical
4-key shape.
"""

from __future__ import annotations

from .anthropic import AnthropicPricer
from .base import ProviderPricer
from .openai import OpenAIPricer


class CursorAgentPricer(ProviderPricer):
    provider_name = "cursor-agent"

    def __init__(self) -> None:
        self._anthropic = AnthropicPricer()
        self._openai = OpenAIPricer()

    def canonicalize(self, model_id: str) -> str:
        """Pass the model id through unchanged for prefix-based routing."""
        return model_id or ""

    def normalize_tokens(self, raw: dict[str, int]) -> dict[str, int]:
        """No-op. The adapter already emits canonical 4-key shape."""
        return {
            "input": int(raw.get("input", 0) or 0),
            "output": int(raw.get("output", 0) or 0),
            "cache_creation": int(raw.get("cache_creation", 0) or 0),
            "cache_read": int(raw.get("cache_read", 0) or 0),
        }

    def rates_for(
        self, canonical: str
    ) -> tuple[float, float, float, float] | None:
        """Delegate by model-id prefix; bare ``cursor-agent`` returns None."""
        if not canonical:
            return None
        lowered = canonical.lower()
        if lowered.startswith("claude-"):
            return self._anthropic.rates_for(self._anthropic.canonicalize(lowered))
        if lowered.startswith("gpt-"):
            return self._openai.rates_for(self._openai.canonicalize(lowered))
        return None

    def supports_per_message_tokens(self) -> bool:
        """Tokens are estimated from character length; not authoritative."""
        return False
