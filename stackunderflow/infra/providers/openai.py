"""OpenAI pricer.

Owns ``gpt-*`` and ``codex`` model heuristics, the OpenAI-specific 4-tier
rate table (cache-write at 0.0× because OpenAI doesn't bill writes), and the
``cached_input_tokens`` subtraction that used to live in ``adapters/codex.py``.

Migrating ``normalize_tokens`` here means Codex (and any future OpenAI-shape
adapter) emits raw API tokens — cached nested in input, reasoning bundled
into output — and the pricer is the single place that flattens to canonical
shape. Spec §1.5 / §2.4.
"""

from __future__ import annotations

from enum import Enum, auto

from .base import ProviderPricer


class _Family(Enum):
    # OpenAI Codex variants
    GPT_5_CODEX = auto()
    GPT_52_CODEX = auto()
    GPT_53_CODEX = auto()
    # OpenAI base GPT families
    GPT_54 = auto()
    GPT_5 = auto()
    GPT_5_MINI = auto()
    GPT_4O = auto()
    GPT_4O_MINI = auto()
    GPT_41 = auto()


# (input $/M, output $/M, cache-write $/M, cache-read $/M).
# cache-write is 0 — OpenAI does not bill prompt-cache writes.
# cache-read is ~10% of input for Codex/gpt-5, ~50% for gpt-4o.
_RATES: dict[_Family, tuple[float, float, float, float]] = {
    _Family.GPT_5_CODEX:  (1.25,  10.0,  0.0,   0.125),
    _Family.GPT_52_CODEX: (1.25,  10.0,  0.0,   0.125),
    _Family.GPT_53_CODEX: (1.25,  10.0,  0.0,   0.125),
    _Family.GPT_54:       (2.50,  20.0,  0.0,   0.25),
    _Family.GPT_5:        (2.50,  20.0,  0.0,   0.25),
    _Family.GPT_5_MINI:   (0.25,  2.00,  0.0,   0.025),
    _Family.GPT_4O:       (2.50,  10.0,  0.0,   1.25),
    _Family.GPT_4O_MINI:  (0.15,  0.60,  0.0,   0.075),
    _Family.GPT_41:       (2.50,  10.0,  0.0,   0.625),
}

_FALLBACK = _Family.GPT_5_CODEX


def _safe_int(val: object) -> int:
    """Coerce a token count to a non-negative int; garbage → 0.

    ``normalize_tokens`` is the one pricer seam handed *raw provider
    JSON* at ingest time (Codex ``last_token_usage`` via
    ``adapters/codex.py`` and ``etl/normalize/codex.py``). A string,
    list, or ``1e999``-shaped (inf) value must degrade to 0 — raising
    here propagates out of the adapter's ``read()`` generator and aborts
    the whole ingest batch.
    """
    try:
        return max(int(val or 0), 0)
    except (TypeError, ValueError, OverflowError):
        return 0


