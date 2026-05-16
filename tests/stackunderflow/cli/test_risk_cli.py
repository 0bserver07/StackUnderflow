"""CLI tests for ``stackunderflow risk file <path>``.

Spec 16. Mirrors the pattern in ``test_discovery_cli.py``: monkeypatch
``deps.store_path`` to a tmp store, build a tiny fixture, run the
command via ``CliRunner``. We verify exit codes, both formats, and the
empty-result edge case.
"""

from __future__ import annotations

import json
from pathlib import Path

from click.testing import CliRunner

import stackunderflow.deps as deps
from stackunderflow.cli import cli
from stackunderflow.store import db, schema

# ── seeding helpers ─────────────────────────────────────────────────────────


def _edit_blob(file_path: str = "/x/cost.py") -> str:
    return json.dumps([{"name": "Edit", "input": {"file_path": file_path}}])


def _seed_failing_session(store_db: Path, *, session_id: str = "fail-1") -> None:
    """Seed a project + one session whose edit on /x/cost.py was complained about."""
    conn = db.connect(store_db)
    schema.apply(conn)
    pcur = conn.execute(
        "INSERT INTO projects (provider, slug, path, display_name, "
        " first_seen, last_modified) VALUES "
        "('claude', '-Users-yad-dev-foo', NULL, 'foo', 0.0, 0.0)"
    )
    pid = int(pcur.lastrowid)
    sfk_cur = conn.execute(
        "INSERT INTO sessions (project_id, session_id, first_ts, last_ts, "
        " message_count) VALUES (?, ?, ?, ?, 2)",
        (pid, session_id, "2026-04-01T00:00:00+00:00",
         "2026-04-01T00:00:00+00:00"),
    )
    sfk = int(sfk_cur.lastrowid)
    for seq, (role, content_text, tools_json) in enumerate([
        ("assistant", "", _edit_blob("/x/cost.py")),
        ("user", "no, that broke the cost endpoint", "[]"),
    ]):
        conn.execute(
            "INSERT INTO messages (session_fk, seq, timestamp, role, model, "
            " input_tokens, output_tokens, cache_create_tokens, "
            " cache_read_tokens, content_text, tools_json, raw_json, "
            " is_sidechain) VALUES "
            "(?, ?, '2026-04-01T00:00:00+00:00', ?, 'claude-sonnet-4-5', "
            " 0, 0, 0, 0, ?, ?, '{}', 0)",
            (sfk, seq, role, content_text, tools_json),
        )
    conn.commit()
    conn.close()


def _seed_empty(store_db: Path) -> None:
    conn = db.connect(store_db)
    schema.apply(conn)
    conn.close()


def _invoke(runner: CliRunner, args: list[str], store_db: Path, monkeypatch):
    monkeypatch.setattr(deps, "store_path", store_db)
    return runner.invoke(cli, args)


# ── happy path ──────────────────────────────────────────────────────────────


class TestRiskFileText:
    def test_text_output_lists_buckets(self, tmp_path, monkeypatch):
        store_db = tmp_path / "store.db"
        _seed_failing_session(store_db)
        runner = CliRunner()
        r = _invoke(
            runner, ["risk", "file", "/x/cost.py"], store_db, monkeypatch,
        )
        assert r.exit_code == 0, r.output
        assert "File risk for /x/cost.py" in r.output
        assert "reverted:" in r.output
        assert "failed:" in r.output
        assert "worked:" in r.output
        # The seeded session is a failure ⇒ it shows up in the recent list.
        assert "fail-1" in r.output


class TestRiskFileJson:
    def test_json_output_locks_keys(self, tmp_path, monkeypatch):
        store_db = tmp_path / "store.db"
        _seed_failing_session(store_db)
        runner = CliRunner()
        r = _invoke(
            runner,
            ["risk", "file", "/x/cost.py", "--format", "json"],
            store_db, monkeypatch,
        )
        assert r.exit_code == 0, r.output
        body = json.loads(r.output)
        assert set(body) == {
            "path", "since", "total_sessions",
            "reverted", "failed", "worked", "recent_session_ids",
        }
        assert body["failed"] == 1
        assert body["recent_session_ids"] == ["fail-1"]

    def test_empty_store_emits_zeros(self, tmp_path, monkeypatch):
        store_db = tmp_path / "store.db"
        _seed_empty(store_db)
        runner = CliRunner()
        r = _invoke(
            runner,
            ["risk", "file", "/never/touched.py", "--format", "json"],
            store_db, monkeypatch,
        )
        assert r.exit_code == 0, r.output
        body = json.loads(r.output)
        assert body["total_sessions"] == 0
        assert body["reverted"] == body["failed"] == body["worked"] == 0
        assert body["recent_session_ids"] == []

    def test_invalid_since_is_user_error(self, tmp_path, monkeypatch):
        store_db = tmp_path / "store.db"
        _seed_failing_session(store_db)
        runner = CliRunner()
        r = _invoke(
            runner,
            ["risk", "file", "/x/cost.py", "--since", "yesterday"],
            store_db, monkeypatch,
        )
        assert r.exit_code != 0
        assert "since" in r.output.lower()
