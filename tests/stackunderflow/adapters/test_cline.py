"""Unit tests for the Cline adapter.

Builds a synthetic VS Code globalStorage tasks tree under ``tmp_path`` and
points the adapter at it (the constructor accepts a ``tasks_root`` override
for exactly this use case). Exercises:

- ``enumerate()`` emits one ``SessionRef`` per task directory with
  ``provider="cline"``, ``source_kind="file"``, and the task-dir name as
  ``session_id``.
- ``read(ref)`` parses the ``api_req_started`` event tokens, extracts the
  ``<model>...</model>`` declaration from the first user message, and emits
  one assistant ``Record``.
- Resumable reads via ``since_offset`` skip records at-or-before the seq
  floor and yield strictly fewer records than a full read.

Spec §3.2; codeburn-catalog §15.
"""

from __future__ import annotations

import json
import unittest
from pathlib import Path

import pytest

from stackunderflow.adapters.base import Record, SessionRef
from stackunderflow.adapters.cline import ClineAdapter
from tests.stackunderflow.adapters.contract import AdapterContract


# ── fixture builders ──────────────────────────────────────────────────


def _write_task(
    tasks_root: Path,
    task_id: str,
    *,
    model: str = "anthropic/claude-3-5-sonnet",
    api_events: list[dict] | None = None,
    extra_ui_events: list[dict] | None = None,
) -> Path:
    """Create one synthetic Cline task dir and return its path.

    ``api_events`` are merged into ``ui_messages.json`` after the user
    message and before any ``extra_ui_events`` (which append at the end).
    The api_conversation_history.json file gets a single user message
    embedding the model tag and an assistant reply.
    """
    task_dir = tasks_root / task_id
    task_dir.mkdir(parents=True, exist_ok=True)

    user_text = (
        f"Please refactor this function. <model>{model}</model> "
        f"Here's the code…"
    )

    ui_events: list[dict] = [
        {"type": "say", "say": "user_feedback", "ts": 1700000000000, "text": user_text},
    ]
    if api_events is None:
        api_events = [
            {
                "type": "say",
                "say": "api_req_started",
                "ts": 1700000001000,
                "text": json.dumps({
                    "tokensIn": 120,
                    "tokensOut": 80,
                    "cacheWrites": 0,
                    "cacheReads": 0,
                    "cost": 0.001,
                }),
            },
        ]
    ui_events.extend(api_events)
    if extra_ui_events:
        ui_events.extend(extra_ui_events)

    (task_dir / "ui_messages.json").write_text(json.dumps(ui_events))

    history = [
        {"role": "user", "content": [{"type": "text", "text": user_text}]},
        {"role": "assistant", "content": [{"type": "text", "text": "ok"}]},
    ]
    (task_dir / "api_conversation_history.json").write_text(json.dumps(history))

    return task_dir


@pytest.fixture
def synthetic_tasks(tmp_path: Path) -> Path:
    tasks_root = tmp_path / "tasks"
    tasks_root.mkdir()
    _write_task(tasks_root, "task-0001")
    return tasks_root


# ── tests ─────────────────────────────────────────────────────────────


def test_enumerate_yields_one_session_ref(synthetic_tasks: Path) -> None:
    adapter = ClineAdapter(tasks_root=synthetic_tasks)
    refs = list(adapter.enumerate())

    assert len(refs) == 1
    ref = refs[0]
    assert isinstance(ref, SessionRef)
    assert ref.provider == "cline"
    assert ref.source_kind == "file"
    assert ref.session_id == "task-0001"
    assert ref.project_slug == "cline"
    assert ref.file_path.name == "ui_messages.json"
    assert ref.file_size > 0


def test_enumerate_skips_dirs_without_ui_messages(tmp_path: Path) -> None:
    tasks_root = tmp_path / "tasks"
    tasks_root.mkdir()
    # One valid task, one empty dir.
    _write_task(tasks_root, "valid-task")
    (tasks_root / "empty-task").mkdir()

    adapter = ClineAdapter(tasks_root=tasks_root)
    refs = list(adapter.enumerate())
    assert [r.session_id for r in refs] == ["valid-task"]


def test_enumerate_returns_nothing_when_root_missing(tmp_path: Path) -> None:
    adapter = ClineAdapter(tasks_root=tmp_path / "does-not-exist")
    assert list(adapter.enumerate()) == []


def test_read_yields_assistant_record_with_tokens_and_model(
    synthetic_tasks: Path,
) -> None:
    adapter = ClineAdapter(tasks_root=synthetic_tasks)
    ref = next(iter(adapter.enumerate()))
    records = list(adapter.read(ref))

    assert len(records) == 1
    rec = records[0]
    assert isinstance(rec, Record)
    assert rec.provider == "cline"
    assert rec.session_id == "task-0001"
    assert rec.role == "assistant"
    assert rec.input_tokens == 120
    assert rec.output_tokens == 80
    assert rec.cache_create_tokens == 0
    assert rec.cache_read_tokens == 0
    assert rec.model == "anthropic/claude-3-5-sonnet"