class OpenAIPricer(ProviderPricer):
    provider_name = "openai"
    provider_aliases = ("codex",)  # Record.provider string that prices here
    model_id_substrings = ("gpt", "codex")

    def canonicalize(self, model_id: str) -> str:
        # Manifest first: a family declared in ``data/models.toml`` with
        # provider="openai" wins (identity + rates as DATA — new dotted
        # point-releases like gpt-5.5 need no code change). The in-code
        # ``_identify`` ladder remains the fallback for everything else.
        from stackunderflow.infra.model_manifest import canonicalize as _m_canon

        fam = _m_canon(model_id, provider="openai")
        if fam is not None:
            return fam
        return self._identify(model_id).name

    def normalize_tokens(self, raw: dict[str, int]) -> dict[str, int]:
        """Flatten OpenAI's raw token shape into canonical 4 keys.

        OpenAI embeds cached-input tokens inside ``input_tokens`` and bills
        reasoning under output. We:
          * subtract ``cached_input_tokens`` from raw input so the canonical
            ``input`` counts only fresh (uncached) input — matches Anthropic.
          * fold ``reasoning_output_tokens`` into ``output``.
          * map ``cached_input_tokens`` → ``cache_read``.
          * leave ``cache_creation`` at 0 (OpenAI does not bill writes).

        Accepts either provider-shape keys (``input_tokens``,
        ``output_tokens``, ``cached_input_tokens``,
        ``reasoning_output_tokens``) or already-canonical keys (in which
        case it's a no-op). This dual-shape tolerance keeps us safe during
        the adapter migration.
        """
        if "input_tokens" in raw or "cached_input_tokens" in raw:
            raw_input = _safe_int(raw.get("input_tokens", 0))
            cached = _safe_int(raw.get("cached_input_tokens", 0))
            raw_output = _safe_int(raw.get("output_tokens", 0))
            reasoning = _safe_int(raw.get("reasoning_output_tokens", 0))
            return {
                "input": max(raw_input - cached, 0),
                "output": raw_output + reasoning,
                "cache_creation": 0,
                "cache_read": cached,
            }
        return {
            "input": _safe_int(raw.get("input", 0)),
            "output": _safe_int(raw.get("output", 0)),
            "cache_creation": _safe_int(raw.get("cache_creation", 0)),
            "cache_read": _safe_int(raw.get("cache_read", 0)),
        }

    def rates_for(
        self, canonical: str
    ) -> tuple[float, float, float, float] | None:
        # Manifest first: a family declared in ``data/models.toml`` is
        # authoritative (it may share a name with the in-code enum — e.g.
        # GPT_54 — and the manifest's current row must win so rate
        # corrections are data edits). The in-code table remains the
        # fallback for families the manifest doesn't carry.
        from stackunderflow.infra.model_manifest import (
            rates_for as _m_rates,
        )

        manifest = _m_rates(canonical, provider="openai")
        if manifest is not None:
            return manifest
        try:
            fam = _Family[canonical]
        except KeyError:
            return _RATES[_FALLBACK]
        return _RATES.get(fam, _RATES[_FALLBACK])

    def supports_per_message_tokens(self) -> bool:
        return True

    # ── effective-dated compute override ─────────────────────────────

    def compute(
        self,
        tokens: dict[str, int],
        model: str,
        *,
        speed: str = "standard",  # noqa: ARG002 — OpenAI has no fast tier
        at_ts: str | None = None,
    ) -> dict[str, float]:
        """Price at the manifest rate in effect at ``at_ts``.

        Mirrors ``AnthropicPricer.compute``: manifest families resolve
        through effective-dated price rows (e.g. GPT_54's $20→$15 output
        cut), so a historical event keeps the rate that was actually
        billed. Families the manifest doesn't carry fall back to the
        in-code table exactly as before.
        """
        canonical = self.canonicalize(model)
        from stackunderflow.infra.model_manifest import (
            rates_for as _m_rates,
        )

        rates = _m_rates(canonical, provider="openai", at_ts=at_ts)
        if rates is None:
            rates = self.rates_for(canonical)
        return self._apply_overlay_rates(tokens, rates)

    # ── internals ────────────────────────────────────────────────────

    @staticmethod
    def _identify(model_id: str) -> _Family:
        if not model_id:
            return _FALLBACK
        parts = set(model_id.lower().replace(".", "-").split("-"))

        if "codex" in parts:
            if "5" in parts and "3" in parts:
                return _Family.GPT_53_CODEX
            if "5" in parts and "2" in parts:
                return _Family.GPT_52_CODEX
            if "5" in parts:
                return _Family.GPT_5_CODEX
            return _Family.GPT_5_CODEX

        if "gpt" in parts:
            has_mini = "mini" in parts
            if "5" in parts and "4" in parts:
                return _Family.GPT_54
            if "5" in parts:
                return _Family.GPT_5_MINI if has_mini else _Family.GPT_5
            if "4o" in parts or ("4" in parts and "o" in parts):
                return _Family.GPT_4O_MINI if has_mini else _Family.GPT_4O
            if ("4" in parts and "1" in parts) or "4-1" in parts:
                return _Family.GPT_41

        return _FALLBACK
