"""CLI tests for the ``stackunderflow memory`` namespace (Moves 1 & 2 of
``docs/specs/agent-memory-cli.md``).

Mirrors ``test_discovery_cli.py``: monkeypatch ``deps.store_path`` to a
tmp store, build a synthetic fixture, drive the command with Click's
``CliRunner``. Every test uses ``tmp_path`` — the real
``~/.stackunderflow/store.db`` is never touched.

Coverage: every subcommand in text + json, the envelope shape and
contract, ``--context-budget`` truncation (both the BudgetedResult path
and the pack-here path), the ``--project`` cwd default, and the error
envelope.
"""

from __future__ import annotations

import json
from pathlib import Path

from click.testing import CliRunner

import stackunderflow.deps as deps
from stackunderflow.cli import cli
from stackunderflow.store import db, schema

_CORE_ENVELOPE_FIELDS = {
    "schema", "command", "query", "results",
    "result_count", "token_estimate", "budget", "truncated",
}


# ── seeding helpers ─────────────────────────────────────────────────────────


def _edit_tools(path: str) -> str:
    """An Edit tool call whose args reference ``path``."""
    return json.dumps([{
        "name": "Edit",
        "input": {"file_path": path, "old_string": "a", "new_string": "b"},
    }])


def _add_session(conn, project_id: int, sid: str, day: str, turns: list[tuple]) -> None:
    """Insert one session with ``turns`` = ``(role, content_text, tools_json)``."""
    cur = conn.execute(
        "INSERT INTO sessions (project_id, session_id, first_ts, last_ts, "
        " message_count) VALUES (?, ?, ?, ?, ?)",
        (project_id, sid, f"{day}T00:00:00+00:00", f"{day}T01:00:00+00:00", len(turns)),
    )
    sfk = int(cur.lastrowid)
    for seq, (role, text, tools) in enumerate(turns):
        conn.execute(
            "INSERT INTO messages (session_fk, seq, timestamp, role, model, "
            " input_tokens, output_tokens, cache_create_tokens, "
            " cache_read_tokens, content_text, tools_json, raw_json, "
            " is_sidechain) VALUES (?, ?, ?, ?, NULL, 0, 0, 0, 0, ?, ?, '{}', 0)",
            (sfk, seq, f"{day}T00:{seq:02d}:00+00:00", role, text, tools),
        )


def _seed_empty(store_db: Path) -> None:
    conn = db.connect(store_db)
    schema.apply(conn)
    conn.close()


def _seed_basic(store_db: Path) -> None:
    """One project, three sessions: a decision, a worked edit, a broken edit."""
    conn = db.connect(store_db)
    schema.apply(conn)
    cur = conn.execute(
        "INSERT INTO projects (provider, slug, path, display_name, "
        " first_seen, last_modified) VALUES "
        "('claude', '-Users-yad-dev-foo', NULL, 'foo', 0.0, 0.0)"
    )
    pid = int(cur.lastrowid)
    # A recorded decision worth recalling.
    _add_session(conn, pid, "s-decide", "2026-05-01", [
        ("assistant", "we decided to use sqlite for the store", "[]"),
    ])
    # An Edit the next user turn confirmed.
    _add_session(conn, pid, "s-worked", "2026-05-02", [
        ("assistant", "applied the edit", _edit_tools("/Users/yad/dev/foo/util.py")),
        ("user", "thanks, that worked!", "[]"),
    ])
    # An Edit the next user turn rejected.
    _add_session(conn, pid, "s-broke", "2026-05-03", [
        ("assistant", "applied the edit", _edit_tools("/Users/yad/dev/foo/cost.py")),
        ("user", "no, that broke it", "[]"),
    ])
    conn.commit()
    conn.close()


