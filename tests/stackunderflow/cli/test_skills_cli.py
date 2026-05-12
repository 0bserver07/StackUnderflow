"""CLI tests for ``stackunderflow skills generate / list / clean``.

Mirrors ``test_discovery_cli.py``: monkeypatch ``deps.store_path`` to a tmp
store, seed a tiny fixture, run via ``CliRunner``. Verifies exit codes,
both output formats, ``--dry-run`` shape, the ``--scope`` boundary, ``--out``
directory creation, idempotent re-run, and the ``clean`` safety guard. The
real ``~/.stackunderflow/store.db`` is never touched.
"""

from __future__ import annotations

import json
from pathlib import Path

import pytest
from click.testing import CliRunner

import stackunderflow.deps as deps
from stackunderflow.cli import cli
from stackunderflow.store import db, schema

# ── seeding ─────────────────────────────────────────────────────────────────


def _claude_raw(role: str, text: str | None, tool_uses: list[tuple[str, dict]]) -> dict:
    content: list[dict] = []
    if text:
        content.append({"type": "text", "text": text})
    for i, (name, inp) in enumerate(tool_uses):
        content.append({"type": "tool_use", "id": f"toolu_{i}", "name": name, "input": inp})
    return {"type": role, "uuid": "u", "message": {"role": role, "content": content}}


def _seed_store(store_db: Path, *, project_path: str | None = None, n_sessions: int = 7) -> str:
    """Seed a store with one project and ``n_sessions`` edit→pytest sessions.

    Returns the project slug.
    """
    slug = "-Users-yad-dev-foo"
    conn = db.connect(store_db)
    schema.apply(conn)
    pid = int(
        conn.execute(
            "INSERT INTO projects (provider, slug, path, display_name, first_seen, last_modified) "
            "VALUES ('claude', ?, ?, 'foo', 0.0, 0.0)",
            (slug, project_path),
        ).lastrowid
    )
    for k in range(n_sessions):
        sfk = int(
            conn.execute(
                "INSERT INTO sessions (project_id, session_id, first_ts, last_ts, message_count) "
                "VALUES (?, ?, '2026-05-01T00:00:00+00:00', '2026-05-01T01:00:00+00:00', 3)",
                (pid, f"s-{k}"),
            ).lastrowid
        )
        edit_args = {"file_path": "/Users/yad/dev/foo/pkg/m.py", "old_string": "a", "new_string": "b"}
        turns = [
            ("user", "add a feature", []),
            ("assistant", "editing", [("Edit", edit_args)]),
            ("assistant", "running tests", [("Bash", {"command": "pytest tests/ -q"})]),
        ]
        for i, (role, text, tcs) in enumerate(turns):
            conn.execute(
                "INSERT INTO messages (session_fk, seq, timestamp, role, model, "
                " input_tokens, output_tokens, cache_create_tokens, cache_read_tokens, "
                " content_text, tools_json, raw_json, is_sidechain) "
                "VALUES (?, ?, ?, ?, 'claude-sonnet-4-5', 0, 0, 0, 0, ?, ?, ?, 0)",
                (sfk, i, f"2026-05-01T00:0{i}:00+00:00", role, text, json.dumps([t[0] for t in tcs]),
                 json.dumps(_claude_raw(role, text, tcs))),
            )
    conn.commit()
    conn.close()
    return slug


def _seed_empty(store_db: Path) -> None:
    conn = db.connect(store_db)
    schema.apply(conn)
    conn.close()


@pytest.fixture
def runner_env(tmp_path, monkeypatch):
    store_db = tmp_path / "store.db"
    monkeypatch.setattr(deps, "store_path", store_db)
    return tmp_path, store_db, CliRunner()


# ── generate ────────────────────────────────────────────────────────────────