def test_read_uses_default_model_when_tag_missing(tmp_path: Path) -> None:
    tasks_root = tmp_path / "tasks"
    tasks_root.mkdir()
    task_dir = tasks_root / "no-tag-task"
    task_dir.mkdir()

    (task_dir / "ui_messages.json").write_text(json.dumps([
        {"type": "say", "say": "text", "ts": 1700000000000, "text": "hello"},
        {
            "type": "say",
            "say": "api_req_started",
            "ts": 1700000001000,
            "text": json.dumps({"tokensIn": 1, "tokensOut": 2}),
        },
    ]))
    (task_dir / "api_conversation_history.json").write_text(json.dumps([
        {"role": "user", "content": [{"type": "text", "text": "no model here"}]},
    ]))

    adapter = ClineAdapter(tasks_root=tasks_root)
    ref = next(iter(adapter.enumerate()))
    records = list(adapter.read(ref))
    assert len(records) == 1
    assert records[0].model == "cline-auto"


def test_read_resume_with_since_offset(tmp_path: Path) -> None:
    tasks_root = tmp_path / "tasks"
    tasks_root.mkdir()
    # Multi-turn task: two api_req_started events so we can observe a
    # midpoint where since_offset matters.
    api_events = [
        {
            "type": "say",
            "say": "api_req_started",
            "ts": 1700000001000,
            "text": json.dumps({"tokensIn": 10, "tokensOut": 5}),
        },
        {"type": "say", "say": "text", "ts": 1700000002000, "text": "intermediate"},
        {
            "type": "say",
            "say": "api_req_started",
            "ts": 1700000003000,
            "text": json.dumps({"tokensIn": 20, "tokensOut": 7}),
        },
    ]
    _write_task(tasks_root, "resume-task", api_events=api_events)

    adapter = ClineAdapter(tasks_root=tasks_root)
    ref = next(iter(adapter.enumerate()))

    full = list(adapter.read(ref))
    assert len(full) == 2

    first_seq = full[0].seq
    resumed = list(adapter.read(ref, since_offset=first_seq))
    # Strictly past the floor, strictly fewer records than the full read.
    assert all(r.seq > first_seq for r in resumed)
    assert len(resumed) < len(full)
    assert len(resumed) == 1
    assert resumed[0].input_tokens == 20


def test_read_tolerates_malformed_api_req_text(tmp_path: Path) -> None:
    tasks_root = tmp_path / "tasks"
    tasks_root.mkdir()
    bad_event = {
        "type": "say",
        "say": "api_req_started",
        "ts": 1700000001000,
        "text": "not-json",
    }
    _write_task(tasks_root, "malformed-task", api_events=[bad_event])

    adapter = ClineAdapter(tasks_root=tasks_root)
    ref = next(iter(adapter.enumerate()))
    records = list(adapter.read(ref))
    # Record still emitted; counts default to zero.
    assert len(records) == 1
    rec = records[0]
    assert rec.input_tokens == 0
    assert rec.output_tokens == 0


# ── shared adapter contract ───────────────────────────────────────────


class TestClineAdapterContract(unittest.TestCase, AdapterContract):
    """Runs the shared AdapterContract invariants against a synthetic task."""

    def setUp(self):
        # tmp_path-style setup without pytest's fixture machinery: build a
        # disposable tasks tree inside ``self._tmp`` and point an adapter at
        # it. self.adapter is what AdapterContract reads.
        import tempfile

        self._tmp = tempfile.TemporaryDirectory()
        tasks_root = Path(self._tmp.name) / "tasks"
        tasks_root.mkdir()
        _write_task(tasks_root, "contract-task")
        self.adapter = ClineAdapter(tasks_root=tasks_root)

    def tearDown(self):
        self._tmp.cleanup()


# ── malformed-input hardening (ingest-surface sweep, 2026-07) ─────────


def test_out_of_range_timestamp_does_not_crash(tmp_path: Path) -> None:
    """A ``ts`` like 1e300 passes float() but overflows fromtimestamp —
    the record must still emit (with an empty timestamp), not raise."""
    tasks_root = tmp_path / "tasks"
    _write_task(
        tasks_root,
        "task-huge-ts",
        api_events=[
            {
                "type": "say",
                "say": "api_req_started",
                "ts": 1e300,
                "text": json.dumps({
                    "tokensIn": 5,
                    "tokensOut": 2,
                    "cacheWrites": 0,
                    "cacheReads": 0,
                }),
            },
        ],
    )
    adapter = ClineAdapter(tasks_root=tasks_root)
    ref = next(iter(adapter.enumerate()))
    records = list(adapter.read(ref))
    assert len(records) == 1
    assert records[0].timestamp == ""
    assert records[0].input_tokens == 5


def test_inf_token_counts_coerce_to_zero(tmp_path: Path) -> None:
    """JSON ``1e999`` in the api_req payload parses to inf; the coercer
    must yield 0 instead of raising OverflowError."""
    tasks_root = tmp_path / "tasks"
    _write_task(
        tasks_root,
        "task-inf-tokens",
        api_events=[
            {
                "type": "say",
                "say": "api_req_started",
                "ts": 1700000001000,
                "text": '{"tokensIn": 1e999, "tokensOut": 3}',
            },
        ],
    )
    adapter = ClineAdapter(tasks_root=tasks_root)
    ref = next(iter(adapter.enumerate()))
    records = list(adapter.read(ref))
    assert len(records) == 1
    assert records[0].input_tokens == 0
    assert records[0].output_tokens == 3
