"""``stackunderflow hooks install`` / ``uninstall`` / ``status`` — the spec's
non-negotiables, locked.

Covered here (the regression tier from ``.notes/specs/05-hybrid-capture-hooks.md``):

* ``install`` never touches a non-StackUnderflow hook entry — snapshot test on
  a settings file carrying four other tools' hooks.
* ``install`` writes ``settings.json.bak.<utc-ts>`` *before* mutating, and
  *not* on a no-op re-install or under ``--dry-run``.
* ``install`` is idempotent + convergent (re-run = no change; a stale entry or
  a flag change is replaced, never duplicated).
* ``uninstall`` removes only our entries; the file and every other entry stay.
* ``--scope project`` and ``--scope user`` don't bleed into each other.
"""

from __future__ import annotations

import json
from pathlib import Path

import pytest

from stackunderflow.hooks import _install as install_mod
from stackunderflow.hooks import install, status, uninstall
from stackunderflow.hooks.templates import HOOK_IDS, canonical_command

# A realistic settings.json with FOUR other tools' hooks across three events
# plus unrelated keys — the thing install/uninstall must never disturb.
_OTHER_HOOKS_SETTINGS = {
    "permissions": {"allow": ["Bash(git*)"], "deny": []},
    "model": "claude-sonnet-4-5",
    "hooks": {
        "PreToolUse": [
            {"matcher": "Bash", "hooks": [{"type": "command", "command": "guard-dangerous-bash.sh"}]},
        ],
        "PostToolUse": [
            {"matcher": "Edit|Write", "hooks": [{"type": "command", "command": "prettier --write {file}"}]},
            {"matcher": "Bash", "hooks": [{"type": "command", "command": "log-bash.py"}]},
        ],
        "SessionStart": [
            {"hooks": [{"type": "command", "command": "echo session-start"}]},
        ],
    },
}


@pytest.fixture
def project_root(tmp_path: Path) -> Path:
    """A tmp dir that looks like a git repo (so ``--scope project`` resolves here)."""
    (tmp_path / ".git").mkdir()
    return tmp_path


def _settings_path(root: Path) -> Path:
    return root / ".claude" / "settings.json"


def _write_settings(root: Path, data: dict) -> Path:
    p = _settings_path(root)
    p.parent.mkdir(parents=True, exist_ok=True)
    p.write_text(json.dumps(data, indent=2))
    return p


def _backups(root: Path) -> list[Path]:
    d = _settings_path(root).parent
    return sorted(d.glob("settings.json.bak.*")) if d.exists() else []


# ── path resolution ─────────────────────────────────────────────────────────


class TestScopeResolution:
    def test_project_scope_uses_git_root(self, tmp_path: Path) -> None:
        (tmp_path / ".git").mkdir()
        sub = tmp_path / "a" / "b" / "c"
        sub.mkdir(parents=True)
        p = install_mod.resolve_settings_path("project", cwd=sub)
        assert p == tmp_path / ".claude" / "settings.json"

    def test_project_scope_falls_back_to_cwd_without_git(self, tmp_path: Path) -> None:
        sub = tmp_path / "no" / "repo"
        sub.mkdir(parents=True)
        p = install_mod.resolve_settings_path("project", cwd=sub)
        assert p == sub / ".claude" / "settings.json"

    def test_user_scope_is_home_claude(self) -> None:
        p = install_mod.resolve_settings_path("user")
        assert p == Path.home() / ".claude" / "settings.json"

    def test_unknown_scope_rejected(self) -> None:
        with pytest.raises(ValueError):
            install_mod.resolve_settings_path("everywhere")


# ── install: fresh file ─────────────────────────────────────────────────────


