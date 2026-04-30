"""Unit tests for ``CursorPricer``.

Validates the Anthropic delegation seam (Claude-via-Cursor sessions get
the same rates as native Claude records), the no-rate-yet behaviour for
``cursor-auto``, and the ``normalize_tokens`` no-op contract.
"""

from __future__ import annotations

from stackunderflow.infra.providers.anthropic import AnthropicPricer
from stackunderflow.infra.providers.cursor import CursorPricer


def test_rates_for_claude_delegates_to_anthropic() -> None:
    cursor = CursorPricer()
    anthropic = AnthropicPricer()
    canonical = "claude-sonnet-4-6"
    assert cursor.rates_for(canonical) == anthropic.rates_for(
        anthropic.canonicalize(canonical)
    )


def test_rates_for_cursor_auto_returns_none() -> None:
    """No Cursor-specific rate card yet — bare ``cursor-auto`` is unknown."""
    assert CursorPricer().rates_for("cursor-auto") is None
    # ``canonicalize`` strips the prefix; the bare label is still unknown.
    assert CursorPricer().rates_for("auto") is None


def test_normalize_tokens_is_noop() -> None:
    raw = {
        "input": 100,
        "output": 50,
        "cache_creation": 0,
        "cache_read": 0,
    }
    assert CursorPricer().normalize_tokens(raw) == raw


def test_normalize_tokens_partial_input() -> None:
    """Missing keys default to 0 — matches the AnthropicPricer no-op shape."""
    out = CursorPricer().normalize_tokens({"input": 100, "output": 50})
    assert out == {
        "input": 100,
        "output": 50,
        "cache_creation": 0,
        "cache_read": 0,
    }


def test_supports_per_message_tokens_is_false() -> None:
    """vscdb token counts are estimates; aggregator must skip per-msg cost."""
    assert CursorPricer().supports_per_message_tokens() is False


def test_canonicalize_passes_claude_through() -> None:
    assert CursorPricer().canonicalize("claude-sonnet-4-6") == "claude-sonnet-4-6"


def test_canonicalize_strips_cursor_prefix() -> None:
    assert CursorPricer().canonicalize("cursor-auto") == "auto"
