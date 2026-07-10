"""Backup covers every adapter's sources — not just ~/.claude.

Pins the contract that closed the "backup is claude-only" gap: each
adapter self-declares its roots (``source_roots()`` / ``watch_paths()``),
``_backup_adapter_sources`` copies whatever exists under
``sources/<adapter>/``, writes a manifest mapping subdirs back to their
original absolute paths, and degrades per-adapter (missing roots, broken
adapters) without failing the backup. Restore must never inject the
backup-internal dirs back into ~/.claude.
"""

from __future__ import annotations

import json
from pathlib import Path

from stackunderflow.cli import _backup_adapter_sources


class _DirAdapter:
    def __init__(self, name: str, root: Path):
        self.name = name
        self._root = root

    def source_roots(self):
        return [self._root]


class _FileRootAdapter:
    """Adapter whose root is a single file (cursor's vscdb pattern)."""

    def __init__(self, name: str, db: Path):
        self.name = name
        self._db = db

    def source_roots(self):
        return [self._db]


class _WatchOnlyAdapter:
    """No source_roots(); falls back to watch_paths()."""

    def __init__(self, name: str, root: Path):
        self.name = name
        self._root = root

    def watch_paths(self):
        return [self._root]


class _BrokenAdapter:
    name = "broken"

    def source_roots(self):
        raise RuntimeError("cannot resolve roots")


class _ClaudeLike:
    name = "claude"

    def source_roots(self):  # pragma: no cover - must never be called
        raise AssertionError("claude is covered at the backup top level")


def _mk_sources(tmp_path: Path):
    codexish = tmp_path / "codexish-sessions"
    (codexish / "2026").mkdir(parents=True)
    (codexish / "2026" / "a.jsonl").write_text('{"x":1}\n')
    dbfile = tmp_path / "state.vscdb"
    dbfile.write_bytes(b"sqlite-ish")
    watched = tmp_path / "grokish"
    watched.mkdir()
    (watched / "s1.jsonl").write_text("{}\n")
    missing = tmp_path / "not-installed"
    return codexish, dbfile, watched, missing


def _adapters(tmp_path: Path):
    codexish, dbfile, watched, missing = _mk_sources(tmp_path)
    return [
        _ClaudeLike(),
        _DirAdapter("codexish", codexish),
        _FileRootAdapter("cursorish", dbfile),
        _WatchOnlyAdapter("grokish", watched),
        _DirAdapter("ghost", missing),
        _BrokenAdapter(),
    ]


def test_all_present_roots_copied_with_manifest(tmp_path, monkeypatch):
    import stackunderflow.adapters as adapters_pkg

    monkeypatch.setattr(adapters_pkg, "registered", lambda: _adapters(tmp_path))
    dest = tmp_path / "backup-1"
    dest.mkdir()

    copied = _backup_adapter_sources(dest, previous=None)

    names = {c[0] for c in copied}
    assert names == {"codexish", "cursorish", "grokish"}
    # claude skipped (top-level), ghost missing, broken degraded — no crash.
    assert (dest / "sources" / "codexish" / "0-codexish-sessions" / "2026" / "a.jsonl").is_file()
    assert (dest / "sources" / "cursorish" / "0-state.vscdb" / "state.vscdb").is_file()
    assert (dest / "sources" / "grokish" / "0-grokish" / "s1.jsonl").is_file()
    assert not (dest / "sources" / "ghost").exists() or not any(
        (dest / "sources" / "ghost").rglob("*")
    )

    manifest = json.loads((dest / "sources" / "manifest.json").read_text())
    assert manifest["codexish"]["codexish/0-codexish-sessions"].endswith(
        "codexish-sessions"
    )
    assert set(manifest) == {"codexish", "cursorish", "grokish"}


def test_second_backup_hardlinks_unchanged_files(tmp_path, monkeypatch):
    import shutil
    import stackunderflow.adapters as adapters_pkg

    if shutil.which("rsync") is None:  # pragma: no cover - CI without rsync
        import pytest

        pytest.skip("rsync unavailable; hardlink dedup is rsync-only")

    adapters = _adapters(tmp_path)
    monkeypatch.setattr(adapters_pkg, "registered", lambda: adapters)
    b1 = tmp_path / "backup-1"
    b1.mkdir()
    _backup_adapter_sources(b1, previous=None)
    b2 = tmp_path / "backup-2"
    b2.mkdir()
    _backup_adapter_sources(b2, previous=b1)

    f1 = b1 / "sources" / "codexish" / "0-codexish-sessions" / "2026" / "a.jsonl"
    f2 = b2 / "sources" / "codexish" / "0-codexish-sessions" / "2026" / "a.jsonl"
    assert f1.stat().st_ino == f2.stat().st_ino  # same inode = zero extra disk


def test_no_agents_with_data_writes_no_manifest(tmp_path, monkeypatch):
    import stackunderflow.adapters as adapters_pkg

    monkeypatch.setattr(
        adapters_pkg, "registered",
        lambda: [_ClaudeLike(), _DirAdapter("ghost", tmp_path / "nope")],
    )
    dest = tmp_path / "backup-1"
    dest.mkdir()
    assert _backup_adapter_sources(dest, previous=None) == []
    assert not (dest / "sources" / "manifest.json").exists()
