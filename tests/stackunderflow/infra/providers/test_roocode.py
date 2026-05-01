"""RooCodePricer: vendor-prefix delegation matches Cline.

Roo Code shares its on-disk model encoding with Cline (codeburn-catalog
§14, §15) — vendor-prefixed model ids like ``anthropic/...`` or
``openai/...``. The pricer subclasses :class:`ClinePricer` and only
changes ``provider_name``; these tests exercise the delegation path and
the unknown-vendor fallback to ``None``.

Spec §3.2.
"""

from __future__ import annotations

from stackunderflow.infra.providers import get_pricer
from stackunderflow.infra.providers.openai import OpenAIPricer
from stackunderflow.infra.providers.roocode import RooCodePricer


def test_openai_prefix_delegates_to_openai_pricer() -> None:
    roo = RooCodePricer()
    oai = OpenAIPricer()
    roo_rates = roo.rates_for(roo.canonicalize("openai/gpt-4o-mini"))
    oai_rates = oai.rates_for(oai.canonicalize("gpt-4o-mini"))
    assert roo_rates is not None
    assert roo_rates == oai_rates


def test_unknown_vendor_returns_none() -> None:
    roo = RooCodePricer()
    assert roo.rates_for(roo.canonicalize("local/llama-3")) is None
    assert roo.rates_for(roo.canonicalize("ollama/mistral")) is None


def test_registry_resolves_roocode_provider() -> None:
    """``get_pricer('roocode')`` returns the RooCodePricer singleton."""
    p = get_pricer("roocode")
    assert isinstance(p, RooCodePricer)
    assert get_pricer("roocode") is p


def test_provider_name_is_roocode() -> None:
    assert RooCodePricer().provider_name == "roocode"
