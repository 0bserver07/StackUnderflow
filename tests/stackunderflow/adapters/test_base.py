from pathlib import Path

from stackunderflow.adapters.base import Record, SessionRef


def test_session_ref_fields() -> None:
    ref = SessionRef(
        provider="claude",
        project_slug="-a",
        session_id="abc",
        file_path=Path("/tmp/a.jsonl"),
        file_mtime=1.0,
        file_size=10,
    )
    assert ref.provider == "claude"


def test_record_fields() -> None:
    rec = Record(
        provider="claude",
        session_id="abc",
        seq=0,
        timestamp="2026-01-01T00:00:00+00:00",
        role="user",
        model=None,
        input_tokens=10,
        output_tokens=20,
        cache_create_tokens=0,
        cache_read_tokens=0,
        content_text="hi",
        tools=(),
        cwd=None,
        is_sidechain=False,
        uuid="u",
        parent_uuid=None,
        raw={"x": 1},
    )
    assert rec.role == "user"
    assert rec.tools == ()


def test_record_is_frozen() -> None:
    import dataclasses
    rec = Record(
        provider="claude", session_id="s", seq=0,
        timestamp="2026-01-01T00:00:00+00:00", role="user", model=None,
        input_tokens=0, output_tokens=0,
        cache_create_tokens=0, cache_read_tokens=0,
        content_text="", tools=(), cwd=None,
        is_sidechain=False, uuid="u", parent_uuid=None, raw={},
    )
    try:
        rec.role = "assistant"
    except dataclasses.FrozenInstanceError:
        return
    raise AssertionError("Record should be frozen")
