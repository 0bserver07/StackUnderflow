"""CopilotNormalizer — explicit transcript tokens vs. legacy estimation path."""

from __future__ import annotations

import json

from stackunderflow.etl.normalize import CopilotNormalizer
from stackunderflow.etl.normalize.base import (
    COST_SOURCE_ESTIMATED,
    COST_SOURCE_RATE_CARD,
    COST_SOURCE_UNKNOWN,
)


def _msg_row(**overrides) -> dict:
    base = {
        "id": 500,
        "provider": "copilot",
        "project_id": 5,
        "session_id": "copilot-sess",
        "timestamp": "2026-04-25T14:00:00+00:00",
        "role": "assistant",
        "model": "gpt-4o",
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
    row = _msg_row(role="user", output_tokens=10)
    assert list(CopilotNormalizer().normalize(row)) == []


def test_explicit_transcript_tokens_use_rate_card() -> None:
    row = _msg_row(input_tokens=300, output_tokens=200)
    events = list(CopilotNormalizer().normalize(row))
    assert len(events) == 1
    ev = events[0]
    assert ev["input_tokens"] == 300
    assert ev["output_tokens"] == 200
    assert ev["cache_read_tokens"] == 0
    assert ev["cache_create_tokens"] == 0
    assert ev["cost_source"] == COST_SOURCE_RATE_CARD


def test_legacy_event_estimates_from_text_length() -> None:
    """Legacy event without explicit output tokens — estimate from text."""
    row = _msg_row(content_text="hello copilot world response")
    events = list(CopilotNormalizer().normalize(row))
    assert len(events) == 1
    ev = events[0]
    # len("hello copilot world response") == 28; 28 // 4 == 7
    assert ev["output_tokens"] == 7
    assert ev["cost_source"] == COST_SOURCE_ESTIMATED


def test_data_subkey_in_raw_json_used() -> None:
    """VS Code transcript shape — tokens nested in ``data``."""
    raw = {"data": {"inputTokens": 400, "outputTokens": 250, "producer": "copilot-agent"}}
    row = _msg_row(raw_json=json.dumps(raw))
    events = list(CopilotNormalizer().normalize(row))
    assert len(events) == 1
    ev = events[0]
    assert ev["input_tokens"] == 400
    assert ev["output_tokens"] == 250
    extras = json.loads(ev["raw_extras"])
    assert extras["producer"] == "copilot-agent"


def test_unknown_model_stamps_unknown() -> None:
    row = _msg_row(input_tokens=100, output_tokens=100, model="copilot-mystery-2030")
    events = list(CopilotNormalizer().normalize(row))
    assert events[0]["cost_source"] == COST_SOURCE_UNKNOWN


def test_no_tokens_no_text_yields_zero_events() -> None:
    row = _msg_row()
    assert list(CopilotNormalizer().normalize(row)) == []
