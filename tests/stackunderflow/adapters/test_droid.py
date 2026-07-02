"""Unit tests for the Droid (Factory) adapter.

Builds a synthetic ``sessions/{projectHash}/...`` tree under ``tmp_path``
and points the adapter at it via the ``sessions_root`` constructor
override.

Exercises:

- ``enumerate()`` discovers ``*.jsonl`` under each project hash dir and
  hydrates ``project_slug`` from the ``cwd`` carried on ``session_start``.
- ``read(ref)`` parses ``message`` events and distributes the
  session-level token totals (in ``.settings.json``) evenly across
  detected assistant messages, with the leftover landing on the last
  assistant turn so the sum matches the totals.
- Missing settings file degrades gracefully (zero tokens, no crash).
- Resumable reads via ``since_offset`` skip records at-or-before the
  byte floor and yield strictly fewer records than a full read.

Spec §3; codeburn-catalog §6.
"""

from __future__ import annotations

import json
import unittest
from pathlib import Path

import pytest

from stackunderflow.adapters.base import SessionRef
from stackunderflow.adapters.droid import DroidAdapter
from tests.stackunderflow.adapters.contract import AdapterContract


# ── fixture builders ──────────────────────────────────────────────────


def _write_session(
    sessions_root: Path,
    project_hash: str,
    session_id: str,
    *,
    cwd: str = "/tmp/work",
    model: str = "claude-3-5-sonnet",
    n_assistant: int = 1,
    totals: dict | None = None,
) -> Path:
    """Create one synthetic Droid session and return the .jsonl path."""
    project_dir = sessions_root / project_hash
    project_dir.mkdir(parents=True, exist_ok=True)
    fp = project_dir / f"{session_id}.jsonl"

    lines: list[dict] = [
        {
            "type": "session_start",
            "id": session_id,
            "timestamp": "2026-04-30T10:00:00Z",
            "cwd": cwd,
        },
        {
            "type": "message",
            "id": f"{session_id}-user-0",
            "timestamp": "2026-04-30T10:00:01Z",
            "cwd": cwd,
            "message": {
                "role": "user",
                "content": [{"type": "text", "text": "hello"}],
            },
        },
    ]
    for i in range(n_assistant):
        lines.append({
            "type": "message",
            "id": f"{session_id}-assistant-{i}",
            "timestamp": f"2026-04-30T10:00:{2 + i:02d}Z",
            "cwd": cwd,
            "message": {
                "role": "assistant",
                "content": [{"type": "text", "text": f"reply-{i}"}],
            },
        })

    fp.write_text("\n".join(json.dumps(line) for line in lines) + "\n")

    if totals is None:
        totals = {
            "inputTokens": 100,
            "outputTokens": 50,
            "cacheCreationTokens": 0,
            "cacheReadTokens": 0,
            "thinkingTokens": 0,
        }
    settings = {"model": model, "tokenUsage": totals}
    (project_dir / f"{session_id}.settings.json").write_text(json.dumps(settings))

    return fp


@pytest.fixture
def synthetic_sessions(tmp_path: Path) -> Path:
    sessions = tmp_path / "sessions"
    sessions.mkdir()
    _write_session(sessions, "projectABC", "sess-01")
    return sessions


# ── tests ─────────────────────────────────────────────────────────────


def test_enumerate_yields_one_session_ref(synthetic_sessions: Path) -> None:
    adapter = DroidAdapter(sessions_root=synthetic_sessions)
    refs = list(adapter.enumerate())

    assert len(refs) == 1
    ref = refs[0]
    assert isinstance(ref, SessionRef)
    assert ref.provider == "droid"
    assert ref.source_kind == "file"
    assert ref.session_id == "sess-01"
    # cwd was /tmp/work → claude-style slug is `-tmp-work` (or
    # `-private-tmp-work` on macOS where /tmp resolves through a
    # symlink). Just check the session_id was carried through.
    assert ref.file_path.name == "sess-01.jsonl"


def test_enumerate_returns_nothing_when_root_missing(tmp_path: Path) -> None:
    adapter = DroidAdapter(sessions_root=tmp_path / "does-not-exist")
    assert list(adapter.enumerate()) == []


def test_session_token_distribution_evenly_split(tmp_path: Path) -> None:
    """Three assistant messages with totals 90/30 split exactly 30/10 each."""
    sessions = tmp_path / "sessions"
    sessions.mkdir()
    _write_session(
        sessions, "p", "s",
        n_assistant=3,
        totals={
            "inputTokens": 90,
            "outputTokens": 30,
            "cacheCreationTokens": 6,
            "cacheReadTokens": 12,
            "thinkingTokens": 0,
        },
    )
    adapter = DroidAdapter(sessions_root=sessions)
    ref = next(iter(adapter.enumerate()))
    records = list(adapter.read(ref))

    # 1 user + 3 assistant = 4 records.
    assistants = [r for r in records if r.role == "assistant"]
    assert len(assistants) == 3

    # Even split: 30 input each, 10 output each, 2 cache_create, 4 cache_read.
    for r in assistants:
        assert r.input_tokens == 30
        assert r.output_tokens == 10
        assert r.cache_create_tokens == 2
        assert r.cache_read_tokens == 4

    # Sum across assistants = totals.
    assert sum(r.input_tokens for r in assistants) == 90
    assert sum(r.output_tokens for r in assistants) == 30


