"""Unit tests for the Roo Code adapter.

Roo Code reuses the Cline parser surface (codeburn-catalog §14, §15) —
only the globalStorage extension directory differs. These tests assert
the extension-id wiring and confirm one full enumerate→read pass over a
synthetic task tree.

Spec §3.2; codeburn-catalog §14.
"""

from __future__ import annotations

import json
import unittest
from pathlib import Path

import pytest

from stackunderflow.adapters.base import Record, SessionRef
from stackunderflow.adapters.cline import RooCodeAdapter
from tests.stackunderflow.adapters.contract import AdapterContract

# ── fixture builders ──────────────────────────────────────────────────


def _write_task(
    tasks_root: Path,
    task_id: str,
    *,
    model: str = "openai/gpt-4o",
) -> Path:
    """Create one synthetic Roo Code task directory."""
    task_dir = tasks_root / task_id
    task_dir.mkdir(parents=True, exist_ok=True)

    user_text = f"Fix this bug. <model>{model}</model> code follows…"

    ui_events = [
        {
            "type": "say",
            "say": "user_feedback",
            "ts": 1700000000000,
            "text": user_text,
        },
        {
            "type": "say",
            "say": "api_req_started",
            "ts": 1700000001000,
            "text": json.dumps({
                "tokensIn": 150,
                "tokensOut": 75,
                "cacheWrites": 0,
                "cacheReads": 0,
                "cost": 0.0015,
            }),
        },
    ]
    (task_dir / "ui_messages.json").write_text(json.dumps(ui_events))

    history = [
        {"role": "user", "content": [{"type": "text", "text": user_text}]},
        {"role": "assistant", "content": [{"type": "text", "text": "fixed"}]},
    ]
    (task_dir / "api_conversation_history.json").write_text(json.dumps(history))

    return task_dir


@pytest.fixture
def synthetic_tasks(tmp_path: Path) -> Path:
    tasks_root = tmp_path / "tasks"
    tasks_root.mkdir()
    _write_task(tasks_root, "roo-task-0001")
    return tasks_root


# ── tests ─────────────────────────────────────────────────────────────


def test_extension_id_and_provider_name() -> None:
    """Roo Code points at its own VS Code globalStorage directory."""
    adapter = RooCodeAdapter()
    assert adapter.name == "roocode"
    assert adapter._extension_id == "rooveterinaryinc.roo-cline"
    # Default tasks root resolves under the roo-cline extension folder.
    assert adapter._root.parent.name == "rooveterinaryinc.roo-cline"
    assert adapter._root.name == "tasks"


def test_enumerate_yields_session_with_roocode_provider(
    synthetic_tasks: Path,
) -> None:
    adapter = RooCodeAdapter(tasks_root=synthetic_tasks)
    refs = list(adapter.enumerate())

    assert len(refs) == 1
    ref = refs[0]
    assert isinstance(ref, SessionRef)
    assert ref.provider == "roocode"
    assert ref.project_slug == "roocode"
    assert ref.session_id == "roo-task-0001"
    assert ref.source_kind == "file"
    assert ref.file_path.name == "ui_messages.json"


def test_read_yields_assistant_record_with_token_data(
    synthetic_tasks: Path,
) -> None:
    adapter = RooCodeAdapter(tasks_root=synthetic_tasks)
    ref = next(iter(adapter.enumerate()))
    records = list(adapter.read(ref))

    assert len(records) == 1
    rec = records[0]
    assert isinstance(rec, Record)
    assert rec.provider == "roocode"
    assert rec.role == "assistant"
    assert rec.input_tokens == 150
    assert rec.output_tokens == 75
    assert rec.model == "openai/gpt-4o"


# ── shared adapter contract ───────────────────────────────────────────


class TestRooCodeAdapterContract(unittest.TestCase, AdapterContract):
    """Runs the shared AdapterContract invariants against a synthetic task."""

    def setUp(self):
        import tempfile

        self._tmp = tempfile.TemporaryDirectory()
        tasks_root = Path(self._tmp.name) / "tasks"
        tasks_root.mkdir()
        _write_task(tasks_root, "contract-task")
        self.adapter = RooCodeAdapter(tasks_root=tasks_root)

    def tearDown(self):
        self._tmp.cleanup()
