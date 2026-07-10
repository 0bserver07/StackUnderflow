"""Cursor provider pricer.

Cursor itself has no public per-token rate card — users pay a flat
subscription and the IDE multiplexes Anthropic / OpenAI / Google /
Cursor-trained models behind the scenes. Real Cursor session data shows
three distinct classes of model id, and each class needs different
pricing logic:

1. **Vendor-prefixed ids** — ``claude-4.5-sonnet-thinking``,
   ``claude-4.6-sonnet``, ``gpt-5-codex``, ``gpt-4o``,
   ``gemini-2.5-pro-preview-05-06``, ``gemini-3-pro``, …
   Cursor proxies the underlying model and bills against the upstream
   provider's rate card. We delegate to ``AnthropicPricer`` /
   ``OpenAIPricer`` / ``GeminiPricer`` accordingly.

2. **Cursor's own composer line** — ``composer-1``, ``composer-2``.
   These are Cursor-trained agents. Cursor doesn't publish per-token
   pricing for them as of 2026-04, so the rates here are **ESTIMATED**
   at Anthropic Sonnet 4.x level (input $3/M, output $15/M, cache-write
   $3.75/M, cache-read $0.30/M) — the closest publicly-acknowledged
   pricing analogue for an agentic Sonnet-class model. Records priced
   against this estimate stay flagged as ``cost_source="estimated"``
   via the cursor adapter's len/4 token heuristic, so the dashboard
   renders the ≈ marker.

3. **Aliases / autoselectors** — ``cursor-auto``, ``cursor-fast``.
   When Cursor picks the model for the user we don't know which engine
   actually ran. We use the same Sonnet-level rates as the composer
   line so cost figures reflect a defensible average rather than $0;
   marked ESTIMATED in the rate table comments.

Token shape: ``CursorAdapter`` already emits canonical 4-key tokens
(input / output / cache_creation / cache_read), so ``normalize_tokens``
is a no-op.

``supports_per_message_tokens()`` still returns ``False`` because the
vscdb stores estimated counts at the bubble level only — the aggregator
must skip per-message cost for Cursor records and rely on session-level
totals when available (spec §2.5).

Spec: ``docs/specs/multi-provider/spec.md`` §2 / §3.1.
"""

from __future__ import annotations

from .anthropic import AnthropicPricer
from .base import ProviderPricer
from .gemini import GeminiPricer
from .openai import OpenAIPricer

# (input $/M, output $/M, cache-write $/M, cache-read $/M).
#
# Sonnet 4.x tier — Anthropic's published rate card for the
# Sonnet-class model that's the closest analogue for Cursor's
# Cursor-trained / autoselected models. ESTIMATED for cursor-native
# ids; not a Cursor-published number. Update when Cursor publishes
# definitive per-token pricing.
_SONNET_TIER: tuple[float, float, float, float] = (3.0, 15.0, 3.75, 0.30)

# Composer 1 — Cursor's published rate for the original Composer model
# (Cursor models & pricing page, retrieved 2026-05-13): $1.25/M input,
# $10.00/M output. Cache-write/cache-read multipliers match Anthropic's
# convention since the underlying Composer 1 caches behave per the
# Anthropic shape. Cite: cursor.com/docs/models-and-pricing.
_COMPOSER_1_TIER: tuple[float, float, float, float] = (1.25, 10.00, 1.5625, 0.125)

# Cursor-specific rate overrides keyed by canonical (lower-cased)
# model id. composer-1 has its own published number; composer-2 and the
# auto/fast selectors stay at the Sonnet-tier ESTIMATE — Cursor hasn't
# published a definitive per-token rate for those yet.
_CURSOR_RATES: dict[str, tuple[float, float, float, float]] = {
    "composer-1": _COMPOSER_1_TIER,  # Published — see _COMPOSER_1_TIER source note
    "composer-2": _SONNET_TIER,      # ESTIMATED — Cursor-trained, no public rate card
    # ``canonicalize`` strips the ``cursor-`` prefix, so the bare label is
    # what ``rates_for`` actually sees. Keep both forms so a future caller
    # that bypasses canonicalize still hits a row.
    "cursor-auto": _SONNET_TIER,  # ESTIMATED — autoselector fallback
    "cursor-fast": _SONNET_TIER,  # ESTIMATED — autoselector fallback
    "auto": _SONNET_TIER,         # ESTIMATED — bare-label form
    "fast": _SONNET_TIER,         # ESTIMATED — bare-label form
}


