"""``stackunderflow guide {install,uninstall,status}`` — the CLI surface.

The merge behaviour is locked in ``tests/stackunderflow/test_agentsmd.py``;
this file checks the *plumbing*: option parsing, ``--dry-run``, ``--format json``,
scope choices, and that the commands round-trip a CLAUDE.md / AGENTS.md edit.

Every path is a ``tmp_path`` with ``$HOME`` redirected — the real ``~/.claude``
is never touched.
"""

from __future__ import annotations

from pathlib import Path

import pytest
from click.testing import CliRunner

from stackunderflow.agentsmd import GUIDE_START
from stackunderflow.cli import cli


@pytest.fixture
def project(tmp_path: Path, monkeypatch) -> Path:
    """A git-rooted tmp project, CWD set to it, $HOME redirected to tmp."""
    monkeypatch.chdir(tmp_path)
    (tmp_path / ".git").mkdir()
    home = tmp_path / "home"
    home.mkdir()
    monkeypatch.setattr(Path, "home", staticmethod(lambda: home))
    return tmp_path


class TestGuideInstall:
    def test_install_creates_both_instruction_files(self, project: Path) -> None:
        res = CliRunner().invoke(cli, ["guide", "install"])
        assert res.exit_code == 0
        assert "Installed the StackUnderflow guide snippet" in res.output
        assert (project / "CLAUDE.md").exists()
        assert (project / "AGENTS.md").exists()
        assert GUIDE_START in (project / "CLAUDE.md").read_text()

    def test_dry_run_writes_nothing(self, project: Path) -> None:
        res = CliRunner().invoke(cli, ["guide", "install", "--dry-run"])
        assert res.exit_code == 0
        assert "Would install" in res.output
        assert not (project / "CLAUDE.md").exists()

    def test_reinstall_reports_no_change(self, project: Path) -> None:
        CliRunner().invoke(cli, ["guide", "install"])
        res = CliRunner().invoke(cli, ["guide", "install"])
        assert res.exit_code == 0
        assert "no change" in res.output

    def test_install_user_scope(self, project: Path) -> None:
        res = CliRunner().invoke(cli, ["guide", "install", "--scope", "user"])
        assert res.exit_code == 0
        assert (Path.home() / ".claude" / "CLAUDE.md").exists()
        assert not (project / "CLAUDE.md").exists()  # project scope untouched

    def test_bad_scope_rejected(self, project: Path) -> None:
        res = CliRunner().invoke(cli, ["guide", "install", "--scope", "everywhere"])
        assert res.exit_code != 0


class TestGuideUninstall:
    def test_uninstall_removes_block(self, project: Path) -> None:
        (project / "CLAUDE.md").write_text("# Notes\n")
        CliRunner().invoke(cli, ["guide", "install"])
        res = CliRunner().invoke(cli, ["guide", "uninstall"])
        assert res.exit_code == 0
        assert "Removed" in res.output
        text = (project / "CLAUDE.md").read_text()
        assert GUIDE_START not in text
        assert "# Notes" in text

    def test_uninstall_nothing_to_remove(self, project: Path) -> None:
        res = CliRunner().invoke(cli, ["guide", "uninstall"])
        assert res.exit_code == 0
        assert "no change" in res.output


class TestGuideStatus:
    def test_status_text_before_and_after_install(self, project: Path) -> None:
        res = CliRunner().invoke(cli, ["guide", "status"])
        assert res.exit_code == 0
        assert "not installed" in res.output or "no file" in res.output
        CliRunner().invoke(cli, ["guide", "install"])
        res = CliRunner().invoke(cli, ["guide", "status"])
        assert "[project]" in res.output and "[user]" in res.output
        assert "installed" in res.output

    def test_status_json(self, project: Path) -> None:
        import json

        CliRunner().invoke(cli, ["guide", "install"])
        res = CliRunner().invoke(cli, ["guide", "status", "--format", "json"])
        assert res.exit_code == 0
        payload = json.loads(res.output)
        assert set(payload) == {"project", "user"}
        claude = next(e for e in payload["project"] if e["path"].endswith("CLAUDE.md"))
        assert claude["installed"] is True
        assert claude["up_to_date"] is True


def test_guide_group_help_lists_subcommands() -> None:
    res = CliRunner().invoke(cli, ["guide", "--help"])
    assert res.exit_code == 0
    for sub in ("install", "uninstall", "status"):
        assert sub in res.output
