"""OpenClawNormalizer — explicit usage block with cacheRead/cacheWrite."""

from __future__ import annotations

import json

from stackunderflow.etl.normalize import OpenClawNormalizer
from stackunderflow.etl.normalize.base import (
    COST_SOURCE_RATE_CARD,
    COST_SOURCE_UNKNOWN,
)


def _msg_row(**overrides) -> dict:
    base = {
        "id": 1100,
        "provider": "openclaw",
        "project_id": 11,
        "session_id": "openclaw-sess",
        "timestamp": "2026-04-25T20:00:00+00:00",
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
    assert list(OpenClawNormalizer().normalize(row)) == []


def test_explicit_usage_block_canonical_mapping() -> None:
    """Spec: cacheWrite → cache_create_tokens, cacheRead → cache_read_tokens."""
    raw = {
        "message": {
            "usage": {
                "input": 800,
                "output": 400,
                "cacheRead": 1200,
                "cacheWrite": 200,
            }
        }
    }
    row = _msg_row(raw_json=json.dumps(raw))
    events = list(OpenClawNormalizer().normalize(row))
    assert len(events) == 1
    ev = events[0]
    assert ev["input_tokens"] == 800
    assert ev["output_tokens"] == 400
    assert ev["cache_read_tokens"] == 1200
    assert ev["cache_create_tokens"] == 200
    assert ev["cost_source"] == COST_SOURCE_RATE_CARD


def test_canonical_columns_fallback() -> None:
    row = _msg_row(
        input_tokens=200,
        output_tokens=100,
        cache_read_tokens=50,
        cache_create_tokens=25,
    )
    events = list(OpenClawNormalizer().normalize(row))
    ev = events[0]
    assert ev["input_tokens"] == 200
    assert ev["cache_read_tokens"] == 50
    assert ev["cache_create_tokens"] == 25


def test_embedded_cost_preserved_in_raw_extras() -> None:
    raw = {
        "message": {
            "provider": "anthropic",
            "usage": {
                "input": 100,
                "output": 50,
                "cacheRead": 0,
                "cacheWrite": 0,
                "cost": {"total": 0.0042},
            },
        }
    }
    row = _msg_row(raw_json=json.dumps(raw))
    events = list(OpenClawNormalizer().normalize(row))
    extras = json.loads(events[0]["raw_extras"])
    assert extras["embeddedCost"] == {"total": 0.0042}
    assert extras["provider"] == "anthropic"


def test_no_usage_data_yields_zero_events() -> None:
    row = _msg_row()
    assert list(OpenClawNormalizer().normalize(row)) == []


def test_unknown_model_stamps_unknown() -> None:
    row = _msg_row(
        input_tokens=100,
        output_tokens=100,
        model="openclaw-future-2030",
    )
    events = list(OpenClawNormalizer().normalize(row))
    assert events[0]["cost_source"] == COST_SOURCE_UNKNOWN
