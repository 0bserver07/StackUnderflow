"""Unit tests for the Hermes adapter.

Exercises:
- ``enumerate()`` discovers sessions under roots recursively.
- ``read(ref)`` emits one assistant Record per ``message`` event with
  usage data.
- Handles model_change events to establish current model.
- Default model is ``hermes-unknown`` when model is missing.
- Resumable reads via ``since_offset`` skip records at-or-before the
  byte floor.

Spec §3; codeburn-catalog §12.
"""

from __future__ import annotations

import json
import unittest
from pathlib import Path

from stackunderflow.adapters.base import Record
from stackunderflow.adapters.hermes import HermesAdapter
from tests.stackunderflow.adapters.contract import AdapterContract


# ── fixture builders ──────────────────────────────────────────────────


def _write_session(
    sessions_root: Path,
    session_id: str,
    *,
    model: str | None = "claude-3-5-sonnet",
    n_assistant: int = 1,
) -> Path:
    sessions_root.mkdir(parents=True, exist_ok=True)
    fp = sessions_root / f"{session_id}.jsonl"

    lines: list[dict] = [
        {
            "type": "session",
            "id": session_id,
            "timestamp": "2026-05-26T10:00:00Z",
        },
        {
            "type": "message",
            "id": f"{session_id}-u",
            "timestamp": "2026-05-26T10:00:01Z",
            "message": {
                "role": "user",
                "content": [{"type": "text", "text": "hello"}],
            },
        },
    ]
    for i in range(n_assistant):
        msg: dict = {
            "role": "assistant",
            "content": [{"type": "text", "text": f"reply-{i}"}],
            "usage": {
                "input": 100 + i,
                "output": 50 + i,
                "cacheRead": 10,
                "cacheWrite": 5,
            },
        }
        if model is not None:
            msg["model"] = model
        lines.append({
            "type": "message",
            "id": f"{session_id}-a-{i}",
            "timestamp": f"2026-05-26T10:00:{2 + i:02d}Z",
            "message": msg,
        })

    fp.write_text("\n".join(json.dumps(line) for line in lines) + "\n")
    return fp


# ── tests ─────────────────────────────────────────────────────────────


def test_enumerate_discovers_nested_roots(tmp_path: Path) -> None:
    """HermesAdapter scans the roots recursively using glob("**/*.jsonl")."""
    hermes_root = tmp_path / "sessions"
    project1_dir = hermes_root / "project-alpha"
    _write_session(hermes_root, "flat-sess")
    _write_session(project1_dir, "nested-sess")

    adapter = HermesAdapter(roots=[hermes_root])
    refs = list(adapter.enumerate())
    sids = sorted(r.session_id for r in refs)
    assert sids == ["flat-sess", "nested-sess"]

    # project_slug should resolve properly based on directory hierarchy
    by_id = {r.session_id: r for r in refs}
    assert by_id["flat-sess"].project_slug == "hermes"
    assert by_id["nested-sess"].project_slug == "project-alpha"


def test_enumerate_returns_nothing_when_roots_missing(tmp_path: Path) -> None:
    hermes_root = tmp_path / "no-hermes"
    adapter = HermesAdapter(roots=[hermes_root])
    assert list(adapter.enumerate()) == []


def test_read_yields_assistant_record_with_usage(tmp_path: Path) -> None:
    hermes_root = tmp_path / "sessions"
    _write_session(hermes_root, "s")
    adapter = HermesAdapter(roots=[hermes_root])
    ref = next(iter(adapter.enumerate()))
    records = list(adapter.read(ref))

    assert len(records) == 1
    rec = records[0]
    assert isinstance(rec, Record)
    assert rec.provider == "hermes"
    assert rec.role == "assistant"
    assert rec.model == "claude-3-5-sonnet"
    assert rec.input_tokens == 100
    assert rec.output_tokens == 50
    assert rec.cache_read_tokens == 10
    assert rec.cache_create_tokens == 5


