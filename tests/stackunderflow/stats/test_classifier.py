"""Unit tests for stackunderflow.stats.classifier."""

from __future__ import annotations

import pytest

from stackunderflow.stats.classifier import (
    INTERRUPT_API,
    INTERRUPT_PREFIX,
    RawEntry,
    TaggedEntry,
    _categorise,
    _detect_error,
    _determine_kind,
    _surface_text,
    tag,
)


# ── helpers ───────────────────────────────────────────────────────────────────

def _raw(payload: dict) -> RawEntry:
    return RawEntry(payload=payload, session_id="s1", origin="test")


# ── _determine_kind ───────────────────────────────────────────────────────────

def test_determine_kind_human():
    assert _determine_kind({"type": "human"}) == "user"


def test_determine_kind_assistant():
    assert _determine_kind({"type": "assistant"}) == "assistant"


def test_determine_kind_summary():
    assert _determine_kind({"type": "summary"}) == "summary"


def test_determine_kind_compact_summary():
    assert _determine_kind({"type": "compact_summary"}) == "compact_summary"


def test_determine_kind_task_start():
    assert _determine_kind({"type": "task_start"}) == "task"


def test_determine_kind_task():
    assert _determine_kind({"type": "task"}) == "task"


def test_determine_kind_message_role_user():
    assert _determine_kind({"message": {"role": "user"}}) == "user"


def test_determine_kind_message_role_assistant():
    assert _determine_kind({"message": {"role": "assistant"}}) == "assistant"


def test_determine_kind_fallback_to_assistant():
    assert _determine_kind({"type": "unknown_type"}) == "assistant"


def test_determine_kind_empty_payload():
    assert _determine_kind({}) == "assistant"


# ── _surface_text ─────────────────────────────────────────────────────────────

def test_surface_text_summary_string():
    assert _surface_text({"summary": "my summary"}) == "my summary"


def test_surface_text_message_string_content():
    payload = {"message": {"content": "hello world"}}
    assert _surface_text(payload) == "hello world"


def test_surface_text_message_list_text_blocks():
    payload = {
        "message": {
            "content": [
                {"type": "text", "text": "first"},
                {"type": "text", "text": "second"},
            ]
        }
    }
    assert _surface_text(payload) == "first\nsecond"


def test_surface_text_no_message():
    assert _surface_text({}) == ""


def test_surface_text_message_not_dict():
    assert _surface_text({"message": "plain string"}) == ""


def test_surface_text_list_with_string_items():
    payload = {"message": {"content": ["bare string", {"type": "text", "text": "also"}]}}
    result = _surface_text(payload)
    assert "bare string" in result
    assert "also" in result


# ── _detect_error ─────────────────────────────────────────────────────────────

def test_detect_error_tool_result_is_error():
    payload = {
        "message": {
            "content": [
                {
                    "type": "tool_result",
                    "is_error": True,
                    "content": "permission denied: /etc/passwd",
                }
            ]
        }
    }
    is_err, cat = _detect_error(payload, "user", "")
    assert is_err is True
    assert cat == "Permission Error"


def test_detect_error_tool_result_list_content():
    payload = {
        "message": {
            "content": [
                {
                    "type": "tool_result",
                    "is_error": True,
                    "content": [{"text": "file has not been read yet"}],
                }
            ]
        }
    }
    is_err, cat = _detect_error(payload, "user", "")
    assert is_err is True
    assert cat == "File Not Read"


def test_detect_error_tool_result_not_error_flag():
    payload = {
        "message": {
            "content": [
                {"type": "tool_result", "is_error": False, "content": "ok"}
            ]
        }
    }
    is_err, cat = _detect_error(payload, "user", "")
    assert is_err is False
    assert cat is None


def test_detect_error_abort_signal_assistant():
    is_err, cat = _detect_error({}, "assistant", INTERRUPT_API)
    assert is_err is True
    assert cat == "User Interruption"


def test_detect_error_abort_signal_not_assistant():
    is_err, _ = _detect_error({}, "user", INTERRUPT_API)
    assert is_err is False


def test_detect_error_no_error():
    is_err, cat = _detect_error({"message": {"content": "all good"}}, "assistant", "all good")
    assert is_err is False
    assert cat is None


# ── _categorise ───────────────────────────────────────────────────────────────

@pytest.mark.parametrize("text,expected", [
    ("user doesn't want to proceed", "User Interruption"),
    ("[Request interrupted by user for tool use]", "User Interruption"),
    ("command timed out after 120s", "Command Timeout"),
    ("file has not been read yet", "File Not Read"),
    ("file has been modified since read", "File Modified"),
    ("exceeds maximum allowed size", "File Too Large"),
    ("string to replace not found in file", "Content Not Found"),
    ("file does not exist at that path", "Content Not Found"),
    ("npm error enoent missing package", "Content Not Found"),
    ("no changes to make", "No Changes"),
    ("permission denied for /etc/passwd", "Permission Error"),
    ("traceback (most recent call last)", "Code Runtime Error"),
    ("cannot find module 'express'", "Code Runtime Error"),
    ("attempting to bind on address 0.0.0.0:3000", "Port Binding Error"),
    ("SyntaxError: unexpected token", "Syntax Error"),
    ("replace_all is false but multiple matches", "Syntax Error"),
    ("jq: error compiling filter", "Syntax Error"),
    ("command not found: foo", "Tool Not Found"),
    ("something completely unrecognised happened", "Other"),
])
def test_categorise(text: str, expected: str):
    assert _categorise(text) == expected


# ── interruption constants ────────────────────────────────────────────────────

def test_interrupt_prefix_is_cancel_marker():
    assert INTERRUPT_PREFIX == "[Request interrupted by user for tool use]"


def test_interrupt_api_is_abort_signal():
    assert INTERRUPT_API == "API Error: Request was aborted."


# ── tag() ─────────────────────────────────────────────────────────────────────

def test_tag_empty_list():
    assert tag([]) == []


def test_tag_returns_tagged_entries():
    entries = [_raw({"type": "human", "message": {"content": "hello"}})]
    result = tag(entries)
    assert len(result) == 1
    assert isinstance(result[0], TaggedEntry)


def test_tag_user_message_no_error():
    entries = [_raw({"type": "human", "message": {"content": "do something"}})]
    te = tag(entries)[0]
    assert te.kind == "user"
    assert te.is_error is False
    assert te.is_interruption is False


def test_tag_detects_cancel_interruption():
    text = "[Request interrupted by user for tool use] in Bash"
    entries = [_raw({"type": "human", "message": {"content": text}})]
    te = tag(entries)[0]
    assert te.is_interruption is True


def test_tag_detects_abort_interruption():
    entries = [_raw({"type": "assistant", "message": {"content": INTERRUPT_API}})]
    te = tag(entries)[0]
    assert te.is_interruption is True


def test_tag_detects_tool_error():
    payload = {
        "type": "human",
        "message": {
            "content": [
                {
                    "type": "tool_result",
                    "is_error": True,
                    "content": "command not found: foo",
                }
            ]
        },
    }
    te = tag([_raw(payload)])[0]
    assert te.is_error is True
    assert te.error_category == "Tool Not Found"


def test_tag_preserves_session_id_and_origin():
    entry = RawEntry(payload={"type": "assistant"}, session_id="session-abc", origin="file.jsonl")
    te = tag([entry])[0]
    assert te.session_id == "session-abc"
    assert te.origin == "file.jsonl"
