"""CLI tests for ``stackunderflow recommend skills``.

Mirrors ``test_skills_cli.py`` patterns: monkeypatch ``deps.store_path``
to a tmp store, seed a tiny fixture, run via ``CliRunner``. Verifies
exit codes, both output formats, the threshold gate, the
already-installed filter, and graceful handling of an unresolved
project. The real ``~/.stackunderflow/store.db`` is never touched.
"""

from __future__ import annotations

import json
from pathlib import Path

import pytest
from click.testing import CliRunner

import stackunderflow.deps as deps
from stackunderflow.cli import cli
from stackunderflow.store import db, schema

# ── seeding (copied from test_skills_cli.py for isolation) ─────────────────


def _claude_raw(role: str, text: str | None, tool_uses: list) -> dict:
    content: list[dict] = []
    if text:
        content.append({"type": "text", "text": text})
    for i, (name, inp) in enumerate(tool_uses):
        content.append({"type": "tool_use", "id": f"toolu_{i}", "name": name, "input": inp})
    return {"type": role, "uuid": "u", "message": {"role": role, "content": content}}


def _seed_store(
    store_db: Path, *, project_path: str | None = None, n_sessions: int = 7,
    slug: str = "-Users-yad-dev-foo",
) -> str:
    conn = db.connect(store_db)
    schema.apply(conn)
    pid = int(
        conn.execute(
            "INSERT INTO projects (provider, slug, path, display_name, "
            "first_seen, last_modified) VALUES ('claude', ?, ?, 'foo', 0.0, 0.0)",
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
                (sfk, i, f"2026-05-01T00:0{i}:00+00:00", role, text,
                 json.dumps([t[0] for t in tcs]),
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
    # Redirect the cache to the tmp dir so we never touch the real one.
    fake_home = tmp_path / "fake_home"
    fake_home.mkdir()
    monkeypatch.setattr(Path, "home", classmethod(lambda cls: fake_home))
    return tmp_path, store_db, CliRunner()


# ── happy paths ─────────────────────────────────────────────────────────────


def test_recommend_text_output(runner_env):
    tmp_path, store_db, runner = runner_env
    slug = _seed_store(store_db, n_sessions=7)
    r = runner.invoke(cli, ["recommend", "skills", "--project", slug,
                            "--window-days", "365"])
    assert r.exit_code == 0, r.output
    assert "Found" in r.output
    assert "auto-canonical-test-command" in r.output
    assert "accept:" in r.output
    assert "stackunderflow skills generate" in r.output


def test_recommend_json_output(runner_env):
    tmp_path, store_db, runner = runner_env
    slug = _seed_store(store_db, n_sessions=7)
    r = runner.invoke(cli, ["recommend", "skills", "--project", slug,
                            "--window-days", "365", "--format", "json"])
    assert r.exit_code == 0, r.output
    body = json.loads(r.output)
    assert body["project"] == slug
    assert body["threshold"] == 5
    assert body["window_days"] == 365
    assert body["cache_status"] in {"hit", "miss", "bypassed"}
    assert isinstance(body["recommendations"], list)
    assert body["recommendations"]
    rec = body["recommendations"][0]
    assert {"pattern_id", "pattern_kind", "suggested_skill_name", "occurrences",
            "accept_command", "suggested_skill_template"} <= set(rec)


def test_recommend_below_threshold_is_graceful(runner_env):
    tmp_path, store_db, runner = runner_env
    slug = _seed_store(store_db, n_sessions=3)
    r = runner.invoke(cli, ["recommend", "skills", "--project", slug,
                            "--threshold", "5", "--window-days", "365"])
    assert r.exit_code == 0, r.output
    assert "No skill recommendations" in r.output


def test_recommend_threshold_flag_below_threshold(runner_env):
    tmp_path, store_db, runner = runner_env
    slug = _seed_store(store_db, n_sessions=4)
    # threshold 5 — below
    r = runner.invoke(cli, ["recommend", "skills", "--project", slug,
                            "--threshold", "5", "--window-days", "365"])
    assert r.exit_code == 0
    assert "No skill recommendations" in r.output
    # threshold 4 — at the limit, should surface
    r2 = runner.invoke(cli, ["recommend", "skills", "--project", slug,
                             "--threshold", "4", "--window-days", "365",
                             "--no-cache"])
    assert r2.exit_code == 0, r2.output
    assert "Found" in r2.output


def test_recommend_no_project_no_cwd_match(runner_env):
    """No --project + cwd not in store → UsageError."""
    tmp_path, store_db, runner = runner_env
    _seed_store(store_db, n_sessions=5)  # different project_path than cwd
    r = runner.invoke(cli, ["recommend", "skills"])
    assert r.exit_code != 0
    assert "could not infer a project" in r.output


def test_recommend_unknown_project_returns_empty(runner_env):
    tmp_path, store_db, runner = runner_env
    _seed_store(store_db, n_sessions=7)
    r = runner.invoke(cli, ["recommend", "skills", "--project", "-no-such",
                            "--window-days", "365"])
    assert r.exit_code == 0, r.output
    assert "No skill recommendations" in r.output


# ── filter against installed skills (CLI-end smoke) ────────────────────────


def test_recommend_filters_installed_skill(runner_env, tmp_path):
    _, store_db, runner = runner_env
    proj_path = tmp_path / "myproj"
    proj_path.mkdir()
    slug = _seed_store(store_db, n_sessions=7, project_path=str(proj_path),
                       slug="-myproj")

    # First find what pattern would be recommended.
    first = runner.invoke(cli, ["recommend", "skills", "--project", slug,
                                "--window-days", "365", "--format", "json"])
    body = json.loads(first.output)
    pattern_id = body["recommendations"][0]["pattern_id"]

    # Install a fake auto-skill with that pattern_id in the project dir.
    skills_dir = proj_path / ".claude" / "skills" / "auto-canonical-test-command"
    skills_dir.mkdir(parents=True)
    (skills_dir / "SKILL.md").write_text(
        "---\nname: auto-canonical-test-command\n"
        "description: existing skill description here\n"
        "auto_generated: true\n"
        f"pattern_id: {pattern_id}\n---\n\n# x\n\nbody\n",
        encoding="utf-8",
    )

    second = runner.invoke(cli, ["recommend", "skills", "--project", slug,
                                 "--window-days", "365", "--no-cache",
                                 "--format", "json"])
    body2 = json.loads(second.output)
    assert all(r["pattern_id"] != pattern_id for r in body2["recommendations"])
    assert body2["filtered_already_installed"] >= 1
