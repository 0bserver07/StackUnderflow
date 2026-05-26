"""HermesPricer: route by model name, fall back to Anthropic.

Hermes deployments are most often Claude-backed; the default route is
``AnthropicPricer`` and ``gpt-*`` / Codex models go to ``OpenAIPricer``.
Unknown ids still get a number from the Anthropic family heuristic
(SONNET_35 fallback) — that's the conservative choice.

Spec §3.
"""

from __future__ import annotations

from stackunderflow.infra.providers import get_pricer
from stackunderflow.infra.providers.anthropic import AnthropicPricer
from stackunderflow.infra.providers.openai import OpenAIPricer
from stackunderflow.infra.providers.hermes import HermesPricer


def test_claude_model_routes_to_anthropic() -> None:
    hp = HermesPricer()
    anth = AnthropicPricer()
    assert hp.rates_for(hp.canonicalize("claude-3-5-sonnet")) == \
        anth.rates_for(anth.canonicalize("claude-3-5-sonnet"))


def test_gpt_model_routes_to_openai() -> None:
    hp = HermesPricer()
    oai = OpenAIPricer()
    assert hp.rates_for(hp.canonicalize("gpt-4o-mini")) == \
        oai.rates_for(oai.canonicalize("gpt-4o-mini"))


def test_codex_model_routes_to_openai() -> None:
    hp = HermesPricer()
    oai = OpenAIPricer()
    assert hp.rates_for(hp.canonicalize("gpt-5-codex")) == \
        oai.rates_for(oai.canonicalize("gpt-5-codex"))


def test_unknown_model_falls_back_to_anthropic_table() -> None:
    """Unknown ids still get a rate (Anthropic SONNET_35 fallback) rather than None.
    Matches the registry-level default for unknown providers."""
    hp = HermesPricer()
    anth = AnthropicPricer()
    rates = hp.rates_for(hp.canonicalize("unknown-vendor-model"))
    # Anthropic's heuristic falls back to SONNET_35.
    assert rates is not None
    assert rates == anth.rates_for("SONNET_35")


def test_registry_resolves_hermes_provider() -> None:
    p = get_pricer("hermes")
    assert isinstance(p, HermesPricer)
    assert get_pricer("hermes") is p
