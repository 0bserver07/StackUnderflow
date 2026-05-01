"""KiloCodePricer: vendor-prefix delegation matches Cline.

KiloCode shares its on-disk model encoding with Cline (codeburn-catalog
§8, §15) — vendor-prefixed model ids like ``anthropic/...`` or
``openai/...``. The pricer subclasses :class:`ClinePricer` and only
changes ``provider_name``; these tests exercise the delegation path and
the unknown-vendor fallback to ``None``.

Spec §3.2.
"""

from __future__ import annotations

from stackunderflow.infra.providers import get_pricer
from stackunderflow.infra.providers.anthropic import AnthropicPricer
from stackunderflow.infra.providers.kilocode import KiloCodePricer


def test_anthropic_prefix_delegates_to_anthropic_pricer() -> None:
    kilo = KiloCodePricer()
    anth = AnthropicPricer()
    kilo_rates = kilo.rates_for(kilo.canonicalize("anthropic/claude-3-5-sonnet"))
    anth_rates = anth.rates_for(anth.canonicalize("claude-3-5-sonnet"))
    assert kilo_rates is not None
    assert kilo_rates == anth_rates


def test_unknown_vendor_returns_none() -> None:
    kilo = KiloCodePricer()
    assert kilo.rates_for(kilo.canonicalize("local/llama-3")) is None
    assert kilo.rates_for(kilo.canonicalize("ollama/mistral")) is None


def test_registry_resolves_kilocode_provider() -> None:
    """``get_pricer('kilocode')`` returns the KiloCodePricer singleton."""
    p = get_pricer("kilocode")
    assert isinstance(p, KiloCodePricer)
    assert get_pricer("kilocode") is p


def test_provider_name_is_kilocode() -> None:
    assert KiloCodePricer().provider_name == "kilocode"
