"""Unit tests for the OpenClaw adapter.

Builds synthetic ``{base}/{agent}/sessions/`` trees under ``tmp_path``
and points the adapter at explicit base dirs via the ``base_dirs``
constructor override.

Exercises:

- ``enumerate()`` walks each candidate base in the configured order and
  yields sessions from whichever ones exist.
- ``read(ref)`` emits one assistant Record per ``message`` event with
  usage data; user messages and ``model_change`` events don't produce
  Records but ``model_change`` updates the model context for subsequent
  records that don't carry an explicit ``message.model``.
- Token usage maps from ``{input, output, cacheRead, cacheWrite}`` to
  the canonical 4-key shape.
- Resumable reads via ``since_offset`` skip records at-or-before the
  byte floor and yield strictly fewer records than a full read; the
  model context from a pre-resume ``model_change`` is preserved.

Spec §3; codeburn-catalog §10.
"""

from __future__ import annotations

import json
import unittest
from pathlib import Path


from stackunderflow.adapters.base import Record
from stackunderflow.adapters.openclaw import OpenClawAdapter
from tests.stackunderflow.adapters.contract import AdapterContract


# ── fixture builders ──────────────────────────────────────────────────


def _write_session(
    base_dir: Path,
    agent: str,
    session_id: str,
    *,
    extra_lines: list[dict] | None = None,
) -> Path:
    sessions_dir = base_dir / agent / "sessions"
    sessions_dir.mkdir(parents=True, exist_ok=True)
    fp = sessions_dir / f"{session_id}.jsonl"

    lines: list[dict] = [
        {"type": "session", "id": session_id, "timestamp": "2026-04-30T10:00:00Z"},
        {
            "type": "message",
            "id": "u-1",
            "timestamp": "2026-04-30T10:00:01Z",
            "message": {
                "role": "user",
                "content": [{"type": "text", "text": "hello"}],
            },
        },
        {
            "type": "message",
            "id": "a-1",
            "timestamp": "2026-04-30T10:00:02Z",
            "message": {
                "role": "assistant",
                "content": [{"type": "text", "text": "hi back"}],
                "model": "claude-3-5-sonnet",
                "provider": "anthropic",
                "usage": {
                    "input": 100,
                    "output": 50,
                    "cacheRead": 10,
                    "cacheWrite": 5,
                    "cost": 0.0012,
                },
            },
        },
    ]
    if extra_lines:
        lines.extend(extra_lines)

    fp.write_text("\n".join(json.dumps(line) for line in lines) + "\n")
    return fp


# ── tests ─────────────────────────────────────────────────────────────


def test_enumerate_walks_candidate_bases_in_order(tmp_path: Path) -> None:
    """Adapter checks each base dir; sessions in different bases all surface."""
    base_a = tmp_path / "openclaw" / "agents"
    base_b = tmp_path / "clawdbot" / "agents"
    _write_session(base_a, "agent-a", "sess-a")
    _write_session(base_b, "agent-b", "sess-b")

    adapter = OpenClawAdapter(base_dirs=[base_a, base_b])
    refs = list(adapter.enumerate())
    sids = sorted(r.session_id for r in refs)
    assert sids == ["sess-a", "sess-b"]
    # Project slug = agent dir name.
    slugs = sorted(r.project_slug for r in refs)
    assert slugs == ["agent-a", "agent-b"]


def test_enumerate_skips_missing_bases(tmp_path: Path) -> None:
    """Missing candidate base dirs are silently skipped."""
    real = tmp_path / "real" / "agents"
    missing = tmp_path / "ghost" / "agents"
    _write_session(real, "agent", "s")

    adapter = OpenClawAdapter(base_dirs=[missing, real])
    refs = list(adapter.enumerate())
    assert len(refs) == 1
    assert refs[0].session_id == "s"


def test_enumerate_returns_nothing_when_all_missing(tmp_path: Path) -> None:
    adapter = OpenClawAdapter(base_dirs=[tmp_path / "a", tmp_path / "b"])
    assert list(adapter.enumerate()) == []


