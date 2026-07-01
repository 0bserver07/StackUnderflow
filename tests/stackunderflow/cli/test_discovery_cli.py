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


def _seed_many(store_db: Path, n: int = 8) -> None:
    """Seed one project under /Users/yad/dev/foo with ``n`` sessions."""
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
            " message_count) VALUES (?, ?, ?, ?, ?)",
            (pid, f"s-{i:02d}", f"2026-04-{i + 1:02d}T00:00:00+00:00",
             f"2026-04-{i + 1:02d}T00:00:00+00:00", i + 1),
        )
    conn.commit()
    conn.close()


def _budget_keys(body: dict) -> None:
    """Every budgeted JSON output carries the budget accounting keys."""
    assert "_budget_used_tokens" in body
    assert "_budget_max_tokens" in body
    assert isinstance(body["_budget_used_tokens"], int)
    assert isinstance(body["_budget_max_tokens"], int)


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
        body = json.loads(r.output)
        assert body["sessions"] == []
        assert "_truncated" not in body
        _budget_keys(body)
        assert body["_budget_used_tokens"] == 0

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
        assert json.loads(r.output)["sessions"] == []

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
        body = json.loads(r.output)
        assert body["sessions"] == []
        assert "_truncated" not in body
        _budget_keys(body)

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
        body = json.loads(r.output)
        assert body["sessions"] == []
        _budget_keys(body)

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
        assert json.loads(r.output)["sessions"] == []

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
        assert json.loads(r.output)["sessions"] == []


# ── token budget (--context-budget) ─────────────────────────────────────────


class TestContextBudget:
    def test_budget_keys_present_when_nothing_dropped(self, tmp_path, monkeypatch):
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
        assert len(body["sessions"]) == 2
        assert "_truncated" not in body
        assert "_more_available" not in body
        _budget_keys(body)
        assert body["_budget_max_tokens"] == 2000  # configured default
        assert 0 < body["_budget_used_tokens"] <= 2000

    def test_tiny_budget_truncates_everything_json(self, tmp_path, monkeypatch):
        store_db = tmp_path / "store.db"
        _seed_many(store_db, n=8)
        runner = CliRunner()
        r = _invoke(
            runner,
            ["find-sessions-in-path", "/Users/yad/dev/foo",
             "--context-budget", "1", "--format", "json"],
            store_db, monkeypatch,
        )
        assert r.exit_code == 0, r.output
        body = json.loads(r.output)
        assert body["sessions"] == []
        assert body["_truncated"] is True
        assert body["_more_available"] == 8
        assert body["_budget_max_tokens"] == 1
        assert body["_budget_used_tokens"] == 0

    def test_tiny_budget_truncates_text_footer(self, tmp_path, monkeypatch):
        store_db = tmp_path / "store.db"
        _seed_many(store_db, n=8)
        runner = CliRunner()
        r = _invoke(
            runner,
            ["find-sessions-in-path", "/Users/yad/dev/foo",
             "--context-budget", "1"],
            store_db, monkeypatch,
        )
        assert r.exit_code == 0, r.output
        assert "8 more sessions matched but truncated" in r.output
        assert "--context-budget" in r.output

    def test_partial_budget_keeps_top_subset(self, tmp_path, monkeypatch):
        store_db = tmp_path / "store.db"
        _seed_many(store_db, n=8)
        runner = CliRunner()
        r = _invoke(
            runner,
            ["find-sessions-in-path", "/Users/yad/dev/foo",
             "--context-budget", "150", "--format", "json"],
            store_db, monkeypatch,
        )
        assert r.exit_code == 0, r.output
        body = json.loads(r.output)
        kept = len(body["sessions"])
        assert 0 < kept < 8
        assert body["_truncated"] is True
        assert kept + body["_more_available"] == 8
        # Greedy pack respects the budget.
        assert body["_budget_used_tokens"] <= 150
        # Rank order: recency dominates, so the most-recent sessions win.
        # _seed_many gives later indices later last_ts → "s-07" is newest.
        sids = [s["session_id"] for s in body["sessions"]]
        assert sids[0] == "s-07"

    def test_zero_budget_disables_enforcement(self, tmp_path, monkeypatch):
        store_db = tmp_path / "store.db"
        _seed_many(store_db, n=8)
        runner = CliRunner()
        r = _invoke(
            runner,
            ["find-sessions-in-path", "/Users/yad/dev/foo",
             "--context-budget", "0", "--format", "json"],
            store_db, monkeypatch,
        )
        assert r.exit_code == 0, r.output
        body = json.loads(r.output)
        assert len(body["sessions"]) == 8
        assert "_truncated" not in body
        assert body["_budget_max_tokens"] == 0

    def test_limit_still_a_hard_cap_under_budget(self, tmp_path, monkeypatch):
        store_db = tmp_path / "store.db"
        _seed_many(store_db, n=8)
        runner = CliRunner()
        r = _invoke(
            runner,
            ["find-sessions-in-path", "/Users/yad/dev/foo",
             "--limit", "3", "--context-budget", "99999", "--format", "json"],
            store_db, monkeypatch,
        )
        assert r.exit_code == 0, r.output
        body = json.loads(r.output)
        # 8 sessions matched but --limit 3 caps; budget 99999 drops nothing.
        assert len(body["sessions"]) == 3
        assert "_truncated" not in body

    def test_env_var_sets_default_budget(self, tmp_path, monkeypatch):
        store_db = tmp_path / "store.db"
        _seed_many(store_db, n=8)
        monkeypatch.setenv("STACKUNDERFLOW_DISCOVERY_BUDGET_TOKENS", "1")
        runner = CliRunner()
        r = _invoke(
            runner,
            ["find-sessions-in-path", "/Users/yad/dev/foo", "--format", "json"],
            store_db, monkeypatch,
        )
        assert r.exit_code == 0, r.output
        body = json.loads(r.output)
        assert body["sessions"] == []
        assert body["_truncated"] is True
        assert body["_budget_max_tokens"] == 1

    def test_search_decisions_budget_text_footer(self, tmp_path, monkeypatch):
        store_db = tmp_path / "store.db"
        _seed_with_data(store_db)  # 's-new' has "we decided to use sqlite"
        runner = CliRunner()
        r = _invoke(
            runner,
            ["search-past-decisions", "decided", "--context-budget", "1"],
            store_db, monkeypatch,
        )
        assert r.exit_code == 0, r.output
        # 's-new' matched but the budget can't fit it.
        assert "1 more session matched but truncated" in r.output


