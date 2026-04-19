"""Unit tests for stackunderflow.stats.formatter."""

from __future__ import annotations

from stackunderflow.stats.enricher import EnrichedDataset, Interaction, Record
from stackunderflow.stats.formatter import to_dicts


# ── helpers ───────────────────────────────────────────────────────────────────

_TOKENS = {"input": 10, "output": 5, "cache_creation": 0, "cache_read": 0}
_MODEL = "claude-sonnet-4-20250514"


def _rec(
    *,
    session_id: str = "s1",
    kind: str = "assistant",
    timestamp: str = "2026-01-01T12:00:00Z",
    model: str = _MODEL,
    content: str = "content",
    tokens: dict | None = None,
    tools: list[dict] | None = None,
    is_error: bool = False,
    is_interruption: bool = False,
    has_tool_result: bool = False,
    uuid: str = "u1",
    parent_uuid: str | None = None,
    cwd: str = "/tmp",
) -> Record:
    return Record(
        session_id=session_id,
        kind=kind,
        timestamp=timestamp,
        model=model,
        content=content,
        tokens=tokens if tokens is not None else dict(_TOKENS),
        tools=tools if tools is not None else [],
        is_error=is_error,
        error_category=None,
        is_interruption=is_interruption,
        has_tool_result=has_tool_result,
        uuid=uuid,
        parent_uuid=parent_uuid,
        is_sidechain=False,
        message_id="m1",
        cwd=cwd,
        raw_data={},
    )


def _ds(records: list[Record], interactions: list[Interaction] | None = None) -> EnrichedDataset:
    return EnrichedDataset(
        records=records,
        interactions=interactions or [],
        sessions={},
    )


# ── to_dicts ──────────────────────────────────────────────────────────────────

def test_to_dicts_empty_dataset():
    assert to_dicts(_ds([])) == []


def test_to_dicts_returns_one_dict_per_record():
    ds = _ds([_rec(), _rec(uuid="u2")])
    result = to_dicts(ds)
    assert len(result) == 2


def test_to_dicts_sorted_by_timestamp():
    recs = [
        _rec(timestamp="2026-01-01T14:00:00Z", uuid="u3"),
        _rec(timestamp="2026-01-01T10:00:00Z", uuid="u1"),
        _rec(timestamp="2026-01-01T12:00:00Z", uuid="u2"),
    ]
    result = to_dicts(_ds(recs))
    assert result[0]["timestamp"] == "2026-01-01T10:00:00Z"
    assert result[1]["timestamp"] == "2026-01-01T12:00:00Z"
    assert result[2]["timestamp"] == "2026-01-01T14:00:00Z"


def test_to_dicts_limit_truncates():
    recs = [_rec(uuid=f"u{i}", timestamp=f"2026-01-01T{i:02d}:00:00Z") for i in range(5)]
    result = to_dicts(_ds(recs), limit=3)
    assert len(result) == 3


def test_to_dicts_limit_none_returns_all():
    recs = [_rec(uuid=f"u{i}") for i in range(10)]
    assert len(to_dicts(_ds(recs), limit=None)) == 10


def test_to_dicts_field_mapping():
    rec = _rec(
        session_id="session-1",
        kind="user",
        timestamp="2026-03-15T08:00:00Z",
        model="N/A",
        content="do the thing",
        tokens={"input": 20, "output": 0, "cache_creation": 5, "cache_read": 3},
        tools=[{"name": "Bash", "id": "t1", "input": {}}],
        is_error=False,
        is_interruption=False,
        has_tool_result=False,
        uuid="uuid-x",
        parent_uuid="parent-x",
        cwd="/home/user/proj",
    )
    result = to_dicts(_ds([rec]))[0]
    assert result["session_id"] == "session-1"
    assert result["type"] == "user"
    assert result["timestamp"] == "2026-03-15T08:00:00Z"
    assert result["model"] == "N/A"
    assert result["content"] == "do the thing"
    assert result["tokens"] == {"input": 20, "output": 0, "cache_creation": 5, "cache_read": 3}
    assert result["tools"] == [{"name": "Bash", "id": "t1", "input": {}}]
    assert result["error"] is False
    assert result["is_interruption"] is False
    assert result["has_tool_result"] is False
    assert result["uuid"] == "uuid-x"
    assert result["parent_uuid"] == "parent-x"
    assert result["cwd"] == "/home/user/proj"
    assert result["is_sidechain"] is False
    assert result["message_id"] == "m1"


def test_to_dicts_stamps_interaction_metadata_on_command():
    cmd = _rec(kind="user", has_tool_result=False, timestamp="2026-01-01T10:00:00Z")
    asst = _rec(kind="assistant", timestamp="2026-01-01T10:01:00Z", uuid="u2")
    ix = Interaction(
        interaction_id="ix1",
        command=cmd,
        session_id="s1",
        start_time=cmd.timestamp,
        end_time=asst.timestamp,
    )
    ix.tool_count = 3
    ix.model = _MODEL
    ix.assistant_steps = 2

    ds = _ds([cmd, asst], interactions=[ix])
    result = to_dicts(ds)

    cmd_dict = next(d for d in result if d["type"] == "user")
    assert cmd_dict["interaction_tool_count"] == 3
    assert cmd_dict["interaction_model"] == _MODEL
    assert cmd_dict["interaction_assistant_steps"] == 2


def test_to_dicts_non_command_records_have_no_interaction_keys():
    asst = _rec(kind="assistant")
    ds = _ds([asst])
    result = to_dicts(ds)[0]
    assert "interaction_tool_count" not in result
    assert "interaction_model" not in result


def test_to_dicts_empty_timestamp_sorts_to_front():
    recs = [
        _rec(timestamp="2026-01-01T10:00:00Z", uuid="u1"),
        _rec(timestamp="", uuid="u2"),
    ]
    result = to_dicts(_ds(recs))
    assert result[0]["uuid"] == "u2"


def test_to_dicts_error_flag_mapped():
    rec = _rec(is_error=True)
    result = to_dicts(_ds([rec]))[0]
    assert result["error"] is True