def test_generate_json_writes_skill(runner_env):
    tmp_path, store_db, runner = runner_env
    slug = _seed_store(store_db, n_sessions=7)
    out_dir = tmp_path / ".claude" / "skills"
    r = runner.invoke(cli, ["skills", "generate", "--project", slug, "--window", "all",
                            "--out", str(out_dir), "--format", "json"])
    assert r.exit_code == 0, r.output
    body = json.loads(r.output)
    assert body["out_dir"] == str(out_dir)
    assert body["dry_run"] is False
    names = {c["name"] for c in body["candidates"]}
    assert "auto-canonical-test-command" in names
    assert any(w["action"] == "created" for w in body["written"])
    assert (out_dir / "auto-canonical-test-command" / "SKILL.md").is_file()


def test_generate_text_and_dry_run(runner_env):
    tmp_path, store_db, runner = runner_env
    slug = _seed_store(store_db, n_sessions=6)
    out_dir = tmp_path / ".claude" / "skills"
    r = runner.invoke(cli, ["skills", "generate", "--project", slug, "--window", "all",
                            "--out", str(out_dir), "--dry-run"])
    assert r.exit_code == 0, r.output
    assert "Would generate" in r.output
    assert "would-create" in r.output
    assert "(dry run" in r.output
    assert not out_dir.exists()  # nothing written


def test_generate_creates_out_dir(runner_env):
    tmp_path, store_db, runner = runner_env
    slug = _seed_store(store_db, n_sessions=6)
    out_dir = tmp_path / "a" / "b" / "c" / "skills"
    assert not out_dir.exists()
    r = runner.invoke(cli, ["skills", "generate", "--project", slug, "--window", "all", "--out", str(out_dir)])
    assert r.exit_code == 0, r.output
    assert out_dir.is_dir()


def test_generate_idempotent_rerun(runner_env):
    tmp_path, store_db, runner = runner_env
    slug = _seed_store(store_db, n_sessions=6)
    out_dir = tmp_path / ".claude" / "skills"
    runner.invoke(cli, ["skills", "generate", "--project", slug, "--window", "all", "--out", str(out_dir)])
    r2 = runner.invoke(cli, ["skills", "generate", "--project", slug, "--window", "all",
                             "--out", str(out_dir), "--format", "json"])
    assert r2.exit_code == 0, r2.output
    body = json.loads(r2.output)
    assert all(w["action"] == "unchanged" for w in body["written"])
    # exactly one skill dir, no duplicates
    assert sorted(p.name for p in out_dir.iterdir()) == ["auto-canonical-test-command"]


def test_generate_no_patterns_is_graceful(runner_env):
    tmp_path, store_db, runner = runner_env
    slug = _seed_store(store_db, n_sessions=2)  # below default min-occurrences
    out_dir = tmp_path / ".claude" / "skills"
    r = runner.invoke(cli, ["skills", "generate", "--project", slug, "--window", "all", "--out", str(out_dir)])
    assert r.exit_code == 0, r.output
    assert "nothing generated" in r.output.lower()
    assert not out_dir.exists()


def test_generate_scope_user_requires_explicit_projects(runner_env):
    tmp_path, store_db, runner = runner_env
    _seed_empty(store_db)
    r = runner.invoke(cli, ["skills", "generate", "--scope", "user"])
    assert r.exit_code != 0
    assert "no implicit all-projects" in r.output


def test_generate_unknown_project_yields_no_candidates(runner_env):
    tmp_path, store_db, runner = runner_env
    _seed_store(store_db, n_sessions=7)
    out_dir = tmp_path / ".claude" / "skills"
    r = runner.invoke(cli, ["skills", "generate", "--project", "-no-such-project",
                            "--window", "all", "--out", str(out_dir)])
    assert r.exit_code == 0, r.output
    assert "nothing generated" in r.output.lower()


def test_generate_autodetects_cwd_project(tmp_path, monkeypatch):
    store_db = tmp_path / "store.db"
    monkeypatch.setattr(deps, "store_path", store_db)
    runner = CliRunner()
    with runner.isolated_filesystem(temp_dir=tmp_path) as fs:
        cwd = str(Path(fs).resolve())
        _seed_store(store_db, project_path=cwd, n_sessions=6)
        out_dir = Path(fs) / ".claude" / "skills"
        r = runner.invoke(cli, ["skills", "generate", "--window", "all", "--out", str(out_dir)])
        assert r.exit_code == 0, r.output
        assert "Generated" in r.output
        assert (out_dir / "auto-canonical-test-command" / "SKILL.md").is_file()


