"""``stackunderflow hooks repair`` — the ``$HOME`` walk and the canonicalisation.

Locked here (the spec's repair tier):

* finds stale StackUnderflow hook commands and rewrites *only* the ``command``
  string to the portable form (preserving ``--capture-content``); changes
  nothing else; backs up first.
* ``--dry-run`` reports without mutating.
* the ``$HOME`` walk: prunes ``node_modules`` / ``.git`` / ``.npm`` /
  ``.cache`` / ``.nvm`` (and friends), never follows symlinks (no infinite
  loop on a self-pointing link), depth-limited to 8.
* ``--scope project|user`` narrows to a single file; never the file or other
  tools' hooks.
"""

from __future__ import annotations

import json
import os
import sys
from pathlib import Path

import pytest

from stackunderflow.hooks import _repair as repair_mod
from stackunderflow.hooks import repair
from stackunderflow.hooks.templates import canonical_command


def _claude_settings(project_dir: Path, data: dict) -> Path:
    p = project_dir / ".claude" / "settings.json"
    p.parent.mkdir(parents=True, exist_ok=True)
    p.write_text(json.dumps(data, indent=2))
    return p


def _stale_settings(*, hook_id: str = "stackunderflow-stop", event: str = "Stop",
                    command: str | None = None) -> dict:
    cmd = command or f"/old/venv-12345/bin/stackunderflow hook run {hook_id}"
    return {"hooks": {event: [{"hooks": [{"type": "command", "command": cmd}]}]}}


def _other_tools_settings() -> dict:
    return {
        "hooks": {
            "PreToolUse": [{"matcher": "Bash", "hooks": [{"type": "command", "command": "guard.sh"}]}],
            "PostToolUse": [{"matcher": "Edit", "hooks": [{"type": "command", "command": "fmt.py"}]}],
        },
    }


# ── per-file canonicalisation ───────────────────────────────────────────────


