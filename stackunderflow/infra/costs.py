"""Public cost API — a thin shim over ``infra/providers/`` pricers.

Keeps the same ``compute_cost(tokens, model, provider="anthropic")``
signature every other module already calls, plus the back-compat helpers
(``format_dollars``, ``get_dynamic_pricing``, ``get_model_pricing``,
``RATE_CARD``). The actual pricing logic now lives in pluggable
``ProviderPricer`` modules — see ``docs/specs/multi-provider/spec.md`` §2.
"""

from __future__ import annotations

from functools import lru_cache

from stackunderflow.infra.model_manifest import (
    canonical_id_groups as _manifest_canonical_id_groups,
    canonical_ids as _manifest_canonical_ids,
    clear_manifest_caches as _clear_manifest_caches,
)

from typing import Any

from .providers import get_pricer
from .providers.anthropic import AnthropicPricer
from .providers.base import ProviderPricer
from .providers.openai import OpenAIPricer

_MILLION = 1_000_000.0


# ── alias resolution ─────────────────────────────────────────────────────────

def resolve_model_alias(model_id: str, aliases: dict[str, str]) -> str:
    """Map ``model_id`` through a user-provided alias table.

    Returns the canonical id when ``model_id`` appears as a key, otherwise
    the input unchanged. Single-step lookup only — no recursive chasing,
    so a self-alias (``foo`` → ``foo``) terminates trivially and a
    misconfigured chain (``a`` → ``b`` → ``c``) returns ``b`` rather than
    iterating. Empty / non-dict input is treated as an empty map.
    """
    if not aliases or not isinstance(aliases, dict):
        return model_id
    return aliases.get(model_id, model_id)


def _user_aliases() -> dict[str, str]:
    """Load the user's alias map from settings.

    Defensive: any error reading settings (corrupt file, unexpected types,
    import failure during early bootstrap) falls back to an empty map so
    cost computation never raises just because aliases are misconfigured.
    """
    try:
        from stackunderflow.settings import Settings
        raw = Settings().get("model_aliases", {})
    except Exception:  # noqa: S110 - settings I/O errors must not break pricing
        return {}
    if not isinstance(raw, dict):
        return {}
    # Only string→string entries are usable; silently drop the rest.
    return {
        str(k): str(v)
        for k, v in raw.items()
        if isinstance(k, str) and isinstance(v, str)
    }


# ── public API ───────────────────────────────────────────────────────────────

def compute_cost(
    tokens: dict[str, int],
    model: str,
    provider: str = "anthropic",
    *,
    speed: str = "standard",
    at_ts: str | None = None,
) -> dict[str, float]:
    """Return cost breakdown.

    ``provider`` defaults to ``"anthropic"`` so every existing call site
    still works unchanged. Tokens are normalised by the provider's pricer
    first (a no-op for Anthropic, the cached-input subtraction for
    OpenAI), then priced.

    ``speed`` lets callers thread Anthropic's priority/fast tier flag
    through to the pricer (only the Anthropic pricer interprets it
    today; everywhere else it's a no-op). Pass ``"fast"`` for Claude
    records whose ``message.usage.service_tier == "priority"`` — the
    Anthropic pricer applies a 6× multiplier to input + output rates
    for Opus models in that case. See
    ``ClaudeAdapter._parse_line`` for detection.

    A user-configured alias map (``settings.model_aliases``) is consulted
    first so proxy-rewritten model ids (e.g. ``openrouter/claude-opus``)
    resolve to a canonical id our rate tables know about. See
    ``docs/cli-reference.md`` for CLI usage.

    A PricingService overlay (if initialised) takes precedence over the
    hardcoded rates — preserves the pre-refactor behaviour of letting
    LiteLLM upstream override the canonical rate card. Note: overlay
    rates are not multiplied by the speed flag because the overlay
    table is upstream-authoritative; users who want fast-tier overlay
    pricing should provide separate entries.

    ``at_ts`` (ISO date string) prices the event at the manifest rate in
    effect at that time (effective-dated price rows). It applies only to
    manifest-priced models — the overlay is a single current snapshot with
    no history, so ``at_ts`` is ignored for overlay-covered models.
    """
    model = resolve_model_alias(model, _user_aliases())

    pricer = get_pricer(provider)
    normalized = pricer.normalize_tokens(tokens)

    overlay = _overlay_rates(model)
    if overlay is not None:
        # Overlay (LiteLLM feed) is a single current snapshot with no
        # effective-dated history, so ``at_ts`` does not apply here.
        return ProviderPricer._apply_overlay_rates(normalized, overlay)

    # Unified price book (store-backed, opt-in). When a store is wired
    # (``model_manifest.use_price_book_store``) and carries a matching row,
    # it is the source — at the SAME precedence the manifest path has here
    # (after the overlay). A miss returns None and falls through to the
    # in-code pricer below, so a fresh store prices identically to today.
    book = _price_book_rates(model, provider, at_ts)
    if book is not None:
        if speed == "fast":
            book = _apply_fast_multiplier(book, model, provider)
        return ProviderPricer._apply_overlay_rates(normalized, book)
    return pricer.compute(normalized, model, speed=speed, at_ts=at_ts)


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

