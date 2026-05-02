"""Unit tests for ``CursorPricer``.

Validates the four-class pricing model:

1. Vendor-prefixed Claude / OpenAI / Gemini ids delegate to the upstream
   pricer (so Cursor records cost the same as native records on the same
   model).
2. ``composer-*`` lines hit the cursor-native ESTIMATED Sonnet-tier
   table.
3. ``cursor-auto`` / ``cursor-fast`` autoselectors fall back to the same
   Sonnet-tier estimate so the dashboard never shows $0 for a record
   with token counts.
4. Unknown ids never silently zero out — they price at the Sonnet-tier
   fallback (the underlying record stays flagged
   ``cost_source="estimated"`` so the UI renders the ≈ marker).

Plus the ``normalize_tokens`` no-op contract and the
``supports_per_message_tokens`` False contract.
"""

from __future__ import annotations

import pytest

from stackunderflow.infra.costs import compute_cost
from stackunderflow.infra.providers.anthropic import AnthropicPricer
from stackunderflow.infra.providers.cursor import CursorPricer
from stackunderflow.infra.providers.gemini import GeminiPricer
from stackunderflow.infra.providers.openai import OpenAIPricer

# Sonnet 4.x tier — the ESTIMATED fallback used for cursor-native and
# unknown ids. Pinned here so an accidental rate-table edit in cursor.py
# trips this test.
_SONNET = (3.0, 15.0, 3.75, 0.30)


# ── delegation tests ────────────────────────────────────────────────────────


def test_rates_for_claude_delegates_to_anthropic() -> None:
    cursor = CursorPricer()
    anthropic = AnthropicPricer()
    canonical = "claude-sonnet-4-6"
    assert cursor.rates_for(canonical) == anthropic.rates_for(
        anthropic.canonicalize(canonical)
    )


def test_rates_for_claude_46_sonnet_matches_anthropic() -> None:
    """Vendor-prefixed Claude 4.6 Sonnet matches native Anthropic rate."""
    cursor = CursorPricer()
    anthropic = AnthropicPricer()
    assert cursor.rates_for("claude-4.6-sonnet") == anthropic.rates_for(
        anthropic.canonicalize("claude-4.6-sonnet")
    )


def test_rates_for_claude_45_sonnet_thinking_matches_anthropic() -> None:
    """The real-world cursor model id resolves to Anthropic Sonnet 4.5."""
    cursor = CursorPricer()
    anthropic = AnthropicPricer()
    canonical = "claude-4.5-sonnet-thinking"
    assert cursor.rates_for(canonical) == anthropic.rates_for(
        anthropic.canonicalize(canonical)
    )


def test_rates_for_gpt_4o_delegates_to_openai() -> None:
    cursor = CursorPricer()
    openai = OpenAIPricer()
    assert cursor.rates_for("gpt-4o") == openai.rates_for(
        openai.canonicalize("gpt-4o")
    )


def test_rates_for_gpt_5_codex_delegates_to_openai() -> None:
    """Real-world cursor model id ``gpt-5-codex``."""
    cursor = CursorPricer()
    openai = OpenAIPricer()
    assert cursor.rates_for("gpt-5-codex") == openai.rates_for(
        openai.canonicalize("gpt-5-codex")
    )


def test_rates_for_gemini_25_pro_delegates_to_gemini() -> None:
    """Gemini ids that match the Gemini rate table delegate cleanly."""
    cursor = CursorPricer()
    gemini = GeminiPricer()
    assert cursor.rates_for("gemini-2.5-pro") == gemini.rates_for(
        gemini.canonicalize("gemini-2.5-pro")
    )


def test_rates_for_gemini_3_pro_falls_back_to_sonnet_tier() -> None:
    """Unknown Gemini id (``gemini-3-pro``) doesn't $0 — Sonnet-tier estimate.

    The Gemini rate table keys ``gemini-3.0-pro`` / ``gemini-3.1-pro``;
    the bare ``gemini-3-pro`` shape Cursor emits doesn't match. Rather
    than returning None we fall back to Sonnet-tier so the record
    contributes a real dollar figure (flagged ESTIMATED upstream).
    """
    assert CursorPricer().rates_for("gemini-3-pro") == _SONNET


def test_rates_for_gemini_preview_suffix_strips_to_base() -> None:
    """``gemini-2.5-pro-preview-05-06`` → falls back to ``gemini-2.5-pro``."""
    cursor = CursorPricer()
    gemini = GeminiPricer()
    expected = gemini.rates_for(gemini.canonicalize("gemini-2.5-pro"))
    assert cursor.rates_for("gemini-2.5-pro-preview-05-06") == expected


# ── cursor-native (ESTIMATED) ────────────────────────────────────────────────


def test_rates_for_composer_1_uses_sonnet_tier() -> None:
    """``composer-1`` is Cursor-trained — ESTIMATED at Sonnet-tier."""
    assert CursorPricer().rates_for("composer-1") == _SONNET


