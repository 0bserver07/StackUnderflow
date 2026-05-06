"""CodeiumNormalizer — discovery-only stub yields zero events."""

from __future__ import annotations

from stackunderflow.etl.normalize import CodeiumNormalizer


def _msg_row(**overrides) -> dict:
    base = {
        "id": 1,
        "provider": "codeium",
        "project_id": 1,
        "session_id": "codeium-sess",
        "timestamp": "2026-04-25T10:00:00+00:00",
        "role": "assistant",
        "model": "claude-sonnet-4-5-20250929",
        "input_tokens": 100,
        "output_tokens": 100,
        "content_text": "hello world",
        "raw_json": "{}",
    }
    base.update(overrides)
    return base


def test_codeium_yields_zero_events_on_assistant_row() -> None:
    """Stub never emits events even on a fully-populated assistant row."""
    row = _msg_row()
    assert list(CodeiumNormalizer().normalize(row)) == []


def test_codeium_yields_zero_events_on_user_row() -> None:
    row = _msg_row(role="user")
    assert list(CodeiumNormalizer().normalize(row)) == []


def test_codeium_yields_zero_events_on_empty_row() -> None:
    row = {"id": 1, "role": "assistant"}
    assert list(CodeiumNormalizer().normalize(row)) == []


def test_codeium_provider_name() -> None:
    assert CodeiumNormalizer.provider_name == "codeium"
