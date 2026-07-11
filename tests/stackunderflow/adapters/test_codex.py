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
            # NOTE: real session_meta carries NO model — it lives in
            # turn_context events. Keep this fixture shaped like reality.
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


def _turn_context(
    model: str = "gpt-5.4", ts: str = "2026-04-19T20:00:01.500Z"
) -> dict:
    return {
        "timestamp": ts,
        "type": "turn_context",
        "payload": {
            "cwd": "/Users/test/dev/sample-project",
            "model": model,
            "reasoning_effort": "medium",
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

    # ``since_offset`` is "the highest seq the caller has already processed";
    # the adapter yields strictly past it. We want to skip the first three
    # source lines (session_meta, turn_context, user "refactor this function")
    # and include from the first assistant onward — so the floor is the byte
    # position of that user-message line (its seq).
    raw = ref.file_path.read_bytes()
    line_ends: list[int] = []
    pos = 0
    for b in raw.splitlines(keepends=True):
        pos += len(b)
        line_ends.append(pos)
    # line_ends[1] is the start byte of line index 2 (the user "refactor"
    # message). Passing it as since_offset means "I've already seen the user
    # message; give me everything strictly after it."
    offset = line_ends[1]

    full = list(adapter.read(ref))
    partial = list(adapter.read(ref, since_offset=offset))

    # Partial read must have strictly fewer records than a full read.
    assert len(partial) < len(full)

    # Partial read must NOT contain the first user message (it was at offset).
    assert not any(
        "refactor this function" in r.content_text for r in partial if r.role == "user"
    )
    # Partial read SHOULD still contain content from after the offset.
    assistant_texts = [r.content_text for r in partial if r.role == "assistant"]
    assert any("refactor" in t for t in assistant_texts)


# ── model attribution (turn_context) ───────────────────────────────────
#
# The model's only home in real rollouts is turn_context.payload.model
# (verified against every 2026 rollout on a real install). A None model
# makes the normalizer drop the turn as unpriceable — the bug that left
# 1,486 base messages at 0 usage_events while unit suites stayed green.


def test_records_carry_model_from_turn_context(tmp_path: Path) -> None:
    fp = tmp_path / "2026" / "04" / "19" / "rollout-m1.jsonl"
    _write_jsonl(fp, [
        _session_meta(session_id="m1-uuid"),
        _turn_context(model="gpt-5.5"),
        _user_msg("hi"),
        _assistant_msg("hello"),
        {"timestamp": "2026-04-19T20:00:04.000Z", "type": "response_item",
         "payload": {"type": "function_call", "name": "exec_command",
                     "arguments": "{}"}},
    ])
    adapter = CodexAdapter(sessions_root=tmp_path)
    ref = list(adapter.enumerate())[0]
    records = list(adapter.read(ref))
    assert records, "fixture yielded no records"
    for rec in records:
        assert rec.model == "gpt-5.5", (rec.role, rec.content_text)


def test_model_switch_mid_session_applies_to_later_records(
    tmp_path: Path,
) -> None:
    fp = tmp_path / "2026" / "04" / "19" / "rollout-m2.jsonl"
    _write_jsonl(fp, [
        _session_meta(session_id="m2-uuid"),
        _turn_context(model="gpt-5.4"),
        _assistant_msg("first turn", ts="2026-04-19T20:00:02.000Z"),
        _turn_context(model="gpt-5.5", ts="2026-04-19T20:00:03.000Z"),
        _assistant_msg("second turn", ts="2026-04-19T20:00:04.000Z"),
    ])
    adapter = CodexAdapter(sessions_root=tmp_path)
    ref = list(adapter.enumerate())[0]
    by_text = {r.content_text: r.model for r in adapter.read(ref)}
    assert by_text["first turn"] == "gpt-5.4"
    assert by_text["second turn"] == "gpt-5.5"


def test_resumed_read_seeds_model_from_prefix(tmp_path: Path) -> None:
    """A since_offset landing after a turn's turn_context must not strand
    the rest of the turn: the resumed read seeds current_model from the
    already-ingested prefix. This is the watcher batch-boundary case — the
    ingest watermark is always a response_item offset, so it systematically
    lands past the turn_context; without the seed these records carried
    model=None and the normalizer silently dropped their usage_events."""
    fp = tmp_path / "2026" / "04" / "19" / "rollout-seed.jsonl"
    _write_jsonl(fp, [
        _session_meta(session_id="seed-uuid"),
        _turn_context(model="gpt-5.4"),
        _user_msg("start"),
        _assistant_msg("first half", ts="2026-04-19T20:00:03.000Z"),
        _assistant_msg("second half", ts="2026-04-19T20:00:04.000Z"),
    ])
    adapter = CodexAdapter(sessions_root=tmp_path)
    ref = list(adapter.enumerate())[0]

    # Watermark = the first assistant record's seq (its line-start offset),
    # exactly what the writer persists after a batch ending on that record.
    raw = ref.file_path.read_bytes()
    line_ends: list[int] = []
    pos = 0
    for chunk in raw.splitlines(keepends=True):
        pos += len(chunk)
        line_ends.append(pos)
    watermark = line_ends[2]  # start of the "first half" assistant line

    resumed = list(adapter.read(ref, since_offset=watermark))
    assert resumed, "resumed read yielded nothing"
    # Contract intact: only records strictly past the watermark, none extra.
    assert all(r.seq > watermark for r in resumed)
    # The fix: the boundary-straddling turn keeps its model.
    assert [r.model for r in resumed] == ["gpt-5.4"]


def test_resumed_read_seeds_latest_model_after_switch(tmp_path: Path) -> None:
    """The seed is the LAST pre-offset turn_context — a mid-session /model
    switch before the watermark wins over the session's first model."""
    fp = tmp_path / "2026" / "04" / "19" / "rollout-seed2.jsonl"
    _write_jsonl(fp, [
        _session_meta(session_id="seed2-uuid"),
        _turn_context(model="gpt-5.4"),
        _assistant_msg("old turn", ts="2026-04-19T20:00:02.000Z"),
        _turn_context(model="gpt-5.5", ts="2026-04-19T20:00:03.000Z"),
        _assistant_msg("new turn A", ts="2026-04-19T20:00:04.000Z"),
        _assistant_msg("new turn B", ts="2026-04-19T20:00:05.000Z"),
    ])
    adapter = CodexAdapter(sessions_root=tmp_path)
    ref = list(adapter.enumerate())[0]
    raw = ref.file_path.read_bytes()
    line_ends = []
    pos = 0
    for chunk in raw.splitlines(keepends=True):
        pos += len(chunk)
        line_ends.append(pos)
    watermark = line_ends[3]  # start of "new turn A" (past the 5.5 switch)

    resumed = list(adapter.read(ref, since_offset=watermark))
    assert [r.model for r in resumed] == ["gpt-5.5"]


def test_records_before_any_turn_context_have_no_model(
    tmp_path: Path,
) -> None:
    """Legacy rollouts without turn_context stay model-less — never invented."""
    fp = tmp_path / "2026" / "04" / "19" / "rollout-m3.jsonl"
    _write_jsonl(fp, [
        _session_meta(session_id="m3-uuid"),
        _assistant_msg("no context yet"),
    ])
    adapter = CodexAdapter(sessions_root=tmp_path)
    ref = list(adapter.enumerate())[0]
    records = list(adapter.read(ref))
    assert records[0].model is None


# ── shared adapter contract ────────────────────────────────────────────

import unittest  # noqa: E402

from tests.stackunderflow.adapters.contract import AdapterContract  # noqa: E402


class TestCodexAdapterContract(unittest.TestCase, AdapterContract):
    """Runs every AdapterContract invariant against the Codex fixture."""

    def setUp(self):
        self.adapter = CodexAdapter(sessions_root=FIXTURE_ROOT)


# ── malformed-input hardening (ingest-surface sweep, 2026-07) ─────────


def test_enumerate_survives_non_dict_first_line(tmp_path: Path) -> None:
    """A rollout whose first line is valid JSON but not an object must be
    skipped without aborting enumerate() for the whole provider."""
    bogus = tmp_path / "2026" / "04" / "19" / "rollout-bogus.jsonl"
    _write_jsonl(bogus, ["[1, 2, 3]", _user_msg("hi")])
    valid = tmp_path / "2026" / "04" / "19" / "rollout-valid.jsonl"
    _write_jsonl(valid, [_session_meta(session_id="good-uuid"), _user_msg("hi")])

    adapter = CodexAdapter(sessions_root=tmp_path)
    refs = list(adapter.enumerate())
    assert [r.session_id for r in refs] == ["good-uuid"]


def test_read_skips_non_dict_lines_and_non_dict_payload(tmp_path: Path) -> None:
    """Non-object JSON lines and a non-dict ``payload`` must be skipped;
    the surrounding valid records still come through."""
    fp = tmp_path / "2026" / "04" / "19" / "rollout-mixed.jsonl"
    _write_jsonl(
        fp,
        [
            _session_meta(session_id="mixed-uuid"),
            "[1, 2, 3]",
            '"just a string"',
            "42",
            # payload is a (truthy) string — previously crashed .get().
            {"type": "response_item", "payload": "garbage"},
            {"type": "event_msg", "payload": [1, 2]},
            _assistant_msg("still here"),
        ],
    )
    adapter = CodexAdapter(sessions_root=tmp_path)
    refs = list(adapter.enumerate())
    assert len(refs) == 1
    records = list(adapter.read(refs[0]))
    assert len(records) == 1
    assert records[0].role == "assistant"
    assert records[0].content_text == "still here"


def test_read_survives_garbage_token_count_values(tmp_path: Path) -> None:
    """A ``token_count`` event whose ``last_token_usage`` carries string /
    inf values must attach zeros, not raise out of read()."""
    fp = tmp_path / "2026" / "04" / "19" / "rollout-badtokens.jsonl"
    _write_jsonl(
        fp,
        [
            _session_meta(session_id="bad-tokens-uuid"),
            _assistant_msg("answer"),
            {
                "timestamp": "2026-04-19T20:00:04.000Z",
                "type": "event_msg",
                "payload": {
                    "type": "token_count",
                    "info": {
                        "last_token_usage": {
                            "input_tokens": "garbage",
                            "cached_input_tokens": [1],
                            "output_tokens": 1e999,
                        }
                    },
                },
            },
        ],
    )
    adapter = CodexAdapter(sessions_root=tmp_path)
    refs = list(adapter.enumerate())
    assert len(refs) == 1
    records = list(adapter.read(refs[0]))
    assert len(records) == 1
    assert records[0].content_text == "answer"
    assert records[0].input_tokens == 0
    assert records[0].output_tokens == 0