def test_session_token_distribution_remainder_lands_on_last(
    tmp_path: Path,
) -> None:
    """Two assistants, 7 input total → 3 + 4 (remainder on last)."""
    sessions = tmp_path / "sessions"
    sessions.mkdir()
    _write_session(
        sessions, "p", "s",
        n_assistant=2,
        totals={
            "inputTokens": 7,
            "outputTokens": 5,
            "cacheCreationTokens": 0,
            "cacheReadTokens": 0,
            "thinkingTokens": 0,
        },
    )
    adapter = DroidAdapter(sessions_root=sessions)
    ref = next(iter(adapter.enumerate()))
    records = [r for r in adapter.read(ref) if r.role == "assistant"]

    assert [r.input_tokens for r in records] == [3, 4]
    assert [r.output_tokens for r in records] == [2, 3]


def test_thinking_tokens_fold_into_output(tmp_path: Path) -> None:
    """``thinkingTokens`` should add to ``output`` so cost matches Anthropic billing."""
    sessions = tmp_path / "sessions"
    sessions.mkdir()
    _write_session(
        sessions, "p", "s",
        n_assistant=1,
        totals={
            "inputTokens": 10,
            "outputTokens": 5,
            "cacheCreationTokens": 0,
            "cacheReadTokens": 0,
            "thinkingTokens": 7,
        },
    )
    adapter = DroidAdapter(sessions_root=sessions)
    ref = next(iter(adapter.enumerate()))
    [user, assistant] = list(adapter.read(ref))
    assert assistant.output_tokens == 12  # 5 + 7


def test_read_handles_missing_settings_file(tmp_path: Path) -> None:
    """No companion .settings.json → records still emit, with zero tokens."""
    sessions = tmp_path / "sessions"
    sessions.mkdir()
    fp = _write_session(sessions, "p", "s", n_assistant=1)
    fp.with_suffix(".settings.json").unlink()

    adapter = DroidAdapter(sessions_root=sessions)
    ref = next(iter(adapter.enumerate()))
    records = list(adapter.read(ref))
    assistant = next(r for r in records if r.role == "assistant")
    assert assistant.input_tokens == 0
    assert assistant.output_tokens == 0
    assert assistant.model is None


def test_read_resume_with_since_offset(tmp_path: Path) -> None:
    sessions = tmp_path / "sessions"
    sessions.mkdir()
    _write_session(sessions, "p", "s", n_assistant=2)
    adapter = DroidAdapter(sessions_root=sessions)
    ref = next(iter(adapter.enumerate()))

    full = list(adapter.read(ref))
    assert len(full) >= 2
    midpoint = full[len(full) // 2].seq

    resumed = list(adapter.read(ref, since_offset=midpoint))
    assert all(r.seq > midpoint for r in resumed)
    assert len(resumed) < len(full)


# ── shared adapter contract ───────────────────────────────────────────


class TestDroidAdapterContract(unittest.TestCase, AdapterContract):
    def setUp(self):
        import tempfile

        self._tmp = tempfile.TemporaryDirectory()
        sessions = Path(self._tmp.name) / "sessions"
        sessions.mkdir()
        _write_session(sessions, "phash", "contract-session", n_assistant=2)
        self.adapter = DroidAdapter(sessions_root=sessions)

    def tearDown(self):
        self._tmp.cleanup()


# ── malformed-input hardening (ingest-surface sweep, 2026-07) ─────────


def test_malformed_lines_do_not_crash_read(tmp_path: Path) -> None:
    """Non-object JSON lines and a string ``message`` block must be skipped;
    the valid assistant turn still gets the distributed session totals."""
    sessions = tmp_path / "sessions"
    project_dir = sessions / "projX"
    project_dir.mkdir(parents=True)
    fp = project_dir / "sess-bad.jsonl"
    fp.write_text(
        json.dumps({"type": "session_start", "id": "sess-bad", "cwd": "/tmp/w"}) + "\n"
        + "[1, 2, 3]\n"
        + '"just a string"\n'
        + json.dumps({"type": "message", "id": "m0", "message": "not a dict"}) + "\n"
        + json.dumps({
            "type": "message",
            "id": "m1",
            "timestamp": "2026-04-30T10:00:02Z",
            "cwd": {"bad": 1},  # non-string cwd must be dropped, not crash
            "message": {"role": "assistant", "content": [{"type": "text", "text": "ok"}]},
        }) + "\n"
    )
    (project_dir / "sess-bad.settings.json").write_text(json.dumps({
        "model": "claude-3-5-sonnet",
        "tokenUsage": {"inputTokens": 10, "outputTokens": 4},
    }))
    adapter = DroidAdapter(sessions_root=sessions)
    ref = next(iter(adapter.enumerate()))
    records = list(adapter.read(ref))
    assert len(records) == 1
    assert records[0].role == "assistant"
    assert records[0].input_tokens == 10
    assert records[0].output_tokens == 4
    assert records[0].cwd is None


def test_enumerate_survives_non_dict_first_line(tmp_path: Path) -> None:
    """A session whose first line is ``[1,2]`` must not crash enumerate()
    (the session-meta peek) — it falls back to the filename stem."""
    sessions = tmp_path / "sessions"
    project_dir = sessions / "projX"
    project_dir.mkdir(parents=True)
    (project_dir / "weird.jsonl").write_text("[1, 2]\n")
    adapter = DroidAdapter(sessions_root=sessions)
    refs = list(adapter.enumerate())
    assert len(refs) == 1
    assert refs[0].session_id == "weird"
    assert list(adapter.read(refs[0])) == []
