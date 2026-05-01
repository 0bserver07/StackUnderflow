"""Unit tests for the KiloCode adapter.

KiloCode reuses the Cline parser surface (codeburn-catalog §8, §15) — only
the globalStorage extension directory differs. These tests assert the
extension-id wiring and confirm one full enumerate→read pass over a
synthetic task tree.

Spec §3.2; codeburn-catalog §8.
"""

from __future__ import annotations

import json
import unittest
from pathlib import Path

import pytest

from stackunderflow.adapters.base import Record, SessionRef
from stackunderflow.adapters.cline import KiloCodeAdapter
from tests.stackunderflow.adapters.contract import AdapterContract

# ── fixture builders ──────────────────────────────────────────────────


def _write_task(
    tasks_root: Path,
    task_id: str,
    *,
    model: str = "anthropic/claude-3-5-sonnet",
) -> Path:
    """Create one synthetic KiloCode task directory."""
    task_dir = tasks_root / task_id
    task_dir.mkdir(parents=True, exist_ok=True)

    user_text = f"Refactor this. <model>{model}</model> here is the code…"

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
                "tokensIn": 200,
                "tokensOut": 90,
                "cacheWrites": 5,
                "cacheReads": 12,
                "cost": 0.002,
            }),
        },
    ]
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
    _write_task(tasks_root, "kilo-task-0001")
    return tasks_root


# ── tests ─────────────────────────────────────────────────────────────


def test_extension_id_and_provider_name() -> None:
    """KiloCode points at its own VS Code globalStorage directory."""
    adapter = KiloCodeAdapter()
    assert adapter.name == "kilocode"
    assert adapter._extension_id == "kilocode.kilo-code"
    # Default tasks root resolves under the kilocode extension folder.
    assert adapter._root.parent.name == "kilocode.kilo-code"
    assert adapter._root.name == "tasks"


def test_enumerate_yields_session_with_kilocode_provider(
    synthetic_tasks: Path,
) -> None:
    adapter = KiloCodeAdapter(tasks_root=synthetic_tasks)
    refs = list(adapter.enumerate())

    assert len(refs) == 1
    ref = refs[0]
    assert isinstance(ref, SessionRef)
    assert ref.provider == "kilocode"
    assert ref.project_slug == "kilocode"
    assert ref.session_id == "kilo-task-0001"
    assert ref.source_kind == "file"
    assert ref.file_path.name == "ui_messages.json"


def test_read_yields_assistant_record_with_token_data(
    synthetic_tasks: Path,
) -> None:
    adapter = KiloCodeAdapter(tasks_root=synthetic_tasks)
    ref = next(iter(adapter.enumerate()))
    records = list(adapter.read(ref))

    assert len(records) == 1
    rec = records[0]
    assert isinstance(rec, Record)
    assert rec.provider == "kilocode"
    assert rec.role == "assistant"
    assert rec.input_tokens == 200
    assert rec.output_tokens == 90
    assert rec.cache_create_tokens == 5
    assert rec.cache_read_tokens == 12
    assert rec.model == "anthropic/claude-3-5-sonnet"


# ── shared adapter contract ───────────────────────────────────────────


class TestKiloCodeAdapterContract(unittest.TestCase, AdapterContract):
    """Runs the shared AdapterContract invariants against a synthetic task."""

    def setUp(self):
        import tempfile

        self._tmp = tempfile.TemporaryDirectory()
        tasks_root = Path(self._tmp.name) / "tasks"
        tasks_root.mkdir()
        _write_task(tasks_root, "contract-task")
        self.adapter = KiloCodeAdapter(tasks_root=tasks_root)

    def tearDown(self):
        self._tmp.cleanup()
