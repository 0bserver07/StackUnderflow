"""Anthropic model billing.

Pricing is stored in a flat table indexed by model-family enum values.
Model IDs are resolved to families by splitting on hyphens and matching
known tokens — no regex involved.
"""

from __future__ import annotations

import logging
from enum import Enum, auto
from typing import Any

_log = logging.getLogger(__name__)


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


# (input $/M, output $/M, cache-write $/M, cache-read $/M)
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
_MILLION = 1_000_000.0


def _identify(model_id: str) -> _Family:
    """Resolve a model ID string to its pricing family.

    Works by splitting on hyphens and checking for known version + tier
    tokens.  No regex needed — just set membership tests.
    """
    parts = set(model_id.lower().replace(".", "-").split("-"))

    has_opus = "opus" in parts
    has_sonnet = "sonnet" in parts
    has_haiku = "haiku" in parts

    # version detection by checking for version-specific tokens
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


def _get_rates(family: _Family) -> tuple[float, float, float, float]:
    return _RATES.get(family, _RATES[_FALLBACK])


# ── dynamic pricing overlay ──────────────────────────────────────────────────

_overlay: dict[_Family, tuple[float, float, float, float]] | None = None


def _load_overlay() -> dict[_Family, tuple[float, float, float, float]]:
    global _overlay
    if _overlay is not None:
        return _overlay
    try:
        from stackunderflow.services.pricing_service import PricingService
        raw = PricingService().get_pricing().get("pricing", {})
        merged: dict[_Family, tuple[float, float, float, float]] = {}
        for mid, vals in raw.items():
            fam = _identify(mid)
            if fam not in merged:
                merged[fam] = (
                    vals.get("input_cost_per_token", 0) * _MILLION,
                    vals.get("output_cost_per_token", 0) * _MILLION,
                    vals.get("cache_creation_cost_per_token", 0) * _MILLION,
                    vals.get("cache_read_cost_per_token", 0) * _MILLION,
                )
        _overlay = {**_RATES, **merged}
    except Exception:
        _overlay = _RATES
    return _overlay


def _effective(model_id: str) -> tuple[float, float, float, float]:
    """Return (input, output, cache_write, cache_read) in $/M tokens."""
    overlay = _load_overlay()
    fam = _identify(model_id)
    return overlay.get(fam, _RATES[_FALLBACK])


# ── public API ───────────────────────────────────────────────────────────────

def compute_cost(tokens: dict[str, int], model: str) -> dict[str, float]:
    """Return cost breakdown.  Rates are $/M so we divide token counts by 1M."""
    inp_r, out_r, cw_r, cr_r = _effective(model)
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


def format_dollars(amount: float) -> str:
    magnitude = abs(amount)
    if magnitude >= 100:
        return f"${amount:,.0f}"
    if magnitude >= 1:
        return f"${amount:,.2f}"
    if magnitude >= 0.01:
        return f"${amount:.3f}"
    return f"${amount:.4f}"


# ── compat shims ─────────────────────────────────────────────────────────────

_CANONICAL_IDS = [
    "claude-opus-4-6", "claude-sonnet-4-6",
    "claude-opus-4-5-20251101", "claude-sonnet-4-5-20250929", "claude-haiku-4-5-20251001",
    "claude-opus-4-20250514", "claude-sonnet-4-20250514",
    "claude-3-5-sonnet-20241022", "claude-3-5-haiku-20241022",
    "claude-3-opus-20240229", "claude-3-sonnet-20240229", "claude-3-haiku-20240307",
]


def get_dynamic_pricing() -> dict[str, Any]:
    return {mid: get_model_pricing(mid) for mid in _CANONICAL_IDS}


def get_model_pricing(model: str) -> dict[str, float] | None:
    i, o, cw, cr = _effective(model)
    return {
        "input_cost_per_token": i / _MILLION,
        "output_cost_per_token": o / _MILLION,
        "cache_creation_cost_per_token": cw / _MILLION,
        "cache_read_cost_per_token": cr / _MILLION,
    }


calculate_cost = compute_cost
format_cost = format_dollars
RATE_CARD = {mid: get_model_pricing(mid) for mid in _CANONICAL_IDS}
