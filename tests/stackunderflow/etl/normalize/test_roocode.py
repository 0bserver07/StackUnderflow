"""RooCodeNormalizer — Cline-family wrapper, only provider_name differs."""

from __future__ import annotations

import json

from stackunderflow.etl.normalize import RooCodeNormalizer


def _msg_row(**overrides) -> dict:
    base = {
        "id": 1500,
        "provider": "roocode",
        "project_id": 15,
        "session_id": "roo-task-id",
        "timestamp": "2026-04-26T00:00:00+00:00",
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


def test_provider_name_is_roocode() -> None:
    assert RooCodeNormalizer.provider_name == "roocode"


def test_api_req_started_tokens_parsed_from_text_blob() -> None:
    """Cline-shape api_req_started.text JSON parses identically."""
    text = json.dumps({
        "tokensIn": 800,
        "tokensOut": 400,
        "cacheReads": 600,
        "cacheWrites": 200,
        "cost": 0.0066,
    })
    raw = {"text": text}
    row = _msg_row(raw_json=json.dumps(raw))
    events = list(RooCodeNormalizer().normalize(row))
    assert len(events) == 1
    ev = events[0]
    assert ev["input_tokens"] == 800
    assert ev["output_tokens"] == 400
    assert ev["cache_read_tokens"] == 600
    assert ev["cache_create_tokens"] == 200
    assert ev["provider"] == "roocode"


def test_user_role_yields_zero_events() -> None:
    row = _msg_row(role="user")
    assert list(RooCodeNormalizer().normalize(row)) == []


def test_no_tokens_yields_zero_events() -> None:
    row = _msg_row()
    assert list(RooCodeNormalizer().normalize(row)) == []


def test_canonical_columns_fallback() -> None:
    row = _msg_row(input_tokens=100, output_tokens=50)
    events = list(RooCodeNormalizer().normalize(row))
    assert events[0]["input_tokens"] == 100
