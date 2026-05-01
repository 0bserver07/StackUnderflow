"""KiroPricer: vendor-prefix routing, ``supports_per_message_tokens`` False.

Kiro estimates tokens from content length; ``supports_per_message_tokens``
must be False so the cost layer can skip per-message attribution.
Vendor-prefix routing matches Cline's behaviour (Anthropic / OpenAI /
None for unknown).

Spec §3.
"""

from __future__ import annotations

from stackunderflow.infra.providers import get_pricer
from stackunderflow.infra.providers.anthropic import AnthropicPricer
from stackunderflow.infra.providers.kiro import KiroPricer
from stackunderflow.infra.providers.openai import OpenAIPricer


def test_supports_per_message_tokens_is_false() -> None:
    """Tokens are estimated; the cost layer must know not to trust them per-message."""
    assert KiroPricer().supports_per_message_tokens() is False


def test_claude_model_routes_to_anthropic_pricer() -> None:
    kiro = KiroPricer()
    anth = AnthropicPricer()
    rates = kiro.rates_for(kiro.canonicalize("claude-3-5-sonnet"))
    assert rates == anth.rates_for(anth.canonicalize("claude-3-5-sonnet"))


def test_dotted_model_id_normalised_then_routed() -> None:
    """``claude.3.5.sonnet`` (Kiro's raw shape) → ``claude-3-5-sonnet``."""
    kiro = KiroPricer()
    anth = AnthropicPricer()
    rates = kiro.rates_for(kiro.canonicalize("claude.3.5.sonnet"))
    assert rates == anth.rates_for(anth.canonicalize("claude-3-5-sonnet"))


def test_gpt_model_routes_to_openai_pricer() -> None:
    kiro = KiroPricer()
    oai = OpenAIPricer()
    rates = kiro.rates_for(kiro.canonicalize("gpt-4o"))
    assert rates == oai.rates_for(oai.canonicalize("gpt-4o"))


def test_kiro_auto_returns_none() -> None:
    """Unknown auto-routed model → no rate, no invented dollars."""
    assert KiroPricer().rates_for(KiroPricer().canonicalize("kiro-auto")) is None


def test_registry_resolves_kiro_provider() -> None:
    p = get_pricer("kiro")
    assert isinstance(p, KiroPricer)
    assert get_pricer("kiro") is p
