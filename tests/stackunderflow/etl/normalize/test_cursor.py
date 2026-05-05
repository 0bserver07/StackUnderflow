"""CursorNormalizer — Cursor v3 estimation path + explicit-tokens path."""

from __future__ import annotations

import json

from stackunderflow.etl.normalize import CursorNormalizer
from stackunderflow.etl.normalize.base import (
    COST_SOURCE_ESTIMATED,
    COST_SOURCE_RATE_CARD,
    COST_SOURCE_UNKNOWN,
)


def _msg_row(**overrides) -> dict:
    base = {
        "id": 300,
        "provider": "cursor",
        "project_id": 3,
        "session_id": "cursor-conv-id",
        "timestamp": "2026-04-25T12:00:00+00:00",
        "role": "assistant",
        "model": "cursor-auto",  # adapter's placeholder
        "speed": "standard",
        "input_tokens": 0,
        "output_tokens": 0,
        "cache_read_tokens": 0,
        "cache_create_tokens": 0,
        "content_text": "",
        "raw_json": "{}",
    }
    base.update(overrides)
    return base


def test_estimate_from_text_when_no_explicit_tokens() -> None:
    """Spec: text="hello world" → input ≈ 2 tokens, cost_source='estimated'."""
    row = _msg_row(content_text="hello world")
    events = list(CursorNormalizer().normalize(row))
    assert len(events) == 1
    ev = events[0]
    # len("hello world") == 11; 11 // 4 == 2
    assert ev["input_tokens"] == 2
    assert ev["output_tokens"] == 0
    assert ev["cost_source"] == COST_SOURCE_ESTIMATED


def test_explicit_token_count_uses_rate_card() -> None:
    """Spec: explicit tokenCount.inputTokens=500 → input=500, cost_source='rate_card'."""
    raw = {"tokenCount": {"inputTokens": 500, "outputTokens": 250}}
    row = _msg_row(
        model="claude-sonnet-4-5-20250929",
        raw_json=json.dumps(raw),
        content_text="some prompt",
    )
    events = list(CursorNormalizer().normalize(row))
    assert len(events) == 1
    ev = events[0]
    assert ev["input_tokens"] == 500
    assert ev["output_tokens"] == 250
    assert ev["cost_source"] == COST_SOURCE_RATE_CARD


def test_explicit_token_count_via_msg_row_field() -> None:
    """Synthetic test path: tokenCount can sit on msg_row directly."""
    row = _msg_row(
        model="claude-sonnet-4-5-20250929",
        tokenCount={"inputTokens": 500, "outputTokens": 0},
    )
    events = list(CursorNormalizer().normalize(row))
    assert events[0]["input_tokens"] == 500
    assert events[0]["cost_source"] == COST_SOURCE_RATE_CARD


def test_canonical_columns_used_when_set() -> None:
    """If the adapter already lifted real tokens onto columns, trust them."""
    row = _msg_row(
        model="claude-sonnet-4-5-20250929",
        input_tokens=1000,
        output_tokens=500,
    )
    events = list(CursorNormalizer().normalize(row))
    ev = events[0]
    assert ev["input_tokens"] == 1000
    assert ev["output_tokens"] == 500
    assert ev["cost_source"] == COST_SOURCE_RATE_CARD


def test_default_model_when_only_cursor_auto_placeholder() -> None:
    """Adapter writes ``cursor-auto`` when no model can be resolved.
    The normalizer substitutes the documented default.
    """
    row = _msg_row(content_text="hello world", model="cursor-auto")
    events = list(CursorNormalizer().normalize(row))
    assert events[0]["model"] == "composer-1"


def test_real_model_overrides_default() -> None:
    row = _msg_row(
        content_text="hi there",
        model="claude-sonnet-4-5-20250929",
    )
    events = list(CursorNormalizer().normalize(row))
    assert events[0]["model"] == "claude-sonnet-4-5-20250929"


def test_user_role_yields_zero_events() -> None:
    row = _msg_row(
        role="user",
        content_text="hello world",
    )
    assert list(CursorNormalizer().normalize(row)) == []


def test_empty_assistant_message_yields_zero_events() -> None:
    """Pure-empty assistant text → zero estimated tokens → drop."""
    row = _msg_row(content_text="")
    assert list(CursorNormalizer().normalize(row)) == []


def test_estimated_short_text_yields_at_least_zero() -> None:
    """A 3-char message estimates to 0 tokens — caller drops the row."""
    row = _msg_row(content_text="abc")
    # 3 // 4 == 0 → no usage → drop
    assert list(CursorNormalizer().normalize(row)) == []


def test_provenance_fields_in_raw_extras() -> None:
    raw = {
        "conversationId": "conv-123",
        "composerData": {"composerId": "c1"},
        "cost_source": "estimated",
        "tokenCount": {"inputTokens": 0, "outputTokens": 0},
    }
    row = _msg_row(content_text="hello world", raw_json=json.dumps(raw))
    events = list(CursorNormalizer().normalize(row))
    extras = json.loads(events[0]["raw_extras"])
    assert extras["conversationId"] == "conv-123"
    assert extras["composerData"] == {"composerId": "c1"}


def test_unknown_model_stamps_cost_source_unknown_with_explicit_tokens() -> None:
    row = _msg_row(
        model="cursor-future-2030",
        tokenCount={"inputTokens": 100, "outputTokens": 100},
    )
    events = list(CursorNormalizer().normalize(row))
    assert events[0]["cost_source"] == COST_SOURCE_UNKNOWN
