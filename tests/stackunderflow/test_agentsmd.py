"""``stackunderflow.agentsmd`` — the agent-discovery snippet installer (Move 4).

Locks the idempotent-merge contract it shares with the hooks installer:

* ``install`` writes a marked block; re-running converges (never a second copy).
* content outside the markers is preserved byte-for-byte.
* a timestamped backup is written before any mutation — never on a no-op,
  never under ``--dry-run``.
* ``uninstall`` strips only our block and never deletes the file.
* a half-written file (one orphan marker) converges on the next install.
* ``project`` scope targets ``CLAUDE.md`` + ``AGENTS.md``; ``user`` scope
  targets ``~/.claude/CLAUDE.md``; the two never bleed.

Every path is a ``tmp_path`` — the real ``~/.claude`` is never touched.
"""

from __future__ import annotations

from pathlib import Path

import pytest

from stackunderflow import agentsmd
from stackunderflow.agentsmd import GUIDE_END, GUIDE_START, render_block


@pytest.fixture
def project_root(tmp_path: Path) -> Path:
    """A tmp dir that looks like a git repo (so ``project`` scope resolves here)."""
    (tmp_path / ".git").mkdir()
    return tmp_path


@pytest.fixture
def fake_home(tmp_path: Path, monkeypatch) -> Path:
    """Point ``Path.home()`` at a tmp dir so ``user`` scope stays off the real ~."""
    home = tmp_path / "home"
    home.mkdir()
    monkeypatch.setattr(Path, "home", staticmethod(lambda: home))
    return home


def _claude(root: Path) -> Path:
    return root / "CLAUDE.md"


def _agents(root: Path) -> Path:
    return root / "AGENTS.md"


def _backups(path: Path) -> list[Path]:
    return sorted(path.parent.glob(f"{path.name}.bak.*"))


# ── install: fresh ──────────────────────────────────────────────────────────


class TestInstallFresh:
    def test_project_scope_creates_both_files(self, project_root: Path) -> None:
        report = agentsmd.install("project", cwd=project_root)
        assert report.changed is True
        assert {Path(f.path).name for f in report.files} == {"CLAUDE.md", "AGENTS.md"}
        for path in (_claude(project_root), _agents(project_root)):
            text = path.read_text()
            assert GUIDE_START in text and GUIDE_END in text
            assert text == render_block() + "\n"

    def test_snippet_names_the_memory_commands(self, project_root: Path) -> None:
        agentsmd.install("project", cwd=project_root)
        text = _claude(project_root).read_text()
        for sub in ("memory file", "memory decisions", "memory worked", "memory sessions", "memory ask"):
            assert sub in text
        assert "--json" in text
        assert "stackunderflow.memory/1" in text  # the Move 2 contract

    def test_fresh_install_writes_no_backup(self, project_root: Path) -> None:
        report = agentsmd.install("project", cwd=project_root)
        assert all(f.backup_path is None for f in report.files)
        assert all(f.created for f in report.files)
        assert _backups(_claude(project_root)) == []


# ── install: idempotency + convergence ──────────────────────────────────────


class TestInstallIdempotent:
    def test_reinstall_is_no_op(self, project_root: Path) -> None:
        agentsmd.install("project", cwd=project_root)
        report2 = agentsmd.install("project", cwd=project_root)
        assert report2.changed is False
        assert all(f.action == "unchanged" for f in report2.files)
        assert _backups(_claude(project_root)) == []  # no churn

    def test_existing_content_preserved_block_appended(self, project_root: Path) -> None:
        original = "# My Project\n\nHand-written notes.\n"
        _claude(project_root).write_text(original)
        agentsmd.install("project", cwd=project_root)
        text = _claude(project_root).read_text()
        assert text.startswith("# My Project\n\nHand-written notes.")
        assert text.endswith(render_block() + "\n")
        # And re-installing still converges.
        agentsmd.install("project", cwd=project_root)
        assert _claude(project_root).read_text() == text

    def test_stale_block_is_replaced_not_duplicated(self, project_root: Path) -> None:
        stale = f"# Notes\n\n{GUIDE_START}\nOLD AND WRONG\n{GUIDE_END}\n"
        _claude(project_root).write_text(stale)
        report = agentsmd.install("project", cwd=project_root)
        claude_result = next(f for f in report.files if Path(f.path).name == "CLAUDE.md")
        assert claude_result.action == "updated"
        text = _claude(project_root).read_text()
        assert "OLD AND WRONG" not in text
        assert text.count(GUIDE_START) == 1  # exactly one block, not stacked
        assert text.startswith("# Notes")

    def test_orphan_marker_converges(self, project_root: Path) -> None:
        # A half-written file with only the start marker — install must land clean.
        _claude(project_root).write_text(f"# Notes\n{GUIDE_START}\nstray\n")
        agentsmd.install("project", cwd=project_root)
        text = _claude(project_root).read_text()
        assert text.count(GUIDE_START) == 1
        assert text.count(GUIDE_END) == 1


# ── install: backup + dry-run ───────────────────────────────────────────────


