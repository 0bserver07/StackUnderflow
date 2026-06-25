"""ClaudeNormalizer — Anthropic 4-token shape passes through unchanged."""

from __future__ import annotations

import pytest

from stackunderflow.etl.normalize import ClaudeNormalizer
from stackunderflow.etl.normalize.base import (
    COST_SOURCE_RATE_CARD,
    COST_SOURCE_UNKNOWN,
)


def _msg_row(**overrides) -> dict:
    base = {
        "id": 100,
        "provider": "claude",
        "project_id": 1,
        "session_id": "sess-abc",
        "timestamp": "2026-04-25T10:30:00+00:00",
        "role": "assistant",
        "model": "claude-sonnet-4-5-20250929",
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


def test_assistant_with_usage_yields_one_event_with_token_shape() -> None:
    row = _msg_row(
        input_tokens=1234,
        output_tokens=567,
        cache_read_tokens=4096,
        cache_create_tokens=2048,
    )

    events = list(ClaudeNormalizer().normalize(row))

    assert len(events) == 1
    ev = events[0]
    assert ev["source_message_fk"] == 100
    assert ev["provider"] == "claude"
    assert ev["session_id"] == "sess-abc"
    assert ev["project_id"] == 1
    assert ev["ts"] == "2026-04-25T10:30:00+00:00"
    assert ev["day"] == "2026-04-25"
    assert ev["model"] == "claude-sonnet-4-5-20250929"
    assert ev["speed"] == "standard"
    assert ev["role"] == "assistant"
    # 4-token shape forwarded unchanged
    assert ev["input_tokens"] == 1234
    assert ev["output_tokens"] == 567
    assert ev["cache_read_tokens"] == 4096
    assert ev["cache_create_tokens"] == 2048
    # Cost computed once during normalization
    assert ev["cost_usd"] > 0.0
    assert ev["cost_source"] == COST_SOURCE_RATE_CARD


def test_user_message_yields_zero_events() -> None:
    row = _msg_row(role="user", input_tokens=10, output_tokens=0)
    assert list(ClaudeNormalizer().normalize(row)) == []


def test_assistant_missing_usage_yields_zero_events() -> None:
    """Assistant rows with all-zero token counts (e.g. tool-result
    attachments, error stubs) are not billable and must not pass through.
    """
    row = _msg_row()  # all zeros
    assert list(ClaudeNormalizer().normalize(row)) == []


def test_assistant_with_no_model_yields_zero_events() -> None:
    """Synthetic placeholder rows (model stripped to None upstream) are skipped."""
    row = _msg_row(model=None, input_tokens=10, output_tokens=10)
    assert list(ClaudeNormalizer().normalize(row)) == []


def test_unknown_model_stamps_cost_source_unknown() -> None:
    row = _msg_row(
        input_tokens=100,
        output_tokens=100,
        model="claude-future-2030",
    )
    events = list(ClaudeNormalizer().normalize(row))
    assert len(events) == 1
    assert events[0]["cost_source"] == COST_SOURCE_UNKNOWN
    # Spec invariant (docs/specs/session-schema-v1.md): an unknown model
    # contributes 0 dollars — never a phantom Anthropic-heuristic fallback.
    # Regression for the cost_source='unknown'-with-nonzero-cost drift.
    assert events[0]["cost_usd"] == 0.0


def test_speed_passes_through() -> None:
    row = _msg_row(
        input_tokens=100,
        output_tokens=100,
        model="claude-opus-4-5-20251101",
        speed="fast",
    )
    events = list(ClaudeNormalizer().normalize(row))
    assert events[0]["speed"] == "fast"


@pytest.mark.parametrize(
    "ts,expected_day",
    [
        ("2026-04-25T10:30:00+00:00", "2026-04-25"),
        ("2026-01-01T00:00:00Z", "2026-01-01"),
        ("", ""),
        ("not-a-date", ""),
    ],
)
def test_day_derived_from_timestamp(ts: str, expected_day: str) -> None:
    row = _msg_row(timestamp=ts, input_tokens=10, output_tokens=10)
    events = list(ClaudeNormalizer().normalize(row))
    if events:
        assert events[0]["day"] == expected_day
