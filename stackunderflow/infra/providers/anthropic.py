"""Anthropic pricer.

Owns the ``claude-*`` model heuristics, the standard 4-tier (input / output /
cache-write / cache-read) rate table, and a no-op ``normalize_tokens`` —
Anthropic's API shape is the canonical shape used elsewhere in the codebase.

The matching logic was moved verbatim out of ``infra/costs.py`` during the
multi-provider work (spec §2). Existing rates are preserved.
"""

from __future__ import annotations

from enum import Enum, auto

from .base import ProviderPricer


class _Family(Enum):
    OPUS_46 = auto()
    SONNET_46 = auto()
    OPUS_45 = auto()
    SONNET_45 = auto()
    HAIKU_45 = auto()
    OPUS_4 = auto()
    SONNET_4 = auto()
    SONNET_35 = auto()
    HAIKU_35 = auto()
    OPUS_3 = auto()
    SONNET_3 = auto()
    HAIKU_3 = auto()


# (input $/M, output $/M, cache-write $/M, cache-read $/M).
# cache-write at 1.25× input is the Anthropic billing convention.
_RATES: dict[_Family, tuple[float, float, float, float]] = {
    _Family.OPUS_46:   (15.0,  75.0,  18.75, 1.50),
    _Family.SONNET_46: (3.0,   15.0,  3.75,  0.30),
    _Family.OPUS_45:   (15.0,  75.0,  18.75, 1.50),
    _Family.SONNET_45: (3.0,   15.0,  3.75,  0.30),
    _Family.HAIKU_45:  (1.0,   5.0,   1.25,  0.10),
    _Family.OPUS_4:    (15.0,  75.0,  18.75, 1.50),
    _Family.SONNET_4:  (3.0,   15.0,  3.75,  0.30),
    _Family.SONNET_35: (3.0,   15.0,  3.75,  0.30),
    _Family.HAIKU_35:  (1.0,   5.0,   1.25,  0.10),
    _Family.OPUS_3:    (15.0,  75.0,  18.75, 1.50),
    _Family.SONNET_3:  (3.0,   15.0,  3.75,  0.30),
    _Family.HAIKU_3:   (0.25,  1.25,  0.30,  0.03),
}

_FALLBACK = _Family.SONNET_35


class AnthropicPricer(ProviderPricer):
    provider_name = "anthropic"

    def canonicalize(self, model_id: str) -> str:
        """Resolve a Claude model id to its family enum name.

        Returns a stable string keyed off the family enum so the registry
        can compare canonical ids across pricers without leaking the
        ``_Family`` type.
        """
        return self._identify(model_id).name

    def normalize_tokens(self, raw: dict[str, int]) -> dict[str, int]:
        """Anthropic shape == canonical shape; no-op."""
        return {
            "input": int(raw.get("input", 0) or 0),
            "output": int(raw.get("output", 0) or 0),
            "cache_creation": int(raw.get("cache_creation", 0) or 0),
            "cache_read": int(raw.get("cache_read", 0) or 0),
        }

    def rates_for(
        self, canonical: str
    ) -> tuple[float, float, float, float] | None:
        try:
            fam = _Family[canonical]
        except KeyError:
            return _RATES[_FALLBACK]
        return _RATES.get(fam, _RATES[_FALLBACK])

    def supports_per_message_tokens(self) -> bool:
        return True

    # ── internals ────────────────────────────────────────────────────

    @staticmethod
    def _identify(model_id: str) -> _Family:
        """Token-set heuristic over hyphen-split model ids — no regex."""
        if not model_id:
            return _FALLBACK
        parts = set(model_id.lower().replace(".", "-").split("-"))

        has_opus = "opus" in parts
        has_sonnet = "sonnet" in parts
        has_haiku = "haiku" in parts

        if "6" in parts and "4" in parts:
            if has_opus:
                return _Family.OPUS_46
            if has_sonnet:
                return _Family.SONNET_46
        if "5" in parts and "4" in parts:
            if has_opus:
                return _Family.OPUS_45
            if has_sonnet:
                return _Family.SONNET_45
            if has_haiku:
                return _Family.HAIKU_45
        if "4" in parts:
            if has_opus:
                return _Family.OPUS_4
            if has_sonnet:
                return _Family.SONNET_4
        if "5" in parts and "3" in parts:
            if has_sonnet:
                return _Family.SONNET_35
            if has_haiku:
                return _Family.HAIKU_35
        if "3" in parts:
            if has_opus:
                return _Family.OPUS_3
            if has_sonnet:
                return _Family.SONNET_3
            if has_haiku:
                return _Family.HAIKU_3

        return _FALLBACK