# ── empty store ────────────────────────────────────────────────────────────


class TestEmptyStore:
    @pytest.mark.parametrize(
        "args",
        [
            ["find-sessions-in-path", "/Users/yad/dev/foo", "--format", "json"],
            ["find-sessions-touching-file", "/Users/yad/dev/foo/main.py",
             "--format", "json"],
            ["search-past-decisions", "anything", "--format", "json"],
            ["find-sessions-where-action-worked", "Edit", "--format", "json"],
            ["find-failure-modes-for-file", "/Users/yad/dev/foo/cost.py",
             "--format", "json"],
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
        # The 3 budget-aware commands also emit `_budget_*` keys; the 2
        # outcome commands don't. Either way, `sessions` is the empty list.
        assert json.loads(r.output)["sessions"] == []


# ── outcome-aware discovery commands ────────────────────────────────────────


def _seed_outcome_store(store_db: Path, *, session_id: str, turns: list[tuple]) -> None:
    """Seed one project + one session with ``turns`` = ``(role, text, tools_json)``."""
    conn = db.connect(store_db)
    schema.apply(conn)
    cur = conn.execute(
        "INSERT INTO projects (provider, slug, path, display_name, "
        " first_seen, last_modified) VALUES "
        "('claude', '-Users-yad-dev-foo', NULL, 'foo', 0.0, 0.0)"
    )
    pid = int(cur.lastrowid)
    cur_s = conn.execute(
        "INSERT INTO sessions (project_id, session_id, first_ts, last_ts, "
        " message_count) VALUES (?, ?, '2026-05-01T00:00:00+00:00', "
        "'2026-05-01T01:00:00+00:00', ?)",
        (pid, session_id, len(turns)),
    )
    sfk = int(cur_s.lastrowid)
    for seq, turn in enumerate(turns):
        role, text = turn[0], turn[1]
        tools_json = turn[2] if len(turn) > 2 else "[]"
        conn.execute(
            "INSERT INTO messages (session_fk, seq, timestamp, role, model, "
            " input_tokens, output_tokens, cache_create_tokens, "
            " cache_read_tokens, content_text, tools_json, raw_json, "
            " is_sidechain) VALUES (?, ?, ?, ?, NULL, 0, 0, 0, 0, ?, ?, '{}', 0)",
            (sfk, seq, f"2026-05-01T00:{seq:02d}:00+00:00", role, text, tools_json),
        )
    conn.commit()
    conn.close()


_EDIT_COST = json.dumps([{"name": "Edit", "input": {
    "file_path": "/Users/yad/dev/foo/cost.py", "old_string": "a", "new_string": "b",
}}])


class TestFindSessionsWhereActionWorked:
    def test_text_format_shows_outcome(self, tmp_path, monkeypatch):
        store_db = tmp_path / "store.db"
        _seed_outcome_store(store_db, session_id="ok", turns=[
            ("assistant", "applied edit", _EDIT_COST),
            ("user", "thanks, that worked!"),
        ])
        runner = CliRunner()
        r = _invoke(
            runner,
            ["find-sessions-where-action-worked", "cost.py"],
            store_db, monkeypatch,
        )
        assert r.exit_code == 0, r.output
        assert "ok" in r.output
        assert "worked" in r.output

    def test_json_shape_has_outcome_keys(self, tmp_path, monkeypatch):
        store_db = tmp_path / "store.db"
        _seed_outcome_store(store_db, session_id="ok", turns=[
            ("assistant", "applied edit", _EDIT_COST),
            ("user", "perfect, ship it"),
        ])
        runner = CliRunner()
        r = _invoke(
            runner,
            ["find-sessions-where-action-worked", "Edit", "--format", "json"],
            store_db, monkeypatch,
        )
        assert r.exit_code == 0, r.output
        body = json.loads(r.output)
        assert len(body["sessions"]) == 1
        s = body["sessions"][0]
        assert s["session_id"] == "ok"
        assert s["outcome"] == "worked"
        assert isinstance(s["outcome_evidence"], str) and s["outcome_evidence"]
        assert isinstance(s["outcome_msg_id"], int)
        # Still carries the base SessionMatch keys.
        assert set(s.keys()) >= {
            "session_id", "project_slug", "project_path", "provider",
            "first_ts", "last_ts", "message_count", "cost_usd", "snippet",
        }

    def test_failed_session_not_returned(self, tmp_path, monkeypatch):
        store_db = tmp_path / "store.db"
        _seed_outcome_store(store_db, session_id="broke", turns=[
            ("assistant", "applied edit", _EDIT_COST),
            ("user", "no, that broke it"),
        ])
        runner = CliRunner()
        r = _invoke(
            runner,
            ["find-sessions-where-action-worked", "cost.py", "--format", "json"],
            store_db, monkeypatch,
        )
        assert r.exit_code == 0, r.output
        assert json.loads(r.output) == {"sessions": []}

    def test_bad_since_is_user_error(self, tmp_path, monkeypatch):
        store_db = tmp_path / "store.db"
        _seed_outcome_store(store_db, session_id="ok", turns=[
            ("assistant", "applied edit", _EDIT_COST),
            ("user", "thanks"),
        ])
        runner = CliRunner()
        r = _invoke(
            runner,
            ["find-sessions-where-action-worked", "cost.py", "--since", "yesterday"],
            store_db, monkeypatch,
        )
        assert r.exit_code != 0

    def test_silence_session_filtered_by_default(self, tmp_path, monkeypatch):
        # Session ends without explicit confirmation → confidence 0.3 →
        # filtered out by the new default 0.5 threshold. Old behaviour:
        # always surfaced. Verified at the CLI surface, not just the
        # service.
        store_db = tmp_path / "store.db"
        _seed_outcome_store(store_db, session_id="quiet", turns=[
            ("assistant", "applied edit", _EDIT_COST),
            ("assistant", "All done."),
        ])
        runner = CliRunner()
        r = _invoke(
            runner,
            ["find-sessions-where-action-worked", "cost.py", "--format", "json"],
            store_db, monkeypatch,
        )
        assert r.exit_code == 0, r.output
        assert json.loads(r.output) == {"sessions": []}

    def test_min_confidence_zero_resurfaces_silence(self, tmp_path, monkeypatch):
        # The legacy "anything that didn't break" reading is one
        # ``--min-confidence 0.0`` away.
        store_db = tmp_path / "store.db"
        _seed_outcome_store(store_db, session_id="quiet", turns=[
            ("assistant", "applied edit", _EDIT_COST),
            ("assistant", "All done."),
        ])
        runner = CliRunner()
        r = _invoke(
            runner,
            ["find-sessions-where-action-worked", "cost.py",
             "--min-confidence", "0.0", "--format", "json"],
            store_db, monkeypatch,
        )
        assert r.exit_code == 0, r.output
        body = json.loads(r.output)
        assert len(body["sessions"]) == 1
        assert body["sessions"][0]["session_id"] == "quiet"
        assert body["sessions"][0]["outcome_confidence"] < 0.5

    def test_outcome_confidence_in_json_output(self, tmp_path, monkeypatch):
        store_db = tmp_path / "store.db"
        _seed_outcome_store(store_db, session_id="ok", turns=[
            ("assistant", "applied edit", _EDIT_COST),
            ("user", "perfect, ship it"),
        ])
        runner = CliRunner()
        r = _invoke(
            runner,
            ["find-sessions-where-action-worked", "cost.py", "--format", "json"],
            store_db, monkeypatch,
        )
        assert r.exit_code == 0, r.output
        body = json.loads(r.output)
        assert body["sessions"][0]["outcome_confidence"] >= 0.8

    def test_verbose_flag_shows_confidence_in_text(self, tmp_path, monkeypatch):
        store_db = tmp_path / "store.db"
        _seed_outcome_store(store_db, session_id="ok", turns=[
            ("assistant", "applied edit", _EDIT_COST),
            ("user", "thanks, that worked!"),
        ])
        runner = CliRunner()
        r = _invoke(
            runner,
            ["find-sessions-where-action-worked", "cost.py", "-v"],
            store_db, monkeypatch,
        )
        assert r.exit_code == 0, r.output
        assert "confidence" in r.output
        # Without -v the confidence is not in the text output.
        r2 = _invoke(
            runner,
            ["find-sessions-where-action-worked", "cost.py"],
            store_db, monkeypatch,
        )
        assert r2.exit_code == 0, r2.output
        assert "confidence" not in r2.output


class TestFindFailureModesForFile:
    def test_text_format_shows_failure(self, tmp_path, monkeypatch):
        store_db = tmp_path / "store.db"
        _seed_outcome_store(store_db, session_id="broke", turns=[
            ("assistant", "applied edit", _EDIT_COST),
            ("user", "no, that doesn't work"),
        ])
        runner = CliRunner()
        r = _invoke(
            runner,
            ["find-failure-modes-for-file", "/Users/yad/dev/foo/cost.py"],
            store_db, monkeypatch,
        )
        assert r.exit_code == 0, r.output
        assert "broke" in r.output
        assert "failed" in r.output

    def test_json_shape_has_outcome_keys(self, tmp_path, monkeypatch):
        store_db = tmp_path / "store.db"
        _seed_outcome_store(store_db, session_id="broke", turns=[
            ("assistant", "applied edit", _EDIT_COST),
            ("user", "no, that's wrong"),
        ])
        runner = CliRunner()
        r = _invoke(
            runner,
            ["find-failure-modes-for-file", "/Users/yad/dev/foo/cost.py",
             "--format", "json"],
            store_db, monkeypatch,
        )
        assert r.exit_code == 0, r.output
        body = json.loads(r.output)
        assert len(body["sessions"]) == 1
        s = body["sessions"][0]
        assert s["session_id"] == "broke"
        assert s["outcome"] in ("failed", "reverted")
        assert "outcome_evidence" in s and "outcome_msg_id" in s

    def test_worked_session_not_returned(self, tmp_path, monkeypatch):
        store_db = tmp_path / "store.db"
        _seed_outcome_store(store_db, session_id="fine", turns=[
            ("assistant", "applied edit", _EDIT_COST),
            ("user", "thanks, works now"),
        ])
        runner = CliRunner()
        r = _invoke(
            runner,
            ["find-failure-modes-for-file", "/Users/yad/dev/foo/cost.py",
             "--format", "json"],
            store_db, monkeypatch,
        )
        assert r.exit_code == 0, r.output
        # find-failure-modes-for-file has no --context-budget flag, so its
        # JSON output carries no budget-accounting keys.
        assert json.loads(r.output) == {"sessions": []}


# ── search-past-decisions --use-embeddings ──────────────────────────────────


class TestSearchPastDecisionsEmbeddingsCLI:
    """CLI surface for the opt-in semantic-search mode (Ollama backend).

    The full re-rank semantics are covered by the unit tests under
    ``services/test_search_past_decisions_embeddings.py``; these tests
    pin the *CLI* contract — the flag exists, JSON output carries
    ``embedding_score``, text output appends ``cos=…``, and (crucially)
    an unreachable Ollama degrades cleanly to substring ranking with a
    zero exit code, no error.

    The Ollama path is stubbed by monkey-patching
    ``embeddings.embed_texts`` — no network, no Ollama, no numpy.
    """

    def _stub_embed_texts(self, monkeypatch):
        """Deterministic stand-in for the Ollama embed call.

        query/sqlite/watcher text → axis-0 vector; everything else → axis-1.
        Aligned 1:1 with the input (query first). Records nothing — tests
        assert on output, not call shape.
        """
        from stackunderflow.services import embeddings

        def _fake(texts, *, model=None, **_kw):  # noqa: ARG001 — API compat
            out = []
            for t in texts:
                low = t.lower()
                if "watcher" in low or "sqlite" in low:
                    out.append([1.0, 0.0, 0.0, 0.0])
                else:
                    out.append([0.0, 1.0, 0.0, 0.0])
            return out

        monkeypatch.setattr(embeddings, "embed_texts", _fake)

    def test_json_output_carries_embedding_score(self, tmp_path, monkeypatch):
        self._stub_embed_texts(monkeypatch)
        store_db = tmp_path / "store.db"
        _seed_with_data(store_db)
        runner = CliRunner()
        r = _invoke(
            runner,
            ["search-past-decisions", "sqlite",
             "--use-embeddings", "--embed-model", "stub",
             "--format", "json"],
            store_db, monkeypatch,
        )
        assert r.exit_code == 0, r.output
        body = json.loads(r.output)
        assert body["sessions"]
        s = body["sessions"][0]
        assert "embedding_score" in s
        assert 0.0 <= s["embedding_score"] <= 1.0

    def test_text_output_appends_cos(self, tmp_path, monkeypatch):
        self._stub_embed_texts(monkeypatch)
        store_db = tmp_path / "store.db"
        _seed_with_data(store_db)
        runner = CliRunner()
        r = _invoke(
            runner,
            ["search-past-decisions", "sqlite",
             "--use-embeddings", "--embed-model", "stub"],
            store_db, monkeypatch,
        )
        assert r.exit_code == 0, r.output
        # The text renderer appends "cos=X.XX" to the headline.
        assert "cos=" in r.output

    def test_ollama_unreachable_degrades_cleanly(self, tmp_path, monkeypatch):
        """``--use-embeddings`` with Ollama down must exit 0 and fall back
        to substring ranking — the error path (missing extra) is gone.
        """
        from stackunderflow.services import embeddings

        # Ollama unreachable → embed_texts returns None (its documented
        # "embedding unavailable" signal).
        monkeypatch.setattr(
            embeddings, "embed_texts",
            lambda texts, *, model=None, **_kw: None,  # noqa: ARG005
        )

        store_db = tmp_path / "store.db"
        _seed_with_data(store_db)
        runner = CliRunner()
        r = _invoke(
            runner,
            ["search-past-decisions", "sqlite", "--use-embeddings",
             "--format", "json"],
            store_db, monkeypatch,
        )
        # Clean exit, results present, no crash, no install hint.
        assert r.exit_code == 0, r.output
        body = json.loads(r.output)
        assert body["sessions"]
        # Fell back to substring ranking: no embedding_score attached.
        for s in body["sessions"]:
            assert "embedding_score" not in s

    def test_default_off_does_not_embed(self, tmp_path, monkeypatch):
        """Sanity check: without the flag, the Ollama backend is never hit."""
        from stackunderflow.services import embeddings

        def _explode(texts, *, model=None, **_kw):  # noqa: ARG001
            raise AssertionError(
                "embed_texts should never be called without --use-embeddings"
            )
        monkeypatch.setattr(embeddings, "embed_texts", _explode)

        store_db = tmp_path / "store.db"
        _seed_with_data(store_db)
        runner = CliRunner()
        r = _invoke(
            runner,
            ["search-past-decisions", "sqlite", "--format", "json"],
            store_db, monkeypatch,
        )
        assert r.exit_code == 0, r.output
        body = json.loads(r.output)
        # No embedding_score on substring-mode rows.
        for s in body["sessions"]:
            assert "embedding_score" not in s
