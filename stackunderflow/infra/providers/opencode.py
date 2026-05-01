"""OpenCode provider pricer.

OpenCode is a thin orchestration layer — it doesn't run inference itself
and it doesn't bill per-token. Instead each ``message.data`` row carries
the upstream model id (``modelID``) plus an optional embedded ``cost``
field. Pricing strategy:

1. Inspect ``modelID`` and route to the matching real pricer:
   ``claude-*`` → ``AnthropicPricer``; ``gpt-*`` (also bare ``codex-*``)
   → ``OpenAIPricer``. Unknown families return ``None`` so the cost layer
   surfaces "no rate available" rather than mispricing against an
   arbitrary table.

2. The OpenCode adapter stamps any embedded ``cost`` value onto
   ``record.raw["embedded_cost"]``. Downstream consumers can compare it
   to the recomputed value as a parity check; ``cost_source`` is set to
   ``"embedded"`` when an embedded cost is present and the upstream
   pricer returned no rate (so the user sees *some* number).

Token shape: the adapter has already collapsed OpenCode's 5-key shape
(``input``, ``output``, ``reasoning``, ``cache.read``, ``cache.write``)
to the canonical 4-key shape, with reasoning folded into output. So
``normalize_tokens`` here is a no-op — same pattern as ``ClinePricer``.

codeburn-catalog.md §11.
"""

from __future__ import annotations

from .anthropic import AnthropicPricer
from .base import ProviderPricer
from .openai import OpenAIPricer


class OpenCodePricer(ProviderPricer):
    provider_name = "opencode"

    def __init__(self) -> None:
        self._anthropic = AnthropicPricer()
        self._openai = OpenAIPricer()

    def canonicalize(self, model_id: str) -> str:
        """Pass the model id through unchanged.

        Routing happens in ``rates_for`` based on the prefix; stripping
        anything here would lose the routing signal.
        """
        return model_id or ""

    def normalize_tokens(self, raw: dict[str, int]) -> dict[str, int]:
        """No-op. OpenCode adapter pre-emits the canonical 4-key shape."""
        return {
            "input": int(raw.get("input", 0) or 0),
            "output": int(raw.get("output", 0) or 0),
            "cache_creation": int(raw.get("cache_creation", 0) or 0),
            "cache_read": int(raw.get("cache_read", 0) or 0),
        }

    def rates_for(
        self, canonical: str
    ) -> tuple[float, float, float, float] | None:
        """Delegate to the matching upstream pricer based on model prefix.

        Recognised prefixes (case-insensitive):
        - bare ``claude-*`` → ``AnthropicPricer``
        - bare ``gpt-*`` or ``codex-*`` → ``OpenAIPricer``

        Anything else returns ``None``. The cost layer then has the
        option of falling back to the embedded ``cost`` field on the
        message (see module docstring).
        """
        if not canonical:
            return None

        lowered = canonical.lower()
        if lowered.startswith("claude-"):
            return self._anthropic.rates_for(self._anthropic.canonicalize(lowered))
        if lowered.startswith("gpt-") or lowered.startswith("codex-"):
            return self._openai.rates_for(self._openai.canonicalize(lowered))
        return None

    def supports_per_message_tokens(self) -> bool:
        """Per-message token counts are explicit in the OpenCode DB."""
        return True

    @staticmethod
    def has_embedded_cost(raw: dict) -> bool:
        """Helper for the cost layer.

        ``True`` when the adapter stamped an ``embedded_cost`` on
        ``record.raw``. Callers can prefer the embedded value when
        ``rates_for`` returned None.
        """
        return raw.get("embedded_cost") is not None
