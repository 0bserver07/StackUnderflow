"""ClineNormalizer — per-task, per-event grain preserved.

The Cline adapter splits each ui_messages.json task into one Record
per ``api_req_started`` event, so per-task with N events lands as N
messages-table rows. Normalising those rows yields N usage_events.
"""

from __future__ import annotations

import json

from stackunderflow.etl.normalize import ClineNormalizer
from stackunderflow.etl.normalize.base import COST_SOURCE_RATE_CARD


def _api_req_event(
    *,
    tokens_in: int,
    tokens_out: int,
    cache_writes: int = 0,
    cache_reads: int = 0,
    cost: float = 0.0,
) -> dict:
    """Build a single ``api_req_started`` ui_messages event."""
    return {
        "type": "say",
        "say": "api_req_started",
        "ts": 1714044000000,
        "text": json.dumps({
            "tokensIn": tokens_in,
            "tokensOut": tokens_out,
            "cacheWrites": cache_writes,
            "cacheReads": cache_reads,
            "cost": cost,
        }),
    }


def _msg_row(api_event: dict, **overrides) -> dict:
    """One messages-table row representing one api_req_started event."""
    base = {
        "id": 400,
        "provider": "cline",
        "project_id": 4,
        "session_id": "task-uuid",
        "timestamp": "2026-04-25T13:00:00+00:00",
        "role": "assistant",
        "model": "claude-sonnet-4-5-20250929",
        "speed": "standard",
        # Adapter pre-fills the columns from text JSON; we keep them in
        # sync with the event payload here so both code paths match.
        "input_tokens": 0,
        "output_tokens": 0,
        "cache_read_tokens": 0,
        "cache_create_tokens": 0,
        "content_text": "",
        "raw_json": json.dumps(api_event),
    }
    base.update(overrides)
    return base


def test_three_api_req_events_yield_three_usage_events() -> None:
    """Spec contract: task with 3 api_req_started → 3 events,
    tokens parsed from each event's text JSON."""
    rows = [
        _msg_row(
            _api_req_event(tokens_in=100, tokens_out=50, cost=0.001),
            id=401,
        ),
        _msg_row(
            _api_req_event(tokens_in=200, tokens_out=80, cost=0.002),
            id=402,
        ),
        _msg_row(
            _api_req_event(tokens_in=400, tokens_out=120, cost=0.003),
            id=403,
        ),
    ]

    norm = ClineNormalizer()
    all_events = []
    for row in rows:
        all_events.extend(norm.normalize(row))

    assert len(all_events) == 3
    assert [ev["input_tokens"] for ev in all_events] == [100, 200, 400]
    assert [ev["output_tokens"] for ev in all_events] == [50, 80, 120]
    assert [ev["source_message_fk"] for ev in all_events] == [401, 402, 403]


def test_cache_fields_carry_through() -> None:
    row = _msg_row(_api_req_event(
        tokens_in=100,
        tokens_out=50,
        cache_writes=10,
        cache_reads=20,
    ))
    events = list(ClineNormalizer().normalize(row))
    assert len(events) == 1
    ev = events[0]
    assert ev["cache_create_tokens"] == 10
    assert ev["cache_read_tokens"] == 20


def test_text_directly_on_msg_row() -> None:
    """Synthetic test path: text JSON can sit on msg_row directly
    instead of being wrapped in raw_json.
    """
    row = _msg_row(
        _api_req_event(tokens_in=300, tokens_out=200),
        text=json.dumps({
            "tokensIn": 300,
            "tokensOut": 200,
            "cacheWrites": 0,
            "cacheReads": 0,
        }),
        raw_json=None,
    )
    events = list(ClineNormalizer().normalize(row))
    assert events[0]["input_tokens"] == 300
    assert events[0]["output_tokens"] == 200


def test_falls_back_to_columns_when_text_missing() -> None:
    row = _msg_row(
        {},  # no text field at all
        input_tokens=500,
        output_tokens=250,
        cache_create_tokens=10,
        cache_read_tokens=20,
        raw_json="{}",
    )
    events = list(ClineNormalizer().normalize(row))
    ev = events[0]
    assert ev["input_tokens"] == 500
    assert ev["output_tokens"] == 250
    assert ev["cache_create_tokens"] == 10
    assert ev["cache_read_tokens"] == 20


def test_user_role_yields_zero_events() -> None:
    row = _msg_row(
        _api_req_event(tokens_in=100, tokens_out=50),
        role="user",
    )
    assert list(ClineNormalizer().normalize(row)) == []


def test_zero_tokens_yields_zero_events() -> None:
    row = _msg_row(_api_req_event(tokens_in=0, tokens_out=0))
    assert list(ClineNormalizer().normalize(row)) == []


def test_default_model_when_unset() -> None:
    row = _msg_row(
        _api_req_event(tokens_in=100, tokens_out=50),
        model="",
    )
    events = list(ClineNormalizer().normalize(row))
    assert events[0]["model"] == "cline-auto"


def test_known_model_stamps_rate_card() -> None:
    row = _msg_row(_api_req_event(tokens_in=100, tokens_out=50))
    events = list(ClineNormalizer().normalize(row))
    assert events[0]["cost_source"] == COST_SOURCE_RATE_CARD
    assert events[0]["cost_usd"] > 0.0


def test_cost_preserved_in_raw_extras() -> None:
    row = _msg_row(_api_req_event(
        tokens_in=100, tokens_out=50, cost=0.005,
    ))
    events = list(ClineNormalizer().normalize(row))
    extras = json.loads(events[0]["raw_extras"])
    assert extras["cost"] == 0.005
