"""GeminiNormalizer — cached subtraction + thoughts fold into output."""

from __future__ import annotations

import json

from stackunderflow.etl.normalize import GeminiNormalizer
from stackunderflow.etl.normalize.base import (
    COST_SOURCE_RATE_CARD,
    COST_SOURCE_UNKNOWN,
)


def _msg_row(**overrides) -> dict:
    base = {
        "id": 800,
        "provider": "gemini",
        "project_id": 8,
        "session_id": "gemini-sess",
        "timestamp": "2026-04-25T17:00:00+00:00",
        "role": "assistant",
        "model": "gemini-2.5-pro",
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
    assert list(GeminiNormalizer().normalize(row)) == []


def test_subtract_cached_from_input() -> None:
    """Spec: prompt=1000, cached=300 → input=700, cache_read=300."""
    row = _msg_row(
        promptTokenCount=1000,
        cachedContentTokenCount=300,
        candidatesTokenCount=500,
        thoughtsTokenCount=0,
    )
    events = list(GeminiNormalizer().normalize(row))
    assert len(events) == 1
    ev = events[0]
    assert ev["input_tokens"] == 700
    assert ev["output_tokens"] == 500
    assert ev["cache_read_tokens"] == 300
    assert ev["cache_create_tokens"] == 0


def test_fold_thoughts_into_output() -> None:
    """Spec: candidates=500, thoughts=200 → output=700."""
    row = _msg_row(
        promptTokenCount=100,
        cachedContentTokenCount=0,
        candidatesTokenCount=500,
        thoughtsTokenCount=200,
    )
    events = list(GeminiNormalizer().normalize(row))
    assert events[0]["output_tokens"] == 700
    assert events[0]["input_tokens"] == 100


def test_combined_subtract_and_fold() -> None:
    row = _msg_row(
        promptTokenCount=1000,
        cachedContentTokenCount=300,
        candidatesTokenCount=500,
        thoughtsTokenCount=200,
    )
    events = list(GeminiNormalizer().normalize(row))
    ev = events[0]
    assert ev["input_tokens"] == 700
    assert ev["output_tokens"] == 700
    assert ev["cache_read_tokens"] == 300
    assert ev["cache_create_tokens"] == 0


def test_raw_json_usage_metadata_path() -> None:
    """Production JSONL ≥0.39 path: usageMetadata in raw_json."""
    raw = {
        "usageMetadata": {
            "promptTokenCount": 1000,
            "cachedContentTokenCount": 300,
            "candidatesTokenCount": 500,
            "thoughtsTokenCount": 200,
        }
    }
    row = _msg_row(raw_json=json.dumps(raw))
    events = list(GeminiNormalizer().normalize(row))
    ev = events[0]
    assert ev["input_tokens"] == 700
    assert ev["output_tokens"] == 700
    assert ev["cache_read_tokens"] == 300


def test_legacy_tokens_block_in_raw_json() -> None:
    """Older single-JSON ≤0.38 path: ``tokens`` block."""
    raw = {
        "tokens": {
            "input": 1000,
            "output": 500,
            "cached": 300,
            "thoughts": 200,
        }
    }
    row = _msg_row(raw_json=json.dumps(raw))
    events = list(GeminiNormalizer().normalize(row))
    ev = events[0]
    assert ev["input_tokens"] == 700
    assert ev["output_tokens"] == 700
    assert ev["cache_read_tokens"] == 300


def test_role_gemini_accepted() -> None:
    """Some adapter versions emit role='gemini' for assistant turns."""
    row = _msg_row(role="gemini", promptTokenCount=100, candidatesTokenCount=50)
    events = list(GeminiNormalizer().normalize(row))
    assert len(events) == 1
    assert events[0]["input_tokens"] == 100
    assert events[0]["output_tokens"] == 50


def test_unknown_model_stamps_unknown() -> None:
    row = _msg_row(
        promptTokenCount=100,
        candidatesTokenCount=100,
        model="gemini-future-2030",
    )
    events = list(GeminiNormalizer().normalize(row))
    assert events[0]["cost_source"] == COST_SOURCE_UNKNOWN


def test_known_model_stamps_rate_card() -> None:
    row = _msg_row(
        promptTokenCount=100,
        candidatesTokenCount=100,
        model="claude-sonnet-4-5-20250929",  # ensure RATE_CARD lookup works
    )
    events = list(GeminiNormalizer().normalize(row))
    assert events[0]["cost_source"] == COST_SOURCE_RATE_CARD