def _seed_many_in_path(store_db: Path, n: int = 8) -> None:
    """One project under /Users/yad/dev/foo with ``n`` plain sessions."""
    conn = db.connect(store_db)
    schema.apply(conn)
    cur = conn.execute(
        "INSERT INTO projects (provider, slug, path, display_name, "
        " first_seen, last_modified) VALUES "
        "('claude', '-Users-yad-dev-foo', NULL, 'foo', 0.0, 0.0)"
    )
    pid = int(cur.lastrowid)
    for i in range(n):
        conn.execute(
            "INSERT INTO sessions (project_id, session_id, first_ts, last_ts, "
            " message_count) VALUES (?, ?, ?, ?, 1)",
            (pid, f"s-{i:02d}", f"2026-04-{i + 1:02d}T00:00:00+00:00",
             f"2026-04-{i + 1:02d}T00:00:00+00:00"),
        )
    conn.commit()
    conn.close()


def _seed_many_worked(store_db: Path, n: int = 6) -> None:
    """``n`` sessions, each an Edit on util.py the user confirmed worked."""
    conn = db.connect(store_db)
    schema.apply(conn)
    cur = conn.execute(
        "INSERT INTO projects (provider, slug, path, display_name, "
        " first_seen, last_modified) VALUES "
        "('claude', '-Users-yad-dev-foo', NULL, 'foo', 0.0, 0.0)"
    )
    pid = int(cur.lastrowid)
    for i in range(n):
        _add_session(conn, pid, f"w-{i:02d}", f"2026-04-{i + 1:02d}", [
            ("assistant", "applied the edit", _edit_tools("/Users/yad/dev/foo/util.py")),
            ("user", "thanks, that worked!", "[]"),
        ])
    conn.commit()
    conn.close()


def _seed_project_at(store_db: Path, *, slug: str, path: str) -> None:
    """A project whose filesystem path is ``path`` (for the cwd-default test)."""
    conn = db.connect(store_db)
    schema.apply(conn)
    cur = conn.execute(
        "INSERT INTO projects (provider, slug, path, display_name, "
        " first_seen, last_modified) VALUES ('claude', ?, ?, 'p', 0.0, 0.0)",
        (slug, path),
    )
    pid = int(cur.lastrowid)
    _add_session(conn, pid, "s-cwd", "2026-05-05", [
        ("assistant", "we decided to use sqlite right here", "[]"),
    ])
    conn.commit()
    conn.close()


def _invoke(runner: CliRunner, args: list[str], store_db: Path, monkeypatch):
    monkeypatch.setattr(deps, "store_path", store_db)
    return runner.invoke(cli, args)


def _assert_envelope(body: dict, *, command: str) -> None:
    """Every result envelope carries the eight core fields, correctly typed."""
    assert _CORE_ENVELOPE_FIELDS <= set(body.keys())
    assert body["schema"] == "stackunderflow.memory/1"
    assert body["command"] == command
    assert isinstance(body["results"], list)
    assert body["result_count"] == len(body["results"])
    assert isinstance(body["token_estimate"], int)
    assert isinstance(body["budget"], int)
    assert isinstance(body["truncated"], bool)
    assert isinstance(body["query"], dict)


# ── memory decisions ────────────────────────────────────────────────────────


