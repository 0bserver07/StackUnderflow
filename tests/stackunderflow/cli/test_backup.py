"""Tests for ``stackunderflow backup`` failure exit codes and ``backup verify``.

Covers three behaviours added in the backup-hardening passes:

* ``backup create`` must exit non-zero (not 0) when the underlying rsync
  fails or times out, so wrapper scripts can detect the failure.
* ``backup create`` must NOT fail on rsync 24 / 23 — a live ``~/.claude``
  loses files mid-copy (shell snapshots, todos, session JSONL rotate under
  the mirror) and the generation is still good. Exercised with a real
  subprocess: a stub ``rsync`` on ``PATH``, not a patched ``subprocess.run``.
* ``backup verify`` confirms the latest (or a named) backup holds all four
  critical artifacts — store.db plus the search / Q&A / tags sidecars — since
  the SQLite store alone is not the full source of truth.
"""

from __future__ import annotations

import os
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
    # The old module constant is gone — backup resolves the home per
    # call via CLAUDE_CONFIG_DIR (the fix this exercises for free).
    monkeypatch.setenv("CLAUDE_CONFIG_DIR", str(d))
    return d


@pytest.fixture
def state_dir(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Path:
    """A fake ~/.stackunderflow holding all four critical artifacts.

    ``backup create`` captures these into the backup; pointing the CLI at a
    tmp dir keeps the real state directory untouched.
    """
    import sqlite3

    d = tmp_path / "state"
    d.mkdir()
    for db_name in ("store.db", "search_index.db", "qa_pairs.db"):
        conn = sqlite3.connect(d / db_name)
        conn.execute("CREATE TABLE t (v TEXT)")
        conn.execute("INSERT INTO t VALUES ('marker')")
        conn.commit()
        conn.close()
    (d / "tags.json").write_text("{}")
    monkeypatch.setattr(cli_mod, "_STATE_DIR", d)
    return d


# ── fake rsync on PATH ──────────────────────────────────────────────────────
#
# Exit-code tolerance has to be proven through the real subprocess path: a
# monkeypatched ``subprocess.run`` cannot show that the argv we build actually
# runs, and the codes we care about (24 / 23) only ever arrive from a live
# binary. The stub materializes the destination like rsync would, prints the
# stderr real rsync prints, and exits with ``FAKE_RSYNC_EXIT``.

_FAKE_RSYNC = """#!/bin/sh
for arg in "$@"; do dst="$arg"; done
mkdir -p "$dst"
: > "$dst/settings.json"
echo 'file has vanished: "/h/.claude/shell-snapshots/snapshot-zsh-1.sh"' >&2
echo 'rsync: [sender] link_stat "/h/.claude/todos/t-42.json" failed: No such file or directory (2)' >&2
echo "rsync error: stub exit ${FAKE_RSYNC_EXIT:-0} at main.c(1338) [sender=3.1.3]" >&2
exit ${FAKE_RSYNC_EXIT:-0}
"""


@pytest.fixture
def fake_rsync(tmp_path: Path, monkeypatch: pytest.MonkeyPatch):
    """Put a stub ``rsync`` first on ``PATH``; returns a setter for its code."""
    bindir = tmp_path / "fakebin"
    bindir.mkdir()
    stub = bindir / "rsync"
    stub.write_text(_FAKE_RSYNC)
    stub.chmod(0o755)
    monkeypatch.setenv("PATH", f"{bindir}{os.pathsep}{os.environ['PATH']}")

    def _set_exit(code: int) -> None:
        monkeypatch.setenv("FAKE_RSYNC_EXIT", str(code))

    _set_exit(0)
    return _set_exit


@pytest.fixture
def no_adapters(monkeypatch: pytest.MonkeyPatch):
    """No other agents on this machine — keeps the source pass out of the way."""
    import stackunderflow.adapters as adapters_pkg

    monkeypatch.setattr(adapters_pkg, "registered", lambda: [])


# ── backup create: exit codes ───────────────────────────────────────────────


def test_backup_create_exits_1_on_rsync_failure(
    runner, backup_dir, claude_dir, monkeypatch
):
    def fake_run(cmd, **kwargs):
        # 11 = "error in file IO" — a real failure, not the live-tree race
        # (24 / 23), which is tolerated below.
        return subprocess.CompletedProcess(
            cmd, returncode=11, stdout="", stderr="rsync: link failed"
        )

    monkeypatch.setattr(subprocess, "run", fake_run)
    result = runner.invoke(cli, ["backup", "create"])
    assert result.exit_code == 1, result.output
    assert "rsync error" in result.output


def test_backup_create_survives_vanished_files_exit_24(
    runner, backup_dir, claude_dir, state_dir, fake_rsync, no_adapters
):
    """Exit 24 = source files vanished mid-copy: note it, keep the backup."""
    fake_rsync(24)
    result = runner.invoke(cli, ["backup", "create"])

    assert result.exit_code == 0, result.output
    assert "rsync 24" in result.output
    assert "vanished" in result.output
    # …and everything downstream of the mirror still ran.
    assert "State: captured" in result.output
    assert "Sources:" in result.output
    assert "Done:" in result.output
    backup = next(backup_dir.iterdir())
    assert (backup / "settings.json").is_file()
    for artifact in cli_mod._CRITICAL_ARTIFACTS:
        assert (backup / "stackunderflow-state" / artifact).is_file()


def test_backup_create_survives_partial_transfer_exit_23(
    runner, backup_dir, claude_dir, state_dir, fake_rsync, no_adapters
):
    """Exit 23 = partial transfer: warn, list what rsync reported, keep it."""
    fake_rsync(23)
    result = runner.invoke(cli, ["backup", "create"])

    assert result.exit_code == 0, result.output
    assert "rsync 23" in result.output
    assert "partial transfer" in result.output
    # The warning names the paths rsync could not transfer …
    assert "snapshot-zsh-1.sh" in result.output
    assert "t-42.json" in result.output
    # … and drops rsync's generic trailing recap, which says nothing.
    assert "at main.c(1338)" not in result.output
    assert "State: captured" in result.output
    assert "Done:" in result.output


def test_tolerated_exit_still_prunes_and_verifies(
    runner, backup_dir, claude_dir, state_dir, fake_rsync, no_adapters
):
    """A tolerated exit code must not short-circuit prune, and the backup it
    leaves behind must still pass ``backup verify``."""
    stale = backup_dir / "20200101-000000"
    stale.mkdir(parents=True)
    (stale / "old.txt").write_text("x")

    fake_rsync(24)
    result = runner.invoke(cli, ["backup", "create", "--keep", "1"])

    assert result.exit_code == 0, result.output
    assert "Pruned old backup: 20200101-000000" in result.output
    assert not stale.exists()
    assert [d.name for d in backup_dir.iterdir()] != ["20200101-000000"]

    verify = runner.invoke(cli, ["backup", "verify"])
    assert verify.exit_code == 0, verify.output


def test_backup_create_still_fails_on_untolerated_exit(
    runner, backup_dir, claude_dir, state_dir, fake_rsync, no_adapters
):
    """Only 24 / 23 are forgiven — a real rsync error still exits 1 and the
    half-written destination is removed."""
    fake_rsync(12)  # "error in rsync protocol data stream"
    result = runner.invoke(cli, ["backup", "create"])

    assert result.exit_code == 1, result.output
    assert "rsync error" in result.output
    assert list(backup_dir.iterdir()) == []


def test_rsync_outcome_classification():
    """The helper both CLI paths share, pinned directly."""
    ok, msg = cli_mod._rsync_outcome(0, "", what="x")
    assert (ok, msg) == (True, "")
    ok, msg = cli_mod._rsync_outcome(24, 'file has vanished: "a"\n', what="x")
    assert ok and "rsync 24" in msg and 'file has vanished: "a"' in msg
    ok, msg = cli_mod._rsync_outcome(23, "rsync: link_stat b failed\n", what="x")
    assert ok and "rsync 23" in msg and "link_stat b failed" in msg
    ok, msg = cli_mod._rsync_outcome(23, "", what="x")
    assert ok and "no detail on stderr" in msg
    ok, msg = cli_mod._rsync_outcome(1, "syntax error\n", what="x")
    assert not ok and msg == "syntax error"


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
    runner, backup_dir, claude_dir, state_dir, monkeypatch
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


# ── backup create: state capture (create must satisfy its own verify) ────────


def test_backup_create_captures_state_then_verify_passes(
    runner, backup_dir, claude_dir, state_dir, monkeypatch
):
    """A fresh backup must contain every critical artifact — end to end."""
    import sqlite3

    def fake_run(cmd, **kwargs):
        dest = Path(cmd[-1].rstrip("/"))
        dest.mkdir(parents=True, exist_ok=True)
        return subprocess.CompletedProcess(cmd, returncode=0, stdout="", stderr="")

    monkeypatch.setattr(subprocess, "run", fake_run)
    result = runner.invoke(cli, ["backup", "create"])
    assert result.exit_code == 0, result.output
    assert "State: captured" in result.output

    backup = next(backup_dir.iterdir())
    state = backup / "stackunderflow-state"
    for artifact in cli_mod._CRITICAL_ARTIFACTS:
        assert (state / artifact).is_file(), f"{artifact} not captured"

    # SQLite copies are real, openable snapshots — not torn byte copies.
    conn = sqlite3.connect(state / "store.db")
    assert conn.execute("SELECT v FROM t").fetchone() == ("marker",)
    conn.close()

    verify = runner.invoke(cli, ["backup", "verify"])
    assert verify.exit_code == 0, verify.output


def test_backup_create_warns_when_artifact_missing_and_verify_fails(
    runner, backup_dir, claude_dir, state_dir, monkeypatch
):
    (state_dir / "tags.json").unlink()

    def fake_run(cmd, **kwargs):
        dest = Path(cmd[-1].rstrip("/"))
        dest.mkdir(parents=True, exist_ok=True)
        return subprocess.CompletedProcess(cmd, returncode=0, stdout="", stderr="")

    monkeypatch.setattr(subprocess, "run", fake_run)
    result = runner.invoke(cli, ["backup", "create"])
    assert result.exit_code == 0, result.output  # capture gap warns, not fails
    assert "MISSING tags.json" in result.output

    verify = runner.invoke(cli, ["backup", "verify"])
    assert verify.exit_code == 1, verify.output
    assert "tags.json" in verify.output


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
