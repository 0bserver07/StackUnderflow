"""Gemini pricer.

Gemini Code (the CLI) calls Google's Gemini API. Rate provenance for
the table below — recorded so future audits can spot drift:

* ``gemini-2.5-pro`` / ``gemini-2.5-flash`` — Google AI for Developers
  list price at the time codeburn-catalog §7 was authored. Pricing is
  tier-based above 200K input tokens; the simple table here uses the
  short-context tier (≤200K input). If long-context billing matters
  to a downstream report, extend ``rates_for`` to inspect the canonical
  id for a long-context variant.
* ``gemini-2.5-flash-lite`` — derived from the published flash-lite
  short-context tier.
* ``gemini-3.1-pro`` (placeholder) — listed in codeburn's ``models.ts``
  forward-looking entries; rates are estimates pegged to ``gemini-2.5-pro``
  until Google publishes definitive numbers. Documented as such here.
* ``gemini-auto`` — the adapter's default when a message omits
  ``model``. Maps to ``gemini-2.5-pro`` rates (the Gemini CLI default).

Token shape: ``GeminiAdapter`` already flattens ``tokens.{input, output,
cached, thoughts}`` into the canonical 4-key shape (cached subtracted
from input, thoughts folded into output) so ``normalize_tokens`` here
is a no-op — same pattern as ``ClinePricer`` / ``QwenPricer``.

Cache-write stays at 0.0 for every row: Gemini's implicit caching does
not surface a separate write event in the on-disk data, mirroring the
OpenAI / Qwen convention. cache-read is set at ~25% of input — Google's
documented "implicit cache discount" range — and can be tuned per-row
when the API publishes per-model values.

Spec: ``docs/specs/multi-provider/spec.md`` §2; codeburn-catalog §7.
"""

from __future__ import annotations

from .base import ProviderPricer


# (input $/M, output $/M, cache-write $/M, cache-read $/M).
_RATES: dict[str, tuple[float, float, float, float]] = {
    # Gemini 2.5 family (current production, ≤200K input tier)
    "gemini-2.5-pro":         (1.25,  10.00, 0.0, 0.31),
    "gemini-2.5-flash":       (0.30,  2.50,  0.0, 0.075),
    "gemini-2.5-flash-lite":  (0.10,  0.40,  0.0, 0.025),
    # Gemini 1.5 family (legacy but still queryable)
    "gemini-1.5-pro":         (1.25,  5.00,  0.0, 0.3125),
    "gemini-1.5-flash":       (0.075, 0.30,  0.0, 0.01875),
    # Forward-looking placeholders — rates pegged to 2.5-pro until
    # Google publishes definitive numbers (see module docstring).
    "gemini-3.1-pro":         (1.25,  10.00, 0.0, 0.31),
    "gemini-3.0-pro":         (1.25,  10.00, 0.0, 0.31),
    # Adapter default
    "gemini-auto":            (1.25,  10.00, 0.0, 0.31),
}


class GeminiPricer(ProviderPricer):
    provider_name = "gemini"

    def canonicalize(self, model_id: str) -> str:
        """Lower-case and pass through.

        Gemini ids are stable strings (``gemini-2.5-pro``,
        ``gemini-2.5-flash``, …) so a heuristic family enum is overkill.
        Lower-casing protects against capitalisation drift in the
        upstream emitter.
        """
        if not isinstance(model_id, str) or not model_id:
            return ""
        return model_id.strip().lower()

    def normalize_tokens(self, raw: dict[str, int]) -> dict[str, int]:
        """Adapter pre-normalises tokens; this is a no-op."""
        return {
            "input": int(raw.get("input", 0) or 0),
            "output": int(raw.get("output", 0) or 0),
            "cache_creation": int(raw.get("cache_creation", 0) or 0),
            "cache_read": int(raw.get("cache_read", 0) or 0),
        }

    def rates_for(
        self, canonical: str
    ) -> tuple[float, float, float, float] | None:
        """Return the rate tuple for ``canonical`` or ``None`` on miss.

        Unknowns return ``None`` so the cost layer surfaces "no rate
        available" instead of inventing dollars. New Gemini model ids
        should be added to ``_RATES`` with a documented source.
        """
        if not canonical:
            return None
        return _RATES.get(canonical)

    def supports_per_message_tokens(self) -> bool:
        # The ``tokens`` block is emitted on every assistant message.
        return True