# The canonical-id list feeds ``RATE_CARD``, whose membership is what the
# ETL normalizers use to stamp ``cost_source`` (``rate_card`` vs ``unknown``).
# It lives in the data manifest (``stackunderflow/data/models.toml``,
# ``[canonical_ids]``) with every other piece of model identity — adding a
# model id is a manifest edit, never a change to this module.
_CANONICAL_IDS = list(_manifest_canonical_ids())


@lru_cache(maxsize=1)
def _exact_id_routing() -> dict[str, str]:
    """id → pricer key, from ``models.toml [canonical_ids]`` — each group
    name IS the pricer key (the manifest states this contract)."""
    return {
        mid: pricer
        for pricer, ids in _manifest_canonical_id_groups().items()
        for mid in ids
    }


@lru_cache(maxsize=1)
def _hint_routing() -> tuple[tuple[str, str, bool], ...]:
    """``(hint, pricer_key, is_prefix)`` rules from each pricer's own
    ``model_id_prefixes`` / ``model_id_substrings`` declarations, sorted
    longest-hint-first (prefix outranks substring at equal length) so the
    most specific rule wins regardless of registration order."""
    from stackunderflow.infra.providers import registered_pricers

    rules: list[tuple[str, str, bool]] = []
    seen: set[int] = set()
    for pricer in registered_pricers().values():
        if id(pricer) in seen:  # aliases share singletons
            continue
        seen.add(id(pricer))
        key = pricer.provider_name
        for hint in pricer.model_id_prefixes:
            rules.append((hint, key, True))
        for hint in pricer.model_id_substrings:
            rules.append((hint, key, False))
    rules.sort(key=lambda r: (-len(r[0]), not r[2], r[0], r[1]))
    return tuple(rules)


def clear_pricing_caches() -> None:
    """Invalidate manifest + routing caches (tests patching ``models.toml``
    that also exercise cost routing call this). CAVEAT: ``_CANONICAL_IDS``
    and ``RATE_CARD`` are built once at import and are NOT refreshed here —
    a test needing patched RATE_CARD membership must rebind those itself.
    """
    _clear_manifest_caches()
    _exact_id_routing.cache_clear()
    _hint_routing.cache_clear()


def vendor_for_model(model: str) -> str | None:
    """Pricer key the model id itself *claims*, or ``None`` for no match.

    Same routing as :func:`_provider_for_model` minus the conservative
    ``anthropic`` default:

    1. exact id from ``models.toml [canonical_ids]`` (group = pricer key);
    2. the pricers' own declared prefix/substring hints, longest first;
    3. ``None`` — "no pricer claims this id".

    The ``None`` is the point. ``_provider_for_model`` answers "who should I
    ask?" and any answer beats raising; callers deciding whether to *override*
    a recorded provider need "did anything actually match?", because
    ``deepseek-v4-flash-free`` matching nothing must not be mistaken for
    ``deepseek-v4-flash-free`` being an Anthropic model.
    """
    lowered = (model or "").strip().lower()
    if not lowered:
        return None
    exact = _exact_id_routing().get(lowered)
    if exact:
        return exact
    for hint, key, is_prefix in _hint_routing():
        if lowered.startswith(hint) if is_prefix else hint in lowered:
            return key
    return None


def _provider_for_model(model: str) -> str:
    """Model-id → pricer key, defaulting to ``anthropic``.

    Thin wrapper over :func:`vendor_for_model`; the fallback matches
    ``get_pricer``'s so an unrouted id still prices rather than raising.
    """
    return vendor_for_model(model) or "anthropic"