class TestInstallBackupAndDryRun:
    def test_backup_written_before_mutating_existing_file(self, project_root: Path) -> None:
        original = "# Project\n"
        _claude(project_root).write_text(original)
        report = agentsmd.install("project", cwd=project_root)
        claude_result = next(f for f in report.files if Path(f.path).name == "CLAUDE.md")
        assert claude_result.backup_path is not None
        backup = Path(claude_result.backup_path)
        assert backup.exists()
        assert backup.read_text() == original  # the pre-mutation content
        assert ":" not in backup.name  # fs-safe timestamp

    def test_dry_run_writes_nothing(self, project_root: Path) -> None:
        report = agentsmd.install("project", cwd=project_root, dry_run=True)
        assert report.dry_run is True
        assert report.changed is True  # would change
        assert not _claude(project_root).exists()
        assert not _agents(project_root).exists()

    def test_dry_run_on_existing_writes_no_backup(self, project_root: Path) -> None:
        _claude(project_root).write_text("# Project\n")
        agentsmd.install("project", cwd=project_root, dry_run=True)
        assert _backups(_claude(project_root)) == []


# ── uninstall ───────────────────────────────────────────────────────────────


class TestUninstall:
    def test_removes_block_keeps_the_rest(self, project_root: Path) -> None:
        original = "# My Project\n\nHand-written notes.\n"
        _claude(project_root).write_text(original)
        agentsmd.install("project", cwd=project_root)
        report = agentsmd.uninstall("project", cwd=project_root)
        assert report.changed is True
        text = _claude(project_root).read_text()
        assert GUIDE_START not in text
        assert "Hand-written notes." in text

    def test_round_trips_to_original(self, project_root: Path) -> None:
        original = "# My Project\n\nHand-written notes.\n"
        _claude(project_root).write_text(original)
        agentsmd.install("project", cwd=project_root)
        agentsmd.uninstall("project", cwd=project_root)
        assert _claude(project_root).read_text() == original

    def test_never_deletes_the_file(self, project_root: Path) -> None:
        agentsmd.install("project", cwd=project_root)  # file is now block-only
        agentsmd.uninstall("project", cwd=project_root)
        # File survives even though our block was all it held — it is just empty.
        assert _claude(project_root).exists()
        assert _claude(project_root).read_text() == ""

    def test_uninstall_absent_file_is_noop(self, project_root: Path) -> None:
        report = agentsmd.uninstall("project", cwd=project_root)
        assert report.changed is False
        assert all(f.action == "absent" for f in report.files)

    def test_uninstall_backs_up_first(self, project_root: Path) -> None:
        agentsmd.install("project", cwd=project_root)
        report = agentsmd.uninstall("project", cwd=project_root)
        claude_result = next(f for f in report.files if Path(f.path).name == "CLAUDE.md")
        assert claude_result.backup_path is not None
        assert Path(claude_result.backup_path).exists()


# ── status ──────────────────────────────────────────────────────────────────


class TestStatus:
    def test_status_reports_install_state(self, project_root: Path, fake_home: Path) -> None:
        agentsmd.install("project", cwd=project_root)
        st = agentsmd.status(cwd=project_root)
        assert set(st) == {"project", "user"}
        project_files = {Path(e["path"]).name: e for e in st["project"]}
        assert project_files["CLAUDE.md"]["installed"] is True
        assert project_files["CLAUDE.md"]["up_to_date"] is True
        # user scope untouched
        assert st["user"][0]["exists"] is False

    def test_status_flags_stale_block(self, project_root: Path) -> None:
        _claude(project_root).write_text(f"{GUIDE_START}\nOLD\n{GUIDE_END}\n")
        st = agentsmd.status("project", cwd=project_root)
        claude_entry = next(e for e in st["project"] if Path(e["path"]).name == "CLAUDE.md")
        assert claude_entry["installed"] is True
        assert claude_entry["up_to_date"] is False


# ── scopes ──────────────────────────────────────────────────────────────────


class TestScopes:
    def test_user_scope_targets_home_claude(self, fake_home: Path) -> None:
        report = agentsmd.install("user")
        assert [Path(f.path) for f in report.files] == [fake_home / ".claude" / "CLAUDE.md"]
        assert (fake_home / ".claude" / "CLAUDE.md").read_text() == render_block() + "\n"

    def test_project_install_does_not_touch_user(self, project_root: Path, fake_home: Path) -> None:
        agentsmd.install("project", cwd=project_root)
        assert not (fake_home / ".claude" / "CLAUDE.md").exists()

    def test_invalid_scope_rejected(self) -> None:
        with pytest.raises(ValueError):
            agentsmd.install("everywhere")
        with pytest.raises(ValueError):
            agentsmd.status("everywhere")


# ── refuses what it cannot round-trip ───────────────────────────────────────


class TestRobustness:
    def test_non_utf8_file_is_refused(self, project_root: Path) -> None:
        _claude(project_root).write_bytes(b"\xff\xfe\x00binary\x00")
        with pytest.raises(ValueError):
            agentsmd.install("project", cwd=project_root)
        # Left exactly as found.
        assert _claude(project_root).read_bytes() == b"\xff\xfe\x00binary\x00"

    def test_report_to_dict_round_trips(self, project_root: Path) -> None:
        report = agentsmd.install("project", cwd=project_root)
        d = report.to_dict()
        assert d["scope"] == "project"
        assert d["operation"] == "install"
        assert d["changed"] is True
        assert len(d["files"]) == 2
