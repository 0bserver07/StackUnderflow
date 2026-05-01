"""Droid (Factory) pricer.

Droid is a thin wrapper around real upstream models — most often Claude
via Anthropic's API. The settings file records the model id directly
(``claude-3-5-sonnet``, ``gpt-4o-mini``, etc.), so we route by name:

- ``claude-*``  → AnthropicPricer
- ``gpt-*`` / OpenAI shape → OpenAIPricer
- everything else → ``None`` (cost layer surfaces "no rate available")

Token shape is canonical Anthropic-style — the adapter has already
distributed session totals across assistant messages. ``normalize_tokens``
is therefore a no-op.

Spec §3 (multi-provider).
"""

from __future__ import annotations

from .anthropic import AnthropicPricer
from .base import ProviderPricer
from .openai import OpenAIPricer


class DroidPricer(ProviderPricer):
    provider_name = "droid"

    def __init__(self) -> None:
        self._anthropic = AnthropicPricer()
        self._openai = OpenAIPricer()

    def canonicalize(self, model_id: str) -> str:
        """Lower-case and pass through; the routing happens in ``rates_for``."""
        if not isinstance(model_id, str):
            return ""
        return model_id.strip().lower()

    def normalize_tokens(self, raw: dict[str, int]) -> dict[str, int]:
        """Adapter has already distributed session totals; no-op here."""
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
        if canonical.startswith("claude-"):
            return self._anthropic.rates_for(self._anthropic.canonicalize(canonical))
        if canonical.startswith("gpt-") or "codex" in canonical:
            return self._openai.rates_for(self._openai.canonicalize(canonical))
        # Unknown vendor — let the cost layer flag rather than misprice.
        return None

    def supports_per_message_tokens(self) -> bool:
        # Adapter distributes session totals across assistant messages,
        # so each Record carries a per-message slice. Treat as
        # per-message for the cost layer.
        return True
