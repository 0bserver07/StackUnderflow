"""Unit tests for ``OpenCodePricer``.

OpenCode delegates pricing to the upstream provider based on the
``modelID`` prefix recorded on each message. Anthropic models route to
``AnthropicPricer``; ``gpt-*`` / ``codex-*`` route to ``OpenAIPricer``;
unknowns return None.
"""

from __future__ import annotations

from stackunderflow.infra.providers import get_pricer
from stackunderflow.infra.providers.anthropic import AnthropicPricer
from stackunderflow.infra.providers.openai import OpenAIPricer
from stackunderflow.infra.providers.opencode import OpenCodePricer


def test_claude_prefix_delegates_to_anthropic() -> None:
    pricer = OpenCodePricer()
    anth = AnthropicPricer()

    out = pricer.rates_for(pricer.canonicalize("claude-sonnet-4-6"))
    expected = anth.rates_for(anth.canonicalize("claude-sonnet-4-6"))
    assert out is not None
    assert out == expected


def test_gpt_prefix_delegates_to_openai() -> None:
    pricer = OpenCodePricer()
    oai = OpenAIPricer()

    out = pricer.rates_for(pricer.canonicalize("gpt-4o-mini"))
    expected = oai.rates_for(oai.canonicalize("gpt-4o-mini"))
    assert out is not None
    assert out == expected


def test_codex_prefix_delegates_to_openai() -> None:
    pricer = OpenCodePricer()
    oai = OpenAIPricer()

    out = pricer.rates_for(pricer.canonicalize("codex-mini-latest"))
    expected = oai.rates_for(oai.canonicalize("codex-mini-latest"))
    assert out is not None
    assert out == expected


def test_unknown_model_returns_none() -> None:
    pricer = OpenCodePricer()
    assert pricer.rates_for(pricer.canonicalize("ollama-llama-3")) is None
    assert pricer.rates_for(pricer.canonicalize("opencode-auto")) is None


def test_empty_canonical_returns_none() -> None:
    assert OpenCodePricer().rates_for("") is None


def test_supports_per_message_tokens_is_true() -> None:
    """OpenCode's DB stores explicit per-message tokens."""
    assert OpenCodePricer().supports_per_message_tokens() is True


def test_normalize_tokens_is_noop() -> None:
    pricer = OpenCodePricer()
    raw = {
        "input": 100,
        "output": 50,
        "cache_creation": 5,
        "cache_read": 10,
    }
    assert pricer.normalize_tokens(raw) == raw


def test_has_embedded_cost_helper() -> None:
    assert OpenCodePricer.has_embedded_cost({"embedded_cost": 0.02}) is True
    assert OpenCodePricer.has_embedded_cost({}) is False
    assert OpenCodePricer.has_embedded_cost({"embedded_cost": None}) is False


def test_registry_resolves_opencode_provider() -> None:
    """``get_pricer('opencode')`` returns the OpenCodePricer singleton."""
    p = get_pricer("opencode")
    assert isinstance(p, OpenCodePricer)
    assert get_pricer("opencode") is p