class TestInstallFresh:
    def test_creates_settings_with_all_four_hooks(self, project_root: Path) -> None:
        report = install("project", cwd=project_root)
        assert report.changed is True
        assert report.created_file is True
        assert report.backup_path is None  # nothing to back up — the file didn't exist
        data = json.loads(_settings_path(project_root).read_text())
        assert set(data["hooks"]) == {"PostToolUse", "UserPromptSubmit", "Stop", "PreCompact"}
        # PostToolUse is the only matcher-scoped one — scoped to Bash.
        ptu = data["hooks"]["PostToolUse"]
        assert ptu == [{"matcher": "Bash", "hooks": [{"type": "command",
                                                      "command": canonical_command("stackunderflow-post-tool-use")}]}]
        for ev in ("UserPromptSubmit", "Stop", "PreCompact"):
            grp = data["hooks"][ev]
            assert "matcher" not in grp[0]
            assert grp[0]["hooks"][0]["command"].startswith("stackunderflow hooks run stackunderflow-")

    def test_commands_are_portable_never_absolute(self, project_root: Path) -> None:
        install("project", cwd=project_root)
        data = json.loads(_settings_path(project_root).read_text())
        for groups in data["hooks"].values():
            for g in groups:
                for entry in g["hooks"]:
                    cmd = entry["command"]
                    assert cmd.startswith("stackunderflow hooks run ")
                    assert "/" not in cmd.split()[0]  # the binary token is bare, not a path

    def test_capture_content_flag_threaded_into_commands(self, project_root: Path) -> None:
        install("project", cwd=project_root, capture_content=True)
        data = json.loads(_settings_path(project_root).read_text())
        for groups in data["hooks"].values():
            for g in groups:
                for entry in g["hooks"]:
                    assert entry["command"].endswith(" --capture-content")


# ── install: idempotency + convergence ──────────────────────────────────────


class TestInstallIdempotent:
    def test_reinstall_is_no_op_no_backup(self, project_root: Path) -> None:
        install("project", cwd=project_root)
        n_backups = len(_backups(project_root))
        report2 = install("project", cwd=project_root)
        assert report2.changed is False
        assert report2.backup_path is None
        assert len(_backups(project_root)) == n_backups  # no new backup churn

    def test_changing_capture_content_replaces_not_duplicates(self, project_root: Path) -> None:
        install("project", cwd=project_root)  # without
        report = install("project", cwd=project_root, capture_content=True)  # with
        assert report.changed is True
        data = json.loads(_settings_path(project_root).read_text())
        # Still exactly one group per event — the old one was replaced, not stacked.
        for ev in ("PostToolUse", "UserPromptSubmit", "Stop", "PreCompact"):
            ours = [g for g in data["hooks"][ev]
                    if any("stackunderflow hooks run" in e["command"] for e in g["hooks"])]
            assert len(ours) == 1
            assert all(e["command"].endswith(" --capture-content") for e in ours[0]["hooks"])

    def test_stale_absolute_path_entry_is_replaced(self, project_root: Path) -> None:
        # Simulate an old install that hardcoded a now-moved venv path.
        stale = {
            "hooks": {
                "Stop": [
                    {"hooks": [{"type": "command",
                                "command": "/old/venv/bin/stackunderflow hook run stackunderflow-stop"}]},
                ],
            },
        }
        _write_settings(project_root, stale)
        report = install("project", cwd=project_root)
        assert report.changed is True
        assert "stackunderflow-stop" in report.stale_entries_replaced
        data = json.loads(_settings_path(project_root).read_text())
        stop_cmds = [e["command"] for g in data["hooks"]["Stop"] for e in g["hooks"]]
        assert stop_cmds.count(canonical_command("stackunderflow-stop")) == 1
        assert all("/old/venv" not in c for c in stop_cmds)


# ── install: never touches other hooks; backs up first ──────────────────────


