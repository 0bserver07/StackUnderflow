"""Wave 2C: assert each default-on adapter exposes ``watch_paths()``
returning its canonical roots.

The watcher dispatches changes by matching paths against this list,
so a missing or wrongly-rooted method silently disables live-watching
for that provider — which would only show up as "dashboard feels
stale" in production.
"""

from __future__ import annotations

from pathlib import Path

from stackunderflow.adapters.claude import ClaudeAdapter
from stackunderflow.adapters.cline import (
    ClineAdapter,
    KiloCodeAdapter,
    RooCodeAdapter,
)
from stackunderflow.adapters.codex import CodexAdapter
from stackunderflow.adapters.cursor import CursorAdapter


def test_claude_watch_paths_includes_default_root():
    paths = ClaudeAdapter().watch_paths()
    assert any(str(p).endswith(".claude/projects") for p in paths), paths
    for p in paths:
        assert isinstance(p, Path)


def test_codex_watch_paths_returns_sessions_root(tmp_path: Path):
    """Codex adapter takes a sessions_root override on init; the
    override should propagate into watch_paths so tests can point at a
    fixture directory."""
    a = CodexAdapter(sessions_root=tmp_path / ".codex" / "sessions")
    assert a.watch_paths() == [tmp_path / ".codex" / "sessions"]


def test_cursor_watch_paths_returns_vscdb_path(tmp_path: Path):
    """Cursor's vscdb is a single SQLite file — watch_paths returns
    the file itself so watchfiles fires on any byte change."""
    db_path = tmp_path / "state.vscdb"
    a = CursorAdapter(vscdb_path=db_path)
    assert a.watch_paths() == [db_path]


def test_cline_family_watch_paths_returns_tasks_root(tmp_path: Path):
    """Cline / KiloCode / Roo Code share the same parser; each
    overrides the extension id and so should report a distinct
    tasks root."""
    for cls in (ClineAdapter, KiloCodeAdapter, RooCodeAdapter):
        custom = tmp_path / cls.__name__
        a = cls(tasks_root=custom)
        assert a.watch_paths() == [custom]
