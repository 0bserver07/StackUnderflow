"""Data-driven model manifest: identity + effective-dated pricing.

The source of truth that replaces the hardcoded per-provider rate dicts and
the token-matching ``_identify`` ladders. Model facts — which id maps to
which family, what it costs, when that price was in effect — live in
``stackunderflow/data/models.toml``: data you edit, diff, and review, not
Python branches. Adding a model or correcting a price is a manifest edit.

Pricing is effective-dated. Each model carries one or more price rows with
optional ``effective_from`` / ``effective_until`` (ISO ``YYYY-MM-DD`` strings).
Pass ``at_ts`` to price a historical event at the rate in effect then; omit it
for the current rate. Rows with no dates are always-current.

The per-provider ``ProviderPricer`` classes keep their token-normalization
logic and delegate identity + rates here.
"""

from __future__ import annotations

import logging
import tomllib
from functools import lru_cache
from pathlib import Path

logger = logging.getLogger(__name__)

_MANIFEST_PATH = Path(__file__).resolve().parent.parent / "data" / "models.toml"

_REQUIRED_PRICE_FIELDS = ("input", "output", "cache_write", "cache_read")


def _valid_price_row(row: object) -> bool:
    if not isinstance(row, dict):
        return False
    return all(
        isinstance(row.get(f), int | float) and not isinstance(row.get(f), bool)
        for f in _REQUIRED_PRICE_FIELDS
    )


def _valid_model(entry: object) -> bool:
    """A usable model entry: a non-empty ``family`` plus a non-empty ``price``
    list whose every row carries numeric input/output/cache_write/cache_read.

    Used to drop malformed manifest entries at load time so they can never
    KeyError at lookup (where the error would be swallowed into a $0 cost).
    """
    if not isinstance(entry, dict):
        return False
    if not isinstance(entry.get("family"), str) or not entry.get("family"):
        return False
    prices = entry.get("price")
    if not isinstance(prices, list) or not prices:
        return False
    return all(_valid_price_row(p) for p in prices)


@lru_cache(maxsize=1)
def _models() -> list[dict]:
    """Parse, validate, and cache the manifest once.

    Order is preserved and load-bearing: ``canonicalize`` returns the first
    matching entry, so more-specific families must appear before broader ones.

    Malformed entries (missing ``family``, or a price row lacking numeric
    input/output/cache_write/cache_read) are DROPPED with a warning rather than
    left to KeyError at lookup time — a silent pricing failure (the ingest
    normalizer swallows exceptions → $0 cost) is worse than a loud, visible skip.
    """
    with open(_MANIFEST_PATH, "rb") as fh:
        data = tomllib.load(fh)
    valid: list[dict] = []
    for entry in data.get("model", []):
        if _valid_model(entry):
            valid.append(entry)
        else:
            fam = entry.get("family") if isinstance(entry, dict) else entry
            logger.warning("model_manifest: dropping malformed model entry %r", fam)
    return valid


def _for_provider(provider: str) -> list[dict]:
    return [m for m in _models() if m.get("provider") == provider]


def _by_family(provider: str) -> dict[str, dict]:
    return {m["family"]: m for m in _for_provider(provider)}


def _fallback_family(provider: str) -> str | None:
    for m in _for_provider(provider):
        if m.get("fallback"):
            return m["family"]
    return None


def canonicalize(model_id: str, provider: str = "anthropic") -> str | None:
    """Map a free-form model id to a manifest family key.

    Splits the id on ``-`` / ``.`` into a token set and returns the first
    entry (in manifest order) whose ``match`` tokens are all present. Falls
    back to the provider's ``fallback`` family when nothing matches.
    """
    fallback = _fallback_family(provider)
    if not model_id:
        return fallback
    parts = set(model_id.lower().replace(".", "-").split("-"))
    for entry in _for_provider(provider):
        match = entry.get("match") or []
        if match and set(match).issubset(parts):
            return entry["family"]
    return fallback


def _select_price(prices: list[dict], at_ts: str | None) -> dict | None:
    """Pick the price row effective at ``at_ts`` (ISO string), or the current
    one when ``at_ts`` is None. Rows without dates always apply."""
    if not prices:
        return None
    if at_ts is None:
        current = [p for p in prices if not p.get("effective_until")]
        return (current or prices)[-1]
    for p in prices:
        ef = p.get("effective_from")
        eu = p.get("effective_until")
        if (ef is None or at_ts >= ef) and (eu is None or at_ts < eu):
            return p
    return prices[-1]


def rates_for(
    canonical: str | None,
    provider: str = "anthropic",
    at_ts: str | None = None,
) -> tuple[float, float, float, float] | None:
    """Return ``(input, output, cache_write, cache_read)`` in $/M for a family.

    An unknown family resolves to the provider's fallback family, preserving
    the pre-manifest contract that the Anthropic pricer never returns None.
    ``at_ts`` selects the effective-dated row; omit for the current rate.
    """
    table = _by_family(provider)
    entry = table.get(canonical) if canonical else None
    if entry is None:
        fb = _fallback_family(provider)
        entry = table.get(fb) if fb else None
    if entry is None:
        return None
    price = _select_price(entry.get("price") or [], at_ts)
    if price is None:
        return None
    return (
        float(price["input"]),
        float(price["output"]),
        float(price["cache_write"]),
        float(price["cache_read"]),
    )


def fast_multiplier(canonical: str | None, provider: str = "anthropic") -> float | None:
    """Per-model input/output multiplier for the priority/fast tier (Opus
    bills ~6×). ``None`` when the model has no fast-tier premium."""
    entry = _by_family(provider).get(canonical) if canonical else None
    if entry is None:
        return None
    mult = entry.get("fast_multiplier")
    return float(mult) if mult else None
