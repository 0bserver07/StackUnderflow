"""Unit tests for stackunderflow.stats.enricher."""

from __future__ import annotations

from stackunderflow.stats.classifier import TaggedEntry
from stackunderflow.stats.enricher import (
    EnrichedDataset,
    _flatten_content_blocks,
    _has_result_block,
    _parse_entry,
    _text_from,
    _tools_from,
    _usage_from,
    build,
)


# ── helpers ───────────────────────────────────────────────────────────────────

_EMPTY_TOKENS = {"input": 0, "output": 0, "cache_creation": 0, "cache_read": 0}


def _tagged(
    payload: dict,
    *,
    session_id: str = "s1",
    origin: str = "test",
    kind: str = "assistant",
    is_error: bool = False,
    error_category: str | None = None,
    is_interruption: bool = False,
) -> TaggedEntry:
    return TaggedEntry(
        payload=payload,
        session_id=session_id,
        origin=origin,
        kind=kind,
        is_error=is_error,
        error_category=error_category,
        is_interruption=is_interruption,
    )


def _user_te(content: str, ts: str = "2026-01-01T10:00:00Z", session_id: str = "s1") -> TaggedEntry:
    return _tagged(
        {"type": "human", "timestamp": ts, "message": {"content": content}},
        kind="user",
        session_id=session_id,
    )


