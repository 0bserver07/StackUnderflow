"""Unit tests for ``CursorAgentPricer``.

Cursor Agent has no native rate card — pricing is delegated to whichever
upstream provider the attribution DB names (Anthropic for ``claude-*``,
OpenAI for ``gpt-*``). The literal ``cursor-agent`` fallback (used when
the DB is missing) returns None so the cost layer flags the row as
"no rate" rather than mispricing it.
"""

from __future__ import annotations

from stackunderflow.infra.providers import get_pricer
from stackunderflow.infra.providers.anthropic import AnthropicPricer
from stackunderflow.infra.providers.cursor_agent import CursorAgentPricer
from stackunderflow.infra.providers.openai import OpenAIPricer


def test_claude_prefix_delegates_to_anthropic() -> None:
    pricer = CursorAgentPricer()
    anth = AnthropicPricer()
    out = pricer.rates_for(pricer.canonicalize("claude-sonnet-4-6"))
    expected = anth.rates_for(anth.canonicalize("claude-sonnet-4-6"))
    assert out is not None
    assert out == expected


def test_gpt_prefix_delegates_to_openai() -> None:
    pricer = CursorAgentPricer()
    oai = OpenAIPricer()
    out = pricer.rates_for(pricer.canonicalize("gpt-4o"))
    expected = oai.rates_for(oai.canonicalize("gpt-4o"))
    assert out is not None
    assert out == expected


def test_cursor_agent_fallback_returns_none() -> None:
    """The literal ``cursor-agent`` model id has no rate card."""
    assert CursorAgentPricer().rates_for("cursor-agent") is None


def test_unknown_model_returns_none() -> None:
    assert CursorAgentPricer().rates_for("ollama/llama-3") is None


def test_empty_canonical_returns_none() -> None:
    assert CursorAgentPricer().rates_for("") is None


def test_supports_per_message_tokens_is_false() -> None:
    """Tokens are estimated from text length — not authoritative."""
    assert CursorAgentPricer().supports_per_message_tokens() is False


def test_normalize_tokens_is_noop() -> None:
    pricer = CursorAgentPricer()
    raw = {
        "input": 10,
        "output": 5,
        "cache_creation": 0,
        "cache_read": 0,
    }
    assert pricer.normalize_tokens(raw) == raw


def test_registry_resolves_cursor_agent_provider() -> None:
    p = get_pricer("cursor-agent")
    assert isinstance(p, CursorAgentPricer)
    assert get_pricer("cursor-agent") is p