class TestMemoryDecisions:
    def test_json_envelope_shape(self, tmp_path, monkeypatch):
        store_db = tmp_path / "store.db"
        _seed_basic(store_db)
        r = _invoke(CliRunner(), ["memory", "decisions", "sqlite", "--json"],
                    store_db, monkeypatch)
        assert r.exit_code == 0, r.output
        body = json.loads(r.output)
        _assert_envelope(body, command="decisions")
        assert body["result_count"] == 1
        assert body["results"][0]["session_id"] == "s-decide"
        assert body["query"]["text"] == "sqlite"

    def test_format_json_equals_json_shortcut(self, tmp_path, monkeypatch):
        store_db = tmp_path / "store.db"
        _seed_basic(store_db)
        runner = CliRunner()
        a = _invoke(runner, ["memory", "decisions", "sqlite", "--json"],
                    store_db, monkeypatch)
        b = _invoke(runner, ["memory", "decisions", "sqlite", "--format", "json"],
                    store_db, monkeypatch)
        assert a.output == b.output

    def test_text_format(self, tmp_path, monkeypatch):
        store_db = tmp_path / "store.db"
        _seed_basic(store_db)
        r = _invoke(CliRunner(), ["memory", "decisions", "sqlite"],
                    store_db, monkeypatch)
        assert r.exit_code == 0, r.output
        assert "s-decide" in r.output
        assert not r.output.lstrip().startswith("{")

    def test_empty_store_emits_valid_envelope(self, tmp_path, monkeypatch):
        store_db = tmp_path / "store.db"
        _seed_empty(store_db)
        r = _invoke(CliRunner(), ["memory", "decisions", "anything", "--json"],
                    store_db, monkeypatch)
        assert r.exit_code == 0, r.output
        body = json.loads(r.output)
        _assert_envelope(body, command="decisions")
        assert body["results"] == []

    def test_deterministic_output(self, tmp_path, monkeypatch):
        store_db = tmp_path / "store.db"
        _seed_basic(store_db)
        runner = CliRunner()
        a = _invoke(runner, ["memory", "decisions", "sqlite", "--json"],
                    store_db, monkeypatch)
        b = _invoke(runner, ["memory", "decisions", "sqlite", "--json"],
                    store_db, monkeypatch)
        assert a.output == b.output

    def test_bad_since_json_mode_emits_error_envelope(self, tmp_path, monkeypatch):
        store_db = tmp_path / "store.db"
        _seed_basic(store_db)
        r = _invoke(CliRunner(),
                    ["memory", "decisions", "x", "--since", "garbage", "--json"],
                    store_db, monkeypatch)
        assert r.exit_code == 1
        body = json.loads(r.output)
        assert "error" in body
        assert "results" not in body
        assert body["command"] == "decisions"
        assert body["schema"] == "stackunderflow.memory/1"

    def test_bad_since_text_mode_is_click_error(self, tmp_path, monkeypatch):
        store_db = tmp_path / "store.db"
        _seed_basic(store_db)
        r = _invoke(CliRunner(),
                    ["memory", "decisions", "x", "--since", "garbage"],
                    store_db, monkeypatch)
        assert r.exit_code != 0
        assert "since" in r.output.lower()


# ── memory file ─────────────────────────────────────────────────────────────


class TestMemoryFile:
    def test_json_envelope_has_risk_block(self, tmp_path, monkeypatch):
        store_db = tmp_path / "store.db"
        _seed_basic(store_db)
        r = _invoke(CliRunner(),
                    ["memory", "file", "/Users/yad/dev/foo/cost.py", "--json"],
                    store_db, monkeypatch)
        assert r.exit_code == 0, r.output
        body = json.loads(r.output)
        _assert_envelope(body, command="file")
        # `risk` is the documented per-command extra.
        assert "risk" in body
        assert body["risk"]["failed"] == 1
        assert body["risk"]["total_sessions"] >= 1

    def test_results_carry_kind_tags(self, tmp_path, monkeypatch):
        store_db = tmp_path / "store.db"
        _seed_basic(store_db)
        r = _invoke(CliRunner(),
                    ["memory", "file", "/Users/yad/dev/foo/cost.py", "--json"],
                    store_db, monkeypatch)
        body = json.loads(r.output)
        assert body["result_count"] >= 1
        kinds = {row["kind"] for row in body["results"]}
        assert kinds <= {"failure_mode", "touched"}
        # The broken edit surfaces as a failure mode.
        broke = [r for r in body["results"] if r["session_id"] == "s-broke"]
        assert broke and broke[0]["kind"] == "failure_mode"

    def test_text_format(self, tmp_path, monkeypatch):
        store_db = tmp_path / "store.db"
        _seed_basic(store_db)
        r = _invoke(CliRunner(),
                    ["memory", "file", "/Users/yad/dev/foo/cost.py"],
                    store_db, monkeypatch)
        assert r.exit_code == 0, r.output
        assert "risk:" in r.output
        assert "cost.py" in r.output

    def test_unknown_file_still_emits_envelope(self, tmp_path, monkeypatch):
        store_db = tmp_path / "store.db"
        _seed_basic(store_db)
        r = _invoke(CliRunner(),
                    ["memory", "file", "/no/such/file.py", "--json"],
                    store_db, monkeypatch)
        assert r.exit_code == 0, r.output
        body = json.loads(r.output)
        _assert_envelope(body, command="file")
        assert body["results"] == []
        assert body["risk"]["total_sessions"] == 0

    def test_context_budget_truncates_the_pack_path(self, tmp_path, monkeypatch):
        store_db = tmp_path / "store.db"
        _seed_basic(store_db)
        r = _invoke(CliRunner(),
                    ["memory", "file", "/Users/yad/dev/foo/cost.py",
                     "--context-budget", "1", "--json"],
                    store_db, monkeypatch)
        assert r.exit_code == 0, r.output
        body = json.loads(r.output)
        assert body["results"] == []
        assert body["truncated"] is True
        assert body["budget"] == 1


