"""OpenClaw pricer.

OpenClaw records the upstream provider directly on each message
(``message.provider`` plus ``message.model``). Token shape is canonical
Anthropic-style (``input``/``output``/``cacheRead``/``cacheWrite``), so
``normalize_tokens`` is a no-op.

Routing by model name:
- ``claude-*`` → AnthropicPricer
- ``gpt-*`` / Codex → OpenAIPricer
- otherwise → fall back to AnthropicPricer (OpenClaw's most common
  upstream is Anthropic; the conservative fallback matches the
  registry-level fallback policy for unknown providers).

Spec §3 (multi-provider).
"""

from __future__ import annotations

from .anthropic import AnthropicPricer
from .base import ProviderPricer
from .openai import OpenAIPricer


class OpenClawPricer(ProviderPricer):
    provider_name = "openclaw"

    def __init__(self) -> None:
        self._anthropic = AnthropicPricer()
        self._openai = OpenAIPricer()

    def canonicalize(self, model_id: str) -> str:
        if not isinstance(model_id, str):
            return ""
        return model_id.strip().lower()

    def normalize_tokens(self, raw: dict[str, int]) -> dict[str, int]:
        return {
            "input": int(raw.get("input", 0) or 0),
            "output": int(raw.get("output", 0) or 0),
            "cache_creation": int(raw.get("cache_creation", 0) or 0),
            "cache_read": int(raw.get("cache_read", 0) or 0),
        }

    def rates_for(
        self, canonical: str
    ) -> tuple[float, float, float, float] | None:
        if not canonical:
            return None
        if canonical.startswith("gpt-") or "codex" in canonical:
            return self._openai.rates_for(self._openai.canonicalize(canonical))
        # Default route is Anthropic — most OpenClaw deployments are
        # Claude-backed and the Anthropic pricer's family heuristic
        # gracefully falls back to SONNET_35 for unknown ids.
        return self._anthropic.rates_for(self._anthropic.canonicalize(canonical))

    def supports_per_message_tokens(self) -> bool:
        return True
