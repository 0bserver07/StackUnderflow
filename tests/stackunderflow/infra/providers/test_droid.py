"""DroidPricer: vendor-prefix routing and unknown-model fallback.

Droid sessions surface real upstream models in the settings file
(``claude-3-5-sonnet``, ``gpt-4o-mini``). DroidPricer routes by name
and returns ``None`` for unknown models so the cost layer flags rather
than mispricing.

Spec §3.
"""

from __future__ import annotations

from stackunderflow.infra.providers import get_pricer
from stackunderflow.infra.providers.anthropic import AnthropicPricer
from stackunderflow.infra.providers.droid import DroidPricer
from stackunderflow.infra.providers.openai import OpenAIPricer


def test_claude_model_routes_to_anthropic_pricer() -> None:
    droid = DroidPricer()
    anth = AnthropicPricer()

    droid_rates = droid.rates_for(droid.canonicalize("claude-3-5-sonnet"))
    anth_rates = anth.rates_for(anth.canonicalize("claude-3-5-sonnet"))

    assert droid_rates is not None
    assert droid_rates == anth_rates


def test_gpt_model_routes_to_openai_pricer() -> None:
    droid = DroidPricer()
    oai = OpenAIPricer()

    droid_rates = droid.rates_for(droid.canonicalize("gpt-4o-mini"))
    oai_rates = oai.rates_for(oai.canonicalize("gpt-4o-mini"))

    assert droid_rates is not None
    assert droid_rates == oai_rates


def test_unknown_model_returns_none() -> None:
    droid = DroidPricer()
    assert droid.rates_for(droid.canonicalize("ollama/llama-3")) is None
    assert droid.rates_for(droid.canonicalize("something-weird")) is None
    assert droid.rates_for("") is None


def test_registry_resolves_droid_provider() -> None:
    p = get_pricer("droid")
    assert isinstance(p, DroidPricer)
    assert get_pricer("droid") is p


def test_droid_auto_routes_to_sonnet_45() -> None:
    """``droid-auto`` (adapter default) pegs to Sonnet 4.5 rates.

    Was returning ``None`` → $0; the v0.7.1 sweep didn't address the
    autoselector default that 104 events in the live store carry.
    """
    droid = DroidPricer()
    anth = AnthropicPricer()
    droid_rates = droid.rates_for(droid.canonicalize("droid-auto"))
    expected = anth.rates_for(anth.canonicalize("claude-sonnet-4-5"))
    assert droid_rates is not None
    assert droid_rates == expected