# ── memory worked ───────────────────────────────────────────────────────────


class TestMemoryWorked:
    def test_json_finds_confirmed_action(self, tmp_path, monkeypatch):
        store_db = tmp_path / "store.db"
        _seed_basic(store_db)
        r = _invoke(CliRunner(),
                    ["memory", "worked", "util.py", "--json"],
                    store_db, monkeypatch)
        assert r.exit_code == 0, r.output
        body = json.loads(r.output)
        _assert_envelope(body, command="worked")
        assert body["result_count"] == 1
        assert body["results"][0]["session_id"] == "s-worked"
        assert body["results"][0]["outcome"] == "worked"

    def test_failed_action_is_not_returned(self, tmp_path, monkeypatch):
        store_db = tmp_path / "store.db"
        _seed_basic(store_db)
        # 'Edit' matches both s-worked and s-broke; only the worked one shows.
        r = _invoke(CliRunner(),
                    ["memory", "worked", "Edit", "--json"],
                    store_db, monkeypatch)
        body = json.loads(r.output)
        sids = {row["session_id"] for row in body["results"]}
        assert sids == {"s-worked"}

    def test_text_format(self, tmp_path, monkeypatch):
        store_db = tmp_path / "store.db"
        _seed_basic(store_db)
        r = _invoke(CliRunner(), ["memory", "worked", "util.py"],
                    store_db, monkeypatch)
        assert r.exit_code == 0, r.output
        assert "s-worked" in r.output

    def test_context_budget_truncates_the_pack_path(self, tmp_path, monkeypatch):
        store_db = tmp_path / "store.db"
        _seed_many_worked(store_db, n=6)
        r = _invoke(CliRunner(),
                    ["memory", "worked", "Edit", "--context-budget", "1", "--json"],
                    store_db, monkeypatch)
        assert r.exit_code == 0, r.output
        body = json.loads(r.output)
        assert body["results"] == []
        assert body["truncated"] is True


# ── memory sessions ─────────────────────────────────────────────────────────


class TestMemorySessions:
    def test_json_lists_sessions_in_path(self, tmp_path, monkeypatch):
        store_db = tmp_path / "store.db"
        _seed_basic(store_db)
        r = _invoke(CliRunner(),
                    ["memory", "sessions", "/Users/yad/dev/foo", "--json"],
                    store_db, monkeypatch)
        assert r.exit_code == 0, r.output
        body = json.loads(r.output)
        _assert_envelope(body, command="sessions")
        assert body["result_count"] == 3
        assert body["query"]["scope"] == "path"

    def test_file_path_switches_to_touching_mode(self, tmp_path, monkeypatch):
        store_db = tmp_path / "store.db"
        _seed_basic(store_db)
        # A real file on disk so the command takes the touching-file branch.
        real_file = tmp_path / "cost.py"
        real_file.write_text("x = 1\n")
        r = _invoke(CliRunner(),
                    ["memory", "sessions", str(real_file), "--json"],
                    store_db, monkeypatch)
        assert r.exit_code == 0, r.output
        body = json.loads(r.output)
        assert body["query"]["scope"] == "file"

    def test_text_format(self, tmp_path, monkeypatch):
        store_db = tmp_path / "store.db"
        _seed_basic(store_db)
        r = _invoke(CliRunner(),
                    ["memory", "sessions", "/Users/yad/dev/foo"],
                    store_db, monkeypatch)
        assert r.exit_code == 0, r.output
        assert "s-decide" in r.output or "Sessions in path" in r.output

    def test_context_budget_truncates_everything(self, tmp_path, monkeypatch):
        store_db = tmp_path / "store.db"
        _seed_many_in_path(store_db, n=8)
        r = _invoke(CliRunner(),
                    ["memory", "sessions", "/Users/yad/dev/foo",
                     "--context-budget", "1", "--json"],
                    store_db, monkeypatch)
        assert r.exit_code == 0, r.output
        body = json.loads(r.output)
        assert body["results"] == []
        assert body["truncated"] is True
        assert body["budget"] == 1

    def test_context_budget_zero_disables(self, tmp_path, monkeypatch):
        store_db = tmp_path / "store.db"
        _seed_many_in_path(store_db, n=8)
        r = _invoke(CliRunner(),
                    ["memory", "sessions", "/Users/yad/dev/foo",
                     "--context-budget", "0", "--json"],
                    store_db, monkeypatch)
        assert r.exit_code == 0, r.output
        body = json.loads(r.output)
        assert body["result_count"] == 8
        assert body["truncated"] is False
        assert body["budget"] == 0

    def test_limit_caps_results(self, tmp_path, monkeypatch):
        store_db = tmp_path / "store.db"
        _seed_many_in_path(store_db, n=8)
        r = _invoke(CliRunner(),
                    ["memory", "sessions", "/Users/yad/dev/foo",
                     "--limit", "3", "--json"],
                    store_db, monkeypatch)
        body = json.loads(r.output)
        assert body["result_count"] == 3


