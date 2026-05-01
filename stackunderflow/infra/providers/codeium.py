"""Codeium pricer — stub.

The Codeium adapter is a discovery-only stub today (see
``stackunderflow/adapters/codeium.py``) and Codeium itself has no
public per-token rate card. This pricer is registered so
``get_pricer("codeium")`` returns a stable instance, but every
``rates_for`` call returns ``None`` and ``supports_per_message_tokens``
returns ``False``. The cost layer interprets that combination as "no
cost computable" and leaves the row's cost at zero.

Spec: ``docs/specs/multi-provider/local-inventory.md`` §8.
"""

from __future__ import annotations

from .base import ProviderPricer


class CodeiumPricer(ProviderPricer):
    provider_name = "codeium"

    def canonicalize(self, model_id: str) -> str:
        """Pass through unchanged — there's no rate-table key to derive."""
        return model_id or ""

    def normalize_tokens(self, raw: dict[str, int]) -> dict[str, int]:
        """No-op token shape passthrough."""
        return {
            "input": int(raw.get("input", 0) or 0),
            "output": int(raw.get("output", 0) or 0),
            "cache_creation": int(raw.get("cache_creation", 0) or 0),
            "cache_read": int(raw.get("cache_read", 0) or 0),
        }

    def rates_for(
        self, canonical: str
    ) -> tuple[float, float, float, float] | None:
        """No published rate card — always ``None``."""
        return None

    def supports_per_message_tokens(self) -> bool:
        # Stub adapter yields no records. False is the conservative
        # answer if it ever does.
        return False
