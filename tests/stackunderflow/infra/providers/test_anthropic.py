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
