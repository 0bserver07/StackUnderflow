"""Unit tests for the Grok (xAI ``grok`` CLI) adapter.

Builds a synthetic ``~/.grok/sessions/<url-encoded-cwd>/<uuid>/`` tree
under ``tmp_path`` and points the adapter at it via the
``sessions_root`` constructor override.

Exercises:

- ``enumerate()`` yields one ref per ``chat_history.jsonl``, with the
  Claude-style ``project_slug`` decoded from the URL-encoded cwd and the
  session UUID dir name as ``session_id``.
- ``read()`` parses a multi-role transcript (user / reasoning / assistant
  / tool_result); the ``system`` prompt and unknown types are skipped.
- An ``encrypted_content`` reasoning record is handled (text unavailable,
  0 tokens, no crash).
- Tokens are estimated as ``len(content) // 4`` and Records carry
  ``raw["cost_source"] = "estimated"``.
- Tool names are pulled from ``tool_calls`` and mapped to canonical labels.
- Timestamps are derived from the session UUIDv7 (mtime fallback).
- ``since_offset`` resumes mid-file; the shared ``AdapterContract`` holds.

Spec §3 (multi-provider).
"""

from __future__ import annotations

import json
import unittest
import urllib.parse
from datetime import UTC, datetime
from pathlib import Path

import pytest

from stackunderflow.adapters.base import Record, SessionRef
from stackunderflow.adapters.grok import GrokAdapter

from tests.stackunderflow.adapters.contract import AdapterContract

# A real-shaped UUIDv7 (its first 48 bits decode to 2026-06-25T15:43:35Z).
_SESSION_UUID = "019eff73-6f8f-7830-a33a-fc37e624d51b"


# ── fixture builders ──────────────────────────────────────────────────


def _system_record(text: str = "You are Grok, a coding agent.") -> dict:
    return {"type": "system", "content": text}


def _user_record(text: str = "Please refactor this function.") -> dict:
    return {
        "type": "user",
        "content": [{"type": "text", "text": text}],
    }


def _reasoning_record(
    *,
    encrypted: str = "gAAAAABk-encrypted-reasoning-blob",
    summary: str = "Plan the refactor",
) -> dict:
    # Reasoning is encrypted at rest: no ``content`` key, only
    # ``encrypted_content`` plus a plaintext ``summary``.
    return {
        "type": "reasoning",
        "id": "rs_646529e9-ecc3-9a9f",
        "status": "completed",
        "summary": [{"type": "summary_text", "text": summary}],
        "encrypted_content": encrypted,
    }


def _assistant_record(
    text: str = "Here's the refactored version.",
    *,
    tool_calls: list[dict] | None = None,
    model_id: str = "grok-build",
) -> dict:
    rec: dict = {
        "type": "assistant",
        "content": text,
        "model_id": model_id,
        "model_fingerprint": "fp_36bb860c5ab2a013",
    }
    if tool_calls is not None:
        rec["tool_calls"] = tool_calls
    return rec


def _tool_result_record(text: str = "def foo(): ...") -> dict:
    return {
        "type": "tool_result",
        "tool_call_id": "call-ad465794-0",
        "content": text,
    }


def _write_session(
    sessions_root: Path,
    cwd: str,
    session_id: str,
    records: list[dict],
) -> Path:
    """Write ``records`` to ``<root>/<url-encoded cwd>/<session_id>/chat_history.jsonl``."""
    encoded = urllib.parse.quote(cwd, safe="")
    session_dir = sessions_root / encoded / session_id
    session_dir.mkdir(parents=True, exist_ok=True)
    fp = session_dir / "chat_history.jsonl"
    fp.write_text("\n".join(json.dumps(r) for r in records) + "\n")
    return fp


@pytest.fixture
def synthetic_sessions(tmp_path: Path) -> Path:
    sessions_root = tmp_path / "sessions"
    sessions_root.mkdir()
    _write_session(
        sessions_root,
        "/Users/me/proj",
        _SESSION_UUID,
        [
            _system_record(),
            _user_record(),
            _reasoning_record(),
            _assistant_record(
                tool_calls=[
                    {"id": "call-1", "name": "read_file", "arguments": "{}"},
                    {"id": "call-2", "name": "run_terminal_command", "arguments": "{}"},
                ],
            ),
            _tool_result_record(),
        ],
    )
    return sessions_root


# ── enumerate ─────────────────────────────────────────────────────────


def test_enumerate_yields_one_session_ref(synthetic_sessions: Path) -> None:
    adapter = GrokAdapter(sessions_root=synthetic_sessions)
    refs = list(adapter.enumerate())

    assert len(refs) == 1
    ref = refs[0]
    assert isinstance(ref, SessionRef)
    assert ref.provider == "grok"
    assert ref.source_kind == "file"
    # Decoded "/Users/me/proj" → Claude-style slug.
    assert ref.project_slug == "-Users-me-proj"
    # The session UUID dir name is the session id.
    assert ref.session_id == _SESSION_UUID
    assert ref.file_path.name == "chat_history.jsonl"
    assert ref.file_size > 0


