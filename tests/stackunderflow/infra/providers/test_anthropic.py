"""AnthropicPricer unit tests — canonicalize, normalize, rates, fallback."""

from __future__ import annotations

from stackunderflow.infra.providers.anthropic import AnthropicPricer


def test_canonicalize_known_models():
    p = AnthropicPricer()
    # Different versions resolve to distinct families.
    assert p.canonicalize("claude-opus-4-6") == "OPUS_46"
    assert p.canonicalize("claude-sonnet-4-6") == "SONNET_46"
    assert p.canonicalize("claude-opus-4-5-20251101") == "OPUS_45"
    assert p.canonicalize("claude-sonnet-4-5-20250929") == "SONNET_45"
    assert p.canonicalize("claude-haiku-4-5-20251001") == "HAIKU_45"
    assert p.canonicalize("claude-3-5-sonnet-20241022") == "SONNET_35"
    assert p.canonicalize("claude-3-5-haiku-20241022") == "HAIKU_35"
    assert p.canonicalize("claude-3-opus-20240229") == "OPUS_3"
    assert p.canonicalize("claude-3-haiku-20240307") == "HAIKU_3"


def test_canonicalize_unknown_falls_back_to_sonnet_35():
    p = AnthropicPricer()
    assert p.canonicalize("not-a-real-model") == "SONNET_35"
    assert p.canonicalize("") == "SONNET_35"


def test_normalize_tokens_is_noop():
    """Anthropic shape == canonical shape; normalize coerces to 4 keys."""
    p = AnthropicPricer()
    raw = {"input": 100, "output": 50, "cache_creation": 20, "cache_read": 30}
    assert p.normalize_tokens(raw) == raw


def test_normalize_tokens_handles_missing_keys():
    p = AnthropicPricer()
    out = p.normalize_tokens({"input": 10})
    assert out == {"input": 10, "output": 0, "cache_creation": 0, "cache_read": 0}


def test_rates_for_known_returns_tuple():
    p = AnthropicPricer()
    rates = p.rates_for("OPUS_46")
    assert rates == (15.0, 75.0, 18.75, 1.50)


def test_rates_for_unknown_falls_back_to_sonnet_35():
    p = AnthropicPricer()
    # Unknown canonical → fallback to Sonnet 3.5 rates (3, 15, 3.75, 0.30)
    assert p.rates_for("nonsense") == (3.0, 15.0, 3.75, 0.30)


def test_supports_per_message_tokens():
    assert AnthropicPricer().supports_per_message_tokens() is True


def test_compute_e2e_sonnet_46():
    p = AnthropicPricer()
    tokens = {"input": 1000, "output": 500, "cache_creation": 200, "cache_read": 800}
    cost = p.compute(tokens, "claude-sonnet-4-6")
    # rates: (3, 15, 3.75, 0.30) per M tokens
    assert cost["input_cost"] == 1000 * 3.0 / 1_000_000
    assert cost["output_cost"] == 500 * 15.0 / 1_000_000
    assert cost["cache_creation_cost"] == 200 * 3.75 / 1_000_000
    assert cost["cache_read_cost"] == 800 * 0.30 / 1_000_000
    expected_total = (
        cost["input_cost"]
        + cost["output_cost"]
        + cost["cache_creation_cost"]
        + cost["cache_read_cost"]
    )
    assert cost["total_cost"] == expected_total


# ── Opus 4.7 — pricing-fixes-round2 ─────────────────────────────────────────


def test_canonicalize_opus_47_resolves_to_own_family():
    """``claude-opus-4-7`` must hit its own family — not Opus 4 fallback.

    Before this fix, the heuristic's ``"4" in parts and has_opus`` branch
    swallowed the 4-7 model and priced it at the (legacy) Opus 4 rate
    ($15/$75). Opus 4.7's published rate is $5/$25.
    """
    assert AnthropicPricer().canonicalize("claude-opus-4-7") == "OPUS_47"


def test_rates_for_opus_47_is_5_25():
    """Per Anthropic's published rate card (May 2026): $5 / $25 per MTok."""
    p = AnthropicPricer()
    assert p.rates_for("OPUS_47") == (5.0, 25.0, 6.25, 0.50)


def test_compute_e2e_opus_47():
    """End-to-end pricing for ``claude-opus-4-7`` matches Anthropic's rates."""
    p = AnthropicPricer()
    tokens = {"input": 1000, "output": 500, "cache_creation": 200, "cache_read": 800}
    cost = p.compute(tokens, "claude-opus-4-7")
    # rates: (5, 25, 6.25, 0.50) per M tokens
    assert cost["input_cost"] == 1000 * 5.0 / 1_000_000
    assert cost["output_cost"] == 500 * 25.0 / 1_000_000
    assert cost["cache_creation_cost"] == 200 * 6.25 / 1_000_000
    assert cost["cache_read_cost"] == 800 * 0.50 / 1_000_000


def test_opus_47_fast_mode_uses_6x_multiplier():
    """Opus 4.7 is in the fast-mode multiplier set (input/output × 6)."""
    import pytest as _pytest
    p = AnthropicPricer()
    tokens = {"input": 1000, "output": 500, "cache_creation": 0, "cache_read": 0}
    standard = p.compute(tokens, "claude-opus-4-7", speed="standard")
    fast = p.compute(tokens, "claude-opus-4-7", speed="fast")
    assert fast["input_cost"] == _pytest.approx(standard["input_cost"] * 6.0)
    assert fast["output_cost"] == _pytest.approx(standard["output_cost"] * 6.0)


# ── ZhipuAI GLM-5 / GLM-5.1 — pricing-fixes-round2 ─────────────────────────


def test_canonicalize_glm_5_resolves_to_own_family():
    """``glm-5`` routes to GLM_5 — not the default Sonnet 3.5 fallback."""
    assert AnthropicPricer().canonicalize("glm-5") == "GLM_5"


def test_canonicalize_glm_51_resolves_to_own_family():
    """``glm-5.1`` token-split → {glm,5,1}; matches GLM_51 (5.1 over 5)."""
    assert AnthropicPricer().canonicalize("glm-5.1") == "GLM_51"


def test_rates_for_glm_5_matches_zhipu_published():
    """GLM-5: $1.00 input / $3.20 output per MTok (Z.ai docs 2026-05-13)."""
    p = AnthropicPricer()
    assert p.rates_for("GLM_5") == (1.00, 3.20, 1.25, 0.10)


def test_rates_for_glm_51_matches_zhipu_published():
    """GLM-5.1: $1.40 input / $4.40 output per MTok (Z.ai docs 2026-05-13)."""
    p = AnthropicPricer()
    assert p.rates_for("GLM_51") == (1.40, 4.40, 1.75, 0.14)


def test_compute_e2e_glm_5():
    """End-to-end: a glm-5 record must price > $0 and match published rates."""
    p = AnthropicPricer()
    tokens = {"input": 1000, "output": 500, "cache_creation": 0, "cache_read": 0}
    cost = p.compute(tokens, "glm-5")
    # 1000 × $1/M + 500 × $3.20/M = 0.001 + 0.0016 = 0.0026
    assert cost["input_cost"] == 1000 * 1.0 / 1_000_000
    assert cost["output_cost"] == 500 * 3.20 / 1_000_000
