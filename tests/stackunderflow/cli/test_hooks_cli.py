"""``stackunderflow hooks {install,uninstall,status,repair,run}`` — CLI surface.

The behavioural depth lives in ``tests/stackunderflow/hooks/`` (service-level);
this file checks the *plumbing*: option parsing, ``--dry-run`` printing the
would-be ``settings.json`` block, ``--format json``, scope choices, and the
internal ``hooks run`` command reading the payload off stdin and exiting 0.
"""

from __future__ import annotations

import json
from pathlib import Path

import pytest
from click.testing import CliRunner

import stackunderflow.deps as deps
from stackunderflow.cli import cli


@pytest.fixture
def project(tmp_path: Path, monkeypatch) -> Path:
    """A git-rooted tmp project, CWD set to it, store + $HOME pointed at tmp."""
    monkeypatch.chdir(tmp_path)
    (tmp_path / ".git").mkdir()
    monkeypatch.setattr(deps, "store_path", tmp_path / "store.db")
    # Keep "user" scope off the developer's real ~/.claude during the test.
    home = tmp_path / "home"
    home.mkdir()
    monkeypatch.setattr(Path, "home", staticmethod(lambda: home))
    return tmp_path


def _settings(root: Path) -> Path:
    return root / ".claude" / "settings.json"


class TestInstallCmd:
    def test_dry_run_prints_block_writes_nothing(self, project: Path) -> None:
        res = CliRunner().invoke(cli, ["hooks", "install", "--dry-run"])
        assert res.exit_code == 0
        assert "Would install" in res.output
        assert "stackunderflow hooks run stackunderflow-post-tool-use" in res.output
        assert '"matcher": "Bash"' in res.output
        assert not _settings(project).exists()  # nothing written

    def test_install_creates_settings(self, project: Path) -> None:
        res = CliRunner().invoke(cli, ["hooks", "install"])
        assert res.exit_code == 0
        assert "Installed StackUnderflow hooks" in res.output
        data = json.loads(_settings(project).read_text())
        assert set(data["hooks"]) == {"PostToolUse", "UserPromptSubmit", "Stop", "PreCompact"}

    def test_install_capture_content_warns(self, project: Path) -> None:
        res = CliRunner().invoke(cli, ["hooks", "install", "--capture-content"])
        assert res.exit_code == 0
        assert "--capture-content" in res.output
        data = json.loads(_settings(project).read_text())
        cmd = data["hooks"]["Stop"][0]["hooks"][0]["command"]
        assert cmd.endswith(" --capture-content")

    def test_install_user_scope(self, project: Path) -> None:
        res = CliRunner().invoke(cli, ["hooks", "install", "--scope", "user"])
        assert res.exit_code == 0
        assert (Path.home() / ".claude" / "settings.json").exists()
        assert not _settings(project).exists()  # project scope untouched

    def test_bad_scope_rejected(self, project: Path) -> None:
        res = CliRunner().invoke(cli, ["hooks", "install", "--scope", "everywhere"])
        assert res.exit_code != 0


class TestStatusCmd:
    def test_status_text_after_install(self, project: Path) -> None:
        CliRunner().invoke(cli, ["hooks", "install"])
        res = CliRunner().invoke(cli, ["hooks", "status"])
        assert res.exit_code == 0
        assert "stackunderflow-post-tool-use" in res.output
        assert "[project]" in res.output and "[user]" in res.output

    def test_status_json(self, project: Path) -> None:
        CliRunner().invoke(cli, ["hooks", "install"])
        res = CliRunner().invoke(cli, ["hooks", "status", "--format", "json"])
        assert res.exit_code == 0
        payload = json.loads(res.output)
        assert set(payload) == {"project", "user"}
        assert set(payload["project"]["hooks"]) == {
            "stackunderflow-post-tool-use", "stackunderflow-user-prompt",
            "stackunderflow-stop", "stackunderflow-pre-compact",
        }
        assert payload["project"]["stale"] == []

    def test_status_flags_stale(self, project: Path) -> None:
        p = _settings(project)
        p.parent.mkdir(parents=True)
        p.write_text(json.dumps({"hooks": {"Stop": [{"hooks": [
            {"type": "command", "command": "/gone/bin/stackunderflow hook run stackunderflow-stop"}]}]}}))
        res = CliRunner().invoke(cli, ["hooks", "status"])
        assert "STALE" in res.output


