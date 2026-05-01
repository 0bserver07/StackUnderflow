"""CodeiumPricer: stub — every ``rates_for`` returns ``None``.

Codeium has no public per-token rate card and the on-disk schema is
protobuf-only (see adapter docstring). The pricer is registered so the
registry can return a stable instance when callers ask for ``codeium``,
but it never produces a rate tuple and it doesn't claim per-message
token support.
"""

from __future__ import annotations

from stackunderflow.infra.providers import get_pricer
from stackunderflow.infra.providers.codeium import CodeiumPricer


def test_rates_for_always_returns_none() -> None:
    p = CodeiumPricer()
    for canonical in ("", "codeium-auto", "claude-3-5-sonnet", "gpt-4o", "anything"):
        assert p.rates_for(p.canonicalize(canonical)) is None


def test_supports_per_message_tokens_is_false() -> None:
    p = CodeiumPricer()
    assert p.supports_per_message_tokens() is False


def test_canonicalize_is_passthrough() -> None:
    p = CodeiumPricer()
    assert p.canonicalize("foo") == "foo"
    assert p.canonicalize("") == ""


def test_normalize_tokens_passthrough() -> None:
    p = CodeiumPricer()
    out = p.normalize_tokens({
        "input": 1, "output": 2, "cache_creation": 3, "cache_read": 4,
    })
    assert out == {
        "input": 1, "output": 2, "cache_creation": 3, "cache_read": 4,
    }


def test_registry_resolves_codeium_provider() -> None:
    p = get_pricer("codeium")
    assert isinstance(p, CodeiumPricer)
    assert get_pricer("codeium") is p


def test_compute_returns_zero_costs_for_any_input() -> None:
    """``compute()`` over a None rate tuple returns the all-zero shape."""
    p = CodeiumPricer()
    out = p.compute(
        {"input": 1000, "output": 1000, "cache_creation": 0, "cache_read": 0},
        "anything",
    )
    assert out["total_cost"] == 0.0
    assert out["input_cost"] == 0.0
    assert out["output_cost"] == 0.0