def test_generate_kind_filter(runner_env):
    tmp_path, store_db, runner = runner_env
    slug = _seed_store(store_db, n_sessions=7)
    out_dir = tmp_path / ".claude" / "skills"
    # restrict to a kind the fixture data doesn't exhibit -> nothing
    r = runner.invoke(cli, ["skills", "generate", "--project", slug, "--window", "all",
                            "--out", str(out_dir), "--kind", "avoids-X"])
    assert r.exit_code == 0, r.output
    assert "nothing generated" in r.output.lower()


# ── list ────────────────────────────────────────────────────────────────────


def test_list_text_and_json(runner_env):
    tmp_path, store_db, runner = runner_env
    slug = _seed_store(store_db, n_sessions=6)
    out_dir = tmp_path / ".claude" / "skills"
    runner.invoke(cli, ["skills", "generate", "--project", slug, "--window", "all", "--out", str(out_dir)])
    r = runner.invoke(cli, ["skills", "list", "--out", str(out_dir)])
    assert r.exit_code == 0, r.output
    assert "auto-canonical-test-command" in r.output
    r2 = runner.invoke(cli, ["skills", "list", "--out", str(out_dir), "--format", "json"])
    assert r2.exit_code == 0, r2.output
    body = json.loads(r2.output)
    assert [s["name"] for s in body["skills"]] == ["auto-canonical-test-command"]


def test_list_empty_dir(runner_env):
    tmp_path, store_db, runner = runner_env
    _seed_empty(store_db)
    r = runner.invoke(cli, ["skills", "list", "--out", str(tmp_path / "nope")])
    assert r.exit_code == 0, r.output
    assert "No auto-generated skills" in r.output


# ── clean ───────────────────────────────────────────────────────────────────


def test_clean_previews_then_deletes_with_yes(runner_env):
    tmp_path, store_db, runner = runner_env
    slug = _seed_store(store_db, n_sessions=6)
    out_dir = tmp_path / ".claude" / "skills"
    runner.invoke(cli, ["skills", "generate", "--project", slug, "--window", "all", "--out", str(out_dir)])
    # also drop a hand-authored skill that must be left alone
    (out_dir / "my-skill").mkdir()
    (out_dir / "my-skill" / "SKILL.md").write_text(
        "---\nname: my-skill\ndescription: a hand authored skill, hands off please\n---\n\n# x\n\nbody\n"
    )
    # no --yes -> preview only
    r = runner.invoke(cli, ["skills", "clean", "--out", str(out_dir)])
    assert r.exit_code == 0, r.output
    assert "Would remove" in r.output
    assert "re-run with --yes" in r.output
    assert (out_dir / "auto-canonical-test-command").exists()
    # --yes -> deletes the auto one, keeps the hand-authored one
    r2 = runner.invoke(cli, ["skills", "clean", "--out", str(out_dir), "--yes"])
    assert r2.exit_code == 0, r2.output
    assert "Removed" in r2.output
    assert not (out_dir / "auto-canonical-test-command").exists()
    assert (out_dir / "my-skill").exists()


def test_clean_nothing_to_do(runner_env):
    tmp_path, store_db, runner = runner_env
    _seed_empty(store_db)
    out_dir = tmp_path / ".claude" / "skills"
    out_dir.mkdir(parents=True)
    r = runner.invoke(cli, ["skills", "clean", "--out", str(out_dir), "--yes"])
    assert r.exit_code == 0, r.output
    assert "No auto-generated skills to remove" in r.output


def test_clean_bad_older_than(runner_env):
    tmp_path, store_db, runner = runner_env
    slug = _seed_store(store_db, n_sessions=6)
    out_dir = tmp_path / ".claude" / "skills"
    runner.invoke(cli, ["skills", "generate", "--project", slug, "--window", "all", "--out", str(out_dir)])
    r = runner.invoke(cli, ["skills", "clean", "--out", str(out_dir), "--older-than", "yesterday", "--yes"])
    assert r.exit_code != 0
    assert "--older-than" in r.output
