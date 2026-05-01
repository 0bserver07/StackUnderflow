"""QwenPricer unit tests.

Validates rate-table lookup for known Qwen model ids, the no-rate
behaviour for unknowns (returns ``None`` rather than mispricing), and
the no-op ``normalize_tokens`` contract.

Spec: codeburn-catalog §13.
"""

from __future__ import annotations

from stackunderflow.infra.providers import get_pricer
from stackunderflow.infra.providers.qwen import QwenPricer


def test_canonicalize_lowercases_and_passes_through() -> None:
    p = QwenPricer()
    assert p.canonicalize("QWEN-MAX") == "qwen-max"
    assert p.canonicalize("qwen-plus") == "qwen-plus"
    assert p.canonicalize("") == ""


def test_rates_for_known_models() -> None:
    p = QwenPricer()
    assert p.rates_for(p.canonicalize("qwen-max")) == (3.00, 12.00, 0.0, 0.30)
    assert p.rates_for(p.canonicalize("qwen-plus")) == (1.20, 3.60, 0.0, 0.12)
    assert p.rates_for(p.canonicalize("qwen-turbo")) == (0.30, 0.60, 0.0, 0.03)


def test_rates_for_qwen_auto_default() -> None:
    """``qwen-auto`` is the adapter's fallback model id; it must price."""
    p = QwenPricer()
    rates = p.rates_for(p.canonicalize("qwen-auto"))
    assert rates is not None
    assert rates[0] > 0
    assert rates[1] > 0


def test_rates_for_unknown_returns_none() -> None:
    """Unknown ids return ``None`` so the cost layer surfaces a missing rate."""
    p = QwenPricer()
    assert p.rates_for(p.canonicalize("not-a-qwen-model")) is None
    assert p.rates_for(p.canonicalize("gpt-5")) is None
    assert p.rates_for("") is None


def test_normalize_tokens_passthrough() -> None:
    """Adapter pre-normalises; the pricer is a no-op."""
    p = QwenPricer()
    raw = {"input": 100, "output": 50, "cache_creation": 0, "cache_read": 10}
    assert p.normalize_tokens(raw) == raw


def test_normalize_tokens_partial_input() -> None:
    p = QwenPricer()
    out = p.normalize_tokens({"input": 100, "output": 50})
    assert out == {"input": 100, "output": 50, "cache_creation": 0, "cache_read": 0}


def test_supports_per_message_tokens_is_true() -> None:
    assert QwenPricer().supports_per_message_tokens() is True


def test_cache_write_is_zero_for_all_qwen() -> None:
    """Qwen does not surface a separate cache-write event."""
    p = QwenPricer()
    for canonical in (
        "qwen-max", "qwen-plus", "qwen-turbo",
        "qwen-coder", "qwen3-coder", "qwen-auto",
    ):
        rates = p.rates_for(canonical)
        assert rates is not None
        assert rates[2] == 0.0


def test_registry_resolves_qwen_provider() -> None:
    """``get_pricer('qwen')`` returns the QwenPricer singleton."""
    p = get_pricer("qwen")
    assert isinstance(p, QwenPricer)
    assert get_pricer("qwen") is p
    assert get_pricer("QWEN") is p


def test_compute_with_qwen_max() -> None:
    """Sanity end-to-end: tokens × rates yields the right dollars."""
    p = QwenPricer()
    tokens = {"input": 1_000_000, "output": 1_000_000, "cache_creation": 0, "cache_read": 0}
    cost = p.compute(tokens, "qwen-max")
    # 1M input × $3 + 1M output × $12 = $15
    assert cost["total_cost"] == 15.00
