"""Defensive coverage for the Codeium discovery-only stub.

The Codeium adapter is intentionally inert (protobuf chat-state has no
public schema) but this file pins the contract: even pathological input
must not crash. The base happy-path is in ``test_codeium.py``; here we
focus on actively malformed states.
"""

from __future__ import annotations

import os
from pathlib import Path

import pytest

from stackunderflow.adapters.base import SessionRef
from stackunderflow.adapters.codeium import CodeiumAdapter


_IS_ROOT = hasattr(os, "geteuid") and os.geteuid() == 0


def test_missing_root_yields_nothing(tmp_path: Path) -> None:
    adapter = CodeiumAdapter(root=tmp_path / "no-such-codeium")
    assert list(adapter.enumerate()) == []


def test_root_full_of_random_files_still_yields_nothing(tmp_path: Path) -> None:
    """The stub must not even attempt to interpret any of these files."""
    root = tmp_path / ".codeium"
    root.mkdir()
    (root / "chat-state.pb").write_bytes(b"\x00\x01\x02\x03" * 100)
    (root / "garbage.json").write_text("not json")
    (root / "binary.bin").write_bytes(os.urandom(256))
    (root / "subdir").mkdir()
    (root / "subdir" / "more.pb").write_bytes(b"\xff" * 50)

    adapter = CodeiumAdapter(root=root)
    assert list(adapter.enumerate()) == []


def test_read_with_arbitrary_session_ref_yields_nothing(tmp_path: Path) -> None:
    """Even with a hand-crafted SessionRef, read() yields nothing — the stub
    never even opens the file."""
    adapter = CodeiumAdapter(root=tmp_path)
    fake_ref = SessionRef(
        provider="codeium",
        project_slug="codeium",
        session_id="x",
        file_path=tmp_path / "no-such-file.pb",
        file_mtime=0.0,
        file_size=0,
        source_kind="file",
        source_hint=None,
    )
    assert list(adapter.read(fake_ref)) == []
    assert list(adapter.read(fake_ref, since_offset=99999)) == []


@pytest.mark.skipif(_IS_ROOT, reason="root bypasses chmod 000")
def test_unreadable_root_does_not_raise(tmp_path: Path) -> None:
    root = tmp_path / ".codeium"
    root.mkdir()
    root.chmod(0o000)
    try:
        adapter = CodeiumAdapter(root=root)
        # Stub never walks the dir, so even an unreadable root is fine.
        assert list(adapter.enumerate()) == []
    finally:
        root.chmod(0o755)