def test_model_change_events(tmp_path: Path) -> None:
    hermes_root = tmp_path / "sessions"
    hermes_root.mkdir(parents=True, exist_ok=True)
    fp = hermes_root / "s.jsonl"

    lines: list[dict] = [
        {
            "type": "session",
            "id": "s",
            "timestamp": "2026-05-26T10:00:00Z",
        },
        {
            "type": "model_change",
            "data": {"model": "claude-3-opus"},
            "timestamp": "2026-05-26T10:00:01Z",
        },
        {
            "type": "message",
            "id": "s-a-1",
            "timestamp": "2026-05-26T10:00:02Z",
            "message": {
                "role": "assistant",
                "content": [{"type": "text", "text": "hello"}],
                "usage": {"input": 10, "output": 5},
            },
        },
    ]
    fp.write_text("\n".join(json.dumps(line) for line in lines) + "\n")

    adapter = HermesAdapter(roots=[hermes_root])
    ref = next(iter(adapter.enumerate()))
    records = list(adapter.read(ref))

    assert len(records) == 1
    assert records[0].model == "claude-3-opus"


def test_default_model_when_missing(tmp_path: Path) -> None:
    hermes_root = tmp_path / "sessions"
    _write_session(hermes_root, "s", model=None)
    adapter = HermesAdapter(roots=[hermes_root])
    ref = next(iter(adapter.enumerate()))
    rec = next(iter(adapter.read(ref)))
    assert rec.model == "hermes-unknown"


def test_read_resume_with_since_offset(tmp_path: Path) -> None:
    hermes_root = tmp_path / "sessions"
    _write_session(hermes_root, "s", n_assistant=2)
    adapter = HermesAdapter(roots=[hermes_root])
    ref = next(iter(adapter.enumerate()))

    full = list(adapter.read(ref))
    assert len(full) == 2
    midpoint = full[0].seq

    resumed = list(adapter.read(ref, since_offset=midpoint))
    assert all(r.seq > midpoint for r in resumed)
    assert len(resumed) == 1
    assert resumed[0].input_tokens == 101


# ── shared adapter contract ───────────────────────────────────────────


class TestHermesAdapterContract(unittest.TestCase, AdapterContract):
    def setUp(self):
        import tempfile

        self._tmp = tempfile.TemporaryDirectory()
        hermes_root = Path(self._tmp.name) / "sessions"
        # Two assistants so the resume contract test has a midpoint.
        _write_session(hermes_root, "contract-s", n_assistant=2)
        self.adapter = HermesAdapter(roots=[hermes_root])

    def tearDown(self):
        self._tmp.cleanup()


# ── malformed-input hardening (ingest-surface sweep, 2026-07) ─────────


def test_non_dict_json_lines_are_skipped(tmp_path: Path) -> None:
    """Lines that parse as JSON but aren't objects (list/str/number) must be
    skipped by read(), not crash the generator."""
    root = tmp_path / "hermes" / "sessions"
    root.mkdir(parents=True)
    (root / "s.jsonl").write_text(
        json.dumps({"type": "session", "id": "s"}) + "\n"
        + "[1, 2, 3]\n"
        + '"just a string"\n'
        + "42\n"
        + json.dumps(
            {
                "type": "message",
                "id": "a",
                "message": {
                    "role": "assistant",
                    "model": "claude-3-5-sonnet",
                    "content": [{"type": "text", "text": "ok"}],
                    "usage": {"input": 1, "output": 1},
                },
            }
        )
        + "\n"
    )
    adapter = HermesAdapter(roots=[root])
    ref = next(iter(adapter.enumerate()))
    records = list(adapter.read(ref))
    assert len(records) == 1
    assert records[0].uuid == "a"


def test_enumerate_survives_non_dict_first_line(tmp_path: Path) -> None:
    """A session file whose first line is ``[1,2]`` must not crash
    enumerate() (the peek helper) — it falls back to the filename stem."""
    root = tmp_path / "hermes" / "sessions"
    root.mkdir(parents=True)
    (root / "weird.jsonl").write_text("[1, 2]\n")
    adapter = HermesAdapter(roots=[root])
    refs = list(adapter.enumerate())
    assert len(refs) == 1
    assert refs[0].session_id == "weird"
    assert list(adapter.read(refs[0])) == []
