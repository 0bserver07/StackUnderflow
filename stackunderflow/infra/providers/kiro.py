"""Kiro pricer.

Kiro estimates tokens from content length / 4 — the underlying source
has no real per-call usage. ``supports_per_message_tokens`` is therefore
``False`` so the cost layer can decide to skip per-message cost
attribution and rely on session aggregates instead.

Model ids come from Kiro's metadata (already normalised by the adapter:
``claude.3.5.sonnet`` → ``claude-3-5-sonnet``). We route by vendor
prefix (Anthropic / OpenAI) and return ``None`` for unknowns.

Spec §3 (multi-provider).
"""

from __future__ import annotations

from .anthropic import AnthropicPricer
from .base import ProviderPricer
from .openai import OpenAIPricer


class KiroPricer(ProviderPricer):
    provider_name = "kiro"

    def __init__(self) -> None:
        self._anthropic = AnthropicPricer()
        self._openai = OpenAIPricer()

    def canonicalize(self, model_id: str) -> str:
        if not isinstance(model_id, str):
            return ""
        # Defensive: if a caller hands us the raw dotted form, normalise
        # here too. The adapter already does this, so we're idempotent.
        return model_id.strip().lower().replace(".", "-")

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
        if not canonical or canonical == "kiro-auto":
            return None
        if canonical.startswith("claude-") or canonical.startswith("anthropic/"):
            target = canonical.split("/", 1)[1] if "/" in canonical else canonical
            return self._anthropic.rates_for(self._anthropic.canonicalize(target))
        if canonical.startswith("gpt-") or canonical.startswith("openai/"):
            target = canonical.split("/", 1)[1] if "/" in canonical else canonical
            return self._openai.rates_for(self._openai.canonicalize(target))
        return None

    def supports_per_message_tokens(self) -> bool:
        # Tokens are estimated from content length, not measured.
        return False