def test_read_yields_assistant_record_with_usage(tmp_path: Path) -> None:
    base = tmp_path / "openclaw" / "agents"
    _write_session(base, "agent", "s1")
    adapter = OpenClawAdapter(base_dirs=[base])
    ref = next(iter(adapter.enumerate()))
    records = list(adapter.read(ref))

    # One assistant message → one Record (user messages don't drive cost).
    assert len(records) == 1
    rec = records[0]
    assert isinstance(rec, Record)
    assert rec.provider == "openclaw"
    assert rec.role == "assistant"
    assert rec.model == "claude-3-5-sonnet"
    assert rec.input_tokens == 100
    assert rec.output_tokens == 50
    assert rec.cache_read_tokens == 10
    assert rec.cache_create_tokens == 5


def test_model_change_event_updates_context(tmp_path: Path) -> None:
    """``model_change`` event affects assistant messages without an explicit model."""
    base = tmp_path / "b" / "agents"
    sessions_dir = base / "agent" / "sessions"
    sessions_dir.mkdir(parents=True)
    lines = [
        {"type": "session", "id": "s", "timestamp": "2026-04-30T10:00:00Z"},
        {
            "type": "model_change",
            "timestamp": "2026-04-30T10:00:00.5Z",
            "data": {"model": "claude-3-5-haiku"},
        },
        {
            "type": "message",
            "id": "a-1",
            "timestamp": "2026-04-30T10:00:01Z",
            "message": {
                "role": "assistant",
                "content": [{"type": "text", "text": "x"}],
                "usage": {"input": 1, "output": 1, "cacheRead": 0, "cacheWrite": 0},
            },
        },
    ]
    (sessions_dir / "s.jsonl").write_text(
        "\n".join(json.dumps(line) for line in lines) + "\n",
    )

    adapter = OpenClawAdapter(base_dirs=[base])
    ref = next(iter(adapter.enumerate()))
    rec = next(iter(adapter.read(ref)))
    assert rec.model == "claude-3-5-haiku"


def test_read_resume_with_since_offset(tmp_path: Path) -> None:
    base = tmp_path / "b" / "agents"
    extra = [
        {
            "type": "message",
            "id": "a-2",
            "timestamp": "2026-04-30T10:00:03Z",
            "message": {
                "role": "assistant",
                "content": [{"type": "text", "text": "second"}],
                "model": "claude-3-5-sonnet",
                "usage": {"input": 7, "output": 3, "cacheRead": 0, "cacheWrite": 0},
            },
        },
    ]
    _write_session(base, "agent", "s", extra_lines=extra)

    adapter = OpenClawAdapter(base_dirs=[base])
    ref = next(iter(adapter.enumerate()))

    full = list(adapter.read(ref))
    assert len(full) == 2
    midpoint = full[0].seq

    resumed = list(adapter.read(ref, since_offset=midpoint))
    assert all(r.seq > midpoint for r in resumed)
    assert len(resumed) == 1
    assert resumed[0].input_tokens == 7


# ── shared adapter contract ───────────────────────────────────────────


class TestOpenClawAdapterContract(unittest.TestCase, AdapterContract):
    def setUp(self):
        import tempfile

        self._tmp = tempfile.TemporaryDirectory()
        base = Path(self._tmp.name) / "openclaw" / "agents"
        # Two assistant messages so the resume contract test has a midpoint.
        extra = [
            {
                "type": "message",
                "id": "a-2",
                "timestamp": "2026-04-30T10:00:03Z",
                "message": {
                    "role": "assistant",
                    "content": [{"type": "text", "text": "again"}],
                    "model": "claude-3-5-sonnet",
                    "usage": {"input": 1, "output": 1, "cacheRead": 0, "cacheWrite": 0},
                },
            },
        ]
        _write_session(base, "agent", "contract-s", extra_lines=extra)
        self.adapter = OpenClawAdapter(base_dirs=[base])

    def tearDown(self):
        self._tmp.cleanup()
