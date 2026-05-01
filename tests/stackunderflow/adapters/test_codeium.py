"""Unit tests for the Codeium discovery-only stub.

The adapter is intentionally inert until protobuf-decoded chat state is
available (see ``stackunderflow/adapters/codeium.py`` module docstring).
These tests pin the contract the stub promises:

  - ``enumerate()`` yields nothing
  - ``read()`` is a no-op even when handed an arbitrary ``SessionRef``
  - missing ``~/.codeium/`` does not raise
  - inherits ``AdapterContract`` so the shared invariants run (vacuously
    — empty-fixture branches all return early)
"""

from __future__ import annotations

import unittest
from pathlib import Path

import pytest

from stackunderflow.adapters.base import SessionRef
from stackunderflow.adapters.codeium import CodeiumAdapter
from tests.stackunderflow.adapters.contract import AdapterContract


@pytest.fixture
def empty_codeium_dir(tmp_path: Path) -> Path:
    root = tmp_path / ".codeium"
    root.mkdir()
    return root


def test_enumerate_yields_nothing_for_empty_root(empty_codeium_dir: Path) -> None:
    adapter = CodeiumAdapter(root=empty_codeium_dir)
    assert list(adapter.enumerate()) == []


def test_enumerate_yields_nothing_when_root_missing(tmp_path: Path) -> None:
    adapter = CodeiumAdapter(root=tmp_path / "does-not-exist")
    assert list(adapter.enumerate()) == []


def test_enumerate_does_not_raise_on_protobuf_blobs(empty_codeium_dir: Path) -> None:
    """Even if codeium-shaped binary blobs are present, the stub is inert."""
    (empty_codeium_dir / "chat-state.pb").write_bytes(b"\x08\x96\x01" * 32)
    (empty_codeium_dir / "config.json").write_text("{}")
    adapter = CodeiumAdapter(root=empty_codeium_dir)
    assert list(adapter.enumerate()) == []


def test_read_yields_nothing(empty_codeium_dir: Path) -> None:
    """``read()`` is a generator that immediately exits.

    Hand it a manufactured ref pointed at an arbitrary file — the stub
    never touches the file and yields nothing.
    """
    adapter = CodeiumAdapter(root=empty_codeium_dir)
    fake_ref = SessionRef(
        provider="codeium",
        project_slug="codeium",
        session_id="anything",
        file_path=empty_codeium_dir,
        file_mtime=0.0,
        file_size=0,
        source_kind="file",
        source_hint=None,
    )
    assert list(adapter.read(fake_ref)) == []
    assert list(adapter.read(fake_ref, since_offset=42)) == []


# ── shared adapter contract ───────────────────────────────────────────


class TestCodeiumAdapterContract(unittest.TestCase, AdapterContract):
    """Empty-fixture path through the shared invariants.

    AdapterContract's tests all early-return when ``enumerate()`` yields
    nothing, which is exactly what we want for the stub.
    """

    def setUp(self) -> None:
        import tempfile

        self._tmp = tempfile.TemporaryDirectory()
        self.adapter = CodeiumAdapter(root=Path(self._tmp.name))

    def tearDown(self) -> None:
        self._tmp.cleanup()
