"""Cline pricer.

Cline doesn't run inference itself — it delegates to a real upstream
provider (Anthropic, OpenAI, OpenRouter, etc.) and records the model used
as a vendor-prefixed string like ``anthropic/claude-3-5-sonnet`` or
``openai/gpt-4o-mini`` in the ``<model>`` tag on the first user message.

Pricing strategy: parse the vendor prefix and delegate ``rates_for`` to
the matching real pricer. Unknown vendors return ``None`` so the cost
layer marks the record as "no rate available" rather than mispricing
against Anthropic's table.

Token shape: Cline reports ``tokensIn / tokensOut / cacheWrites /
cacheReads`` per ``api_req_started`` event. The Cline adapter has
already mapped those to the canonical 4-key shape on the Record, so
``normalize_tokens`` here is a no-op.

Spec §3.2.
"""

from __future__ import annotations

from .anthropic import AnthropicPricer
from .base import ProviderPricer
from .openai import OpenAIPricer


class ClinePricer(ProviderPricer):
    provider_name = "cline"

    def __init__(self) -> None:
        self._anthropic = AnthropicPricer()
        self._openai = OpenAIPricer()

    def canonicalize(self, model_id: str) -> str:
        """Pass the model id through unchanged.

        The vendor prefix is preserved so ``rates_for`` can route to the
        right delegate. Stripping it here would lose the routing signal
        and force ``rates_for`` to redo the prefix split.
        """
        return model_id or ""

    def normalize_tokens(self, raw: dict[str, int]) -> dict[str, int]:
        """Cline events arrive in Anthropic shape after adapter mapping; no-op."""
        return {
            "input": int(raw.get("input", 0) or 0),
            "output": int(raw.get("output", 0) or 0),
            "cache_creation": int(raw.get("cache_creation", 0) or 0),
            "cache_read": int(raw.get("cache_read", 0) or 0),
        }

    def rates_for(
        self, canonical: str
    ) -> tuple[float, float, float, float] | None:
        """Delegate to the upstream provider's pricer.

        Recognised prefixes (case-insensitive):
        - ``anthropic/...`` or bare ``claude-...`` → ``AnthropicPricer``
        - ``openai/...`` or bare ``gpt-...`` → ``OpenAIPricer``

        Anything else returns ``None`` — the cost layer will leave the
        record's cost at 0 and we don't pretend to know the rate. This is
        intentional: silently routing ``ollama/llama-3`` through Anthropic
        would invent dollars the user never spent.
        """
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

        # ``cline-auto`` — the Cline adapter's default when the
        # ``<model>`` tag is missing from the first user message. Real
        # Cline tasks targeting that fallback typically end up on the
        # user's configured Anthropic key (per cline.bot's default
        # configuration), so peg the auto-selector to Sonnet 4.x rates
        # rather than leaving the dollar figure at $0. ESTIMATED in the
        # sense that we don't know the actual engine, but the rate is
        # Anthropic's published Sonnet number.
        if canonical == "cline-auto":
            return self._anthropic.rates_for(
                self._anthropic.canonicalize("claude-sonnet-4-5")
            )

        # Unknown vendor — let the caller observe a missing rate rather
        # than mispricing against an arbitrary table.
        return None

    def supports_per_message_tokens(self) -> bool:
        # api_req_started events carry per-call token usage.
        return True