class CursorPricer(ProviderPricer):
    provider_name = "cursor"
    model_id_prefixes = ("composer-", "cursor-")

    def __init__(self) -> None:
        # Delegate targets for vendor-prefixed model ids. Reusing the
        # singleton-grade behaviour of each pricer keeps rates consistent
        # with native records from the same upstream provider.
        self._anthropic = AnthropicPricer()
        self._openai = OpenAIPricer()
        self._gemini = GeminiPricer()

    def canonicalize(self, model_id: str) -> str:
        """Return a stable canonical id.

        Vendor-prefixed families (``claude-*``, ``gpt-*``, ``gemini-*``,
        ``codex*``) pass through lower-cased so the downstream delegation
        can match the upstream pricer's heuristic. Composer / cursor-*
        ids also pass through unchanged so they hit the
        ``_CURSOR_RATES`` table directly. ``cursor-`` prefix is
        stripped only for unambiguous autoselector ids
        (``cursor-auto`` → ``auto``) so the rate table can be keyed on
        the bare label.
        """
        if not isinstance(model_id, str) or not model_id:
            return ""
        lowered = model_id.strip().lower()
        # Vendor-prefixed: pass through untouched — the delegate decides.
        if lowered.startswith(("claude-", "gpt-", "gemini-", "codex")):
            return lowered
        # Cursor's own composer line: pass through.
        if lowered.startswith("composer-"):
            return lowered
        # Cursor autoselectors: keep the full label so callers can match
        # both ``cursor-auto`` and the bare ``auto`` (the rate table
        # carries both keys for safety).
        if lowered.startswith("cursor-"):
            return lowered
        return lowered

    def normalize_tokens(self, raw: dict[str, int]) -> dict[str, int]:
        """No-op. The adapter already emits the canonical 4-key shape."""
        return {
            "input": int(raw.get("input", 0) or 0),
            "output": int(raw.get("output", 0) or 0),
            "cache_creation": int(raw.get("cache_creation", 0) or 0),
            "cache_read": int(raw.get("cache_read", 0) or 0),
        }

    def rates_for(
        self, canonical: str
    ) -> tuple[float, float, float, float] | None:
        """Return the rate tuple for ``canonical`` or fall back.

        Lookup order:
          1. ``_CURSOR_RATES`` direct hit (composer-*, cursor-auto,
             cursor-fast, auto, fast — all ESTIMATED at Sonnet-tier).
          2. ``claude-*`` → delegate to ``AnthropicPricer``.
          3. ``gpt-*`` / ``codex*`` → delegate to ``OpenAIPricer``.
          4. ``gemini-*`` → delegate to ``GeminiPricer``; if Gemini
             returns ``None`` (e.g. dated preview suffixes like
             ``gemini-2.5-pro-preview-05-06``) we strip the suffix and
             retry against the base id.
          5. Unknown ids fall back to the Sonnet-tier rate so cursor
             records never silently price at $0. This mirrors the
             cursor-auto / composer-* policy: we'd rather show an
             ESTIMATED dollar figure than zero out 944+ messages.
        """
        if not isinstance(canonical, str) or not canonical:
            return None

        # 1. Direct cursor-native hit.
        if canonical in _CURSOR_RATES:
            return _CURSOR_RATES[canonical]

        # 2-4. Vendor delegation.
        if canonical.startswith("claude-"):
            return self._anthropic.rates_for(
                self._anthropic.canonicalize(canonical)
            )
        if canonical.startswith("gpt-") or canonical.startswith("codex"):
            return self._openai.rates_for(
                self._openai.canonicalize(canonical)
            )
        if canonical.startswith("gemini-"):
            rates = self._gemini.rates_for(
                self._gemini.canonicalize(canonical)
            )
            if rates is not None:
                return rates
            # Gemini ids commonly carry a ``-preview-MM-DD`` or
            # ``-experimental`` suffix that doesn't appear in the
            # static rate table. Strip back to the base ``gemini-X.Y-…``
            # token and retry.
            base = _strip_gemini_suffix(canonical)
            if base != canonical:
                rates = self._gemini.rates_for(self._gemini.canonicalize(base))
                if rates is not None:
                    return rates
            # Last resort for an unknown Gemini id — Sonnet-tier
            # estimate keeps the dollar figure non-zero. The
            # ``cost_source="estimated"`` flag on the underlying record
            # already signals approximation.
            return _SONNET_TIER

        # 5. Unknown id — Sonnet-tier estimate so the record contributes
        # *something* to compare/cost reports.
        return _SONNET_TIER

    def supports_per_message_tokens(self) -> bool:
        """Cursor's per-bubble token counts are estimates / often zero."""
        return False


def _strip_gemini_suffix(model_id: str) -> str:
    """Trim trailing ``-preview-…`` / ``-experimental`` / dated tails.

    ``gemini-2.5-pro-preview-05-06`` → ``gemini-2.5-pro``
    ``gemini-2.5-flash-experimental`` → ``gemini-2.5-flash``
    Any id that doesn't carry a recognisable suffix is returned
    unchanged, so this is safe to call unconditionally.
    """
    for marker in ("-preview-", "-experimental"):
        idx = model_id.find(marker)
        if idx != -1:
            return model_id[:idx]
    return model_id
