"""CLI tests for the three discovery commands.

Mirrors the pattern in ``test_etl_status.py``: monkeypatch
``deps.store_path`` to a tmp store, build a tiny fixture, run the
command via ``CliRunner``. We verify exit codes, both formats, and
the empty-result edge case (must succeed and emit ``{"sessions":
[]}`` rather than crash).
"""

from __future__ import annotations

import json
from pathlib import Path

import pytest
from click.testing import CliRunner

import stackunderflow.deps as deps
from stackunderflow.cli import cli
from stackunderflow.store import db, schema

# ── seeding helpers ─────────────────────────────────────────────────────────


def _seed_empty(store_db: Path) -> None:
    conn = db.connect(store_db)
    schema.apply(conn)
    conn.close()


def _seed_with_data(store_db: Path) -> None:
    """Seed a minimal store with one project, two sessions, three messages."""
    conn = db.connect(store_db)
    schema.apply(conn)
    cur = conn.execute(
        "INSERT INTO projects (provider, slug, path, display_name, "
        " first_seen, last_modified) VALUES "
        "('claude', '-Users-yad-dev-foo', NULL, 'foo', 0.0, 0.0)"
    )
    pid = int(cur.lastrowid)

    cur_a = conn.execute(
        "INSERT INTO sessions (project_id, session_id, first_ts, last_ts, "
        " message_count) VALUES (?, 's-old', "
        "'2026-04-01T00:00:00+00:00', '2026-04-01T00:00:00+00:00', 1)",
        (pid,),
    )
    sfk_a = int(cur_a.lastrowid)
    cur_b = conn.execute(
        "INSERT INTO sessions (project_id, session_id, first_ts, last_ts, "
        " message_count) VALUES (?, 's-new', "
        "'2026-05-01T00:00:00+00:00', '2026-05-01T00:00:00+00:00', 2)",
        (pid,),
    )
    sfk_b = int(cur_b.lastrowid)

    # Message in old session: free-form mention of /etc/passwd.
    conn.execute(
        "INSERT INTO messages (session_fk, seq, timestamp, role, model, "
        " input_tokens, output_tokens, cache_create_tokens, "
        " cache_read_tokens, content_text, tools_json, raw_json, "
        " is_sidechain) VALUES "
        "(?, 0, '2026-04-01T00:00:00+00:00', 'assistant', "
        " 'claude-sonnet-4-5', 0, 0, 0, 0, "
        " 'looked at /etc/passwd briefly', '[]', '{}', 0)",
        (sfk_a,),
    )
    # Message in new session: Read tool on /Users/yad/dev/foo/main.py.
    tools = json.dumps([{
        "name": "Read",
        "input": {"file_path": "/Users/yad/dev/foo/main.py"},
    }])
    conn.execute(
        "INSERT INTO messages (session_fk, seq, timestamp, role, model, "
        " input_tokens, output_tokens, cache_create_tokens, "
        " cache_read_tokens, content_text, tools_json, raw_json, "
        " is_sidechain) VALUES "
        "(?, 0, '2026-05-01T00:00:00+00:00', 'assistant', "
        " 'claude-sonnet-4-5', 0, 0, 0, 0, "
        " 'we decided to use sqlite', ?, '{}', 0)",
        (sfk_b, tools),
    )
    # Second message in new session: a search-target keyword.
    conn.execute(
        "INSERT INTO messages (session_fk, seq, timestamp, role, model, "
        " input_tokens, output_tokens, cache_create_tokens, "
        " cache_read_tokens, content_text, tools_json, raw_json, "
        " is_sidechain) VALUES "
        "(?, 1, '2026-05-01T00:01:00+00:00', 'user', "
        " 'claude-sonnet-4-5', 0, 0, 0, 0, "
        " 'how about postgres?', '[]', '{}', 0)",
        (sfk_b,),
    )
    conn.commit()
    conn.close()


def _invoke(runner: CliRunner, args: list[str], store_db: Path, monkeypatch):
    monkeypatch.setattr(deps, "store_path", store_db)
    return runner.invoke(cli, args)


# ── find-sessions-in-path ───────────────────────────────────────────────────


