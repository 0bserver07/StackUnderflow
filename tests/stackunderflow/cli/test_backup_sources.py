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
    """Claude-shaped adapter: primary home (rsynced whole at the backup top
    level — must be SKIPPED here) plus a variant home (~/.claude-opus
    style — must be CAPTURED here; before the fix it landed in NO backup
    path)."""

    name = "claude"

    def __init__(self, main_projects: Path, variant_projects: Path):
        self._roots = [main_projects, variant_projects]

    def source_roots(self):
        return list(self._roots)


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


def _claude_homes(tmp_path: Path, monkeypatch):
    """A fake relocated claude home (via CLAUDE_CONFIG_DIR) + a variant."""
    main = tmp_path / "claude-home"
    (main / "projects").mkdir(parents=True)
    (main / "projects" / "m.jsonl").write_text("{}\n")
    variant = tmp_path / "claude-opus-home" / "projects"
    variant.mkdir(parents=True)
    (variant / "v.jsonl").write_text("{}\n")
    monkeypatch.setenv("CLAUDE_CONFIG_DIR", str(main))
    return main, variant


def _adapters(tmp_path: Path, monkeypatch):
    codexish, dbfile, watched, missing = _mk_sources(tmp_path)
    main, variant = _claude_homes(tmp_path, monkeypatch)
    return [
        _ClaudeLike(main / "projects", variant),
        _DirAdapter("codexish", codexish),
        _FileRootAdapter("cursorish", dbfile),
        _WatchOnlyAdapter("grokish", watched),
        _DirAdapter("ghost", missing),
        _BrokenAdapter(),
    ]


def test_all_present_roots_copied_with_manifest(tmp_path, monkeypatch):
    import stackunderflow.adapters as adapters_pkg

    adapters = _adapters(tmp_path, monkeypatch)
    monkeypatch.setattr(adapters_pkg, "registered", lambda: adapters)
    dest = tmp_path / "backup-1"
    dest.mkdir()

    copied = _backup_adapter_sources(dest, previous=None)

    names = {c[0] for c in copied}
    assert names == {"claude", "codexish", "cursorish", "grokish"}
    # claude's PRIMARY home is skipped (rsynced whole at the top level),
    # its VARIANT home is captured; ghost missing, broken degraded.
    claude_dirs = list((dest / "sources" / "claude").iterdir())
    assert len(claude_dirs) == 1
    assert (claude_dirs[0] / "v.jsonl").is_file()
    assert not any(f.name == "m.jsonl" for f in (dest / "sources").rglob("*"))
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
    assert set(manifest) == {"claude", "codexish", "cursorish", "grokish"}


def test_second_backup_hardlinks_unchanged_files(tmp_path, monkeypatch):
    import shutil
    import stackunderflow.adapters as adapters_pkg

    if shutil.which("rsync") is None:  # pragma: no cover - CI without rsync
        import pytest

        pytest.skip("rsync unavailable; hardlink dedup is rsync-only")

    adapters = _adapters(tmp_path, monkeypatch)
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

    main, _variant = _claude_homes(tmp_path, monkeypatch)
    monkeypatch.setattr(
        adapters_pkg, "registered",
        lambda: [
            # Primary-home-only claude (both roots under the main payload)
            # plus a missing adapter: nothing to capture.
            _ClaudeLike(main / "projects", main / "projects"),
            _DirAdapter("ghost", tmp_path / "nope"),
        ],
    )
    dest = tmp_path / "backup-1"
    dest.mkdir()
    assert _backup_adapter_sources(dest, previous=None) == []
    assert not (dest / "sources" / "manifest.json").exists()


def test_claude_dir_honors_config_dir(tmp_path, monkeypatch):
    """backup's home resolution follows CLAUDE_CONFIG_DIR — a hardcoded
    ~/.claude made `backup create` a silent no-op for relocated configs."""
    from stackunderflow.cli import _claude_dir

    relocated = tmp_path / "relocated-claude"
    monkeypatch.setenv("CLAUDE_CONFIG_DIR", str(relocated))
    assert _claude_dir() == relocated
    monkeypatch.delenv("CLAUDE_CONFIG_DIR")
    assert _claude_dir() == Path.home() / ".claude"
