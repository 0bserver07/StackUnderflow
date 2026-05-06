"""DroidNormalizer — adapter pre-distributes session totals; we trust columns."""

from __future__ import annotations

import json

from stackunderflow.etl.normalize import DroidNormalizer
from stackunderflow.etl.normalize.base import (
    COST_SOURCE_ESTIMATED,
    COST_SOURCE_RATE_CARD,
    COST_SOURCE_UNKNOWN,
)


def _msg_row(**overrides) -> dict:
    base = {
        "id": 700,
        "provider": "droid",
        "project_id": 7,
        "session_id": "droid-sess",
        "timestamp": "2026-04-25T16:00:00+00:00",
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


def test_user_role_yields_zero_events() -> None:
    row = _msg_row(role="user", input_tokens=50)
    assert list(DroidNormalizer().normalize(row)) == []


def test_pre_distributed_tokens_use_rate_card() -> None:
    """Adapter has already split session-level totals across messages."""
    row = _msg_row(
        input_tokens=500,
        output_tokens=300,
        cache_read_tokens=100,
        cache_create_tokens=50,
    )
    events = list(DroidNormalizer().normalize(row))
    assert len(events) == 1
    ev = events[0]
    assert ev["input_tokens"] == 500
    assert ev["output_tokens"] == 300
    assert ev["cache_read_tokens"] == 100
    assert ev["cache_create_tokens"] == 50
    assert ev["cost_source"] == COST_SOURCE_RATE_CARD


def test_thinking_tokens_folded_into_output() -> None:
    """Spec: thinkingTokens (Droid's reasoning slot) folds into output."""
    row = _msg_row(input_tokens=200, output_tokens=300, thinking_tokens=100)
    events = list(DroidNormalizer().normalize(row))
    assert len(events) == 1
    assert events[0]["output_tokens"] == 400


def test_thinking_tokens_from_raw_json() -> None:
    raw = {"tokenUsage": {"thinkingTokens": 75}}
    row = _msg_row(input_tokens=100, output_tokens=200, raw_json=json.dumps(raw))
    events = list(DroidNormalizer().normalize(row))
    assert events[0]["output_tokens"] == 275


def test_estimate_when_no_settings_data() -> None:
    """Adapter couldn't read settings — estimate from text."""
    row = _msg_row(content_text="droid assistant message text")
    events = list(DroidNormalizer().normalize(row))
    assert len(events) == 1
    ev = events[0]
    # len("droid assistant message text") == 28; 28 // 4 == 7
    assert ev["input_tokens"] == 7
    assert ev["cost_source"] == COST_SOURCE_ESTIMATED


def test_unknown_model_stamps_unknown() -> None:
    row = _msg_row(input_tokens=100, output_tokens=100, model="droid-mystery-2030")
    events = list(DroidNormalizer().normalize(row))
    assert events[0]["cost_source"] == COST_SOURCE_UNKNOWN
