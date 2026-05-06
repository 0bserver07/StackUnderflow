"""KiroNormalizer — estimate from text length, always estimated."""

from __future__ import annotations

import json

from stackunderflow.etl.normalize import KiroNormalizer
from stackunderflow.etl.normalize.base import COST_SOURCE_ESTIMATED


def _msg_row(**overrides) -> dict:
    base = {
        "id": 1000,
        "provider": "kiro",
        "project_id": 10,
        "session_id": "kiro-workflow-id",
        "timestamp": "2026-04-25T19:00:00+00:00",
        "role": "assistant",
        "model": "claude-3-5-sonnet-20241022",
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
    row = _msg_row(role="user", content_text="user prompt")
    assert list(KiroNormalizer().normalize(row)) == []


def test_role_bot_accepted_as_assistant() -> None:
    """Kiro source format uses ``role='bot'`` for the assistant."""
    row = _msg_row(role="bot", content_text="bot response text")
    events = list(KiroNormalizer().normalize(row))
    assert len(events) == 1
    assert events[0]["cost_source"] == COST_SOURCE_ESTIMATED


def test_estimate_from_text_length() -> None:
    """Spec: text//4 estimation."""
    row = _msg_row(content_text="kiro agent assistant response message")
    events = list(KiroNormalizer().normalize(row))
    assert len(events) == 1
    ev = events[0]
    # len("kiro agent assistant response message") == 37; 37 // 4 == 9
    assert ev["input_tokens"] == 9
    assert ev["output_tokens"] == 0
    assert ev["cost_source"] == COST_SOURCE_ESTIMATED


def test_no_text_yields_zero_events() -> None:
    row = _msg_row()
    assert list(KiroNormalizer().normalize(row)) == []


def test_raw_extras_preserves_workflow_metadata() -> None:
    raw = {"executionId": "exec-123", "workflowId": "wf-456"}
    row = _msg_row(content_text="response", raw_json=json.dumps(raw))
    events = list(KiroNormalizer().normalize(row))
    extras = json.loads(events[0]["raw_extras"])
    assert extras["executionId"] == "exec-123"
    assert extras["workflowId"] == "wf-456"
