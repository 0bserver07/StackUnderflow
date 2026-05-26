"""Antigravity pricer.

Antigravity is Google's IDE + CLI that runs Gemini models server-side.
Per-message token data is held in encrypted ``*.pb`` files we cannot
decrypt today (see ``stackunderflow/adapters/antigravity.py`` for the
encryption story). The adapter emits zero-token Records with
``raw["cost_source"] = "encrypted"`` so the cost layer can render an
explicit "tokens unavailable" rather than guessing dollars.

Rates here mirror ``GeminiPricer`` — when reverse-engineering unlocks
the cleartext we can swap zero tokens for real numbers without
touching this file (the rate table already covers the Antigravity
defaults: ``gemini-3-pro-preview`` and friends).
"""

from __future__ import annotations

from .base import ProviderPricer
from .gemini import GeminiPricer


class AntigravityPricer(ProviderPricer):
    provider_name = "antigravity"

    def __init__(self) -> None:
        # Delegate to GeminiPricer's rate table. We don't subclass — the
        # `provider_name` distinction matters for ``get_pricer`` routing.
        self._delegate = GeminiPricer()

    def canonicalize(self, model_id: str) -> str:
        return self._delegate.canonicalize(model_id)

    def normalize_tokens(self, raw: dict[str, int]) -> dict[str, int]:
        return self._delegate.normalize_tokens(raw)

    def rates_for(
        self, canonical: str
    ) -> tuple[float, float, float, float] | None:
        return self._delegate.rates_for(canonical)

    def supports_per_message_tokens(self) -> bool:
        # The Adapter emits zero tokens (content is encrypted), so
        # there's nothing per-message to price. The cost layer should
        # see total_cost = 0 and use the "tokens unavailable" badge.
        return False
