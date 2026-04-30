"""Provider pricer ABC.

Each pluggable pricer owns one provider's model heuristics, rates table, and
token-normalization logic. Adapters emit raw provider-shape tokens and
``compute_cost()`` routes through the right pricer per record.

See ``docs/specs/multi-provider/spec.md`` §2.
"""

from __future__ import annotations

from abc import ABC, abstractmethod

_MILLION = 1_000_000.0


class ProviderPricer(ABC):
    """Each provider implements this contract once."""

    provider_name: str

    @abstractmethod
    def canonicalize(self, model_id: str) -> str:
        """Resolve a free-form model string to a stable canonical identifier.

        The canonical id is the key into ``rates_for()``. Heuristics are
        provider-specific (e.g. Anthropic splits on hyphens; OpenAI checks
        for ``codex`` / ``gpt`` tokens).
        """

    @abstractmethod
    def normalize_tokens(self, raw: dict[str, int]) -> dict[str, int]:
        """Return tokens shaped for ``compute()``.

        Output keys: ``input``, ``output``, ``cache_creation``, ``cache_read``.
        Anthropic shape is canonical, so its pricer is a no-op. OpenAI's
        pricer subtracts cached-input tokens from raw input here so callers
        can pass adapter-emitted shape directly.
        """

    @abstractmethod
    def rates_for(
        self, canonical: str
    ) -> tuple[float, float, float, float] | None:
        """Return ``(input, output, cache_write, cache_read)`` in $/M tokens.

        ``None`` means "this canonical id is unknown to me." The registry
        falls back to Anthropic for unknown providers; individual pricers
        may apply their own internal fallback rules and never return None.
        """

    @abstractmethod
    def supports_per_message_tokens(self) -> bool:
        """``True`` when the underlying source emits per-message token usage.

        Cursor returns False — the vscdb stores estimated counts at the
        bubble level only and the aggregator must skip per-message cost
        for those records (relying on session totals instead).
        """

    # ── shared helper ────────────────────────────────────────────────

    def compute(self, tokens: dict[str, int], model: str) -> dict[str, float]:
        """Return cost breakdown — ``tokens × rates_for(canonicalize(model))``.

        Tokens must already be in canonical shape (input / output /
        cache_creation / cache_read) — call ``normalize_tokens()`` first if
        the caller has the raw provider shape.
        """
        canonical = self.canonicalize(model)
        rates = self.rates_for(canonical)
        return self._apply_overlay_rates(tokens, rates)

    @staticmethod
    def _apply_overlay_rates(
        tokens: dict[str, int],
        rates: tuple[float, float, float, float] | None,
    ) -> dict[str, float]:
        """Apply an explicit (input, output, cache_write, cache_read) rate
        tuple to ``tokens``. Used both as the compute helper and as the
        seam ``infra/costs.py`` uses to inject PricingService overlay
        rates without re-routing through ``rates_for()``.
        """
        if rates is None:
            return {
                "input_cost": 0.0,
                "output_cost": 0.0,
                "cache_creation_cost": 0.0,
                "cache_read_cost": 0.0,
                "total_cost": 0.0,
            }
        inp_r, out_r, cw_r, cr_r = rates
        ic = tokens.get("input", 0) * inp_r / _MILLION
        oc = tokens.get("output", 0) * out_r / _MILLION
        cc = tokens.get("cache_creation", 0) * cw_r / _MILLION
        rc = tokens.get("cache_read", 0) * cr_r / _MILLION
        return {
            "input_cost": ic,
            "output_cost": oc,
            "cache_creation_cost": cc,
            "cache_read_cost": rc,
            "total_cost": ic + oc + cc + rc,
        }