def resolve_pricing_provider(provider: str | None, model: str) -> str:
    """Pricer key for a *stored* ``(provider, model)`` pair.

    ``projects.provider`` records which TOOL wrote the transcript (``claude``,
    ``codex``, ``pi``, ``opencode``, …), not whose rate card applies. Most
    shells delegate per model id, but the terminal single-vendor pricers do
    not: ``AnthropicPricer`` resolves any unknown id to its manifest fallback
    family and ``OpenAIPricer`` to ``GPT_5_CODEX``, so a foreign model priced
    through them is silently billed at the wrong card — e.g. a ``pi`` project
    logging ``claude-opus-4-7`` billed at $1.25/$10 instead of $5/$25.

    The model id is the ground truth about which rate card applies, so a
    *definite* model→vendor match wins. Three deliberate exceptions keep the
    override from doing harm:

    * no definite match (``vendor_for_model`` → ``None``) — the recorded
      provider stands, so an unrecognised id keeps its shell's verdict
      (including a shell's honest ``None`` → $0) instead of being re-routed
      into Anthropic's fallback and invented into existence;
    * the vendor pricer can't price the id but the shell can — never trade a
      real number for "I don't know" (Cursor's dated Gemini previews);
    * both agree on the rate — the shell already delegates correctly, so the
      recorded provider is kept and behaviour is bit-for-bit unchanged.
    """
    vendor = vendor_for_model(model)
    if vendor is None:
        return provider or "anthropic"
    if not provider:
        return vendor
    shell = get_pricer(provider)
    upstream = get_pricer(vendor)
    if shell is upstream:
        return provider
    upstream_rates = upstream.rates_for(upstream.canonicalize(model))
    if upstream_rates is None:
        return provider
    shell_rates = shell.rates_for(shell.canonicalize(model))
    if shell_rates == upstream_rates:
        return provider
    return vendor


_overlay_cache: dict[str, tuple[float, float, float, float]] | None = None


def _load_overlay() -> dict[str, tuple[float, float, float, float]]:
    """Load the live LiteLLM-style pricing overlay, lazily and once.

    Mirrors the pre-refactor ``_load_overlay`` — when a PricingService has
    been initialised, its per-model dollar figures take precedence over the
    hardcoded rate table. Cached at module level after first successful
    load; failures fall back to an empty dict so subsequent calls are cheap.
    """
    global _overlay_cache
    if _overlay_cache is not None:
        return _overlay_cache
    out: dict[str, tuple[float, float, float, float]] = {}
    try:
        from stackunderflow.services.pricing_service import PricingService
        raw = PricingService().get_pricing().get("pricing", {})
        for mid, entry in raw.items():
            out[mid] = (
                float(entry.get("input_cost_per_token", 0)) * _MILLION,
                float(entry.get("output_cost_per_token", 0)) * _MILLION,
                float(entry.get("cache_creation_cost_per_token", 0)) * _MILLION,
                float(entry.get("cache_read_cost_per_token", 0)) * _MILLION,
            )
    except Exception:
        pass
    _overlay_cache = out
    return out


def _overlay_rates(model: str) -> tuple[float, float, float, float] | None:
    return _load_overlay().get(model)


# ── unified price book seam ──────────────────────────────────────────────────

def _price_book_rates(
    model: str, provider: str, at_ts: str | None
) -> tuple[float, float, float, float] | None:
    """Resolve $/M rates from the store-backed price book, or ``None``.

    Routes the *pricer-side* provider key (Anthropic for claude/GLM/cursor,
    etc.) — the same key ``model_manifest`` keyed the rows under during
    backfill — so a manifest-family lookup resolves. Returns ``None`` when the
    store isn't wired or the model is absent so ``compute_cost`` falls through
    to the in-code manifest. Never raises.
    """
    try:
        from .model_manifest import store_price_book_lookup
        return store_price_book_lookup(model, _provider_for_model(model), at_ts)
    except Exception:  # noqa: BLE001 — book lookup must never break pricing
        return None


def _apply_fast_multiplier(
    rates: tuple[float, float, float, float], model: str, provider: str
) -> tuple[float, float, float, float]:
    """Fold Anthropic's priority/fast input+output multiplier into book rates.

    The book stores standard rates; the fast premium is a manifest concept the
    in-code Anthropic pricer applies after rate lookup. Mirror that here so a
    book hit for a ``speed='fast'`` Opus record bills identically to the
    in-code path. No-op when the family has no ``fast_multiplier``.
    """
    try:
        from .model_manifest import canonicalize, fast_multiplier
        pkey = _provider_for_model(model)
        mult = fast_multiplier(canonicalize(model, pkey), pkey)
    except Exception:  # noqa: BLE001
        mult = None
    if not mult:
        return rates
    inp_r, out_r, cw_r, cr_r = rates
    return (inp_r * mult, out_r * mult, cw_r, cr_r)