def _asst_te(
    content: str,
    ts: str = "2026-01-01T10:01:00Z",
    session_id: str = "s1",
    model: str = "claude-sonnet-4-20250514",
    tools: list[dict] | None = None,
) -> TaggedEntry:
    content_blocks: list[dict] = [{"type": "text", "text": content}]
    if tools:
        content_blocks += [{"type": "tool_use", "id": t["id"], "name": t["name"], "input": {}} for t in tools]
    return _tagged(
        {
            "timestamp": ts,
            "message": {
                "role": "assistant",
                "model": model,
                "content": content_blocks,
                "usage": {"input_tokens": 10, "output_tokens": 5, "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0},
            },
        },
        kind="assistant",
        session_id=session_id,
    )


# ── _text_from ────────────────────────────────────────────────────────────────

def test_text_from_summary():
    assert _text_from({"summary": "this is a summary"}) == "this is a summary"


def test_text_from_message_string_content():
    assert _text_from({"message": {"content": "hello"}}) == "hello"


def test_text_from_message_list_text_blocks():
    raw = {"message": {"content": [{"type": "text", "text": "a"}, {"type": "text", "text": "b"}]}}
    assert _text_from(raw) == "a\nb"


def test_text_from_tool_use_block():
    raw = {"message": {"content": [{"type": "tool_use", "name": "Bash"}]}}
    assert "[Tool: Bash]" in _text_from(raw)


def test_text_from_tool_result_string():
    raw = {"message": {"content": [{"type": "tool_result", "content": "result text"}]}}
    assert "result text" in _text_from(raw)


def test_text_from_empty_payload():
    assert _text_from({}) == ""


def test_text_from_no_message_dict():
    assert _text_from({"message": "not a dict"}) == ""


# ── _flatten_content_blocks ───────────────────────────────────────────────────

def test_flatten_string_item():
    assert _flatten_content_blocks(["bare"]) == ["bare"]


def test_flatten_nested_tool_result_list():
    blocks = [
        {"type": "tool_result", "content": [{"type": "text", "text": "inner"}]}
    ]
    assert _flatten_content_blocks(blocks) == ["inner"]


def test_flatten_skips_non_dict_non_str():
    assert _flatten_content_blocks([42, None]) == []


# ── _usage_from ───────────────────────────────────────────────────────────────

def test_usage_from_full_usage():
    msg = {
        "usage": {
            "input_tokens": 100,
            "output_tokens": 50,
            "cache_creation_input_tokens": 20,
            "cache_read_input_tokens": 10,
        }
    }
    result = _usage_from(msg)
    assert result == {"input": 100, "output": 50, "cache_creation": 20, "cache_read": 10}


def test_usage_from_partial_usage():
    result = _usage_from({"usage": {"input_tokens": 5}})
    assert result["input"] == 5
    assert result["output"] == 0


def test_usage_from_no_usage_key():
    assert _usage_from({"role": "assistant"}) == _EMPTY_TOKENS


def test_usage_from_not_dict():
    assert _usage_from("not a dict") == _EMPTY_TOKENS  # type: ignore[arg-type]


# ── _tools_from ───────────────────────────────────────────────────────────────

def test_tools_from_extracts_tool_use_blocks():
    msg = {
        "content": [
            {"type": "tool_use", "id": "t1", "name": "Bash", "input": {"command": "ls"}},
            {"type": "text", "text": "ignored"},
        ]
    }
    tools = _tools_from(msg)
    assert len(tools) == 1
    assert tools[0]["name"] == "Bash"
    assert tools[0]["id"] == "t1"
    assert tools[0]["input"] == {"command": "ls"}


def test_tools_from_no_tool_blocks():
    assert _tools_from({"content": [{"type": "text", "text": "hi"}]}) == []


def test_tools_from_non_list_content():
    assert _tools_from({"content": "string"}) == []


def test_tools_from_not_dict():
    assert _tools_from(None) == []  # type: ignore[arg-type]


# ── _has_result_block ─────────────────────────────────────────────────────────

def test_has_result_block_true():
    msg = {"content": [{"type": "tool_result", "content": "ok"}]}
    assert _has_result_block(msg) is True


def test_has_result_block_false_no_tool_result():
    msg = {"content": [{"type": "text", "text": "no tools here"}]}
    assert _has_result_block(msg) is False


def test_has_result_block_false_non_list():
    assert _has_result_block({"content": "string"}) is False


def test_has_result_block_not_dict():
    assert _has_result_block(None) is False  # type: ignore[arg-type]


# ── _parse_entry ──────────────────────────────────────────────────────────────

def test_parse_entry_basic_fields():
    te = _tagged(
        {
            "timestamp": "2026-01-01T12:00:00Z",
            "uuid": "uuid-1",
            "parentUuid": "parent-1",
            "isSidechain": True,
            "cwd": "/tmp/proj",
            "message": {
                "id": "msg-1",
                "model": "claude-opus-4-20250514",
                "content": "response text",
                "usage": {"input_tokens": 8, "output_tokens": 4,
                          "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0},
            },
        },
        kind="assistant",
    )
    rec = _parse_entry(te)
    assert rec.session_id == "s1"
    assert rec.kind == "assistant"
    assert rec.timestamp == "2026-01-01T12:00:00Z"
    assert rec.model == "claude-opus-4-20250514"
    assert rec.content == "response text"
    assert rec.tokens["input"] == 8
    assert rec.tokens["output"] == 4
    assert rec.uuid == "uuid-1"
    assert rec.parent_uuid == "parent-1"
    assert rec.is_sidechain is True
    assert rec.message_id == "msg-1"
    assert rec.cwd == "/tmp/proj"


def test_parse_entry_no_message():
    te = _tagged({}, kind="user")
    rec = _parse_entry(te)
    assert rec.model == "N/A"
    assert rec.tokens == _EMPTY_TOKENS
    assert rec.tools == []
    assert rec.has_tool_result is False


def test_parse_entry_propagates_error_flags():
    te = _tagged({}, is_error=True, error_category="Timeout")
    rec = _parse_entry(te)
    assert rec.is_error is True
    assert rec.error_category == "Timeout"


def test_parse_entry_propagates_interruption():
    te = _tagged({}, is_interruption=True)
    rec = _parse_entry(te)
    assert rec.is_interruption is True


# ── build() ───────────────────────────────────────────────────────────────────

def test_build_empty_input():
    ds = build([], log_dir="/tmp")
    assert isinstance(ds, EnrichedDataset)
    assert ds.records == []
    assert ds.interactions == []
    assert ds.sessions == {}


def test_build_returns_correct_record_count():
    entries = [_user_te("hello"), _asst_te("world")]
    ds = build(entries, log_dir="/tmp")
    assert len(ds.records) == 2


def test_build_creates_interaction_for_user_command():
    entries = [_user_te("do something"), _asst_te("done")]
    ds = build(entries, log_dir="/tmp")
    assert len(ds.interactions) == 1
    assert ds.interactions[0].command.content == "do something"


def test_build_interaction_has_response():
    entries = [_user_te("q"), _asst_te("a")]
    ds = build(entries, log_dir="/tmp")
    ix = ds.interactions[0]
    assert len(ix.responses) == 1
    assert ix.responses[0].content == "a"


def test_build_multiple_interactions():
    entries = [
        _user_te("cmd1", ts="2026-01-01T10:00:00Z"),
        _asst_te("r1", ts="2026-01-01T10:01:00Z"),
        _user_te("cmd2", ts="2026-01-01T10:02:00Z"),
        _asst_te("r2", ts="2026-01-01T10:03:00Z"),
    ]
    ds = build(entries, log_dir="/tmp")
    assert len(ds.interactions) == 2


def test_build_session_metadata():
    entries = [_user_te("hello", session_id="sess-42")]
    ds = build(entries, log_dir="/tmp")
    assert "sess-42" in ds.sessions
    sm = ds.sessions["sess-42"]
    assert sm.message_count == 1


def test_build_interaction_tool_count():
    tools = [{"id": "t1", "name": "Bash"}, {"id": "t2", "name": "Grep"}]
    entries = [
        _user_te("cmd"),
        _asst_te("running", tools=tools),
    ]
    ds = build(entries, log_dir="/tmp")
    ix = ds.interactions[0]
    assert ix.tool_count == 2


def test_build_interaction_has_task_tool():
    tools = [{"id": "t1", "name": "Task"}]
    entries = [_user_te("spawn"), _asst_te("ok", tools=tools)]
    ds = build(entries, log_dir="/tmp")
    assert ds.interactions[0].has_task_tool is True


def test_build_summary_entries_not_in_interactions():
    summary_te = _tagged({"type": "summary", "summary": "context"}, kind="summary")
    entries = [_user_te("hi"), summary_te, _asst_te("hey")]
    ds = build(entries, log_dir="/tmp")
    assert len(ds.interactions) == 1
    assert len(ds.records) == 3


def test_build_deduplicates_tools_by_id():
    tool = {"id": "dup-id", "name": "Edit"}
    entries = [
        _user_te("edit"),
        _asst_te("step1", tools=[tool]),
        _asst_te("step2", ts="2026-01-01T10:02:00Z", tools=[tool]),
    ]
    ds = build(entries, log_dir="/tmp")
    ix = ds.interactions[0]
    assert ix.tool_count == 1
