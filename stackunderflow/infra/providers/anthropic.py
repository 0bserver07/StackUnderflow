"""Anthropic pricer — manifest-backed.

Model identity and rates are no longer hardcoded here. They live in the data
manifest (``stackunderflow/data/models.toml``), loaded via
``infra/model_manifest.py``. A new model or a price change is a manifest edit,
not a code change — and pricing is effective-dated, so historical events can
be priced at the rate that was in effect when they ran.

This module keeps the ``ProviderPricer`` contract and delegates identity +
rates to the manifest. ``normalize_tokens`` stays here because Anthropic's
wire shape *is* the canonical shape (a no-op coercion). ZhipuAI GLM models are
surfaced through an Anthropic-shape proxy (stored as ``provider=claude``) and
are priced here too — see their entries in the manifest.
"""

from __future__ import annotations

from ..model_manifest import (
    canonicalize as manifest_canonicalize,
)
from ..model_manifest import (
    fast_multiplier as manifest_fast_multiplier,
)
from ..model_manifest import (
    rates_for as manifest_rates_for,
)
from .base import ProviderPricer


class AnthropicPricer(ProviderPricer):
    provider_name = "anthropic"

    def canonicalize(self, model_id: str) -> str:
        """Resolve a Claude/GLM model id to its manifest family key.

        Pure data lookup: the manifest declares each family's match tokens
        (most-specific first) and the fallback family. Unknown ids resolve to
        the fallback (Sonnet 3.5), so this never returns ``None`` for Anthropic.
        """
        return manifest_canonicalize(model_id, self.provider_name)

    def normalize_tokens(self, raw: dict[str, int]) -> dict[str, int]:
        """Anthropic shape == canonical shape; coerce to the 4 keys."""
        return {
            "input": int(raw.get("input", 0) or 0),
            "output": int(raw.get("output", 0) or 0),
            "cache_creation": int(raw.get("cache_creation", 0) or 0),
            "cache_read": int(raw.get("cache_read", 0) or 0),
        }

    def rates_for(
        self, canonical: str
    ) -> tuple[float, float, float, float] | None:
        """Return ``(input, output, cache_write, cache_read)`` $/M from the
        manifest. Unknown families resolve to the manifest fallback."""
        return manifest_rates_for(canonical, self.provider_name)

    def supports_per_message_tokens(self) -> bool:
        return True

    # ── tier-aware compute override ──────────────────────────────────

    def compute(
        self,
        tokens: dict[str, int],
        model: str,
        *,
        speed: str = "standard",
        at_ts: str | None = None,
    ) -> dict[str, float]:
        """Apply the priority/fast multiplier when ``speed == "fast"``.

        The multiplier (Opus bills ~6× on input + output; cache rates
        unchanged) is now a per-model ``fast_multiplier`` field in the
        manifest rather than a hardcoded family set. A model with no
        ``fast_multiplier`` is billed at standard rates even when
        ``speed="fast"``, so a misclassified record is never overcharged.
        """
        canonical = self.canonicalize(model)
        rates = manifest_rates_for(canonical, self.provider_name, at_ts=at_ts)
        if speed == "fast" and rates is not None:
            mult = manifest_fast_multiplier(canonical, self.provider_name)
            if mult:
                inp_r, out_r, cw_r, cr_r = rates
                rates = (inp_r * mult, out_r * mult, cw_r, cr_r)
        return self._apply_overlay_rates(tokens, rates)
