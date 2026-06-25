"""GrokNormalizer — estimate from text length, always estimated, $0 cost.

Grok records no token usage, so tokens are estimated (content // 4) and
``cost_source`` is ``estimated``. ``grok-build`` has no rate-card entry,
so ``cost_usd`` must resolve to $0 (no phantom Anthropic-fallback cost)
until a real xAI rate is added.
"""

from __future__ import annotations

import json

from stackunderflow.etl.normalize import GrokNormalizer
from stackunderflow.etl.normalize.base import COST_SOURCE_ESTIMATED


def _msg_row(**overrides) -> dict:
    base = {
        "id": 2000,
        "provider": "grok",
        "project_id": 20,
        "session_id": "019eff73-6f8f-7830-a33a-fc37e624d51b",
        "timestamp": "2026-06-25T15:43:35+00:00",
        "role": "assistant",
        "model": "grok-build",
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
    assert list(GrokNormalizer().normalize(row)) == []


def test_tool_role_yields_zero_events() -> None:
    row = _msg_row(role="tool", content_text="tool result blob")
    assert list(GrokNormalizer().normalize(row)) == []


def test_assistant_role_is_billed_and_estimated() -> None:
    row = _msg_row(role="assistant", content_text="here is the answer")
    events = list(GrokNormalizer().normalize(row))
    assert len(events) == 1
    assert events[0]["cost_source"] == COST_SOURCE_ESTIMATED


def test_reasoning_role_is_billed() -> None:
    """The adapter preserves the ``reasoning`` role; it's a billable turn."""
    row = _msg_row(role="reasoning", content_text="chain of thought text")
    events = list(GrokNormalizer().normalize(row))
    assert len(events) == 1
    assert events[0]["cost_source"] == COST_SOURCE_ESTIMATED


def test_estimate_from_text_length_into_output() -> None:
    """Spec: text // 4 estimation; the model turn's text is output."""
    row = _msg_row(content_text="grok build coding agent assistant reply")
    events = list(GrokNormalizer().normalize(row))
    assert len(events) == 1
    ev = events[0]
    # len("grok build coding agent assistant reply") == 39; 39 // 4 == 9
    assert ev["output_tokens"] == 9
    assert ev["input_tokens"] == 0
    assert ev["cost_source"] == COST_SOURCE_ESTIMATED


def test_trusts_precomputed_output_tokens() -> None:
    """When the adapter already estimated output, don't re-estimate."""
    row = _msg_row(content_text="ignored for counting", output_tokens=123)
    ev = next(iter(GrokNormalizer().normalize(row)))
    assert ev["output_tokens"] == 123


def test_no_text_yields_zero_events() -> None:
    # Empty content (e.g. encrypted reasoning / pure tool-call turn).
    assert list(GrokNormalizer().normalize(_msg_row())) == []


def test_cost_is_zero_no_phantom_fallback() -> None:
    """grok-build has no rate card and must NOT accrue Anthropic-fallback
    dollars even though cost_source is 'estimated'."""
    row = _msg_row(content_text="a" * 4000)  # ~1000 estimated output tokens
    ev = next(iter(GrokNormalizer().normalize(row)))
    assert ev["output_tokens"] == 1000
    assert ev["cost_source"] == COST_SOURCE_ESTIMATED
    assert ev["cost_usd"] == 0.0


def test_default_model_when_missing() -> None:
    row = _msg_row(content_text="reply", model="")
    ev = next(iter(GrokNormalizer().normalize(row)))
    assert ev["model"] == "grok-build"


def test_raw_extras_preserves_grok_fields() -> None:
    raw = {
        "id": "rs_646529e9",
        "model_id": "grok-build",
        "model_fingerprint": "fp_36bb860c5ab2a013",
        "status": "completed",
    }
    row = _msg_row(content_text="reply", raw_json=json.dumps(raw))
    ev = next(iter(GrokNormalizer().normalize(row)))
    extras = json.loads(ev["raw_extras"])
    assert extras["model_id"] == "grok-build"
    assert extras["model_fingerprint"] == "fp_36bb860c5ab2a013"
    assert extras["status"] == "completed"