class TestInstallPreservesOthers:
    def test_other_tools_hooks_untouched_and_counted(self, project_root: Path) -> None:
        _write_settings(project_root, _OTHER_HOOKS_SETTINGS)
        report = install("project", cwd=project_root)
        assert report.other_hooks_preserved == 4  # the 4 other-tool entries
        data = json.loads(_settings_path(project_root).read_text())
        # Every original entry still present, byte-for-byte.
        assert data["permissions"] == _OTHER_HOOKS_SETTINGS["permissions"]
        assert data["model"] == _OTHER_HOOKS_SETTINGS["model"]
        assert data["hooks"]["PreToolUse"] == _OTHER_HOOKS_SETTINGS["hooks"]["PreToolUse"]
        assert data["hooks"]["SessionStart"] == _OTHER_HOOKS_SETTINGS["hooks"]["SessionStart"]
        # PostToolUse: their two groups still there + our new Bash group appended.
        ptu = data["hooks"]["PostToolUse"]
        assert _OTHER_HOOKS_SETTINGS["hooks"]["PostToolUse"][0] in ptu
        assert _OTHER_HOOKS_SETTINGS["hooks"]["PostToolUse"][1] in ptu
        ours = [g for g in ptu if any("stackunderflow hooks run" in e["command"] for e in g["hooks"])]
        assert len(ours) == 1

    def test_backup_written_before_mutation_with_iso_timestamp(self, project_root: Path) -> None:
        original = _write_settings(project_root, _OTHER_HOOKS_SETTINGS)
        original_bytes = original.read_bytes()
        report = install("project", cwd=project_root)
        assert report.backup_path is not None
        backup = Path(report.backup_path)
        assert backup.exists()
        # Name: settings.json.bak.<YYYYMMDDTHHMMSSZ> — basic-format ISO 8601, fs-safe.
        assert backup.name.startswith("settings.json.bak.")
        stamp = backup.name.rsplit(".bak.", 1)[1]
        assert stamp.endswith("Z") and "T" in stamp and ":" not in stamp
        # The backup is the *pre-mutation* content.
        assert backup.read_bytes() == original_bytes
        # And the live file actually changed.
        assert original.read_bytes() != original_bytes

    def test_dry_run_writes_nothing(self, project_root: Path) -> None:
        original = _write_settings(project_root, _OTHER_HOOKS_SETTINGS)
        before = original.read_bytes()
        report = install("project", cwd=project_root, dry_run=True)
        assert report.dry_run is True
        assert report.changed is True  # would change
        assert report.backup_path is None
        assert original.read_bytes() == before  # untouched
        assert _backups(project_root) == []

    def test_refuses_invalid_json_settings(self, project_root: Path) -> None:
        p = _settings_path(project_root)
        p.parent.mkdir(parents=True)
        p.write_text("{ this is not json")
        with pytest.raises(ValueError):
            install("project", cwd=project_root)
        # Left exactly as found.
        assert p.read_text() == "{ this is not json"


# ── uninstall ───────────────────────────────────────────────────────────────


