"""Continue IDE pricer.

The module is named ``continue_pricer`` (not ``continue``) because
``continue`` is a Python keyword and would block ``import``.

Continue is BYO-key — the user plugs in an Anthropic / OpenAI / local
key and the extension forwards completions to the matching upstream.
The on-disk schema (when present) records the model the run actually
used, so this pricer follows the same vendor-prefix delegation pattern
as ``ClinePricer`` / ``CopilotPricer``:

  - ``claude-*``  / ``anthropic/...`` → ``AnthropicPricer``
  - ``gpt-*``     / ``openai/...``    → ``OpenAIPricer``
  - everything else (including ``continue-auto``) → ``None``

Spec: ``docs/specs/multi-provider/local-inventory.md`` §13.
"""

from __future__ import annotations

from .anthropic import AnthropicPricer
from .base import ProviderPricer
from .openai import OpenAIPricer


class ContinuePricer(ProviderPricer):
    provider_name = "continue"

    def __init__(self) -> None:
        self._anthropic = AnthropicPricer()
        self._openai = OpenAIPricer()

    def canonicalize(self, model_id: str) -> str:
        """Pass through; ``rates_for`` does the prefix split."""
        return model_id or ""

    def normalize_tokens(self, raw: dict[str, int]) -> dict[str, int]:
        """No-op — the adapter emits canonical 4-key shape."""
        return {
            "input": int(raw.get("input", 0) or 0),
            "output": int(raw.get("output", 0) or 0),
            "cache_creation": int(raw.get("cache_creation", 0) or 0),
            "cache_read": int(raw.get("cache_read", 0) or 0),
        }

    def rates_for(
        self, canonical: str
    ) -> tuple[float, float, float, float] | None:
        """Vendor-prefix delegation; unknown vendors return ``None``."""
        if not canonical:
            return None

        lowered = canonical.lower()
        if "/" in lowered:
            vendor, _, suffix = lowered.partition("/")
        else:
            vendor, suffix = "", lowered

        if vendor == "anthropic" or lowered.startswith("claude-"):
            target = suffix if vendor == "anthropic" else lowered
            return self._anthropic.rates_for(self._anthropic.canonicalize(target))

        if vendor == "openai" or lowered.startswith("gpt-"):
            target = suffix if vendor == "openai" else lowered
            return self._openai.rates_for(self._openai.canonicalize(target))

        return None

    def supports_per_message_tokens(self) -> bool:
        # The defensive parser tries to extract per-message token counts;
        # falls back to estimation (with cost_source flag) when missing.
        return True
