"""CopilotPricer: vendor-prefix delegation.

Same routing pattern as ``ClinePricer`` — the adapter has already
inferred the upstream model (or synthesised ``claude-auto`` /
``gpt-auto`` from a tool-call id), so this pricer routes ``rates_for``
to the matching real pricer and returns ``None`` for unknowns.
"""

from __future__ import annotations

from stackunderflow.infra.providers import get_pricer
from stackunderflow.infra.providers.anthropic import AnthropicPricer
from stackunderflow.infra.providers.copilot import CopilotPricer
from stackunderflow.infra.providers.openai import OpenAIPricer


def test_claude_prefix_delegates_to_anthropic_pricer() -> None:
    cop = CopilotPricer()
    anth = AnthropicPricer()
    assert cop.rates_for(cop.canonicalize("claude-3-5-sonnet")) == \
        anth.rates_for(anth.canonicalize("claude-3-5-sonnet"))


def test_gpt_prefix_delegates_to_openai_pricer() -> None:
    cop = CopilotPricer()
    oai = OpenAIPricer()
    rates = cop.rates_for(cop.canonicalize("gpt-4o-mini"))
    assert rates is not None
    assert rates == oai.rates_for(oai.canonicalize("gpt-4o-mini"))
    assert rates[0] > 0
    assert rates[1] > 0


def test_anthropic_slash_prefix_delegates_correctly() -> None:
    cop = CopilotPricer()
    anth = AnthropicPricer()
    assert cop.rates_for(cop.canonicalize("anthropic/claude-3-5-sonnet")) == \
        anth.rates_for(anth.canonicalize("claude-3-5-sonnet"))


def test_openai_slash_prefix_delegates_correctly() -> None:
    cop = CopilotPricer()
    oai = OpenAIPricer()
    assert cop.rates_for(cop.canonicalize("openai/gpt-4o")) == \
        oai.rates_for(oai.canonicalize("gpt-4o"))


def test_copilot_auto_returns_none() -> None:
    """``copilot-auto`` is the no-signal fallback — no rate computable."""
    cop = CopilotPricer()
    assert cop.rates_for(cop.canonicalize("copilot-auto")) is None


def test_unknown_vendor_returns_none() -> None:
    cop = CopilotPricer()
    assert cop.rates_for(cop.canonicalize("local/llama-3")) is None
    assert cop.rates_for("") is None


def test_registry_resolves_copilot_provider() -> None:
    """``get_pricer('copilot')`` returns the CopilotPricer singleton."""
    p = get_pricer("copilot")
    assert isinstance(p, CopilotPricer)
    assert get_pricer("copilot") is p


def test_normalize_tokens_passthrough() -> None:
    cop = CopilotPricer()
    out = cop.normalize_tokens({
        "input": 100,
        "output": 50,
        "cache_creation": 5,
        "cache_read": 10,
    })
    assert out == {
        "input": 100,
        "output": 50,
        "cache_creation": 5,
        "cache_read": 10,
    }