def test_rates_for_composer_2_uses_sonnet_tier() -> None:
    """``composer-2`` (forward-looking) — same ESTIMATED Sonnet-tier."""
    assert CursorPricer().rates_for("composer-2") == _SONNET


def test_rates_for_cursor_auto_falls_back_to_sonnet_tier() -> None:
    """Autoselector — ESTIMATED at Sonnet-tier, NOT None.

    This is the regression behaviour: cursor-auto carried 944 of the
    1,035 messages on the user's real data and was returning None →
    $0. Now it prices against the conservative Sonnet-tier estimate.
    """
    assert CursorPricer().rates_for("cursor-auto") == _SONNET
    # ``canonicalize`` keeps the full label, but the bare label
    # (``auto``) also lives in the rate table for callers that pass
    # through canonicalize first.
    assert CursorPricer().rates_for("auto") == _SONNET


def test_rates_for_cursor_fast_falls_back_to_sonnet_tier() -> None:
    """``cursor-fast`` autoselector — same ESTIMATED Sonnet-tier."""
    assert CursorPricer().rates_for("cursor-fast") == _SONNET


# ── unknown / fallback ───────────────────────────────────────────────────────


def test_rates_for_unknown_id_falls_back_to_sonnet_tier() -> None:
    """Never silently $0 — even an unknown id estimates at Sonnet-tier."""
    assert CursorPricer().rates_for("unknown-cursor-model") == _SONNET


def test_rates_for_empty_string_returns_none() -> None:
    """Empty / non-string canonical is the only ``None`` case left."""
    assert CursorPricer().rates_for("") is None


# ── normalize_tokens contract ───────────────────────────────────────────────


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


# ── canonicalize contract ───────────────────────────────────────────────────


def test_canonicalize_passes_claude_through() -> None:
    assert CursorPricer().canonicalize("claude-sonnet-4-6") == "claude-sonnet-4-6"


def test_canonicalize_passes_composer_through() -> None:
    """Composer ids hit the rate table directly, no prefix stripping."""
    assert CursorPricer().canonicalize("composer-1") == "composer-1"


def test_canonicalize_passes_cursor_auto_through() -> None:
    """``cursor-auto`` keeps its full label so the rate-table key matches."""
    assert CursorPricer().canonicalize("cursor-auto") == "cursor-auto"


def test_canonicalize_lowers_case() -> None:
    assert CursorPricer().canonicalize("Composer-1") == "composer-1"


def test_canonicalize_empty_returns_empty() -> None:
    assert CursorPricer().canonicalize("") == ""


# ── end-to-end via compute_cost ─────────────────────────────────────────────


def test_compute_cost_for_composer_1_is_nonzero() -> None:
    """End-to-end: a composer-1 record with token counts must price > $0.

    This is the bug fix: the previous CursorPricer returned None for
    every cursor-native id, so the cost layer multiplied tokens × None
    → 0 dollars regardless of token volume.
    """
    cost = compute_cost(
        {"input": 1000, "output": 500, "cache_creation": 0, "cache_read": 0},
        "composer-1",
        provider="cursor",
    )
    assert cost["total_cost"] > 0
    # 1000 × $3/M + 500 × $15/M = 0.003 + 0.0075 = 0.0105
    assert cost["total_cost"] == pytest.approx(0.0105)


def test_compute_cost_for_cursor_auto_is_nonzero() -> None:
    """The 944-message ``cursor-auto`` slice now contributes real dollars."""
    cost = compute_cost(
        {"input": 2000, "output": 1000, "cache_creation": 0, "cache_read": 0},
        "cursor-auto",
        provider="cursor",
    )
    assert cost["total_cost"] > 0


def test_compute_cost_for_claude_via_cursor_matches_native_anthropic() -> None:
    """Claude-via-Cursor session priced equal to a native Claude record."""
    tokens = {
        "input": 5000,
        "output": 2000,
        "cache_creation": 1000,
        "cache_read": 8000,
    }
    via_cursor = compute_cost(tokens, "claude-4.5-sonnet-thinking", provider="cursor")
    native = compute_cost(tokens, "claude-4.5-sonnet-thinking", provider="anthropic")
    assert via_cursor["total_cost"] == native["total_cost"]


def test_compute_cost_for_gpt_via_cursor_matches_native_openai() -> None:
    """``gpt-5-codex`` via Cursor priced equal to a native Codex record."""
    tokens = {
        "input": 5000,
        "output": 2000,
        "cache_creation": 0,
        "cache_read": 1000,
    }
    via_cursor = compute_cost(tokens, "gpt-5-codex", provider="cursor")
    native = compute_cost(tokens, "gpt-5-codex", provider="openai")
    assert via_cursor["total_cost"] == native["total_cost"]
