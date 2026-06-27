"""Cross-platform path-resolution tests for source adapters.

These exercise the per-platform branch logic in each adapter's storage-root
resolver by monkeypatching ``sys.platform`` (and ``APPDATA`` for Windows).
They run on any host OS — they validate *which* path an adapter selects, not
host filesystem semantics — so they give us Windows-path coverage from Linux
CI without a Windows runner.

Covers the v0.9.x Windows-path work:
- Cline-family (``_vscode_global_storage``) — the only default-on adapter that
  was previously macOS-path-only; the fix flows through Cline, KiloCode and
  Roo Code via the shared ``_VsCodeClineAdapter``.
- Kiro (``_kiro_global_storage``).
- Cursor / Copilot — pre-existing Windows branches, locked in here.
- ``CLAUDE_CONFIG_DIR`` relocation (the WSL / custom-install case).
"""

from __future__ import annotations

import sys
from pathlib import Path

import pytest

from stackunderflow.adapters import cline, copilot, cursor, kiro
from stackunderflow.adapters.claude import ClaudeAdapter, _claude_home
from tests.conftest import set_home_env

# ── Cline-family VS Code globalStorage ─────────────────────────────────


def test_cline_global_storage_windows(monkeypatch, tmp_path: Path) -> None:
    monkeypatch.setattr(sys, "platform", "win32")
    appdata = tmp_path / "AppData" / "Roaming"
    monkeypatch.setenv("APPDATA", str(appdata))
    assert cline._vscode_global_storage() == (appdata / "Code" / "User" / "globalStorage")


def test_cline_global_storage_linux(monkeypatch, tmp_path: Path) -> None:
    monkeypatch.setattr(sys, "platform", "linux")
    set_home_env(monkeypatch, tmp_path)
    assert cline._vscode_global_storage() == (tmp_path / ".config" / "Code" / "User" / "globalStorage")


def test_cline_global_storage_macos(monkeypatch, tmp_path: Path) -> None:
    monkeypatch.setattr(sys, "platform", "darwin")
    set_home_env(monkeypatch, tmp_path)
    assert cline._vscode_global_storage() == (
        tmp_path / "Library" / "Application Support" / "Code" / "User" / "globalStorage"
    )


@pytest.mark.parametrize(
    "adapter_cls",
    [cline.ClineAdapter, cline.KiloCodeAdapter, cline.RooCodeAdapter],
)
def test_cline_family_default_root_uses_windows_storage(monkeypatch, tmp_path: Path, adapter_cls) -> None:
    """One resolver fix lights up all three Cline-family extensions: each
    default root lands under ``%APPDATA%`` on Windows, ending in
    ``{extension_id}/tasks``."""
    monkeypatch.setattr(sys, "platform", "win32")
    appdata = tmp_path / "Roaming"
    monkeypatch.setenv("APPDATA", str(appdata))
    root = adapter_cls()._root
    storage = appdata / "Code" / "User" / "globalStorage"
    assert storage in root.parents
    assert root.name == "tasks"


# ── Kiro globalStorage ─────────────────────────────────────────────────


def test_kiro_global_storage_windows(monkeypatch, tmp_path: Path) -> None:
    monkeypatch.setattr(sys, "platform", "win32")
    monkeypatch.setenv("APPDATA", str(tmp_path))
    assert kiro._kiro_global_storage() == (tmp_path / "Kiro" / "User" / "globalStorage" / "kiro.kiroagent")


def test_kiro_global_storage_linux(monkeypatch, tmp_path: Path) -> None:
    monkeypatch.setattr(sys, "platform", "linux")
    set_home_env(monkeypatch, tmp_path)
    assert kiro._kiro_global_storage() == (tmp_path / ".config" / "Kiro" / "User" / "globalStorage" / "kiro.kiroagent")


def test_kiro_global_storage_macos(monkeypatch, tmp_path: Path) -> None:
    monkeypatch.setattr(sys, "platform", "darwin")
    set_home_env(monkeypatch, tmp_path)
    assert kiro._kiro_global_storage() == (
        tmp_path / "Library" / "Application Support" / "Kiro" / "User" / "globalStorage" / "kiro.kiroagent"
    )


# ── Cursor / Copilot (pre-existing Windows branches) ───────────────────


def test_cursor_vscdb_path_selects_windows_constant(monkeypatch) -> None:
    monkeypatch.setattr(sys, "platform", "win32")
    assert cursor._default_vscdb_path() == cursor._VSCDB_WINDOWS


def test_copilot_workspace_storage_selects_windows_constant(monkeypatch) -> None:
    monkeypatch.setattr(sys, "platform", "win32")
    assert copilot._default_vscode_workspace_storage() == copilot._VSCODE_WORKSPACE_STORAGE_WINDOWS


# ── CLAUDE_CONFIG_DIR relocation (WSL / custom install) ────────────────


def test_claude_home_honors_config_dir(monkeypatch, tmp_path: Path) -> None:
    relocated = tmp_path / "custom" / "claude"
    monkeypatch.setenv("CLAUDE_CONFIG_DIR", str(relocated))
    assert _claude_home() == relocated


def test_claude_home_defaults_to_dot_claude(monkeypatch, tmp_path: Path) -> None:
    monkeypatch.delenv("CLAUDE_CONFIG_DIR", raising=False)
    set_home_env(monkeypatch, tmp_path)
    assert _claude_home() == tmp_path / ".claude"


def test_claude_enumerate_respects_config_dir(monkeypatch, tmp_path: Path) -> None:
    """End-to-end: pointing CLAUDE_CONFIG_DIR at a relocated tree makes
    enumerate() discover sessions there — the WSL-reads-Windows fix."""
    relocated = tmp_path / "relocated"
    project = relocated / "projects" / "-Users-x-proj"
    project.mkdir(parents=True)
    (project / "0001.jsonl").write_text('{"type": "user"}\n', encoding="utf-8")
    monkeypatch.setenv("CLAUDE_CONFIG_DIR", str(relocated))

    refs = list(ClaudeAdapter().enumerate())

    assert [r.project_slug for r in refs] == ["-Users-x-proj"]
