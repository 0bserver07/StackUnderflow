"""GitHub Copilot pricer.

Copilot multiplexes Anthropic and OpenAI behind its own UI / billing.
The adapter has already done the heavy lifting (tool-call-id-prefix
inference + explicit ``model`` extraction), so this pricer's job is
limited to vendor-prefix routing — the same shape used by ``ClinePricer``:

  - ``claude-*``  → ``AnthropicPricer``
  - ``gpt-*``     → ``OpenAIPricer``
  - anything else → ``None`` (no rate available)

We don't price ``copilot-auto`` against any real rate card because we
don't know which upstream model produced that record — silently routing
through Anthropic would invent dollars the user never spent.

``supports_per_message_tokens`` returns ``True`` *for the registry-level
contract*, but individual records may still have ``cost_source ==
"estimated"`` set by the adapter when the JSONL event lacks an explicit
token count. The aggregator already special-cases that flag at the
record level.

Spec: ``docs/specs/multi-provider/spec.md`` §2; codeburn-catalog §3.
"""

from __future__ import annotations

from .anthropic import AnthropicPricer
from .base import ProviderPricer
from .openai import OpenAIPricer


class CopilotPricer(ProviderPricer):
    provider_name = "copilot"

    def __init__(self) -> None:
        self._anthropic = AnthropicPricer()
        self._openai = OpenAIPricer()

    def canonicalize(self, model_id: str) -> str:
        """Pass the model id through unchanged.

        ``rates_for`` does the prefix split itself; preserving the raw
        string keeps the routing signal intact.
        """
        return model_id or ""

    def normalize_tokens(self, raw: dict[str, int]) -> dict[str, int]:
        """No-op — the adapter already emits the canonical 4-key shape."""
        return {
            "input": int(raw.get("input", 0) or 0),
            "output": int(raw.get("output", 0) or 0),
            "cache_creation": int(raw.get("cache_creation", 0) or 0),
            "cache_read": int(raw.get("cache_read", 0) or 0),
        }

    def rates_for(
        self, canonical: str
    ) -> tuple[float, float, float, float] | None:
        """Delegate to the upstream provider's pricer based on prefix.

        Recognised prefixes (case-insensitive):
          - ``claude-*`` (or ``anthropic/...``) → ``AnthropicPricer``
          - ``gpt-*`` (or ``openai/...``)       → ``OpenAIPricer``

        Any other id (notably ``copilot-auto``) returns ``None`` —
        the cost layer should treat that as "no rate available".
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

        return None

    def supports_per_message_tokens(self) -> bool:
        # JSONL events carry per-call output tokens. ``cost_source ==
        # "estimated"`` on individual records gates fallback estimation.
        return True
