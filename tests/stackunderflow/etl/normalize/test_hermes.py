"""HermesNormalizer tests."""

from __future__ import annotations

import json

from stackunderflow.etl.normalize import HermesNormalizer
from stackunderflow.etl.normalize.base import (
    COST_SOURCE_RATE_CARD,
    COST_SOURCE_UNKNOWN,
)


def _msg_row(**overrides) -> dict:
    base = {
        "id": 1400,
        "provider": "hermes",
        "project_id": 14,
        "session_id": "hermes-sess",
        "timestamp": "2026-05-26T22:00:00+00:00",
        "role": "assistant",
        "model": "claude-3-5-sonnet",
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
    row = _msg_row(role="user", input_tokens=10)
    assert list(HermesNormalizer().normalize(row)) == []


def test_explicit_usage_canonical_mapping() -> None:
    """Spec: cacheWrite → cache_create_tokens, cacheRead → cache_read_tokens."""
    raw = {
        "message": {
            "usage": {
                "input": 600,
                "output": 300,
                "cacheRead": 900,
                "cacheWrite": 150,
            }
        }
    }
    row = _msg_row(raw_json=json.dumps(raw))
    events = list(HermesNormalizer().normalize(row))
    assert len(events) == 1
    ev = events[0]
    assert ev["input_tokens"] == 600
    assert ev["output_tokens"] == 300
    assert ev["cache_read_tokens"] == 900
    assert ev["cache_create_tokens"] == 150


def test_canonical_columns_fallback() -> None:
    row = _msg_row(
        input_tokens=100,
        output_tokens=50,
        cache_read_tokens=200,
        cache_create_tokens=25,
    )
    events = list(HermesNormalizer().normalize(row))
    ev = events[0]
    assert ev["input_tokens"] == 100
    assert ev["cache_read_tokens"] == 200


def test_no_usage_data_yields_zero_events() -> None:
    row = _msg_row()
    assert list(HermesNormalizer().normalize(row)) == []


def test_unknown_model_stamps_unknown() -> None:
    row = _msg_row(input_tokens=100, output_tokens=100, model="hermes-future-2030")
    events = list(HermesNormalizer().normalize(row))
    assert events[0]["cost_source"] == COST_SOURCE_UNKNOWN


def test_known_model_stamps_rate_card() -> None:
    row = _msg_row(input_tokens=100, output_tokens=100, model="claude-3-5-sonnet")
    events = list(HermesNormalizer().normalize(row))
    assert events[0]["cost_source"] == COST_SOURCE_RATE_CARD
