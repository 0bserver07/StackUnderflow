"""Pi (and OMP) pricer.

Pi/OMP default to ``gpt-5`` and the on-disk format mirrors OpenAI's
canonical token shape (``input``/``output``/``cacheRead``/``cacheWrite``).
We route everything through ``OpenAIPricer`` — that's where the gpt-5
rate card lives, and the family heuristic falls back to ``GPT_5_CODEX``
for unknown ids.

``normalize_tokens`` is a no-op; the adapter already emits the
canonical 4-key shape.

Spec §3 (multi-provider).
"""

from __future__ import annotations

from .base import ProviderPricer
from .openai import OpenAIPricer


class PiPricer(ProviderPricer):
    provider_name = "pi"
    provider_aliases = ("omp",)  # Pi/OMP share rates

    def __init__(self) -> None:
        self._openai = OpenAIPricer()

    def canonicalize(self, model_id: str) -> str:
        if not isinstance(model_id, str) or not model_id:
            # Default for Pi/OMP is gpt-5.
            return self._openai.canonicalize("gpt-5")
        return self._openai.canonicalize(model_id)

    def normalize_tokens(self, raw: dict[str, int]) -> dict[str, int]:
        # Adapter pre-normalises into canonical shape.
        return {
            "input": int(raw.get("input", 0) or 0),
            "output": int(raw.get("output", 0) or 0),
            "cache_creation": int(raw.get("cache_creation", 0) or 0),
            "cache_read": int(raw.get("cache_read", 0) or 0),
        }

    def rates_for(
        self, canonical: str
    ) -> tuple[float, float, float, float] | None:
        return self._openai.rates_for(canonical)

    def supports_per_message_tokens(self) -> bool:
        return True
