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
    OPUS_47 = auto()
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
    # ZhipuAI GLM models surfaced behind a Claude-shape proxy (provider=claude
    # in our store). Routed through this pricer because the wire format is
    # Anthropic-compatible; rates differ per ZhipuAI's published numbers.
    GLM_5 = auto()
    GLM_51 = auto()


# (input $/M, output $/M, cache-write $/M, cache-read $/M).
# cache-write at 1.25× input is the Anthropic billing convention.
#
# Opus 4.7 — Anthropic's May 2026 list price is $5/$25 with $6.25 5m-cache
# write and $0.50 cache read. Source: platform.claude.com/docs/en/about-claude/pricing
# (the same /MTok rates apply across the full 1M-token context window per
# Anthropic's long-context pricing note — no separate -1m variant).
_RATES: dict[_Family, tuple[float, float, float, float]] = {
    _Family.OPUS_47:   (5.0,   25.0,  6.25,  0.50),
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
    # GLM-5 — ZhipuAI / Z.ai published rate as of May 2026:
    # $1.00 / $3.20 per MTok input / output. Cache-write / cache-read
    # match Anthropic's 1.25× / 0.10× convention since GLM is consumed
    # through Anthropic-shape proxies in our store and the proxy applies
    # the same cache discount multipliers when surfacing usage. Source:
    # docs.z.ai/guides/overview/pricing (retrieved 2026-05-13).
    _Family.GLM_5:     (1.00,  3.20,  1.25,  0.10),
    # GLM-5.1 — ZhipuAI list price as of May 2026: $1.40 / $4.40 per MTok
    # input / output (source: docs.z.ai/guides/overview/pricing).
    _Family.GLM_51:    (1.40,  4.40,  1.75,  0.14),
}

_FALLBACK = _Family.SONNET_35

# Opus families — the only Claude tier that bills the priority/fast rate
# at 6× standard input + 6× standard output. Sonnet/Haiku priority access
# does not change the published $/M rates today, so we only multiply for
# these families. Cache-write/cache-read rates stay at the standard tier
# because Anthropic charges cache traffic at the same rate regardless of
# service_tier.
_OPUS_FAMILIES: frozenset[_Family] = frozenset({
    _Family.OPUS_47,
    _Family.OPUS_46,
    _Family.OPUS_45,
    _Family.OPUS_4,
    _Family.OPUS_3,
})

# 6× input + 6× output, cache rates unchanged. The ratio is an Anthropic-
# published figure (priority tier costs ~6× more for Opus models). When the
# API rate-card changes we update this constant in one place.
_FAST_OPUS_MULTIPLIER: float = 6.0


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

    # ── tier-aware compute override ──────────────────────────────────

    def compute(
        self,
        tokens: dict[str, int],
        model: str,
        *,
        speed: str = "standard",
    ) -> dict[str, float]:
        """Apply Opus fast-mode 6× multiplier when ``speed == "fast"``.

        Only Opus families get the multiplier — Sonnet/Haiku priority
        access doesn't change published rates per Anthropic's docs.
        Unknown families fall back to standard rates × 1 even when
        ``speed="fast"`` is set, so a misclassified record never gets
        silently overcharged.
        """
        canonical = self.canonicalize(model)
        rates = self.rates_for(canonical)
        if speed == "fast" and rates is not None:
            try:
                fam = _Family[canonical]
            except KeyError:
                fam = _FALLBACK
            if fam in _OPUS_FAMILIES:
                inp_r, out_r, cw_r, cr_r = rates
                rates = (
                    inp_r * _FAST_OPUS_MULTIPLIER,
                    out_r * _FAST_OPUS_MULTIPLIER,
                    cw_r,
                    cr_r,
                )
        return self._apply_overlay_rates(tokens, rates)

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
        has_glm = "glm" in parts

        # ZhipuAI GLM models — checked first so they don't fall through to
        # the Claude family heuristic and get mispriced at Sonnet rates.
        # Order matters: 5.1 token-split contains both "5" and "1"; the
        # narrower match wins.
        if has_glm:
            if "5" in parts and "1" in parts:
                return _Family.GLM_51
            if "5" in parts:
                return _Family.GLM_5

        # Opus 4.7 — narrower than the bare "4 + opus" rule below, so it
        # MUST come first. The token-split for "claude-opus-4-7" gives
        # parts {"claude", "opus", "4", "7"}; the Opus 4 branch would
        # otherwise swallow it with the old (15/75) rates.
        if "7" in parts and "4" in parts and has_opus:
            return _Family.OPUS_47
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
