"""ClinePricer: vendor-prefixed model ids route to the right delegate.

Cline records the upstream model as ``anthropic/...`` or ``openai/...`` (and
sometimes bare ``claude-*`` / ``gpt-*``). The pricer parses the prefix and
delegates ``rates_for`` to ``AnthropicPricer`` or ``OpenAIPricer``. Unknown
vendors return ``None`` so the cost layer surfaces "no rate available" rather
than mispricing against an arbitrary table.

Spec §3.2.
"""

from __future__ import annotations

from stackunderflow.infra.providers import get_pricer
from stackunderflow.infra.providers.anthropic import AnthropicPricer
from stackunderflow.infra.providers.cline import ClinePricer
from stackunderflow.infra.providers.openai import OpenAIPricer


def test_anthropic_prefix_delegates_to_anthropic_pricer() -> None:
    cline = ClinePricer()
    anth = AnthropicPricer()

    cline_rates = cline.rates_for(cline.canonicalize("anthropic/claude-3-5-sonnet"))
    anth_rates = anth.rates_for(anth.canonicalize("claude-3-5-sonnet"))

    assert cline_rates is not None
    assert cline_rates == anth_rates


def test_openai_prefix_delegates_to_openai_pricer() -> None:
    cline = ClinePricer()
    oai = OpenAIPricer()

    cline_rates = cline.rates_for(cline.canonicalize("openai/gpt-4o-mini"))
    oai_rates = oai.rates_for(oai.canonicalize("gpt-4o-mini"))

    assert cline_rates is not None
    assert cline_rates == oai_rates
    # Sanity: the rate tuple is (input, output, cache_create, cache_read)
    # and gpt-4o-mini has non-zero input/output.
    assert cline_rates[0] > 0
    assert cline_rates[1] > 0


def test_unknown_vendor_returns_none() -> None:
    cline = ClinePricer()
    assert cline.rates_for(cline.canonicalize("local/llama-3")) is None
    assert cline.rates_for(cline.canonicalize("ollama/mistral")) is None


def test_bare_claude_prefix_routes_to_anthropic() -> None:
    """No vendor slash — bare ``claude-*`` still routes to Anthropic."""
    cline = ClinePricer()
    anth = AnthropicPricer()
    assert cline.rates_for(cline.canonicalize("claude-3-5-sonnet")) == \
        anth.rates_for(anth.canonicalize("claude-3-5-sonnet"))


def test_bare_gpt_prefix_routes_to_openai() -> None:
    """No vendor slash — bare ``gpt-*`` still routes to OpenAI."""
    cline = ClinePricer()
    oai = OpenAIPricer()
    assert cline.rates_for(cline.canonicalize("gpt-4o")) == \
        oai.rates_for(oai.canonicalize("gpt-4o"))


def test_empty_or_none_canonical_returns_none() -> None:
    cline = ClinePricer()
    assert cline.rates_for("") is None


def test_registry_resolves_cline_provider() -> None:
    """``get_pricer('cline')`` returns the ClinePricer singleton."""
    p = get_pricer("cline")
    assert isinstance(p, ClinePricer)
    # Singleton: repeated lookups return the same instance.
    assert get_pricer("cline") is p


def test_normalize_tokens_passthrough() -> None:
    """Cline adapter pre-normalises tokens, so the pricer's normalize is a no-op."""
    cline = ClinePricer()
    out = cline.normalize_tokens({
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