# ── memory ask ──────────────────────────────────────────────────────────────


class TestMemoryAsk:
    def test_json_envelope_command_and_note(self, tmp_path, monkeypatch):
        store_db = tmp_path / "store.db"
        _seed_basic(store_db)
        r = _invoke(CliRunner(),
                    ["memory", "ask", "sqlite", "--json"],
                    store_db, monkeypatch)
        assert r.exit_code == 0, r.output
        body = json.loads(r.output)
        _assert_envelope(body, command="ask")
        # `ask` is hybrid retrieval; with no Ollama the envelope says the
        # semantic half was unavailable and it fell back to keyword search.
        assert "note" in body
        assert "keyword search" in body["note"]
        assert body["vector_used"] is False
        assert body["query"]["question"] == "sqlite"

    def test_ask_falls_back_to_keyword_without_ollama(self, tmp_path, monkeypatch):
        # No search index + no Ollama → the hybrid path degrades exactly to
        # the substring search over the store (zero regression). Same row
        # ``memory decisions`` would surface, with full provenance.
        store_db = tmp_path / "store.db"
        _seed_basic(store_db)
        r = _invoke(CliRunner(),
                    ["memory", "ask", "sqlite", "--json"],
                    store_db, monkeypatch)
        body = json.loads(r.output)
        assert body["result_count"] == 1
        assert body["results"][0]["session_id"] == "s-decide"

    def test_ask_results_carry_provenance(self, tmp_path, monkeypatch):
        # Every returned chunk must carry session / date / cost provenance.
        store_db = tmp_path / "store.db"
        _seed_basic(store_db)
        r = _invoke(CliRunner(),
                    ["memory", "ask", "sqlite", "--json"],
                    store_db, monkeypatch)
        body = json.loads(r.output)
        row = body["results"][0]
        assert row["session_id"] == "s-decide"      # session
        assert row["last_ts"].startswith("2026-05-01")  # date
        assert "cost_usd" in row                     # cost
        assert isinstance(row["cost_usd"], (int, float))

    def test_text_format(self, tmp_path, monkeypatch):
        store_db = tmp_path / "store.db"
        _seed_basic(store_db)
        r = _invoke(CliRunner(), ["memory", "ask", "sqlite"],
                    store_db, monkeypatch)
        assert r.exit_code == 0, r.output
        assert "note:" in r.output


# ── --project resolution ────────────────────────────────────────────────────