class TestUninstall:
    def test_removes_only_ours_keeps_file_and_others(self, project_root: Path) -> None:
        _write_settings(project_root, _OTHER_HOOKS_SETTINGS)
        install("project", cwd=project_root)
        report = uninstall("project", cwd=project_root)
        assert report.changed is True
        assert set(report.hooks_removed) == set(HOOK_IDS)
        assert report.other_hooks_preserved == 4
        assert _settings_path(project_root).exists()  # never deletes the file
        data = json.loads(_settings_path(project_root).read_text())
        # Our events are gone (PostToolUse stays — it still has the *other* groups).
        assert "UserPromptSubmit" not in data["hooks"]
        assert "Stop" not in data["hooks"]
        assert "PreCompact" not in data["hooks"]
        assert data["hooks"]["PreToolUse"] == _OTHER_HOOKS_SETTINGS["hooks"]["PreToolUse"]
        assert data["hooks"]["SessionStart"] == _OTHER_HOOKS_SETTINGS["hooks"]["SessionStart"]
        ptu = data["hooks"]["PostToolUse"]
        assert _OTHER_HOOKS_SETTINGS["hooks"]["PostToolUse"][0] in ptu
        assert _OTHER_HOOKS_SETTINGS["hooks"]["PostToolUse"][1] in ptu
        assert not any("stackunderflow hooks run" in e["command"] for g in ptu for e in g["hooks"])
        assert data["permissions"] == _OTHER_HOOKS_SETTINGS["permissions"]

    def test_uninstall_backs_up_first(self, project_root: Path) -> None:
        install("project", cwd=project_root)
        report = uninstall("project", cwd=project_root)
        assert report.backup_path is not None
        assert Path(report.backup_path).exists()

    def test_uninstall_no_file_is_noop(self, project_root: Path) -> None:
        report = uninstall("project", cwd=project_root)
        assert report.file_existed is False
        assert report.changed is False
        assert report.backup_path is None

    def test_uninstall_no_our_hooks_is_noop(self, project_root: Path) -> None:
        _write_settings(project_root, _OTHER_HOOKS_SETTINGS)
        report = uninstall("project", cwd=project_root)
        assert report.changed is False
        assert report.backup_path is None
        # File unchanged.
        assert json.loads(_settings_path(project_root).read_text()) == _OTHER_HOOKS_SETTINGS

    def test_install_then_uninstall_round_trips_to_original(self, project_root: Path) -> None:
        original = dict(_OTHER_HOOKS_SETTINGS)
        _write_settings(project_root, original)
        install("project", cwd=project_root)
        uninstall("project", cwd=project_root)
        # After a clean round-trip the surviving content equals the original
        # (modulo our PostToolUse group, which uninstall removed; the *other*
        # PostToolUse groups remain → the hooks dict matches the original).
        data = json.loads(_settings_path(project_root).read_text())
        assert data == original


# ── scopes don't bleed ──────────────────────────────────────────────────────


class TestScopeIsolation:
    def test_project_install_does_not_touch_user(self, tmp_path: Path, monkeypatch) -> None:
        # Fake $HOME so "user" scope points somewhere we control & can assert on.
        fake_home = tmp_path / "home"
        fake_home.mkdir()
        monkeypatch.setattr(Path, "home", staticmethod(lambda: fake_home))
        proj = tmp_path / "proj"
        (proj / ".git").mkdir(parents=True)

        install("project", cwd=proj)
        assert (proj / ".claude" / "settings.json").exists()
        assert not (fake_home / ".claude" / "settings.json").exists()  # user scope untouched

    def test_user_install_does_not_touch_project(self, tmp_path: Path, monkeypatch) -> None:
        fake_home = tmp_path / "home"
        fake_home.mkdir()
        monkeypatch.setattr(Path, "home", staticmethod(lambda: fake_home))
        proj = tmp_path / "proj"
        (proj / ".git").mkdir(parents=True)

        install("user")
        assert (fake_home / ".claude" / "settings.json").exists()
        assert not (proj / ".claude" / "settings.json").exists()

    def test_status_reports_both_scopes(self, tmp_path: Path, monkeypatch) -> None:
        fake_home = tmp_path / "home"
        fake_home.mkdir()
        monkeypatch.setattr(Path, "home", staticmethod(lambda: fake_home))
        proj = tmp_path / "proj"
        (proj / ".git").mkdir(parents=True)
        install("project", cwd=proj)

        st = status(cwd=proj)
        assert set(st) == {"project", "user"}
        assert set(st["project"]["hooks"]) == set(HOOK_IDS)
        assert st["project"]["stale"] == []
        assert st["user"]["exists"] is False
        assert st["user"]["hooks"] == {}

    def test_status_flags_stale_entries(self, project_root: Path) -> None:
        _write_settings(project_root, {
            "hooks": {"Stop": [{"hooks": [{"type": "command",
                                           "command": "/moved/bin/stackunderflow hooks run stackunderflow-stop"}]}]},
        })
        st = status("project", cwd=project_root)
        assert "stackunderflow-stop" in st["project"]["stale"]
