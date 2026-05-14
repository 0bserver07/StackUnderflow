"""GeminiPricer unit tests.

Validates rate-table lookup for known Gemini model ids, the no-rate
behaviour for unknowns (returns ``None`` rather than mispricing), and
the no-op ``normalize_tokens`` contract.

Spec: codeburn-catalog §7.
"""

from __future__ import annotations

from stackunderflow.infra.providers import get_pricer
from stackunderflow.infra.providers.gemini import GeminiPricer


def test_canonicalize_lowercases_and_passes_through() -> None:
    p = GeminiPricer()
    assert p.canonicalize("GEMINI-2.5-PRO") == "gemini-2.5-pro"
    assert p.canonicalize("gemini-2.5-flash") == "gemini-2.5-flash"
    assert p.canonicalize("") == ""


def test_rates_for_gemini_2_5_pro() -> None:
    p = GeminiPricer()
    rates = p.rates_for(p.canonicalize("gemini-2.5-pro"))
    assert rates == (1.25, 10.00, 0.0, 0.31)


def test_rates_for_gemini_2_5_flash() -> None:
    p = GeminiPricer()
    rates = p.rates_for(p.canonicalize("gemini-2.5-flash"))
    assert rates == (0.30, 2.50, 0.0, 0.075)


def test_rates_for_gemini_auto_default() -> None:
    """The adapter's fallback model id must price (maps to 2.5-pro)."""
    p = GeminiPricer()
    rates = p.rates_for(p.canonicalize("gemini-auto"))
    assert rates is not None
    assert rates[0] > 0


def test_rates_for_unknown_returns_none() -> None:
    p = GeminiPricer()
    assert p.rates_for(p.canonicalize("not-a-gemini-model")) is None
    assert p.rates_for(p.canonicalize("claude-sonnet-4")) is None
    assert p.rates_for("") is None


def test_normalize_tokens_passthrough() -> None:
    """Adapter pre-normalises; the pricer is a no-op."""
    p = GeminiPricer()
    raw = {"input": 100, "output": 50, "cache_creation": 0, "cache_read": 10}
    assert p.normalize_tokens(raw) == raw


def test_normalize_tokens_partial_input() -> None:
    p = GeminiPricer()
    out = p.normalize_tokens({"input": 100, "output": 50})
    assert out == {"input": 100, "output": 50, "cache_creation": 0, "cache_read": 0}


def test_supports_per_message_tokens_is_true() -> None:
    assert GeminiPricer().supports_per_message_tokens() is True


def test_cache_write_is_zero_for_all_gemini() -> None:
    """Gemini's implicit caching does not surface a separate write event."""
    p = GeminiPricer()
    for canonical in (
        "gemini-2.5-pro", "gemini-2.5-flash", "gemini-2.5-flash-lite",
        "gemini-1.5-pro", "gemini-1.5-flash",
        "gemini-3.1-pro", "gemini-3.0-pro",
        "gemini-3-pro-preview", "gemini-3.1-pro-preview", "gemini-3-flash-preview",
        "gemini-auto",
    ):
        rates = p.rates_for(canonical)
        assert rates is not None
        assert rates[2] == 0.0


def test_registry_resolves_gemini_provider() -> None:
    p = get_pricer("gemini")
    assert isinstance(p, GeminiPricer)
    assert get_pricer("gemini") is p
    assert get_pricer("GEMINI") is p


def test_compute_with_gemini_2_5_pro() -> None:
    """Sanity end-to-end: tokens × rates yields the right dollars."""
    p = GeminiPricer()
    tokens = {"input": 1_000_000, "output": 1_000_000, "cache_creation": 0, "cache_read": 0}
    cost = p.compute(tokens, "gemini-2.5-pro")
    # 1M × $1.25 + 1M × $10 = $11.25
    assert cost["total_cost"] == 11.25


# ── Gemini 3 preview ids — pricing-fixes-round2 ─────────────────────────────


def test_rates_for_gemini_3_pro_preview() -> None:
    """``gemini-3-pro-preview`` — Google's published Pro rate ($2/$12)."""
    p = GeminiPricer()
    rates = p.rates_for(p.canonicalize("gemini-3-pro-preview"))
    assert rates == (2.00, 12.00, 0.0, 0.50)


def test_rates_for_gemini_31_pro_preview() -> None:
    """``gemini-3.1-pro-preview`` — same $2/$12 rate as the 3.0 preview."""
    p = GeminiPricer()
    rates = p.rates_for(p.canonicalize("gemini-3.1-pro-preview"))
    assert rates == (2.00, 12.00, 0.0, 0.50)


def test_rates_for_gemini_3_flash_preview() -> None:
    """``gemini-3-flash-preview`` — Flash tier at $0.30/$2.50 (≤200K)."""
    p = GeminiPricer()
    rates = p.rates_for(p.canonicalize("gemini-3-flash-preview"))
    assert rates == (0.30, 2.50, 0.0, 0.075)


def test_gemini_2_5_pro_normalizer_emits_rate_card() -> None:
    """Regression for v0.7.1 cost-coverage gap.

    Constructs a ``messages``-shape row for ``gemini-2.5-pro`` and runs it
    through ``GeminiNormalizer`` end-to-end; the emitted event must stamp
    ``cost_source='rate_card'`` (not ``'unknown'``). The v0.7.1 rate sweep
    added ``gemini-2.5-pro`` to ``RATE_CARD`` but the live store still
    showed 239 unknown events for this model because they were created
    pre-sweep and never re-derived. Locks in that a fresh row stamps
    correctly.
    """
    from stackunderflow.etl.normalize.base import COST_SOURCE_RATE_CARD
    from stackunderflow.etl.normalize.gemini import GeminiNormalizer

    msg_row = {
        "id": 1,
        "session_id": "s1",
        "project_id": 1,
        "provider": "gemini",
        "role": "assistant",
        "timestamp": "2026-05-13T10:00:00Z",
        "model": "gemini-2.5-pro",
        "input_tokens": 1000,
        "output_tokens": 500,
        "cache_read_tokens": 0,
        "cache_create_tokens": 0,
    }
    events = list(GeminiNormalizer().normalize(msg_row))
    assert len(events) == 1
    assert events[0]["cost_source"] == COST_SOURCE_RATE_CARD
    assert events[0]["cost_usd"] > 0


def test_gemini_3_pro_preview_normalizer_emits_rate_card() -> None:
    """A ``gemini-3-pro-preview`` row stamps ``rate_card`` (not ``unknown``).

    Before this fix the model wasn't in ``RATE_CARD``; the v0.7.1 sweep
    added the bare ``gemini-3.0-pro`` / ``gemini-3.1-pro`` ids but missed
    the ``-preview`` suffix the Gemini CLI actually emits.
    """
    from stackunderflow.etl.normalize.base import COST_SOURCE_RATE_CARD
    from stackunderflow.etl.normalize.gemini import GeminiNormalizer

    msg_row = {
        "id": 2,
        "session_id": "s2",
        "project_id": 1,
        "provider": "gemini",
        "role": "assistant",
        "timestamp": "2026-05-13T10:00:00Z",
        "model": "gemini-3-pro-preview",
        "input_tokens": 1000,
        "output_tokens": 500,
        "cache_read_tokens": 0,
        "cache_create_tokens": 0,
    }
    events = list(GeminiNormalizer().normalize(msg_row))
    assert len(events) == 1
    assert events[0]["cost_source"] == COST_SOURCE_RATE_CARD
    assert events[0]["cost_usd"] > 0
