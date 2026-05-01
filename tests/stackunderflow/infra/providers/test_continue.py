"""ContinuePricer: vendor-prefix delegation.

Continue is BYO-key, so the pricer follows the same routing pattern as
``ClinePricer`` / ``CopilotPricer``: ``claude-*`` / ``anthropic/...``
delegates to ``AnthropicPricer``, ``gpt-*`` / ``openai/...`` delegates
to ``OpenAIPricer``, and everything else (including ``continue-auto``)
returns ``None``.
"""

from __future__ import annotations

from stackunderflow.infra.providers import get_pricer
from stackunderflow.infra.providers.anthropic import AnthropicPricer
from stackunderflow.infra.providers.continue_pricer import ContinuePricer
from stackunderflow.infra.providers.openai import OpenAIPricer


def test_claude_prefix_delegates_to_anthropic_pricer() -> None:
    cont = ContinuePricer()
    anth = AnthropicPricer()
    assert cont.rates_for(cont.canonicalize("claude-3-5-sonnet")) == \
        anth.rates_for(anth.canonicalize("claude-3-5-sonnet"))


def test_gpt_prefix_delegates_to_openai_pricer() -> None:
    cont = ContinuePricer()
    oai = OpenAIPricer()
    rates = cont.rates_for(cont.canonicalize("gpt-4o-mini"))
    assert rates is not None
    assert rates == oai.rates_for(oai.canonicalize("gpt-4o-mini"))


def test_anthropic_slash_prefix_delegates_correctly() -> None:
    cont = ContinuePricer()
    anth = AnthropicPricer()
    assert cont.rates_for(cont.canonicalize("anthropic/claude-3-5-sonnet")) == \
        anth.rates_for(anth.canonicalize("claude-3-5-sonnet"))


def test_continue_auto_returns_none() -> None:
    """The fallback model name has no rate."""
    cont = ContinuePricer()
    assert cont.rates_for(cont.canonicalize("continue-auto")) is None


def test_unknown_vendor_returns_none() -> None:
    cont = ContinuePricer()
    assert cont.rates_for(cont.canonicalize("local/mistral")) is None
    assert cont.rates_for("") is None


def test_registry_resolves_continue_provider() -> None:
    p = get_pricer("continue")
    assert isinstance(p, ContinuePricer)
    assert get_pricer("continue") is p


def test_normalize_tokens_passthrough() -> None:
    cont = ContinuePricer()
    out = cont.normalize_tokens({
        "input": 10, "output": 20, "cache_creation": 0, "cache_read": 0,
    })
    assert out == {
        "input": 10, "output": 20, "cache_creation": 0, "cache_read": 0,
    }
