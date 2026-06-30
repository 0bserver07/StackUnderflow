"""Public cost API — a thin shim over ``infra/providers/`` pricers.

Keeps the same ``compute_cost(tokens, model, provider="anthropic")``
signature every other module already calls, plus the back-compat helpers
(``format_dollars``, ``get_dynamic_pricing``, ``get_model_pricing``,
``RATE_CARD``). The actual pricing logic now lives in pluggable
``ProviderPricer`` modules — see ``docs/specs/multi-provider/spec.md`` §2.
"""

from __future__ import annotations

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

# NOTE: this list feeds ``RATE_CARD``, whose membership is what the ETL
# normalizers use to stamp ``cost_source`` (``rate_card`` vs ``unknown``).
# For Anthropic/GLM, model identity + rates now live in the data manifest
# (``stackunderflow/data/models.toml``); the Anthropic ids below duplicate the
# manifest's families and must be kept in sync until the two are unified
# (planned with the DB-backed registry). Adding a Claude model = a manifest
# entry AND an id here.
_CANONICAL_IDS = [
    # Anthropic — current Fable / Opus / Sonnet / Haiku
    "claude-fable-5",
    "claude-opus-4-8", "claude-opus-4-7",
    "claude-opus-4-6", "claude-sonnet-4-6",
    "claude-opus-4-5-20251101", "claude-sonnet-4-5-20250929", "claude-haiku-4-5-20251001",
    "claude-opus-4-20250514", "claude-sonnet-4-20250514",
    "claude-3-5-sonnet-20241022", "claude-3-5-haiku-20241022",
    "claude-3-opus-20240229", "claude-3-sonnet-20240229", "claude-3-haiku-20240307",
    # Un-dated Anthropic aliases — emitted by adapters that normalize
    # vendor-shape model ids (e.g. Kiro's ``claude.3.5.sonnet`` →
    # ``claude-3-5-sonnet``). AnthropicPricer resolves these via the data
    # manifest's per-family ``match`` tokens (stackunderflow/data/models.toml).
    "claude-3-5-sonnet",
    # ZhipuAI GLM models surfaced behind a Claude-shape proxy; the manifest
    # routes them to dedicated GLM_5 / GLM_51 family rates.
    "glm-5", "glm-5.1",
    # OpenAI Codex + base GPT families
    "gpt-5-codex", "gpt-5.2-codex", "gpt-5.3-codex",
    "gpt-5.4", "gpt-5", "gpt-5-mini",
    "gpt-4o", "gpt-4o-mini", "gpt-4.1",
    # Qwen (Alibaba DashScope) — rates in ``providers/qwen.py``
    "qwen-max", "qwen-max-longcontext",
    "qwen-plus", "qwen-turbo",
    "qwen-coder", "qwen-coder-plus", "qwen3-coder",
    "qwen-auto",
    # Gemini (Google AI for Developers) — rates in ``providers/gemini.py``
    "gemini-2.5-pro", "gemini-2.5-flash", "gemini-2.5-flash-lite",
    "gemini-1.5-pro", "gemini-1.5-flash",
    "gemini-3.0-pro", "gemini-3.1-pro",
    # Preview ids the Gemini CLI emits in the wild today.
    "gemini-3-pro-preview", "gemini-3.1-pro-preview", "gemini-3-flash-preview",
    "gemini-auto",
    # Cursor's own composer line + autoselector defaults — rates in
    # ``providers/cursor.py``.
    "composer-1", "composer-2",
    "cursor-auto", "cursor-fast",
    # Droid (Factory) / Cline auto-defaults — peg to Sonnet 4.x rates in
    # their respective pricers when the concrete model isn't known.
    "droid-auto", "cline-auto",
]


def _provider_for_model(model: str) -> str:
    """Best-effort guess for the legacy single-arg helpers below.

    Routing rules (case-insensitive on the lowered id):

    * ``qwen-*``    → qwen pricer
    * ``gemini-*``  → gemini pricer
    * ``codex-*`` / contains ``gpt`` or ``codex`` → openai pricer
    * ``composer-*`` / ``cursor-*`` → cursor pricer
    * ``droid-auto`` → droid pricer
    * ``cline-auto`` → cline pricer
    * ``glm-*`` → anthropic pricer (consumed through Anthropic-shape proxy)
    * ``claude-*``  → anthropic pricer (also the default fallback)
    """
    lowered = model.lower()
    if lowered.startswith("qwen") or lowered == "qwen-auto":
        return "qwen"
    if lowered.startswith("gemini") or lowered == "gemini-auto":
        return "gemini"
    if lowered.startswith("composer-") or lowered.startswith("cursor-"):
        return "cursor"
    if lowered == "droid-auto":
        return "droid"
    if lowered == "cline-auto":
        return "cline"
    if "claude" in lowered or lowered.startswith("glm-"):
        return "anthropic"
    if "gpt" in lowered or "codex" in lowered:
        return "openai"
    return "anthropic"


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
]