class TestFindSessionsInPath:
    def test_text_format_lists_session(self, tmp_path, monkeypatch):
        store_db = tmp_path / "store.db"
        _seed_with_data(store_db)
        runner = CliRunner()
        r = _invoke(
            runner,
            ["find-sessions-in-path", "/Users/yad/dev/foo/src"],
            store_db, monkeypatch,
        )
        assert r.exit_code == 0, r.output
        assert "s-old" in r.output or "s-new" in r.output
        assert "-Users-yad-dev-foo" in r.output

    def test_json_format_returns_sessions_list(self, tmp_path, monkeypatch):
        store_db = tmp_path / "store.db"
        _seed_with_data(store_db)
        runner = CliRunner()
        r = _invoke(
            runner,
            ["find-sessions-in-path", "/Users/yad/dev/foo", "--format", "json"],
            store_db, monkeypatch,
        )
        assert r.exit_code == 0, r.output
        body = json.loads(r.output)
        assert "sessions" in body
        assert isinstance(body["sessions"], list)
        assert len(body["sessions"]) == 2
        sids = {s["session_id"] for s in body["sessions"]}
        assert sids == {"s-old", "s-new"}
        # All required fields present.
        for s in body["sessions"]:
            assert set(s.keys()) >= {
                "session_id", "project_slug", "project_path",
                "provider", "first_ts", "last_ts",
                "message_count", "cost_usd", "snippet",
            }
            assert s["snippet"] is None

    def test_no_match_returns_empty_sessions(self, tmp_path, monkeypatch):
        store_db = tmp_path / "store.db"
        _seed_with_data(store_db)
        runner = CliRunner()
        r = _invoke(
            runner,
            ["find-sessions-in-path", "/somewhere/else", "--format", "json"],
            store_db, monkeypatch,
        )
        assert r.exit_code == 0, r.output
        assert json.loads(r.output) == {"sessions": []}

    def test_provider_filter(self, tmp_path, monkeypatch):
        store_db = tmp_path / "store.db"
        _seed_with_data(store_db)
        runner = CliRunner()
        r = _invoke(
            runner,
            ["find-sessions-in-path", "/Users/yad/dev/foo",
             "--provider", "codex", "--format", "json"],
            store_db, monkeypatch,
        )
        assert r.exit_code == 0, r.output
        assert json.loads(r.output) == {"sessions": []}

    def test_invalid_since_is_a_user_error(self, tmp_path, monkeypatch):
        store_db = tmp_path / "store.db"
        _seed_with_data(store_db)
        runner = CliRunner()
        r = _invoke(
            runner,
            ["find-sessions-in-path", "/Users/yad/dev/foo",
             "--since", "yesterday"],
            store_db, monkeypatch,
        )
        assert r.exit_code != 0
        assert "since" in r.output.lower() or "Invalid" in r.output

    def test_limit_clamps_count(self, tmp_path, monkeypatch):
        store_db = tmp_path / "store.db"
        _seed_with_data(store_db)
        runner = CliRunner()
        r = _invoke(
            runner,
            ["find-sessions-in-path", "/Users/yad/dev/foo",
             "--limit", "1", "--format", "json"],
            store_db, monkeypatch,
        )
        assert r.exit_code == 0, r.output
        body = json.loads(r.output)
        assert len(body["sessions"]) == 1


# ── find-sessions-touching-file ─────────────────────────────────────────────


class TestFindSessionsTouchingFile:
    def test_default_any_mode_text(self, tmp_path, monkeypatch):
        store_db = tmp_path / "store.db"
        _seed_with_data(store_db)
        runner = CliRunner()
        r = _invoke(
            runner,
            ["find-sessions-touching-file", "/Users/yad/dev/foo/main.py"],
            store_db, monkeypatch,
        )
        assert r.exit_code == 0, r.output
        assert "s-new" in r.output

    def test_read_mode_json(self, tmp_path, monkeypatch):
        store_db = tmp_path / "store.db"
        _seed_with_data(store_db)
        runner = CliRunner()
        r = _invoke(
            runner,
            ["find-sessions-touching-file", "/Users/yad/dev/foo/main.py",
             "--mode", "read", "--format", "json"],
            store_db, monkeypatch,
        )
        assert r.exit_code == 0, r.output
        body = json.loads(r.output)
        assert len(body["sessions"]) == 1
        assert body["sessions"][0]["session_id"] == "s-new"

    def test_no_match_returns_empty_json(self, tmp_path, monkeypatch):
        store_db = tmp_path / "store.db"
        _seed_with_data(store_db)
        runner = CliRunner()
        r = _invoke(
            runner,
            ["find-sessions-touching-file", "/non/existent/path",
             "--format", "json"],
            store_db, monkeypatch,
        )
        assert r.exit_code == 0, r.output
        assert json.loads(r.output) == {"sessions": []}

    def test_invalid_mode_rejected_by_click(self, tmp_path, monkeypatch):
        store_db = tmp_path / "store.db"
        _seed_with_data(store_db)
        runner = CliRunner()
        r = _invoke(
            runner,
            ["find-sessions-touching-file", "/x", "--mode", "execute"],
            store_db, monkeypatch,
        )
        assert r.exit_code != 0