class TestRepairCanonicalisation:
    def test_rewrites_stale_absolute_path(self, tmp_path: Path) -> None:
        (tmp_path / ".git").mkdir()
        p = _claude_settings(tmp_path, _stale_settings())
        report = repair("project", cwd=tmp_path)
        assert len(report.repaired) == 1
        change = report.repaired[0]
        assert change["hook_id"] == "stackunderflow-stop"
        assert change["old"].startswith("/old/venv-12345/bin/stackunderflow hook run")
        assert change["new"] == canonical_command("stackunderflow-stop")
        # File rewritten; command is now portable.
        cmd = json.loads(p.read_text())["hooks"]["Stop"][0]["hooks"][0]["command"]
        assert cmd == canonical_command("stackunderflow-stop")

    def test_rewrites_legacy_singular_hook_spelling(self, tmp_path: Path) -> None:
        (tmp_path / ".git").mkdir()
        p = _claude_settings(tmp_path, _stale_settings(command="stackunderflow hook run stackunderflow-user-prompt"))
        report = repair("project", cwd=tmp_path)
        assert len(report.repaired) == 1
        assert json.loads(p.read_text())["hooks"]["Stop"][0]["hooks"][0]["command"] == \
            canonical_command("stackunderflow-user-prompt")

    def test_preserves_capture_content_flag(self, tmp_path: Path) -> None:
        (tmp_path / ".git").mkdir()
        p = _claude_settings(
            tmp_path,
            _stale_settings(command="/x/bin/stackunderflow hooks run stackunderflow-post-tool-use --capture-content"),
        )
        repair("project", cwd=tmp_path)
        assert json.loads(p.read_text())["hooks"]["Stop"][0]["hooks"][0]["command"] == \
            canonical_command("stackunderflow-post-tool-use", capture_content=True)

    def test_already_canonical_is_noop(self, tmp_path: Path) -> None:
        (tmp_path / ".git").mkdir()
        p = _claude_settings(tmp_path, {"hooks": {"Stop": [{"hooks": [
            {"type": "command", "command": canonical_command("stackunderflow-stop")}]}]}})
        before = p.read_bytes()
        report = repair("project", cwd=tmp_path)
        assert report.repaired == []
        assert p.read_bytes() == before
        assert report.backups == []

    def test_leaves_unrecognised_hooks_alone(self, tmp_path: Path) -> None:
        (tmp_path / ".git").mkdir()
        # A hook that doesn't carry one of our id tokens — not ours, never touched.
        p = _claude_settings(tmp_path, {"hooks": {"PostToolUse": [{"matcher": "Bash", "hooks": [
            {"type": "command", "command": "/some/other/stackunderflow-ish-thing.sh"}]}]}})
        before = p.read_bytes()
        report = repair("project", cwd=tmp_path)
        assert report.repaired == []
        assert p.read_bytes() == before

    def test_does_not_change_other_tools_hooks(self, tmp_path: Path) -> None:
        (tmp_path / ".git").mkdir()
        mixed = _other_tools_settings()
        mixed["hooks"]["Stop"] = [{"hooks": [{"type": "command",
                                               "command": "/old/bin/stackunderflow hook run stackunderflow-stop"}]}]
        p = _claude_settings(tmp_path, mixed)
        repair("project", cwd=tmp_path)
        data = json.loads(p.read_text())
        assert data["hooks"]["PreToolUse"] == mixed["hooks"]["PreToolUse"]
        assert data["hooks"]["PostToolUse"] == mixed["hooks"]["PostToolUse"]
        assert data["hooks"]["Stop"][0]["hooks"][0]["command"] == canonical_command("stackunderflow-stop")

    def test_backs_up_before_rewrite(self, tmp_path: Path) -> None:
        (tmp_path / ".git").mkdir()
        p = _claude_settings(tmp_path, _stale_settings())
        before = p.read_bytes()
        report = repair("project", cwd=tmp_path)
        assert len(report.backups) == 1
        backup = Path(report.backups[0])
        assert backup.exists()
        assert backup.read_bytes() == before  # the pre-rewrite content
        assert "settings.json.bak." in backup.name

    def test_dry_run_reports_without_mutating(self, tmp_path: Path) -> None:
        (tmp_path / ".git").mkdir()
        p = _claude_settings(tmp_path, _stale_settings())
        before = p.read_bytes()
        report = repair("project", cwd=tmp_path, dry_run=True)
        assert report.dry_run is True
        assert len(report.repaired) == 1   # would rewrite
        assert report.backups == []
        assert p.read_bytes() == before     # untouched
        assert list((tmp_path / ".claude").glob("settings.json.bak.*")) == []

    def test_skips_invalid_json_file(self, tmp_path: Path) -> None:
        (tmp_path / ".git").mkdir()
        p = tmp_path / ".claude" / "settings.json"
        p.parent.mkdir(parents=True)
        p.write_text("{not json")
        report = repair("project", cwd=tmp_path)
        assert report.repaired == []
        assert p.read_text() == "{not json"  # left as-is

    def test_missing_file_is_clean_noop(self, tmp_path: Path) -> None:
        (tmp_path / ".git").mkdir()
        report = repair("project", cwd=tmp_path)
        assert report.repaired == []
        assert report.scanned_files  # we still record what we looked for

    def test_unknown_scope_rejected(self) -> None:
        with pytest.raises(ValueError):
            repair("galaxy")


# ── the $HOME walk (scope=all) ──────────────────────────────────────────────