class TestRepairCmd:
    def test_repair_dry_run(self, project: Path) -> None:
        p = _settings(project)
        p.parent.mkdir(parents=True)
        p.write_text(json.dumps({"hooks": {"Stop": [{"hooks": [
            {"type": "command", "command": "/old/bin/stackunderflow hook run stackunderflow-stop"}]}]}}))
        before = p.read_bytes()
        res = CliRunner().invoke(cli, ["hooks", "repair", "--dry-run"])
        assert res.exit_code == 0
        assert "Would rewrite" in res.output
        assert p.read_bytes() == before

    def test_repair_rewrites(self, project: Path) -> None:
        p = _settings(project)
        p.parent.mkdir(parents=True)
        p.write_text(json.dumps({"hooks": {"Stop": [{"hooks": [
            {"type": "command", "command": "/old/bin/stackunderflow hook run stackunderflow-stop"}]}]}}))
        res = CliRunner().invoke(cli, ["hooks", "repair"])
        assert res.exit_code == 0
        assert "Rewrote" in res.output
        assert json.loads(p.read_text())["hooks"]["Stop"][0]["hooks"][0]["command"] == \
            "stackunderflow hooks run stackunderflow-stop"

    def test_repair_nothing_stale(self, project: Path) -> None:
        CliRunner().invoke(cli, ["hooks", "install"])
        res = CliRunner().invoke(cli, ["hooks", "repair"])
        assert res.exit_code == 0
        assert "No stale" in res.output

    def test_repair_all_scope_accepted(self, project: Path) -> None:
        # Just exercise the option path; the $HOME walk's behaviour is covered
        # in test_repair.py. With $HOME pointed at the tmp ``home`` dir there's
        # nothing to find — clean run.
        res = CliRunner().invoke(cli, ["hooks", "repair", "--scope", "all"])
        assert res.exit_code == 0


class TestRunCmd:
    def test_run_reads_payload_from_stdin(self, project: Path) -> None:
        payload = {"hook_event_name": "PostToolUse", "tool_name": "Bash",
                   "session_id": "s1", "tool_response": {"exit_code": 3}}
        res = CliRunner().invoke(cli, ["hooks", "run", "stackunderflow-post-tool-use"],
                                 input=json.dumps(payload))
        assert res.exit_code == 0
        import sqlite3
        conn = sqlite3.connect(deps.store_path)
        try:
            rows = conn.execute("SELECT hook_id, event_kind FROM captured_events").fetchall()
        finally:
            conn.close()
        assert rows == [("stackunderflow-post-tool-use", "failure")]

    def test_run_with_garbage_stdin_exits_zero(self, project: Path) -> None:
        res = CliRunner().invoke(cli, ["hooks", "run", "stackunderflow-stop"], input="<<<not json>>>")
        assert res.exit_code == 0

    def test_run_with_empty_stdin_exits_zero(self, project: Path) -> None:
        res = CliRunner().invoke(cli, ["hooks", "run", "stackunderflow-stop"], input="")
        assert res.exit_code == 0

    def test_run_unknown_hook_id_exits_zero(self, project: Path) -> None:
        res = CliRunner().invoke(cli, ["hooks", "run", "stackunderflow-bogus"], input="{}")
        assert res.exit_code == 0

    def test_run_capture_content_flag_parsed(self, project: Path) -> None:
        payload = {"hook_event_name": "UserPromptSubmit", "prompt": "no, undo that", "session_id": "s"}
        res = CliRunner().invoke(cli, ["hooks", "run", "stackunderflow-user-prompt", "--capture-content"],
                                 input=json.dumps(payload))
        assert res.exit_code == 0
        import sqlite3
        conn = sqlite3.connect(deps.store_path)
        try:
            stored = conn.execute("SELECT payload_json FROM captured_events").fetchone()[0]
        finally:
            conn.close()
        assert "no, undo that" in stored  # full payload kept


def test_hooks_group_help_lists_subcommands() -> None:
    res = CliRunner().invoke(cli, ["hooks", "--help"])
    assert res.exit_code == 0
    for sub in ("install", "uninstall", "status", "repair", "run"):
        assert sub in res.output
