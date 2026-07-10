"""CLI tests for ``stackunderflow init --install-skills``.

Verifies the install helper + CLI flag wiring without ever touching the
real ``~/.claude/skills/`` directory. Every test points the install at a
``tmp_path``-scoped destination via either the public ``--skills-dest``
flag or by calling the underlying ``_install_static_skills`` helper
directly. We monkeypatch ``start_cmd`` to a no-op so the test process
doesn't try to bind a port and hang on the blocking server.
"""

from __future__ import annotations

from pathlib import Path

import pytest
from click.testing import CliRunner

import stackunderflow.cli as cli_module
from stackunderflow.cli import (
    _install_static_skills,
    _shipped_skills_source_dir,
    cli,
)

# Independently re-derived from the packaged tree (the installer discovers
# skills the same way) — adding a skill folder updates this expectation
# automatically; no hand-list to drift.
_SHIPPED_SKILLS: tuple[str, ...] = tuple(sorted(
    d.name for d in _shipped_skills_source_dir().iterdir()
    if d.is_dir() and (d / "SKILL.md").is_file()
))


@pytest.fixture
def stub_start(monkeypatch):
    """Make ``init_cmd`` cheap to invoke under ``CliRunner``.

    ``init_cmd`` ends with ``ctx.invoke(start_cmd, ...)`` which would
    launch a real HTTP server. We replace ``start_cmd``'s body with a
    no-op echo so the CLI flow under test is just the install-skills
    block + the ``start_cmd`` boundary, no server.
    """
    def _stub(*args, **kwargs):
        # no-op; CliRunner just verifies the install-skills block ran
        return None

    # Replace the callback (``Command.callback``) rather than reassigning
    # the module-level binding, since ``cli`` already holds a reference
    # to the registered click command.
    monkeypatch.setattr(cli_module.start_cmd, "callback", _stub)


# ── _install_static_skills helper (direct, no CLI) ──────────────────────────


def test_helper_installs_all_three_skills_to_empty_dest(tmp_path: Path) -> None:
    dest = tmp_path / "skills"
    report = _install_static_skills(dest)
    assert report["created"] == list(_SHIPPED_SKILLS)
    assert report["unchanged"] == []
    assert report["overwritten"] == []
    assert report["skipped_modified"] == []
    assert report["missing_source"] == []

    for name in _SHIPPED_SKILLS:
        skill_md = dest / name / "SKILL.md"
        assert skill_md.is_file(), f"missing {skill_md}"
        # parses as readable utf-8 with the SKILL.md frontmatter shape
        text = skill_md.read_text(encoding="utf-8")
        assert text.startswith("---"), f"{skill_md} doesn't start with frontmatter delimiter"


def test_helper_is_idempotent(tmp_path: Path) -> None:
    """Re-running on a byte-identical dest emits ``unchanged``, never rewrites."""
    dest = tmp_path / "skills"
    _install_static_skills(dest)

    # snapshot mtimes + bytes before the re-run
    before = {
        name: (
            (dest / name / "SKILL.md").stat().st_mtime_ns,
            (dest / name / "SKILL.md").read_bytes(),
        )
        for name in _SHIPPED_SKILLS
    }

    report = _install_static_skills(dest)
    assert report["created"] == []
    assert report["unchanged"] == list(_SHIPPED_SKILLS)
    assert report["overwritten"] == []
    assert report["skipped_modified"] == []

    after = {
        name: (
            (dest / name / "SKILL.md").stat().st_mtime_ns,
            (dest / name / "SKILL.md").read_bytes(),
        )
        for name in _SHIPPED_SKILLS
    }
    # bytes unchanged (the strict idempotency contract)
    for name in _SHIPPED_SKILLS:
        assert before[name][1] == after[name][1], f"{name} bytes changed across idempotent re-run"


def test_helper_skips_modified_dest_without_force(tmp_path: Path) -> None:
    dest = tmp_path / "skills"
    _install_static_skills(dest)

    target = dest / _SHIPPED_SKILLS[0] / "SKILL.md"
    target.write_text("# local edit\n", encoding="utf-8")
    edit_bytes = target.read_bytes()

    report = _install_static_skills(dest, force=False)
    assert _SHIPPED_SKILLS[0] in report["skipped_modified"]
    # the local edit survives — we did not clobber it
    assert target.read_bytes() == edit_bytes


def test_helper_overwrites_modified_dest_with_force(tmp_path: Path) -> None:
    dest = tmp_path / "skills"
    _install_static_skills(dest)

    target = dest / _SHIPPED_SKILLS[0] / "SKILL.md"
    target.write_text("# local edit\n", encoding="utf-8")

    report = _install_static_skills(dest, force=True)
    assert _SHIPPED_SKILLS[0] in report["overwritten"]
    # target now matches the shipped source byte-for-byte
    from stackunderflow.cli import _shipped_skills_source_dir
    src_bytes = (_shipped_skills_source_dir() / _SHIPPED_SKILLS[0] / "SKILL.md").read_bytes()
    assert target.read_bytes() == src_bytes


