"""Cursor provider pricer.

Cursor itself has no public per-token rate card — users pay a flat
subscription and the IDE multiplexes Anthropic / OpenAI / xAI / Google
models behind the scenes. For Claude-via-Cursor sessions (the common
case in real vscdb data) we delegate ``rates_for`` to ``AnthropicPricer``
so cost numbers stay sensible. Cursor-specific rates can later be added
to ``_CURSOR_RATES`` when there's a defensible source.

``supports_per_message_tokens()`` returns ``False`` because the vscdb
stores estimated counts at the bubble level only — the aggregator must
skip per-message cost for Cursor records and rely on session-level
totals when available (spec §2.5).

``normalize_tokens`` is a no-op; the adapter already emits the
canonical 4-key shape (see ``stackunderflow/adapters/cursor.py``).
"""

from __future__ import annotations

from .anthropic import AnthropicPricer
from .base import ProviderPricer

# Reserved for future Cursor-specific rate overrides keyed by canonical
# model id. Empty today — Cursor's pricing is opaque, so we delegate to
# Anthropic's rate card for Claude-family models and return None
# otherwise.
_CURSOR_RATES: dict[str, tuple[float, float, float, float]] = {}


class CursorPricer(ProviderPricer):
    provider_name = "cursor"

    def __init__(self) -> None:
        # Delegate target for Claude-via-Cursor sessions. Reusing the
        # singleton-grade behaviour of ``AnthropicPricer`` keeps rates
        # consistent with native ``claude`` records.
        self._anthropic = AnthropicPricer()

    def canonicalize(self, model_id: str) -> str:
        """Return a stable canonical id.

        Claude family (``claude-*``) passes through unchanged so the
        downstream Anthropic delegation can match the family enum. Other
        ids are normalised by stripping a leading ``cursor-`` prefix —
        ``cursor-auto`` becomes ``auto`` so a future rate-table entry can
        key off the bare label.
        """
        if not isinstance(model_id, str) or not model_id:
            return ""
        lowered = model_id.strip().lower()
        if lowered.startswith("claude-"):
            return lowered
        if lowered.startswith("cursor-"):
            return lowered[len("cursor-") :]
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
        """Return the rate tuple for ``canonical`` or ``None`` on miss.

        Lookup order:
          1. ``_CURSOR_RATES`` (currently empty — placeholder for future
             Cursor-specific overrides).
          2. Delegate to ``AnthropicPricer.rates_for`` when ``canonical``
             looks like a Claude model id. This gives Claude-via-Cursor
             sessions the same dollar values as native Claude records.
          3. ``None`` for everything else (e.g. ``cursor-auto``,
             ``gpt-*`` without a cursor-installed mapping yet).
        """
        if canonical in _CURSOR_RATES:
            return _CURSOR_RATES[canonical]
        if isinstance(canonical, str) and canonical.startswith("claude-"):
            return self._anthropic.rates_for(self._anthropic.canonicalize(canonical))
        return None

    def supports_per_message_tokens(self) -> bool:
        """Cursor's per-bubble token counts are estimates / often zero."""
        return False
