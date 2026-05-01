"""Unit tests for the Pi (and OMP) adapter.

PiAdapter scans both ``~/.pi/agent/sessions/`` and
``~/.omp/agent/sessions/`` so this fixture builds two synthetic roots
under ``tmp_path`` and points the adapter at them via the ``roots``
constructor override.

Exercises:

- ``enumerate()`` discovers sessions under *both* roots.
- ``project_slug`` embeds the root label so Pi vs OMP stays separable.
- ``read(ref)`` emits one assistant Record per ``message`` event with
  usage data; usage shape ``{input, output, cacheRead, cacheWrite}``
  maps to canonical 4 keys.
- Default model is ``gpt-5`` when ``message.model`` is missing.
- Resumable reads via ``since_offset`` skip records at-or-before the
  byte floor and yield strictly fewer records than a full read.

Spec §3; codeburn-catalog §12.
"""

from __future__ import annotations

import json
import unittest
from pathlib import Path


from stackunderflow.adapters.base import Record
from stackunderflow.adapters.pi import PiAdapter
from tests.stackunderflow.adapters.contract import AdapterContract


# ── fixture builders ──────────────────────────────────────────────────


def _write_session(
    sessions_root: Path,
    session_id: str,
    *,
    cwd: str = "/tmp/work",
    model: str | None = "gpt-5",
    n_assistant: int = 1,
) -> Path:
    sessions_root.mkdir(parents=True, exist_ok=True)
    fp = sessions_root / f"{session_id}.jsonl"

    lines: list[dict] = [
        {
            "type": "session",
            "id": session_id,
            "timestamp": "2026-04-30T10:00:00Z",
            "cwd": cwd,
        },
        {
            "type": "message",
            "id": f"{session_id}-u",
            "timestamp": "2026-04-30T10:00:01Z",
            "cwd": cwd,
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
            "responseId": f"resp-{i}",
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
            "timestamp": f"2026-04-30T10:00:{2 + i:02d}Z",
            "cwd": cwd,
            "message": msg,
        })

    fp.write_text("\n".join(json.dumps(line) for line in lines) + "\n")
    return fp


# ── tests ─────────────────────────────────────────────────────────────


def test_enumerate_discovers_both_roots(tmp_path: Path) -> None:
    """PiAdapter scans both Pi and OMP roots (configurable via constructor)."""
    pi_root = tmp_path / ".pi" / "agent" / "sessions"
    omp_root = tmp_path / ".omp" / "agent" / "sessions"
    _write_session(pi_root, "sess-pi")
    _write_session(omp_root, "sess-omp")

    adapter = PiAdapter(roots=[(pi_root, "pi"), (omp_root, "omp")])
    refs = list(adapter.enumerate())
    sids = sorted(r.session_id for r in refs)
    assert sids == ["sess-omp", "sess-pi"]

    # source_hint identifies which root produced each ref.
    by_id = {r.session_id: r for r in refs}
    assert by_id["sess-pi"].source_hint == {"source": "pi"}
    assert by_id["sess-omp"].source_hint == {"source": "omp"}


def test_project_slug_embeds_source_label(tmp_path: Path) -> None:
    """project_slug starts with the root label so Pi vs OMP stays distinct."""
    pi_root = tmp_path / ".pi" / "agent" / "sessions"
    omp_root = tmp_path / ".omp" / "agent" / "sessions"
    _write_session(pi_root, "s")
    _write_session(omp_root, "s")  # same id, different root

    adapter = PiAdapter(roots=[(pi_root, "pi"), (omp_root, "omp")])
    refs = list(adapter.enumerate())
    slugs = sorted(r.project_slug for r in refs)
    # Both slugs start with their label.
    assert all(s.startswith("pi") or s.startswith("omp") for s in slugs)


def test_enumerate_returns_nothing_when_roots_missing(tmp_path: Path) -> None:
    pi_root = tmp_path / "no-pi"
    omp_root = tmp_path / "no-omp"
    adapter = PiAdapter(roots=[(pi_root, "pi"), (omp_root, "omp")])
    assert list(adapter.enumerate()) == []


def test_read_yields_assistant_record_with_usage(tmp_path: Path) -> None:
    pi_root = tmp_path / "pi"
    _write_session(pi_root, "s")
    adapter = PiAdapter(roots=[(pi_root, "pi")])
    ref = next(iter(adapter.enumerate()))
    records = list(adapter.read(ref))

    assert len(records) == 1
    rec = records[0]
    assert isinstance(rec, Record)
    assert rec.provider == "pi"
    assert rec.role == "assistant"
    assert rec.model == "gpt-5"
    assert rec.input_tokens == 100
    assert rec.output_tokens == 50
    assert rec.cache_read_tokens == 10
    assert rec.cache_create_tokens == 5


def test_default_model_when_missing(tmp_path: Path) -> None:
    pi_root = tmp_path / "pi"
    _write_session(pi_root, "s", model=None)
    adapter = PiAdapter(roots=[(pi_root, "pi")])
    ref = next(iter(adapter.enumerate()))
    rec = next(iter(adapter.read(ref)))
    # Adapter falls back to gpt-5 (codeburn-catalog §12).
    assert rec.model == "gpt-5"


def test_read_resume_with_since_offset(tmp_path: Path) -> None:
    pi_root = tmp_path / "pi"
    _write_session(pi_root, "s", n_assistant=2)
    adapter = PiAdapter(roots=[(pi_root, "pi")])
    ref = next(iter(adapter.enumerate()))

    full = list(adapter.read(ref))
    assert len(full) == 2
    midpoint = full[0].seq

    resumed = list(adapter.read(ref, since_offset=midpoint))
    assert all(r.seq > midpoint for r in resumed)
    assert len(resumed) == 1
    assert resumed[0].input_tokens == 101


# ── shared adapter contract ───────────────────────────────────────────


class TestPiAdapterContract(unittest.TestCase, AdapterContract):
    def setUp(self):
        import tempfile

        self._tmp = tempfile.TemporaryDirectory()
        pi_root = Path(self._tmp.name) / "pi" / "agent" / "sessions"
        # Two assistants so the resume contract test has a midpoint.
        _write_session(pi_root, "contract-s", n_assistant=2)
        self.adapter = PiAdapter(roots=[(pi_root, "pi")])

    def tearDown(self):
        self._tmp.cleanup()
