"""Unit tests for the Kiro adapter.

Builds a synthetic ``kiro.kiroagent`` storage tree under ``tmp_path`` and
points the adapter at it via the ``storage_root`` constructor override.

Exercises:

- ``enumerate()`` discovers ``*.chat`` files (recursively).
- ``read(ref)`` rolls up an entire execution into one assistant Record.
- Tokens are estimated as ``len(content) // 4``.
- Records carry ``raw["cost_source"] = "estimated"`` so the cost layer
  knows to flag them.
- ``modelId`` dot-form normalises to dash-form (matching the Anthropic
  pricer's family heuristic).
- ``<tool_use>`` markers in bot text yield a ``tools`` tuple.
- Resumable reads via ``since_offset`` past the only record yield
  nothing (the adapter aggregates per-file, not per-event).

Spec §3; codeburn-catalog §9.
"""

from __future__ import annotations

import json
import unittest
from pathlib import Path

import pytest

from stackunderflow.adapters.base import Record, SessionRef
from stackunderflow.adapters.kiro import KiroAdapter
from tests.stackunderflow.adapters.contract import AdapterContract


# ── fixture builders ──────────────────────────────────────────────────


def _write_chat(
    storage_root: Path,
    name: str,
    *,
    workflow_id: str = "wf-001",
    model_id: str = "claude.3.5.sonnet",
    human_text: str = "Please refactor this function into smaller pieces.",
    bot_text: str = "Here's the refactored version. <tool_use><name>Edit</name></tool_use>",
) -> Path:
    fp = storage_root / f"{name}.chat"
    storage_root.mkdir(parents=True, exist_ok=True)
    obj = {
        "executionId": f"exec-{name}",
        "actionId": f"action-{name}",
        "chat": [
            {"role": "human", "content": human_text},
            {"role": "bot", "content": bot_text},
        ],
        "metadata": {
            "modelId": model_id,
            "startTime": "2026-04-30T10:00:00Z",
            "endTime": "2026-04-30T10:00:30Z",
            "workflowId": workflow_id,
        },
    }
    fp.write_text(json.dumps(obj))
    return fp


@pytest.fixture
def synthetic_storage(tmp_path: Path) -> Path:
    storage = tmp_path / "kiro.kiroagent"
    storage.mkdir()
    _write_chat(storage, "execution-001")
    return storage


# ── tests ─────────────────────────────────────────────────────────────


def test_enumerate_yields_one_session_ref(synthetic_storage: Path) -> None:
    adapter = KiroAdapter(storage_root=synthetic_storage)
    refs = list(adapter.enumerate())

    assert len(refs) == 1
    ref = refs[0]
    assert isinstance(ref, SessionRef)
    assert ref.provider == "kiro"
    assert ref.source_kind == "file"
    # workflow_id from metadata wins over filename for session_id.
    assert ref.session_id == "wf-001"
    assert ref.file_path.name == "execution-001.chat"


def test_enumerate_returns_nothing_when_root_missing(tmp_path: Path) -> None:
    adapter = KiroAdapter(storage_root=tmp_path / "missing")
    assert list(adapter.enumerate()) == []


def test_enumerate_recurses_into_subdirs(tmp_path: Path) -> None:
    """Workspace-scoped chats live under nested subdirs; rglob finds them."""
    storage = tmp_path / "storage"
    nested = storage / "workspace-XYZ"
    _write_chat(nested, "exec-A", workflow_id="wf-A")
    adapter = KiroAdapter(storage_root=storage)
    refs = list(adapter.enumerate())
    assert len(refs) == 1
    assert refs[0].session_id == "wf-A"


def test_read_yields_one_aggregated_record(synthetic_storage: Path) -> None:
    adapter = KiroAdapter(storage_root=synthetic_storage)
    ref = next(iter(adapter.enumerate()))
    records = list(adapter.read(ref))

    assert len(records) == 1
    rec = records[0]
    assert isinstance(rec, Record)
    assert rec.role == "assistant"
    # Model normalised: dots → dashes.
    assert rec.model == "claude-3-5-sonnet"
    # Token estimate: len(content) // 4. The exact numbers depend on the
    # fixture text; assert they're positive (estimation worked).
    assert rec.input_tokens > 0
    assert rec.output_tokens > 0


def test_token_estimate_is_chars_over_4(tmp_path: Path) -> None:
    """Confirm the chars/4 estimate, not some other heuristic."""
    storage = tmp_path / "storage"
    _write_chat(
        storage, "x",
        human_text="a" * 40,  # 40 chars → 10 input tokens
        bot_text="b" * 12,    # 12 chars → 3 output tokens
    )
    adapter = KiroAdapter(storage_root=storage)
    ref = next(iter(adapter.enumerate()))
    rec = next(iter(adapter.read(ref)))
    assert rec.input_tokens == 10
    assert rec.output_tokens == 3


def test_record_carries_estimated_cost_source_flag(
    synthetic_storage: Path,
) -> None:
    """Every Kiro Record must mark cost_source=estimated so the cost
    layer knows it's not an authoritative number."""
    adapter = KiroAdapter(storage_root=synthetic_storage)
    ref = next(iter(adapter.enumerate()))
    rec = next(iter(adapter.read(ref)))
    assert rec.raw.get("cost_source") == "estimated"


def test_tool_extraction_from_bot_text(synthetic_storage: Path) -> None:
    adapter = KiroAdapter(storage_root=synthetic_storage)
    ref = next(iter(adapter.enumerate()))
    rec = next(iter(adapter.read(ref)))
    assert "Edit" in rec.tools


def test_missing_model_falls_back_to_kiro_auto(tmp_path: Path) -> None:
    storage = tmp_path / "storage"
    storage.mkdir()
    obj = {
        "executionId": "e",
        "actionId": "a",
        "chat": [
            {"role": "human", "content": "hello"},
            {"role": "bot", "content": "world"},
        ],
        "metadata": {"workflowId": "wf-X"},
    }
    (storage / "x.chat").write_text(json.dumps(obj))

    adapter = KiroAdapter(storage_root=storage)
    ref = next(iter(adapter.enumerate()))
    rec = next(iter(adapter.read(ref)))
    assert rec.model == "kiro-auto"


def test_read_resume_past_record_yields_nothing(
    synthetic_storage: Path,
) -> None:
    """Kiro emits one Record per file; a since_offset past it yields nothing."""
    adapter = KiroAdapter(storage_root=synthetic_storage)
    ref = next(iter(adapter.enumerate()))
    full = list(adapter.read(ref))
    assert len(full) == 1
    resumed = list(adapter.read(ref, since_offset=full[0].seq + 1))
    assert resumed == []


# ── shared adapter contract ───────────────────────────────────────────


class TestKiroAdapterContract(unittest.TestCase, AdapterContract):
    def setUp(self):
        import tempfile

        self._tmp = tempfile.TemporaryDirectory()
        storage = Path(self._tmp.name) / "storage"
        storage.mkdir()
        _write_chat(storage, "contract-exec")
        self.adapter = KiroAdapter(storage_root=storage)

    def tearDown(self):
        self._tmp.cleanup()