# ── search-past-decisions ───────────────────────────────────────────────────


class TestSearchPastDecisions:
    def test_finds_decision_with_snippet(self, tmp_path, monkeypatch):
        store_db = tmp_path / "store.db"
        _seed_with_data(store_db)
        runner = CliRunner()
        r = _invoke(
            runner,
            ["search-past-decisions", "use sqlite", "--format", "json"],
            store_db, monkeypatch,
        )
        assert r.exit_code == 0, r.output
        body = json.loads(r.output)
        assert len(body["sessions"]) == 1
        s = body["sessions"][0]
        assert s["session_id"] == "s-new"
        assert s["snippet"] is not None
        assert "use sqlite" in s["snippet"].lower()

    def test_text_format_includes_snippet_line(self, tmp_path, monkeypatch):
        store_db = tmp_path / "store.db"
        _seed_with_data(store_db)
        runner = CliRunner()
        r = _invoke(
            runner,
            ["search-past-decisions", "postgres"],
            store_db, monkeypatch,
        )
        assert r.exit_code == 0, r.output
        assert "postgres" in r.output

    def test_no_match_empty_json(self, tmp_path, monkeypatch):
        store_db = tmp_path / "store.db"
        _seed_with_data(store_db)
        runner = CliRunner()
        r = _invoke(
            runner,
            ["search-past-decisions", "kubernetes-cluster-zoo",
             "--format", "json"],
            store_db, monkeypatch,
        )
        assert r.exit_code == 0, r.output
        assert json.loads(r.output) == {"sessions": []}

    def test_project_filter(self, tmp_path, monkeypatch):
        store_db = tmp_path / "store.db"
        _seed_with_data(store_db)
        runner = CliRunner()
        r = _invoke(
            runner,
            ["search-past-decisions", "sqlite",
             "--project", "-no-such-project", "--format", "json"],
            store_db, monkeypatch,
        )
        assert r.exit_code == 0, r.output
        assert json.loads(r.output) == {"sessions": []}

    def test_empty_query_argument_rejected_by_click(
        self, tmp_path, monkeypatch,
    ):
        # Click requires QUERY positional, but an empty *string* should
        # still parse and our service returns empty.
        store_db = tmp_path / "store.db"
        _seed_with_data(store_db)
        runner = CliRunner()
        r = _invoke(
            runner,
            ["search-past-decisions", "", "--format", "json"],
            store_db, monkeypatch,
        )
        assert r.exit_code == 0, r.output
        assert json.loads(r.output) == {"sessions": []}


# ── empty store ────────────────────────────────────────────────────────────


class TestEmptyStore:
    @pytest.mark.parametrize(
        "args",
        [
            ["find-sessions-in-path", "/Users/yad/dev/foo", "--format", "json"],
            ["find-sessions-touching-file", "/Users/yad/dev/foo/main.py",
             "--format", "json"],
            ["search-past-decisions", "anything", "--format", "json"],
        ],
    )
    def test_each_command_returns_empty_sessions(
        self, tmp_path, monkeypatch, args,
    ):
        store_db = tmp_path / "store.db"
        _seed_empty(store_db)
        runner = CliRunner()
        r = _invoke(runner, args, store_db, monkeypatch)
        assert r.exit_code == 0, r.output
        assert json.loads(r.output) == {"sessions": []}
