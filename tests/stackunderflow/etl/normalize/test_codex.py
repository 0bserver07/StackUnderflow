"""CodexNormalizer — OpenAI quirks resolved here.

Spec contract:
* ``input_tokens`` is reduced by ``cached_input_tokens``.
* ``reasoning_output_tokens`` folds into ``output_tokens``.
* ``cached_input_tokens`` becomes ``cache_read_tokens``.
* ``cache_create_tokens`` stays 0 — OpenAI does not bill prompt-cache writes.
"""

from __future__ import annotations

import json

from stackunderflow.etl.normalize import CodexNormalizer
from stackunderflow.etl.normalize.base import COST_SOURCE_UNKNOWN


def _msg_row(**overrides) -> dict:
    base = {
        "id": 200,
        "provider": "codex",
        "project_id": 2,
        "session_id": "codex-sess",
        "timestamp": "2026-04-25T11:00:00+00:00",
        "role": "assistant",
        "model": "gpt-5-codex",
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


def test_subtract_cached_from_input() -> None:
    """Spec: input=1000, cached=300 → input=700, cache_read=300."""
    row = _msg_row(
        input_tokens=1000,
        cached_input_tokens=300,
        output_tokens=500,
        reasoning_output_tokens=0,
    )
    events = list(CodexNormalizer().normalize(row))
    assert len(events) == 1
    ev = events[0]
    assert ev["input_tokens"] == 700
    assert ev["cache_read_tokens"] == 300
    assert ev["cache_create_tokens"] == 0


def test_fold_reasoning_into_output() -> None:
    """Spec: reasoning=200, output=500 → output=700."""
    row = _msg_row(
        input_tokens=100,
        cached_input_tokens=0,
        output_tokens=500,
        reasoning_output_tokens=200,
    )
    events = list(CodexNormalizer().normalize(row))
    assert len(events) == 1
    assert events[0]["output_tokens"] == 700


def test_combined_subtract_and_fold() -> None:
    row = _msg_row(
        input_tokens=1000,
        cached_input_tokens=300,
        output_tokens=500,
        reasoning_output_tokens=200,
    )
    events = list(CodexNormalizer().normalize(row))
    ev = events[0]
    assert ev["input_tokens"] == 700
    assert ev["output_tokens"] == 700
    assert ev["cache_read_tokens"] == 300
    assert ev["cache_create_tokens"] == 0


def test_canonical_columns_path_when_no_raw_keys_present() -> None:
    """When the row only carries the adapter's pre-canonicalised columns
    (no ``cached_input_tokens`` / ``reasoning_output_tokens``), the
    normalizer trusts those columns directly.
    """
    row = _msg_row(
        input_tokens=700,
        output_tokens=700,
        cache_read_tokens=300,
        cache_create_tokens=0,
    )
    events = list(CodexNormalizer().normalize(row))
    assert len(events) == 1
    ev = events[0]
    assert ev["input_tokens"] == 700
    assert ev["output_tokens"] == 700
    assert ev["cache_read_tokens"] == 300


def test_raw_json_payload_path() -> None:
    """Production ingest path: raw OpenAI shape lives in raw_json."""
    raw = {
        "type": "event_msg",
        "payload": {
            "type": "token_count",
            "info": {
                "last_token_usage": {
                    "input_tokens": 1000,
                    "cached_input_tokens": 300,
                    "output_tokens": 500,
                    "reasoning_output_tokens": 200,
                }
            },
        },
    }
    row = _msg_row(raw_json=json.dumps(raw))
    events = list(CodexNormalizer().normalize(row))
    assert len(events) == 1
    ev = events[0]
    assert ev["input_tokens"] == 700
    assert ev["output_tokens"] == 700
    assert ev["cache_read_tokens"] == 300


def test_user_role_yields_zero_events() -> None:
    row = _msg_row(role="user", input_tokens=10, output_tokens=0)
    assert list(CodexNormalizer().normalize(row)) == []


def test_zero_tokens_yields_zero_events() -> None:
    row = _msg_row()
    assert list(CodexNormalizer().normalize(row)) == []


def test_unknown_model_stamps_cost_source_unknown() -> None:
    row = _msg_row(
        input_tokens=100,
        output_tokens=100,
        model="gpt-future-2030",
    )
    events = list(CodexNormalizer().normalize(row))
    # The pricer's family heuristic still yields *some* number for any
    # gpt-* id, but the model is not in the canonical RATE_CARD so we
    # stamp ``unknown`` to keep the source-of-truth flag honest.
    assert events[0]["cost_source"] == COST_SOURCE_UNKNOWN
    assert events[0]["cost_usd"] >= 0.0


def test_provenance_fields_in_raw_extras() -> None:
    raw = {
        "service_tier": "standard",
        "model_provider": "openai",
        "originator": "codex-tui",
    }
    row = _msg_row(
        input_tokens=100,
        cached_input_tokens=0,
        output_tokens=100,
        reasoning_output_tokens=0,
        raw_json=json.dumps(raw),
    )
    events = list(CodexNormalizer().normalize(row))
    assert events[0]["raw_extras"] is not None
    extras = json.loads(events[0]["raw_extras"])
    assert extras["service_tier"] == "standard"
    assert extras["originator"] == "codex-tui"


def test_cost_computed_once_and_stored() -> None:
    row = _msg_row(
        input_tokens=1000,
        cached_input_tokens=300,
        output_tokens=500,
        reasoning_output_tokens=200,
    )
    events = list(CodexNormalizer().normalize(row))
    ev = events[0]
    assert ev["cost_usd"] > 0.0
    # Recompute manually to confirm it matches what compute_cost would do
    # against the post-normalisation tokens.
    from stackunderflow.infra.costs import compute_cost
    expected = compute_cost(
        {"input": 700, "output": 700, "cache_read": 300, "cache_creation": 0},
        "gpt-5-codex",
        provider="openai",
    )
    assert ev["cost_usd"] == expected["total_cost"]
