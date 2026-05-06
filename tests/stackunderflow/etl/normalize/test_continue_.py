"""ContinueNormalizer — defensive SQLite-source path; trust columns or estimate."""

from __future__ import annotations

import json

from stackunderflow.etl.normalize import ContinueNormalizer
from stackunderflow.etl.normalize.base import (
    COST_SOURCE_ESTIMATED,
    COST_SOURCE_RATE_CARD,
    COST_SOURCE_UNKNOWN,
)


def _msg_row(**overrides) -> dict:
    base = {
        "id": 400,
        "provider": "continue",
        "project_id": 4,
        "session_id": "continue-sess",
        "timestamp": "2026-04-25T13:00:00+00:00",
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
    row = _msg_row(role="user", input_tokens=100)
    assert list(ContinueNormalizer().normalize(row)) == []


def test_explicit_tokens_use_rate_card() -> None:
    row = _msg_row(input_tokens=200, output_tokens=100)
    events = list(ContinueNormalizer().normalize(row))
    assert len(events) == 1
    ev = events[0]
    assert ev["input_tokens"] == 200
    assert ev["output_tokens"] == 100
    assert ev["cost_source"] == COST_SOURCE_RATE_CARD


def test_estimate_from_text_when_no_tokens() -> None:
    """No token columns → estimate input from text length / 4."""
    row = _msg_row(content_text="hello world from continue")
    events = list(ContinueNormalizer().normalize(row))
    assert len(events) == 1
    ev = events[0]
    # len("hello world from continue") == 25; 25 // 4 == 6
    assert ev["input_tokens"] == 6
    assert ev["output_tokens"] == 0
    assert ev["cost_source"] == COST_SOURCE_ESTIMATED


def test_unknown_model_stamps_unknown() -> None:
    row = _msg_row(input_tokens=100, output_tokens=100, model="continue-future-2030")
    events = list(ContinueNormalizer().normalize(row))
    assert events[0]["cost_source"] == COST_SOURCE_UNKNOWN


def test_no_tokens_no_text_yields_zero_events() -> None:
    row = _msg_row()
    assert list(ContinueNormalizer().normalize(row)) == []


def test_raw_extras_preserves_provider_quirks() -> None:
    raw = {"provider": "openai", "modelTitle": "Continue / GPT-4"}
    row = _msg_row(input_tokens=100, output_tokens=50, raw_json=json.dumps(raw))
    events = list(ContinueNormalizer().normalize(row))
    extras = json.loads(events[0]["raw_extras"])
    assert extras["provider"] == "openai"
    assert extras["modelTitle"] == "Continue / GPT-4"
