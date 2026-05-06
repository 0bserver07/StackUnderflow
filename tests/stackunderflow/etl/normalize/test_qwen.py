"""QwenNormalizer — Gemini-shaped usageMetadata with cached subtraction."""

from __future__ import annotations

import json

from stackunderflow.etl.normalize import QwenNormalizer
from stackunderflow.etl.normalize.base import COST_SOURCE_UNKNOWN


def _msg_row(**overrides) -> dict:
    base = {
        "id": 1400,
        "provider": "qwen",
        "project_id": 14,
        "session_id": "qwen-sess",
        "timestamp": "2026-04-25T23:00:00+00:00",
        "role": "assistant",
        "model": "qwen-3-coder",
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
    assert list(QwenNormalizer().normalize(row)) == []


def test_subtract_cached_from_input() -> None:
    """Spec: prompt - cached = fresh input."""
    row = _msg_row(
        promptTokenCount=1000,
        cachedContentTokenCount=300,
        candidatesTokenCount=500,
        thoughtsTokenCount=0,
    )
    events = list(QwenNormalizer().normalize(row))
    ev = events[0]
    assert ev["input_tokens"] == 700
    assert ev["output_tokens"] == 500
    assert ev["cache_read_tokens"] == 300
    assert ev["cache_create_tokens"] == 0


def test_fold_thoughts_into_output() -> None:
    row = _msg_row(
        promptTokenCount=200,
        cachedContentTokenCount=0,
        candidatesTokenCount=300,
        thoughtsTokenCount=150,
    )
    events = list(QwenNormalizer().normalize(row))
    ev = events[0]
    assert ev["input_tokens"] == 200
    assert ev["output_tokens"] == 450


def test_combined_subtract_and_fold() -> None:
    row = _msg_row(
        promptTokenCount=1000,
        cachedContentTokenCount=300,
        candidatesTokenCount=500,
        thoughtsTokenCount=200,
    )
    events = list(QwenNormalizer().normalize(row))
    ev = events[0]
    assert ev["input_tokens"] == 700
    assert ev["output_tokens"] == 700
    assert ev["cache_read_tokens"] == 300


def test_raw_json_usage_metadata_path() -> None:
    raw = {
        "usageMetadata": {
            "promptTokenCount": 500,
            "cachedContentTokenCount": 100,
            "candidatesTokenCount": 250,
            "thoughtsTokenCount": 50,
        }
    }
    row = _msg_row(raw_json=json.dumps(raw))
    events = list(QwenNormalizer().normalize(row))
    ev = events[0]
    assert ev["input_tokens"] == 400  # 500 - 100
    assert ev["output_tokens"] == 300  # 250 + 50
    assert ev["cache_read_tokens"] == 100


def test_unknown_model_stamps_unknown() -> None:
    row = _msg_row(promptTokenCount=100, candidatesTokenCount=50)
    events = list(QwenNormalizer().normalize(row))
    assert events[0]["cost_source"] == COST_SOURCE_UNKNOWN


def test_no_usage_yields_zero_events() -> None:
    row = _msg_row()
    assert list(QwenNormalizer().normalize(row)) == []


def test_raw_extras_preserves_function_call() -> None:
    raw = {
        "usageMetadata": {
            "promptTokenCount": 100,
            "candidatesTokenCount": 50,
            "cachedContentTokenCount": 0,
            "thoughtsTokenCount": 0,
        },
        "functionCall": {"name": "read_file", "args": {"path": "foo.py"}},
    }
    row = _msg_row(raw_json=json.dumps(raw))
    events = list(QwenNormalizer().normalize(row))
    extras = json.loads(events[0]["raw_extras"])
    assert extras["functionCall"]["name"] == "read_file"