class TestRepairHomeWalk:
    def test_finds_settings_across_multiple_projects(self, tmp_path: Path) -> None:
        home = tmp_path
        a = home / "dev" / "proj-a"
        b = home / "work" / "deep" / "proj-b"
        _claude_settings(a, _stale_settings())
        _claude_settings(b, _stale_settings(command="stackunderflow hook run stackunderflow-stop"))
        # a project with NO stale hook — scanned but not rewritten
        _claude_settings(home / "clean", {"hooks": {"Stop": [{"hooks": [
            {"type": "command", "command": canonical_command("stackunderflow-stop")}]}]}})

        report = repair("all", home=home)
        scanned = {Path(s) for s in report.scanned_files}
        assert (a / ".claude" / "settings.json") in scanned
        assert (b / ".claude" / "settings.json") in scanned
        assert (home / "clean" / ".claude" / "settings.json") in scanned
        repaired_files = {Path(e["file"]) for e in report.repaired}
        assert repaired_files == {a / ".claude" / "settings.json", b / ".claude" / "settings.json"}
        # both rewritten on disk
        assert json.loads((a / ".claude" / "settings.json").read_text())["hooks"]["Stop"][0]["hooks"][0]["command"] \
            == canonical_command("stackunderflow-stop")

    def test_prunes_named_heavy_dirs(self, tmp_path: Path) -> None:
        home = tmp_path
        # A stale settings buried inside each pruned dir — must NOT be found.
        for pruned in ("node_modules", ".git", ".npm", ".cache", ".nvm"):
            _claude_settings(home / "pkg" / pruned / "nested", _stale_settings())
        # ...and one in a normal dir — must be found.
        _claude_settings(home / "real-proj", _stale_settings())

        report = repair("all", home=home)
        scanned = {Path(s) for s in report.scanned_files}
        assert scanned == {home / "real-proj" / ".claude" / "settings.json"}
        assert report.pruned_dirs >= 5  # at least the five we planted

    def test_does_not_follow_symlinks_no_infinite_loop(self, tmp_path: Path) -> None:
        if sys.platform == "win32":  # pragma: no cover - symlink semantics differ; covered on POSIX
            pytest.skip("symlink loop test is POSIX-only")
        home = tmp_path
        _claude_settings(home / "proj", _stale_settings())
        # A directory that symlinks back to home → would loop forever if followed.
        loop = home / "proj" / "loop"
        os.symlink(home, loop)
        # A symlink pointing at another real project — its settings must NOT be
        # reached via the link (we don't traverse symlinked dirs).
        other = tmp_path.parent / f"{tmp_path.name}-external"
        _claude_settings(other / "ext-proj", _stale_settings())
        try:
            os.symlink(other, home / "via-link")
            report = repair("all", home=home)  # must terminate
            scanned = {Path(s) for s in report.scanned_files}
            assert scanned == {home / "proj" / ".claude" / "settings.json"}
            assert all("ext-proj" not in s for s in report.scanned_files)
        finally:
            import shutil
            shutil.rmtree(other, ignore_errors=True)

    def test_depth_limited(self, tmp_path: Path) -> None:
        home = tmp_path
        # 8 levels under home: a1/a2/.../a8/.claude/settings.json — .claude is the
        # 9th level, past the budget → not found.
        too_deep = home
        for i in range(1, 9):
            too_deep = too_deep / f"a{i}"
        _claude_settings(too_deep, _stale_settings())
        # Shallow one — found.
        _claude_settings(home / "shallow", _stale_settings())

        report = repair("all", home=home)
        scanned = {Path(s) for s in report.scanned_files}
        assert (home / "shallow" / ".claude" / "settings.json") in scanned
        assert (too_deep / ".claude" / "settings.json") not in scanned

    def test_within_depth_budget_is_found(self, tmp_path: Path) -> None:
        home = tmp_path
        # 5 levels: a1/a2/a3/a4/.claude/settings.json — comfortably inside 8.
        d = home / "a1" / "a2" / "a3" / "a4"
        _claude_settings(d, _stale_settings())
        report = repair("all", home=home)
        assert (d / ".claude" / "settings.json") in {Path(s) for s in report.scanned_files}

    def test_all_scope_dry_run_mutates_nothing(self, tmp_path: Path) -> None:
        home = tmp_path
        p = _claude_settings(home / "proj", _stale_settings())
        before = p.read_bytes()
        report = repair("all", home=home, dry_run=True)
        assert len(report.repaired) == 1
        assert report.backups == []
        assert p.read_bytes() == before


# ── the bounded-walk helper, directly ───────────────────────────────────────


class TestScanSettingsFiles:
    def test_finds_only_claude_settings(self, tmp_path: Path) -> None:
        _claude_settings(tmp_path / "proj", {"hooks": {}})
        (tmp_path / "proj" / ".claude" / "other.json").write_text("{}")
        (tmp_path / "proj" / "settings.json").write_text("{}")  # not under .claude
        found, pruned = repair_mod._scan_settings_files(tmp_path)
        assert found == [tmp_path / "proj" / ".claude" / "settings.json"]
        assert pruned == 0

    def test_max_depth_param_respected(self, tmp_path: Path) -> None:
        d = tmp_path / "a" / "b" / "c"
        _claude_settings(d, {"hooks": {}})  # .claude is 4 levels under tmp_path
        assert repair_mod._scan_settings_files(tmp_path, max_depth=3)[0] == []
        assert repair_mod._scan_settings_files(tmp_path, max_depth=8)[0] == \
            [d / ".claude" / "settings.json"]

    def test_pruned_count_is_total_not_snapshot(self, tmp_path: Path) -> None:
        # Plant pruned dirs that come *after* a real project alphabetically so a
        # naive "count at last yield" would report 0 — the total must be right.
        for pruned in ("node_modules", ".git", ".npm"):
            _claude_settings(tmp_path / "zzz" / pruned / "x", {"hooks": {}})
        _claude_settings(tmp_path / "aaa", {"hooks": {}})
        found, pruned_count = repair_mod._scan_settings_files(tmp_path)
        assert found == [tmp_path / "aaa" / ".claude" / "settings.json"]
        assert pruned_count >= 3
