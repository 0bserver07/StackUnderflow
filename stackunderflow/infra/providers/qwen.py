"""Qwen pricer.

Qwen Code (the CLI) routes through Alibaba's DashScope / Qwen API. Public
list-price pages have moved several times; the rate table here is the
best-available estimate at the time of writing and is documented as such.
Unknown models return ``None`` so the cost layer surfaces "no rate
available" rather than mispricing against an arbitrary table.

Rate provenance (recorded for future audit):

* ``qwen-max`` / ``qwen-plus`` / ``qwen-turbo`` — pulled from the public
  DashScope "tokens per million" table at the time this rate table
  was authored. These are USD estimates against the published CNY rates;
  the absolute numbers are conservative placeholders rather than
  contractual values. Treat them as "directionally right, not authoritative."
* ``qwen-auto`` — the adapter's default when the entry has no ``model``
  field. We map it to ``qwen-plus`` rates (the Qwen Code default tier).

Token shape: ``QwenAdapter`` already flattens ``usageMetadata`` into the
canonical 4-key shape (cached subtracted from input, thoughts folded
into output) so ``normalize_tokens`` here is a no-op — same pattern as
``ClinePricer`` / ``CursorPricer``.

Spec: ``docs/specs/multi-provider/spec.md`` §2.
"""

from __future__ import annotations

from .base import ProviderPricer

# (input $/M, output $/M, cache-write $/M, cache-read $/M).
#
# Qwen surfaces cached input only — there is no separate cache-write
# event in the data we ingest, so cache-write stays at 0.0 for every row
# (matches the Codex / OpenAI convention). cache-read is set at ~10% of
# the input rate, mirroring the OpenAI cached-input discount; if Qwen's
# real discount differs the table can be updated in one place.
_RATES: dict[str, tuple[float, float, float, float]] = {
    # qwen-max — flagship; estimate from DashScope public list price
    "qwen-max":     (3.00, 12.00, 0.0, 0.30),
    "qwen-max-longcontext": (3.00, 12.00, 0.0, 0.30),
    # qwen-plus — mid-tier; the Qwen Code default
    "qwen-plus":    (1.20, 3.60,  0.0, 0.12),
    # qwen-turbo — fast tier
    "qwen-turbo":   (0.30, 0.60,  0.0, 0.03),
    # qwen-coder family — coding-tuned variants
    "qwen-coder":      (1.20, 3.60, 0.0, 0.12),
    "qwen-coder-plus": (1.20, 3.60, 0.0, 0.12),
    "qwen3-coder":     (1.20, 3.60, 0.0, 0.12),
    # Adapter default when ``entry.model`` is absent
    "qwen-auto":    (1.20, 3.60,  0.0, 0.12),
}


class QwenPricer(ProviderPricer):
    provider_name = "qwen"

    def canonicalize(self, model_id: str) -> str:
        """Lower-case the model id; otherwise pass through.

        Qwen ids are already short and stable (``qwen-max``, ``qwen-plus``,
        ``qwen-turbo``, ``qwen3-coder`` …) so we don't need a heuristic
        family enum. Lower-casing protects against ``QWEN-MAX`` style
        emissions from upstream tooling.
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

        Unknown ids return ``None`` rather than falling back to a
        guess — the cost layer will mark the record as "no rate
        available" instead of inventing dollars. New Qwen model
        names should be added to ``_RATES`` with a documented source.
        """
        if not canonical:
            return None
        return _RATES.get(canonical)

    def supports_per_message_tokens(self) -> bool:
        # ``usageMetadata`` is emitted on every assistant entry.
        return True
