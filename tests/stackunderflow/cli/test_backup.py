"""Tests for ``stackunderflow backup`` failure exit codes and ``backup verify``.

Covers two behaviours added in the backup-hardening pass:

* ``backup create`` must exit non-zero (not 0) when the underlying rsync
  fails or times out, so wrapper scripts can detect the failure.
* ``backup verify`` confirms the latest (or a named) backup holds all four
  critical artifacts — store.db plus the search / Q&A / tags sidecars — since
  the SQLite store alone is not the full source of truth.
"""

from __future__ import annotations

import subprocess
from pathlib import Path

import pytest
from click.testing import CliRunner

import stackunderflow.cli as cli_mod
from stackunderflow.cli import cli


@pytest.fixture
def runner() -> CliRunner:
    return CliRunner()


@pytest.fixture
def backup_dir(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Path:
    """Point the CLI's backup root at a throwaway tmp dir."""
    d = tmp_path / "backups"
    monkeypatch.setattr(cli_mod, "_BACKUP_DIR", d)
    return d


@pytest.fixture
def claude_dir(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Path:
    """A non-empty fake ~/.claude so ``backup create`` proceeds to rsync."""
    d = tmp_path / "claude"
    d.mkdir()
    (d / "settings.json").write_text("{}")
    monkeypatch.setattr(cli_mod, "_CLAUDE_DIR", d)
    return d


# ── backup create: exit codes ───────────────────────────────────────────────


def test_backup_create_exits_1_on_rsync_failure(
    runner, backup_dir, claude_dir, monkeypatch
):
    def fake_run(cmd, **kwargs):
        return subprocess.CompletedProcess(
            cmd, returncode=23, stdout="", stderr="rsync: link failed"
        )

    monkeypatch.setattr(subprocess, "run", fake_run)
    result = runner.invoke(cli, ["backup", "create"])
    assert result.exit_code == 1, result.output
    assert "rsync error" in result.output


def test_backup_create_exits_1_on_timeout(
    runner, backup_dir, claude_dir, monkeypatch
):
    def fake_run(cmd, **kwargs):
        raise subprocess.TimeoutExpired(cmd, timeout=600)

    monkeypatch.setattr(subprocess, "run", fake_run)
    result = runner.invoke(cli, ["backup", "create"])
    assert result.exit_code == 1, result.output
    assert "timed out" in result.output.lower()


def test_backup_create_succeeds_on_rsync_ok(
    runner, backup_dir, claude_dir, monkeypatch
):
    def fake_run(cmd, **kwargs):
        # cmd[-1] is "<dest>/" — materialize it so the summary stat works.
        dest = Path(cmd[-1].rstrip("/"))
        dest.mkdir(parents=True, exist_ok=True)
        (dest / "settings.json").write_text("{}")
        return subprocess.CompletedProcess(cmd, returncode=0, stdout="", stderr="")

    monkeypatch.setattr(subprocess, "run", fake_run)
    result = runner.invoke(cli, ["backup", "create"])
    assert result.exit_code == 0, result.output
    assert "Done:" in result.output


# ── backup verify ───────────────────────────────────────────────────────────


def _make_backup(backup_dir: Path, name: str, artifacts) -> Path:
    d = backup_dir / name
    d.mkdir(parents=True)
    for a in artifacts:
        (d / a).write_text("x")
    return d


def test_backup_verify_passes_when_all_present(runner, backup_dir):
    _make_backup(backup_dir, "20260101-000000", cli_mod._CRITICAL_ARTIFACTS)
    result = runner.invoke(cli, ["backup", "verify"])
    assert result.exit_code == 0, result.output
    assert "all 4 critical artifacts present" in result.output


def test_backup_verify_fails_when_artifact_missing(runner, backup_dir):
    present = [a for a in cli_mod._CRITICAL_ARTIFACTS if a != "tags.json"]
    _make_backup(backup_dir, "20260101-000000", present)
    result = runner.invoke(cli, ["backup", "verify"])
    assert result.exit_code == 1, result.output
    assert "tags.json" in result.output
    assert "MISSING" in result.output


def test_backup_verify_finds_nested_artifacts(runner, backup_dir):
    d = backup_dir / "20260101-000000"
    nested = d / "dot-stackunderflow"
    nested.mkdir(parents=True)
    for a in cli_mod._CRITICAL_ARTIFACTS:
        (nested / a).write_text("x")
    result = runner.invoke(cli, ["backup", "verify"])
    assert result.exit_code == 0, result.output


def test_backup_verify_fails_with_no_backups(runner, backup_dir):
    result = runner.invoke(cli, ["backup", "verify"])
    assert result.exit_code == 1, result.output
    assert "No backups" in result.output


def test_backup_verify_checks_latest_by_default(runner, backup_dir):
    # Older backup complete, newer one incomplete → default verifies the
    # newer (lexicographically last) backup and fails.
    _make_backup(backup_dir, "20260101-000000", cli_mod._CRITICAL_ARTIFACTS)
    _make_backup(backup_dir, "20260201-000000", ["store.db"])
    result = runner.invoke(cli, ["backup", "verify"])
    assert result.exit_code == 1, result.output


def test_backup_verify_named_missing_backup(runner, backup_dir):
    backup_dir.mkdir(parents=True, exist_ok=True)
    result = runner.invoke(cli, ["backup", "verify", "--name", "nope"])
    assert result.exit_code == 1, result.output
    assert "not found" in result.output
