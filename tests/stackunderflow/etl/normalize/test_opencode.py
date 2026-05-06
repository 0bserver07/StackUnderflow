"""OpenCodeNormalizer — SQLite source; tokens.{input,output,reasoning,cache}."""

from __future__ import annotations

import json

from stackunderflow.etl.normalize import OpenCodeNormalizer
from stackunderflow.etl.normalize.base import (
    COST_SOURCE_RATE_CARD,
    COST_SOURCE_UNKNOWN,
)


def _msg_row(**overrides) -> dict:
    base = {
        "id": 1200,
        "provider": "opencode",
        "project_id": 12,
        "session_id": "opencode-sess",
        "timestamp": "2026-04-25T21:00:00+00:00",
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
    row = _msg_row(role="user", input_tokens=10)
    assert list(OpenCodeNormalizer().normalize(row)) == []


def test_canonical_mapping_with_reasoning_fold() -> None:
    """Spec: tokens.reasoning folds into output; cache.{read,write}."""
    raw = {
        "tokens": {
            "input": 1000,
            "output": 400,
            "reasoning": 200,
            "cache": {"read": 800, "write": 100},
        }
    }
    row = _msg_row(raw_json=json.dumps(raw))
    events = list(OpenCodeNormalizer().normalize(row))
    assert len(events) == 1
    ev = events[0]
    assert ev["input_tokens"] == 1000
    assert ev["output_tokens"] == 600  # 400 + 200 reasoning
    assert ev["cache_read_tokens"] == 800
    assert ev["cache_create_tokens"] == 100
    assert ev["cost_source"] == COST_SOURCE_RATE_CARD


def test_data_wrapped_payload() -> None:
    """Some adapter versions persist the payload nested under ``data``."""
    raw = {
        "data": {
            "modelID": "claude-sonnet-4-5-20250929",
            "tokens": {
                "input": 500,
                "output": 250,
                "reasoning": 0,
                "cache": {"read": 0, "write": 0},
            },
        }
    }
    row = _msg_row(raw_json=json.dumps(raw))
    events = list(OpenCodeNormalizer().normalize(row))
    assert events[0]["input_tokens"] == 500
    assert events[0]["output_tokens"] == 250


def test_canonical_columns_fallback_when_no_raw() -> None:
    row = _msg_row(input_tokens=100, output_tokens=50, cache_read_tokens=20)
    events = list(OpenCodeNormalizer().normalize(row))
    ev = events[0]
    assert ev["input_tokens"] == 100
    assert ev["cache_read_tokens"] == 20


def test_no_tokens_yields_zero_events() -> None:
    row = _msg_row()
    assert list(OpenCodeNormalizer().normalize(row)) == []


def test_unknown_model_stamps_unknown() -> None:
    row = _msg_row(
        input_tokens=100,
        output_tokens=100,
        model="opencode-mystery-2030",
    )
    events = list(OpenCodeNormalizer().normalize(row))
    assert events[0]["cost_source"] == COST_SOURCE_UNKNOWN


def test_embedded_cost_in_raw_extras() -> None:
    raw = {
        "data": {
            "modelID": "claude-sonnet-4-5-20250929",
            "tokens": {"input": 100, "output": 50, "reasoning": 0, "cache": {"read": 0, "write": 0}},
            "cost": 0.0123,
        }
    }
    row = _msg_row(raw_json=json.dumps(raw))
    events = list(OpenCodeNormalizer().normalize(row))
    extras = json.loads(events[0]["raw_extras"])
    assert extras["embeddedCost"] == 0.0123
    assert extras["modelID"] == "claude-sonnet-4-5-20250929"
