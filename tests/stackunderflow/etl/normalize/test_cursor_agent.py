"""CursorAgentNormalizer — always-estimated text/4 path."""

from __future__ import annotations

import json

from stackunderflow.etl.normalize import CursorAgentNormalizer
from stackunderflow.etl.normalize.base import COST_SOURCE_ESTIMATED


def _msg_row(**overrides) -> dict:
    base = {
        "id": 600,
        "provider": "cursor-agent",
        "project_id": 6,
        "session_id": "cursor-agent-sess",
        "timestamp": "2026-04-25T15:00:00+00:00",
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
    row = _msg_row(role="user", content_text="user message")
    assert list(CursorAgentNormalizer().normalize(row)) == []


def test_estimate_from_text_length() -> None:
    """Cursor Agent always estimates from text//4; even known models stamp estimated."""
    row = _msg_row(content_text="this is a cursor agent assistant response")
    events = list(CursorAgentNormalizer().normalize(row))
    assert len(events) == 1
    ev = events[0]
    # len("this is a cursor agent assistant response") == 41; 41 // 4 == 10
    assert ev["input_tokens"] == 10
    assert ev["output_tokens"] == 0
    assert ev["cost_source"] == COST_SOURCE_ESTIMATED


def test_provider_name() -> None:
    # provider_name must equal the adapter's provider string — the old
    # underscore value silently stranded every cursor-agent row.
    assert CursorAgentNormalizer.provider_name == "cursor-agent"


def test_no_text_yields_zero_events() -> None:
    row = _msg_row()
    assert list(CursorAgentNormalizer().normalize(row)) == []


def test_raw_extras_preserves_conversation_metadata() -> None:
    raw = {"conversationId": "conv-abc-123", "transcriptType": "jsonl"}
    row = _msg_row(content_text="response text", raw_json=json.dumps(raw))
    events = list(CursorAgentNormalizer().normalize(row))
    extras = json.loads(events[0]["raw_extras"])
    assert extras["conversationId"] == "conv-abc-123"
    assert extras["transcriptType"] == "jsonl"