def test_helper_creates_nested_dest_dirs(tmp_path: Path) -> None:
    dest = tmp_path / "a" / "b" / "c" / "skills"
    assert not dest.exists()
    _install_static_skills(dest)
    assert dest.is_dir()
    for name in _SHIPPED_SKILLS:
        assert (dest / name / "SKILL.md").is_file()


# ── CLI flag wiring (--install-skills + --skills-dest + --skills-force) ─────


def test_cli_install_skills_writes_files(tmp_path: Path, stub_start) -> None:
    runner = CliRunner()
    dest = tmp_path / "skills"

    r = runner.invoke(
        cli,
        ["init", "--install-skills", "--skills-dest", str(dest), "--no-browser"],
    )
    assert r.exit_code == 0, r.output

    # All three skills land
    for name in _SHIPPED_SKILLS:
        assert (dest / name / "SKILL.md").is_file()
        # User-facing summary mentions each install
        assert name in r.output

    # CLI signals "created" per skill on a cold install
    assert r.output.count("installed skill:") == len(_SHIPPED_SKILLS)


def test_cli_install_skills_idempotent_quiet(tmp_path: Path, stub_start) -> None:
    runner = CliRunner()
    dest = tmp_path / "skills"

    runner.invoke(cli, ["init", "--install-skills", "--skills-dest", str(dest), "--no-browser"])

    # Snapshot bytes
    snapshot = {
        name: (dest / name / "SKILL.md").read_bytes()
        for name in _SHIPPED_SKILLS
    }

    r2 = runner.invoke(
        cli,
        ["init", "--install-skills", "--skills-dest", str(dest), "--no-browser"],
    )
    assert r2.exit_code == 0, r2.output

    # No "installed" or "overwrote" messages on a clean re-run
    assert "installed skill:" not in r2.output
    assert "overwrote skill" not in r2.output
    # Warnings are echoed to stderr (which CliRunner mixes into output by
    # default unless ``mix_stderr=False``); confirm none fired.
    assert "⚠" not in r2.output

    # And bytes are unchanged
    for name in _SHIPPED_SKILLS:
        assert (dest / name / "SKILL.md").read_bytes() == snapshot[name]


def test_cli_install_skills_warns_on_modified_no_force(tmp_path: Path, stub_start) -> None:
    runner = CliRunner()
    dest = tmp_path / "skills"

    runner.invoke(cli, ["init", "--install-skills", "--skills-dest", str(dest), "--no-browser"])

    # User edits one skill locally
    target = dest / _SHIPPED_SKILLS[0] / "SKILL.md"
    target.write_text("# local edit by the user\n", encoding="utf-8")
    edit_bytes = target.read_bytes()

    r = runner.invoke(
        cli,
        ["init", "--install-skills", "--skills-dest", str(dest), "--no-browser"],
    )
    assert r.exit_code == 0, r.output
    # Warning fires for the modified skill, not for the other two
    assert f"skill {_SHIPPED_SKILLS[0]} differs" in r.output
    # The local edit survives
    assert target.read_bytes() == edit_bytes


def test_cli_install_skills_force_overwrites(tmp_path: Path, stub_start) -> None:
    runner = CliRunner()
    dest = tmp_path / "skills"

    runner.invoke(cli, ["init", "--install-skills", "--skills-dest", str(dest), "--no-browser"])

    target = dest / _SHIPPED_SKILLS[0] / "SKILL.md"
    target.write_text("# local edit\n", encoding="utf-8")

    r = runner.invoke(
        cli,
        [
            "init",
            "--install-skills",
            "--skills-force",
            "--skills-dest",
            str(dest),
            "--no-browser",
        ],
    )
    assert r.exit_code == 0, r.output
    assert "overwrote skill" in r.output

    # Now the destination matches the shipped source byte-for-byte
    from stackunderflow.cli import _shipped_skills_source_dir
    src_bytes = (_shipped_skills_source_dir() / _SHIPPED_SKILLS[0] / "SKILL.md").read_bytes()
    assert target.read_bytes() == src_bytes


def test_cli_without_install_skills_does_not_touch_dest(tmp_path: Path, stub_start) -> None:
    """Without ``--install-skills``, ``init`` is the existing alias for ``start``."""
    runner = CliRunner()
    dest = tmp_path / "skills"

    r = runner.invoke(cli, ["init", "--no-browser"])
    assert r.exit_code == 0, r.output
    assert not dest.exists()


# ── source-path resolution via importlib.resources ──────────────────────────


def test_shipped_skills_source_dir_resolves_to_real_files() -> None:
    """``importlib.resources`` should give a path containing all 3 SKILL.md files.

    This is the contract that lets the install work from both a
    source-checkout (returns the repo path) and an installed wheel
    (returns the unpacked-wheel path). The test passes in either mode
    because both layouts have the skills as concrete files under
    ``stackunderflow/skills/<name>/SKILL.md``.
    """
    from stackunderflow.cli import _shipped_skills_source_dir

    src_dir = _shipped_skills_source_dir()
    assert src_dir.is_dir(), f"expected directory at {src_dir}"
    for name in _SHIPPED_SKILLS:
        skill_md = src_dir / name / "SKILL.md"
        assert skill_md.is_file(), f"expected {skill_md} to be a real file"
