"""PiPricer: defaults to gpt-5, all routing goes through OpenAIPricer.

Pi/OMP default to ``gpt-5`` (codeburn-catalog §12) so the pricer always
delegates to ``OpenAIPricer``. Empty / missing model id → gpt-5.

Spec §3.
"""

from __future__ import annotations

from stackunderflow.infra.providers import get_pricer
from stackunderflow.infra.providers.openai import OpenAIPricer
from stackunderflow.infra.providers.pi import PiPricer


def test_default_model_is_gpt5_when_empty() -> None:
    pi = PiPricer()
    oai = OpenAIPricer()
    # Empty model → canonicalize falls back to gpt-5.
    assert pi.rates_for(pi.canonicalize("")) == \
        oai.rates_for(oai.canonicalize("gpt-5"))


def test_explicit_gpt5_routes_to_openai() -> None:
    pi = PiPricer()
    oai = OpenAIPricer()
    assert pi.rates_for(pi.canonicalize("gpt-5")) == \
        oai.rates_for(oai.canonicalize("gpt-5"))


def test_unknown_model_uses_openai_fallback() -> None:
    """Unknown ids still get a rate via OpenAI's family fallback (GPT_5_CODEX)."""
    pi = PiPricer()
    oai = OpenAIPricer()
    rates = pi.rates_for(pi.canonicalize("totally-unknown"))
    # OpenAIPricer falls back to GPT_5_CODEX rates for unknowns.
    assert rates is not None
    assert rates == oai.rates_for("GPT_5_CODEX")


def test_registry_resolves_pi_provider() -> None:
    p = get_pricer("pi")
    assert isinstance(p, PiPricer)
    assert get_pricer("pi") is p
