"""OpenClawPricer: route by model name, fall back to Anthropic.

OpenClaw deployments are most often Claude-backed; the default route is
``AnthropicPricer`` and ``gpt-*`` / Codex models go to ``OpenAIPricer``.
Unknown ids still get a number from the Anthropic family heuristic
(SONNET_35 fallback) — that's the conservative choice.

Spec §3.
"""

from __future__ import annotations

from stackunderflow.infra.providers import get_pricer
from stackunderflow.infra.providers.anthropic import AnthropicPricer
from stackunderflow.infra.providers.openai import OpenAIPricer
from stackunderflow.infra.providers.openclaw import OpenClawPricer


def test_claude_model_routes_to_anthropic() -> None:
    oc = OpenClawPricer()
    anth = AnthropicPricer()
    assert oc.rates_for(oc.canonicalize("claude-3-5-sonnet")) == \
        anth.rates_for(anth.canonicalize("claude-3-5-sonnet"))


def test_gpt_model_routes_to_openai() -> None:
    oc = OpenClawPricer()
    oai = OpenAIPricer()
    assert oc.rates_for(oc.canonicalize("gpt-4o-mini")) == \
        oai.rates_for(oai.canonicalize("gpt-4o-mini"))


def test_codex_model_routes_to_openai() -> None:
    oc = OpenClawPricer()
    oai = OpenAIPricer()
    assert oc.rates_for(oc.canonicalize("gpt-5-codex")) == \
        oai.rates_for(oai.canonicalize("gpt-5-codex"))


def test_unknown_model_falls_back_to_anthropic_table() -> None:
    """Unknown ids still get a rate (Anthropic SONNET_35 fallback) rather than None.
    Matches the registry-level default for unknown providers."""
    oc = OpenClawPricer()
    anth = AnthropicPricer()
    rates = oc.rates_for(oc.canonicalize("unknown-vendor-model"))
    # Anthropic's heuristic falls back to SONNET_35.
    assert rates is not None
    assert rates == anth.rates_for("SONNET_35")


def test_registry_resolves_openclaw_provider() -> None:
    p = get_pricer("openclaw")
    assert isinstance(p, OpenClawPricer)
    assert get_pricer("openclaw") is p