class TestProjectScoping:
    def test_explicit_project_is_echoed(self, tmp_path, monkeypatch):
        store_db = tmp_path / "store.db"
        _seed_basic(store_db)
        r = _invoke(CliRunner(),
                    ["memory", "decisions", "sqlite",
                     "--project", "-Users-yad-dev-foo", "--json"],
                    store_db, monkeypatch)
        body = json.loads(r.output)
        assert body["query"]["project"] == "-Users-yad-dev-foo"
        assert body["result_count"] == 1

    def test_unknown_explicit_project_filters_everything_out(
        self, tmp_path, monkeypatch,
    ):
        store_db = tmp_path / "store.db"
        _seed_basic(store_db)
        r = _invoke(CliRunner(),
                    ["memory", "decisions", "sqlite",
                     "--project", "-no-such-project", "--json"],
                    store_db, monkeypatch)
        body = json.loads(r.output)
        assert body["results"] == []
        assert body["query"]["project"] == "-no-such-project"

    def test_project_defaults_to_cwd(self, tmp_path, monkeypatch):
        # A project whose filesystem path is the cwd → the resolved slug
        # is echoed even though --project was not passed.
        store_db = tmp_path / "store.db"
        proj_dir = tmp_path / "repo"
        proj_dir.mkdir()
        _seed_project_at(store_db, slug="-cwd-proj",
                         path=str(proj_dir.resolve()))
        monkeypatch.chdir(proj_dir)
        r = _invoke(CliRunner(),
                    ["memory", "decisions", "sqlite", "--json"],
                    store_db, monkeypatch)
        assert r.exit_code == 0, r.output
        body = json.loads(r.output)
        assert body["query"]["project"] == "-cwd-proj"
        assert body["result_count"] == 1

    def test_cwd_not_a_project_means_no_filter(self, tmp_path, monkeypatch):
        # cwd is an unrelated tmp dir → no slug resolved → searches all.
        store_db = tmp_path / "store.db"
        _seed_basic(store_db)
        elsewhere = tmp_path / "elsewhere"
        elsewhere.mkdir()
        monkeypatch.chdir(elsewhere)
        r = _invoke(CliRunner(),
                    ["memory", "decisions", "sqlite", "--json"],
                    store_db, monkeypatch)
        body = json.loads(r.output)
        assert body["query"]["project"] is None
        assert body["result_count"] == 1

    def test_cwd_prefers_deepest_nested_project(self, tmp_path, monkeypatch):
        # Real-data shape: projects carry no `path`, the slug encodes the
        # path. cwd inside a nested repo must resolve to that repo, not a
        # busier enclosing project.
        import re as _re
        store_db = tmp_path / "store.db"
        repo = tmp_path / "work" / "repo"
        repo.mkdir(parents=True)
        parent_slug = _re.sub(r"[^A-Za-z0-9]", "-", str((tmp_path / "work").resolve()))
        repo_slug = _re.sub(r"[^A-Za-z0-9]", "-", str(repo.resolve()))
        conn = db.connect(store_db)
        schema.apply(conn)
        for slug in (parent_slug, repo_slug):
            pid = int(conn.execute(
                "INSERT INTO projects (provider, slug, path, display_name, "
                " first_seen, last_modified) VALUES ('claude', ?, NULL, 'p', 0.0, 0.0)",
                (slug,),
            ).lastrowid)
            _add_session(conn, pid, f"s-{slug[-8:]}", "2026-05-05", [
                ("assistant", "we decided to use sqlite right here", "[]"),
            ])
        conn.commit()
        conn.close()
        monkeypatch.chdir(repo)
        r = _invoke(CliRunner(),
                    ["memory", "decisions", "sqlite", "--json"],
                    store_db, monkeypatch)
        assert r.exit_code == 0, r.output
        body = json.loads(r.output)
        assert body["query"]["project"] == repo_slug
        assert body["result_count"] == 1


# ── the mcp command is gone ─────────────────────────────────────────────────


def test_mcp_command_removed(tmp_path, monkeypatch):
    store_db = tmp_path / "store.db"
    _seed_empty(store_db)
    r = _invoke(CliRunner(), ["mcp"], store_db, monkeypatch)
    # No such command — Click exits non-zero before any handler runs.
    assert r.exit_code != 0


def test_memory_group_lists_five_subcommands(tmp_path, monkeypatch):
    store_db = tmp_path / "store.db"
    _seed_empty(store_db)
    r = _invoke(CliRunner(), ["memory", "--help"], store_db, monkeypatch)
    assert r.exit_code == 0, r.output
    for sub in ("decisions", "file", "worked", "sessions", "ask"):
        assert sub in r.output