def test_enumerate_returns_nothing_when_root_missing(tmp_path: Path) -> None:
    adapter = GrokAdapter(sessions_root=tmp_path / "missing")
    assert list(adapter.enumerate()) == []


def test_enumerate_decodes_url_encoded_cwd_to_claude_slug(tmp_path: Path) -> None:
    """The project dir name is the URL-encoded cwd; decode it and run the
    Claude transform (every non-alphanumeric char → ``-``).

    ``_`` and ``.`` must both become ``-`` so a Grok session lines up with
    the same repo's Claude sessions (Claude names ``~/.claude`` →
    ``-Users-yadkonrad--claude``).
    """
    sessions_root = tmp_path / "sessions"
    sessions_root.mkdir()
    _write_session(
        sessions_root,
        "/Users/me/dev_dev/.config/my_proj",
        _SESSION_UUID,
        [_user_record(), _assistant_record()],
    )
    adapter = GrokAdapter(sessions_root=sessions_root)
    ref = next(iter(adapter.enumerate()))
    assert ref.project_slug == "-Users-me-dev-dev--config-my-proj"


def test_enumerate_multiple_sessions_under_one_project(tmp_path: Path) -> None:
    sessions_root = tmp_path / "sessions"
    sessions_root.mkdir()
    sid_a = _SESSION_UUID
    sid_b = "019eff82-2387-7ee1-952a-e6d93fe953ca"
    _write_session(sessions_root, "/Users/me/proj", sid_a, [_assistant_record("a")])
    _write_session(sessions_root, "/Users/me/proj", sid_b, [_assistant_record("b")])

    adapter = GrokAdapter(sessions_root=sessions_root)
    refs = list(adapter.enumerate())
    assert len(refs) == 2
    assert {r.session_id for r in refs} == {sid_a, sid_b}
    assert {r.project_slug for r in refs} == {"-Users-me-proj"}


def test_enumerate_skips_session_dir_without_transcript(tmp_path: Path) -> None:
    sessions_root = tmp_path / "sessions"
    sessions_root.mkdir()
    _write_session(sessions_root, "/Users/me/proj", _SESSION_UUID, [_assistant_record()])
    # A session dir with no chat_history.jsonl is ignored.
    encoded = urllib.parse.quote("/Users/me/proj", safe="")
    (sessions_root / encoded / "empty-session").mkdir(parents=True)

    adapter = GrokAdapter(sessions_root=sessions_root)
    refs = list(adapter.enumerate())
    assert len(refs) == 1
    assert refs[0].session_id == _SESSION_UUID


# ── read ──────────────────────────────────────────────────────────────


def test_read_parses_multi_role_transcript(synthetic_sessions: Path) -> None:
    adapter = GrokAdapter(sessions_root=synthetic_sessions)
    ref = next(iter(adapter.enumerate()))
    records = list(adapter.read(ref))

    # system is skipped; user / reasoning / assistant / tool_result emit.
    roles = [r.role for r in records]
    assert roles == ["user", "reasoning", "assistant", "tool"]
    for rec in records:
        assert isinstance(rec, Record)
        assert rec.provider == "grok"

    user = records[0]
    assert "refactor this function" in user.content_text.lower()

    asst = records[2]
    assert "refactored version" in asst.content_text
    assert asst.model == "grok-build"
    assert asst.output_tokens > 0


def test_read_skips_system_record(synthetic_sessions: Path) -> None:
    adapter = GrokAdapter(sessions_root=synthetic_sessions)
    ref = next(iter(adapter.enumerate()))
    records = list(adapter.read(ref))
    assert all(r.role != "system" for r in records)


def test_encrypted_reasoning_is_handled_no_crash(tmp_path: Path) -> None:
    """A reasoning record carries only ``encrypted_content`` — its text is
    unavailable (not decrypted), so it estimates to 0 tokens without
    raising."""
    sessions_root = tmp_path / "sessions"
    sessions_root.mkdir()
    _write_session(
        sessions_root,
        "/Users/me/proj",
        _SESSION_UUID,
        [_reasoning_record(encrypted="x" * 4000)],  # long blob, must NOT be counted
    )
    adapter = GrokAdapter(sessions_root=sessions_root)
    ref = next(iter(adapter.enumerate()))
    records = list(adapter.read(ref))  # must not raise

    assert len(records) == 1
    rec = records[0]
    assert rec.role == "reasoning"
    assert rec.content_text == ""
    # The 4000-char encrypted blob is NOT counted as tokens.
    assert rec.output_tokens == 0
    assert rec.input_tokens == 0
    # Still flagged estimated.
    assert rec.raw.get("cost_source") == "estimated"
    # The raw payload is preserved (encrypted blob retained verbatim).
    assert rec.raw.get("encrypted_content") == "x" * 4000


def test_token_estimate_is_chars_over_4(tmp_path: Path) -> None:
    """Confirm the chars/4 estimate on the assistant turn's content."""
    sessions_root = tmp_path / "sessions"
    sessions_root.mkdir()
    _write_session(
        sessions_root,
        "/Users/me/proj",
        _SESSION_UUID,
        [_assistant_record("b" * 40)],  # 40 chars → 10 output tokens
    )
    adapter = GrokAdapter(sessions_root=sessions_root)
    ref = next(iter(adapter.enumerate()))
    rec = next(iter(adapter.read(ref)))
    assert rec.output_tokens == 10
    assert rec.input_tokens == 0


