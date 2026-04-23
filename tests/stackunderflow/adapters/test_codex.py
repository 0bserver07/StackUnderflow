"""Unit tests for the Codex adapter.

Exercises discovery of rollout files, record extraction from `response_item`
lines, tool-name mapping, token-count attachment to the most-recent
assistant record, malformed-line tolerance, and resumable reads via
`since_offset`.
"""

from __future__ import annotations

import json
from pathlib import Path

import pytest

from stackunderflow.adapters.base import Record, SessionRef
from stackunderflow.adapters.codex import CodexAdapter


FIXTURE_ROOT = Path(__file__).resolve().parents[2] / "mock-data" / "codex-sessions"
FIXTURE_FILE = (
    FIXTURE_ROOT
    / "2026"
    / "04"
    / "19"
    / "rollout-2026-04-19T20-00-00-test-uuid-0001.jsonl"
)


# ── helpers ────────────────────────────────────────────────────────────

def _write_jsonl(path: Path, lines: list[dict | str]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    out: list[str] = []
    for ln in lines:
        if isinstance(ln, str):
            out.append(ln)
        else:
            out.append(json.dumps(ln))
    path.write_text("\n".join(out) + "\n")


def _session_meta(
    *,
    session_id: str = "test-uuid-0001",
    cwd: str = "/Users/test/dev/sample-project",
    originator: str = "codex_cli",
    timestamp: str = "2026-04-19T20:00:00.000Z",
) -> dict:
    return {
        "timestamp": timestamp,
        "type": "session_meta",
        "payload": {
            "id": session_id,
            "cwd": cwd,
            "originator": originator,
            "cli_version": "0.121.0",
            "model": "gpt-5.4",
        },
    }


def _user_msg(text: str, ts: str = "2026-04-19T20:00:02.000Z") -> dict:
    return {
        "timestamp": ts,
        "type": "response_item",
        "payload": {
            "type": "message",
            "role": "user",
            "content": [{"type": "text", "text": text}],
        },
    }


def _assistant_msg(text: str, ts: str = "2026-04-19T20:00:03.000Z") -> dict:
    return {
        "timestamp": ts,
        "type": "response_item",
        "payload": {
            "type": "message",
            "role": "assistant",
            "content": [{"type": "text", "text": text}],
        },
    }


# ── tests ──────────────────────────────────────────────────────────────

def test_enumerate_discovers_valid_rollout() -> None:
    adapter = CodexAdapter(sessions_root=FIXTURE_ROOT)
    refs = list(adapter.enumerate())
    assert len(refs) == 1
    ref = refs[0]
    assert isinstance(ref, SessionRef)
    assert ref.provider == "codex"
    assert ref.session_id == "test-uuid-0001"
    assert ref.file_path == FIXTURE_FILE
    assert ref.file_mtime > 0


def test_enumerate_skips_files_without_session_meta(tmp_path: Path) -> None:
    # One valid rollout, one "jsonl" file whose first line is NOT session_meta.
    valid = tmp_path / "2026" / "04" / "19" / "rollout-valid.jsonl"
    _write_jsonl(
        valid,
        [
            _session_meta(session_id="good-uuid"),
            _user_msg("hi"),
        ],
    )
    bogus = tmp_path / "2026" / "04" / "19" / "rollout-bogus.jsonl"
    _write_jsonl(
        bogus,
        [
            {"type": "turn_context", "payload": {}},
            _user_msg("hi"),
        ],
    )

    adapter = CodexAdapter(sessions_root=tmp_path)
    refs = list(adapter.enumerate())
    assert len(refs) == 1
    assert refs[0].session_id == "good-uuid"


def test_enumerate_skips_files_with_wrong_originator(tmp_path: Path) -> None:
    wrong = tmp_path / "2026" / "04" / "19" / "rollout-wrong.jsonl"
    _write_jsonl(
        wrong,
        [
            _session_meta(session_id="wrong-uuid", originator="claude_cli"),
            _user_msg("hi"),
        ],
    )
    adapter = CodexAdapter(sessions_root=tmp_path)
    refs = list(adapter.enumerate())
    assert refs == []


def test_project_slug_derived_from_cwd() -> None:
    adapter = CodexAdapter(sessions_root=FIXTURE_ROOT)
    ref = list(adapter.enumerate())[0]
    # Claude's convention: replace path separators with dashes, keep leading dash.
    assert ref.project_slug == "-Users-test-dev-sample-project"


def test_read_yields_records_for_messages_and_tools() -> None:
    adapter = CodexAdapter(sessions_root=FIXTURE_ROOT)
    ref = list(adapter.enumerate())[0]
    records = list(adapter.read(ref))

    # Expect: user msg, assistant msg, read_file tool, exec_command tool,
    # assistant msg #2, and a final user msg. (Malformed line skipped.)
    roles = [r.role for r in records]
    assert roles.count("user") >= 2
    assert roles.count("assistant") >= 2

    # Tool records: one read, one bash.
    tool_records = [r for r in records if r.tools]
    assert len(tool_records) == 2

    # First assistant record text matches the fixture's first assistant message.
    first_assistant = next(r for r in records if r.role == "assistant")
    assert "refactor" in first_assistant.content_text

    # First user record text matches the fixture's first user message.
    first_user = next(r for r in records if r.role == "user")
    assert "refactor this function" in first_user.content_text

    # Every record is a Record with the codex provider.
    assert all(isinstance(r, Record) for r in records)
    assert all(r.provider == "codex" for r in records)


def test_read_tool_name_mapping() -> None:
    adapter = CodexAdapter(sessions_root=FIXTURE_ROOT)
    ref = list(adapter.enumerate())[0]
    records = list(adapter.read(ref))

    tool_records = [r for r in records if r.tools]
    tool_name_tuples = [r.tools for r in tool_records]
    assert ("Read",) in tool_name_tuples
    assert ("Bash",) in tool_name_tuples


def test_token_count_attaches_to_previous_assistant() -> None:
    adapter = CodexAdapter(sessions_root=FIXTURE_ROOT)
    ref = list(adapter.enumerate())[0]
    records = list(adapter.read(ref))

    # token_count attaches to the most recent assistant *text* record
    # (not a tool-only record).
    assistants = [r for r in records if r.role == "assistant" and not r.tools]
    assert len(assistants) >= 2

    first_asst = assistants[0]
    # 1200 input - 200 cached = 1000 non-cache input
    assert first_asst.input_tokens == 1000
    # 350 output + 150 reasoning = 500
    assert first_asst.output_tokens == 500
    assert first_asst.cache_read_tokens == 200
    assert first_asst.cache_create_tokens == 0

    second_asst = assistants[1]
    # Second event had different numbers — the attachment must not reuse the first.
    assert (second_asst.input_tokens, second_asst.output_tokens) != (
        first_asst.input_tokens,
        first_asst.output_tokens,
    )
    # 800 - 100 = 700 ; 200 + 50 = 250 ; cache_read = 100
    assert second_asst.input_tokens == 700
    assert second_asst.output_tokens == 250
    assert second_asst.cache_read_tokens == 100


def test_malformed_json_line_does_not_raise() -> None:
    adapter = CodexAdapter(sessions_root=FIXTURE_ROOT)
    ref = list(adapter.enumerate())[0]
    records = list(adapter.read(ref))  # must not raise

    # Records exist from before AND after the malformed line.
    user_texts = [r.content_text for r in records if r.role == "user"]
    # "Hello, please help me refactor this function." precedes the bad line.
    assert any("refactor this function" in t for t in user_texts)
    # "Thanks, that worked." follows the bad line.
    assert any("Thanks, that worked" in t for t in user_texts)


def test_seq_is_monotonic_per_session() -> None:
    adapter = CodexAdapter(sessions_root=FIXTURE_ROOT)
    ref = list(adapter.enumerate())[0]
    records = list(adapter.read(ref))
    assert len(records) >= 2

    prev = -1
    for rec in records:
        assert rec.seq > prev, f"seq not strictly increasing: {prev} -> {rec.seq}"
        prev = rec.seq


def test_since_offset_resumes_mid_file() -> None:
    adapter = CodexAdapter(sessions_root=FIXTURE_ROOT)
    ref = list(adapter.enumerate())[0]

    # Build a byte offset that lands just after the fixture's first few lines.
    # Skip past session_meta + turn_context + first user message (3 lines).
    raw = ref.file_path.read_bytes()
    line_ends: list[int] = []
    pos = 0
    for b in raw.splitlines(keepends=True):
        pos += len(b)
        line_ends.append(pos)
    # Offset after the first 3 lines (session_meta, turn_context, user msg).
    offset = line_ends[2]

    full = list(adapter.read(ref))
    partial = list(adapter.read(ref, since_offset=offset))

    # Partial read must have strictly fewer records than a full read.
    assert len(partial) < len(full)

    # Partial read must NOT contain the first user message (it preceded offset).
    assert not any(
        "refactor this function" in r.content_text for r in partial if r.role == "user"
    )
    # Partial read SHOULD still contain content from after the offset.
    assistant_texts = [r.content_text for r in partial if r.role == "assistant"]
    assert any("refactor" in t for t in assistant_texts)
