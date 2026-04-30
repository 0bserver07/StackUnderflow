"""Registry: get_pricer returns the right pricer; unknowns fall back to
Anthropic; same provider returns the same instance."""

from __future__ import annotations

from stackunderflow.infra.providers import get_pricer
from stackunderflow.infra.providers.anthropic import AnthropicPricer
from stackunderflow.infra.providers.openai import OpenAIPricer


def test_get_pricer_anthropic():
    assert isinstance(get_pricer("anthropic"), AnthropicPricer)
    assert isinstance(get_pricer("claude"), AnthropicPricer)


def test_get_pricer_openai():
    assert isinstance(get_pricer("openai"), OpenAIPricer)
    assert isinstance(get_pricer("codex"), OpenAIPricer)


def test_get_pricer_case_insensitive():
    assert isinstance(get_pricer("ANTHROPIC"), AnthropicPricer)
    assert isinstance(get_pricer("Codex"), OpenAIPricer)


def test_get_pricer_unknown_falls_back_to_anthropic():
    assert isinstance(get_pricer("cursor"), AnthropicPricer)
    assert isinstance(get_pricer(""), AnthropicPricer)
    assert isinstance(get_pricer("not-a-provider"), AnthropicPricer)


def test_get_pricer_returns_same_instance():
    """Singletons — repeated calls don't create fresh objects."""
    assert get_pricer("anthropic") is get_pricer("anthropic")
    assert get_pricer("anthropic") is get_pricer("claude")
    assert get_pricer("openai") is get_pricer("codex")