def test_record_carries_estimated_cost_source_flag(synthetic_sessions: Path) -> None:
    adapter = GrokAdapter(sessions_root=synthetic_sessions)
    ref = next(iter(adapter.enumerate()))
    for rec in adapter.read(ref):
        assert rec.raw.get("cost_source") == "estimated"


def test_tool_extraction_from_tool_calls(synthetic_sessions: Path) -> None:
    adapter = GrokAdapter(sessions_root=synthetic_sessions)
    ref = next(iter(adapter.enumerate()))
    asst = next(r for r in adapter.read(ref) if r.role == "assistant")
    # read_file -> Read; run_terminal_command -> Bash (per _TOOL_NAME_MAP).
    assert "Read" in asst.tools
    assert "Bash" in asst.tools


def test_unknown_tool_name_passes_through(tmp_path: Path) -> None:
    sessions_root = tmp_path / "sessions"
    sessions_root.mkdir()
    _write_session(
        sessions_root,
        "/Users/me/proj",
        _SESSION_UUID,
        [_assistant_record(tool_calls=[{"id": "c1", "name": "novel_tool", "arguments": "{}"}])],
    )
    adapter = GrokAdapter(sessions_root=sessions_root)
    ref = next(iter(adapter.enumerate()))
    asst = next(r for r in adapter.read(ref) if r.role == "assistant")
    assert "novel_tool" in asst.tools


def test_timestamp_derived_from_session_uuidv7(synthetic_sessions: Path) -> None:
    adapter = GrokAdapter(sessions_root=synthetic_sessions)
    ref = next(iter(adapter.enumerate()))
    rec = next(iter(adapter.read(ref)))
    # 019eff73-6f8f-7... → 2026-06-25 (first 48 bits = unix ms).
    parsed = datetime.fromisoformat(rec.timestamp)
    assert parsed.year == 2026
    assert parsed.month == 6
    assert parsed.day == 25


def test_timestamp_falls_back_to_mtime_for_non_uuid_session(tmp_path: Path) -> None:
    sessions_root = tmp_path / "sessions"
    sessions_root.mkdir()
    fp = _write_session(
        sessions_root,
        "/Users/me/proj",
        "not-a-uuid",
        [_assistant_record()],
    )
    adapter = GrokAdapter(sessions_root=sessions_root)
    ref = next(iter(adapter.enumerate()))
    rec = next(iter(adapter.read(ref)))
    # Falls back to the transcript mtime — must still be a valid ISO stamp.
    expected = datetime.fromtimestamp(fp.stat().st_mtime, tz=UTC).isoformat()
    assert rec.timestamp == expected


def test_since_offset_resumes_mid_file(synthetic_sessions: Path) -> None:
    adapter = GrokAdapter(sessions_root=synthetic_sessions)
    ref = next(iter(adapter.enumerate()))
    full = list(adapter.read(ref))
    assert len(full) == 4

    midpoint = full[1].seq
    resumed = list(adapter.read(ref, since_offset=midpoint))
    assert all(r.seq > midpoint for r in resumed)
    assert len(resumed) < len(full)
    assert "tool" in [r.role for r in resumed]


def test_malformed_json_line_is_skipped(tmp_path: Path) -> None:
    sessions_root = tmp_path / "sessions"
    session_dir = sessions_root / urllib.parse.quote("/Users/me/proj", safe="") / _SESSION_UUID
    session_dir.mkdir(parents=True)
    fp = session_dir / "chat_history.jsonl"
    fp.write_text(
        json.dumps(_user_record("before bad"))
        + "\n"
        + "{not-json}\n"
        + json.dumps(_assistant_record("after bad"))
        + "\n"
    )
    adapter = GrokAdapter(sessions_root=sessions_root)
    ref = next(iter(adapter.enumerate()))
    records = list(adapter.read(ref))  # must not raise
    assert [r.role for r in records] == ["user", "assistant"]


def test_watch_paths_returns_root(tmp_path: Path) -> None:
    root = tmp_path / "sessions"
    adapter = GrokAdapter(sessions_root=root)
    assert adapter.watch_paths() == [root]


# ── shared adapter contract ───────────────────────────────────────────


class TestGrokAdapterContract(unittest.TestCase, AdapterContract):
    def setUp(self):
        import tempfile

        self._tmp = tempfile.TemporaryDirectory()
        sessions_root = Path(self._tmp.name) / "sessions"
        sessions_root.mkdir()
        # Multi-record so the storage-aware contract test has a midpoint.
        _write_session(
            sessions_root,
            "/Users/me/contract",
            _SESSION_UUID,
            [
                _user_record("hi"),
                _reasoning_record(),
                _assistant_record("hello"),
                _tool_result_record(),
            ],
        )
        self.adapter = GrokAdapter(sessions_root=sessions_root)

    def tearDown(self):
        self._tmp.cleanup()
