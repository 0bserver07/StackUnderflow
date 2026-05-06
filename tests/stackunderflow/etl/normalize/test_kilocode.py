"""KiloCodeNormalizer — Cline-family wrapper, only provider_name differs."""

from __future__ import annotations

import json

from stackunderflow.etl.normalize import KiloCodeNormalizer


def _msg_row(**overrides) -> dict:
    base = {
        "id": 900,
        "provider": "kilocode",
        "project_id": 9,
        "session_id": "kilo-task-id",
        "timestamp": "2026-04-25T18:00:00+00:00",
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


def test_provider_name_is_kilocode() -> None:
    assert KiloCodeNormalizer.provider_name == "kilocode"


def test_user_role_yields_zero_events() -> None:
    row = _msg_row(role="user")
    assert list(KiloCodeNormalizer().normalize(row)) == []


def test_api_req_started_tokens_parsed_from_text_blob() -> None:
    """Cline-shape parsing — kilocode shares the api_req_started.text JSON."""
    text = json.dumps({
        "tokensIn": 500,
        "tokensOut": 200,
        "cacheReads": 1000,
        "cacheWrites": 300,
        "cost": 0.0042,
    })
    raw = {"text": text}
    row = _msg_row(raw_json=json.dumps(raw))
    events = list(KiloCodeNormalizer().normalize(row))
    assert len(events) == 1
    ev = events[0]
    assert ev["input_tokens"] == 500
    assert ev["output_tokens"] == 200
    assert ev["cache_read_tokens"] == 1000
    assert ev["cache_create_tokens"] == 300
    assert ev["provider"] == "kilocode"


def test_canonical_columns_fallback() -> None:
    row = _msg_row(input_tokens=100, output_tokens=50)
    events = list(KiloCodeNormalizer().normalize(row))
    assert len(events) == 1
    assert events[0]["input_tokens"] == 100


def test_no_tokens_yields_zero_events() -> None:
    row = _msg_row()
    assert list(KiloCodeNormalizer().normalize(row)) == []