def backfill_price_book(conn) -> int:
    """Populate the ``price_book`` table from the manifest + RATE_CARD.

    The manifest's effective-dated family rows map directly
    (``model_manifest.backfill_price_book``); on top of them this stamps every
    concrete ``_CANONICAL_IDS`` id at its current resolved rate
    (``source='rate_card'``) so the per-id lookup tier covers non-manifest
    providers (openai/qwen/gemini/…). Idempotent (UPSERT); returns rows written.
    """
    from .model_manifest import backfill_price_book as _manifest_backfill

    rate_card_rows: list[dict] = []
    for mid in _CANONICAL_IDS:
        pricing = get_model_pricing(mid)
        if not pricing:
            continue
        rate_card_rows.append(
            {
                "provider": _provider_for_model(mid),
                "model": mid,
                "effective_from": "",
                "effective_until": "",
                "input": pricing["input_cost_per_token"] * _MILLION,
                "output": pricing["output_cost_per_token"] * _MILLION,
                "cache_write": pricing["cache_creation_cost_per_token"] * _MILLION,
                "cache_read": pricing["cache_read_cost_per_token"] * _MILLION,
                "source": "rate_card",
            }
        )
    return _manifest_backfill(conn, rate_card_rows)


def get_model_pricing(model: str) -> dict[str, float] | None:
    model = resolve_model_alias(model, _user_aliases())
    overlay = _overlay_rates(model)
    if overlay is not None:
        i, o, cw, cr = overlay
    else:
        pricer = get_pricer(_provider_for_model(model))
        rates = pricer.rates_for(pricer.canonicalize(model))
        if rates is None:
            return None
        i, o, cw, cr = rates
    return {
        "input_cost_per_token": i / _MILLION,
        "output_cost_per_token": o / _MILLION,
        "cache_creation_cost_per_token": cw / _MILLION,
        "cache_read_cost_per_token": cr / _MILLION,
    }


def get_dynamic_pricing() -> dict[str, Any]:
    return {mid: get_model_pricing(mid) for mid in _CANONICAL_IDS}


RATE_CARD = {mid: get_model_pricing(mid) for mid in _CANONICAL_IDS}


# ── read-only introspection (pricing doctor) ─────────────────────────────────

def is_rate_card_model(model: str) -> bool:
    """True when *model* has an exact entry in :data:`RATE_CARD`.

    This is the same membership test every normalizer uses to decide
    ``cost_source`` (``rate_card`` when the id is present, ``unknown``
    otherwise — see ``etl/normalize/*``): the provider pricers fall back
    to a default family for unrecognised ids, so ``get_model_pricing``
    would never return ``None``, and exact ``RATE_CARD`` membership is the
    only honest "we actually know this model" signal. Read-only — used by
    ``pricing doctor`` to flag models with no resolvable rate card.
    """
    return bool(model) and model in RATE_CARD


def estimate_cost(tokens: dict[str, int], model: str) -> float:
    """Best-effort would-be cost (USD) for *tokens* priced at *model*'s rate.

    Routes through :func:`compute_cost` with the model-name provider
    heuristic so an unpriced row's dollar exposure can be quantified —
    ``pricing doctor`` reports this as the delta a resolvable rate would
    add (an ``unknown`` row stores ``cost_usd`` 0.0, so the delta is the
    full conservative fallback-priced figure). Read-only and never raises:
    returns ``0.0`` when no rate resolves or pricing errors. ``tokens``
    uses the canonical 4-key shape (``input`` / ``output`` /
    ``cache_creation`` / ``cache_read``).
    """
    try:
        breakdown = compute_cost(tokens, model, provider=_provider_for_model(model))
    except Exception:  # noqa: BLE001 — introspection must never raise
        return 0.0
    return float(breakdown.get("total_cost", 0.0) or 0.0)


# Re-export the pricer classes for tests / advanced callers.
__all__ = [
    "AnthropicPricer",
    "OpenAIPricer",
    "RATE_CARD",
    "backfill_price_book",
    "compute_cost",
    "estimate_cost",
    "format_dollars",
    "get_dynamic_pricing",
    "get_model_pricing",
    "is_rate_card_model",
    "resolve_model_alias",
    "resolve_pricing_provider",
    "vendor_for_model",
]
