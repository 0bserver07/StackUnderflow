"""CLI-level coverage for the FTS routing + query hygiene (spec #9).

Proves the ``memory`` namespace: (1) rejects intent-free queries BEFORE the
store is opened, (2) routes ``memory decisions`` through the FTS/bm25 index
(clustering + budget still governed by ``pack_within_budget``), and (3)
handles agent free text containing FTS5 operators (``use NOT null``)
literally instead of tripping the parser.

Mirrors ``test_memory_cli.py``: monkeypatch ``deps.store_path`` to a tmp
store, build the search index beside it, drive with ``CliRunner``. No
network — the search index is a plain FTS5 table; Ollama is never reached
(the vector half of ``ask`` simply no-ops).
"""

from __future__ import annotations

import json
from pathlib import Path

from click.testing import CliRunner

import stackunderflow.cli as cli_module
import stackunderflow.deps as deps
from stackunderflow.cli import cli
from stackunderflow.services.search_service import SearchService
from stackunderflow.store import db, schema


def _seed_store(store_db: Path, sessions: list[tuple[str, list[str]]]) -> None:
    """``sessions`` = list of ``(session_id, [content_text, ...])``."""
    conn = db.connect(store_db)
    schema.apply(conn)
    cur = conn.execute(
        "INSERT INTO projects (provider, slug, path, display_name, "
        " first_seen, last_modified) VALUES "
        "('claude', '-Users-yad-dev-foo', NULL, 'foo', 0.0, 0.0)"
    )
    pid = int(cur.lastrowid)
    for day, (sid, contents) in enumerate(sessions, start=1):
        c = conn.execute(
            "INSERT INTO sessions (project_id, session_id, first_ts, last_ts, "
            " message_count) VALUES (?, ?, ?, ?, ?)",
            (pid, sid, f"2026-05-{day:02d}T00:00:00+00:00",
             f"2026-05-{day:02d}T00:00:00+00:00", len(contents)),
        )
        sfk = int(c.lastrowid)
        for seq, text in enumerate(contents):
            conn.execute(
                "INSERT INTO messages (session_fk, seq, timestamp, role, model, "
                " input_tokens, output_tokens, cache_create_tokens, "
                " cache_read_tokens, content_text, tools_json, raw_json, "
                " is_sidechain) VALUES (?, ?, ?, 'assistant', NULL, 0, 0, 0, 0, "
                " ?, '[]', '{}', 0)",
                (sfk, seq, f"2026-05-{day:02d}T00:{seq:02d}:00+00:00", text),
            )
    conn.commit()
    conn.close()


def _build_index(store_db: Path, sessions: list[tuple[str, list[str]]]) -> None:
    """Index the same sessions into ``search_index.db`` beside the store."""
    svc = SearchService(store_db.parent / "search_index.db")
    msgs = []
    for day, (sid, contents) in enumerate(sessions, start=1):
        for text in contents:
            msgs.append({
                "content": text, "type": "assistant", "session_id": sid,
                "timestamp": f"2026-05-{day:02d}T00:00:00Z", "model": "m",
            })
    svc.index_project("-Users-yad-dev-foo", msgs)


# ── intent gate (store never opened) ─────────────────────────────────────────


class TestIntentGate:
    def test_punctuation_query_errors_before_store_opens(self, tmp_path, monkeypatch):
        store_db = tmp_path / "store.db"
        _seed_store(store_db, [("s1", ["we chose sqlite"])])
        monkeypatch.setattr(deps, "store_path", store_db)

        opened = {"n": 0}
        real_open = cli_module._open_store
        monkeypatch.setattr(
            cli_module, "_open_store",
            lambda: (opened.__setitem__("n", opened["n"] + 1), real_open())[1],
        )

        r = CliRunner().invoke(cli, ["memory", "decisions", "!!!", "--json"])
        assert r.exit_code == 1
        body = json.loads(r.output)
        assert "error" in body and "searchable" in body["error"]
        assert body["command"] == "decisions"
        # The gate short-circuited: the store was never opened.
        assert opened["n"] == 0

    def test_empty_query_errors_and_worked_and_ask_too(self, tmp_path, monkeypatch):
        store_db = tmp_path / "store.db"
        _seed_store(store_db, [("s1", ["we chose sqlite"])])
        monkeypatch.setattr(deps, "store_path", store_db)

        opened = {"n": 0}
        real_open = cli_module._open_store
        monkeypatch.setattr(
            cli_module, "_open_store",
            lambda: (opened.__setitem__("n", opened["n"] + 1), real_open())[1],
        )

        for cmd, q in [("decisions", "   "), ("worked", "!!!"), ("ask", "***")]:
            r = CliRunner().invoke(cli, ["memory", cmd, q, "--json"])
            assert r.exit_code == 1, (cmd, r.output)
            body = json.loads(r.output)
            assert "error" in body and body["command"] == cmd
        assert opened["n"] == 0


# ── FTS-routed memory decisions ──────────────────────────────────────────────


class TestMemoryDecisionsFts:
    def test_clustering_count_in_envelope_row(self, tmp_path, monkeypatch):
        store_db = tmp_path / "store.db"
        sessions = [("s-chatty", ["widget a", "widget b", "widget c"])]
        _seed_store(store_db, sessions)
        _build_index(store_db, sessions)
        monkeypatch.setattr(deps, "store_path", store_db)

        r = CliRunner().invoke(cli, ["memory", "decisions", "widget", "--json"])
        assert r.exit_code == 0, r.output
        body = json.loads(r.output)
        rows = [row for row in body["results"] if row["session_id"] == "s-chatty"]
        assert len(rows) == 1  # clustered to a single row
        assert rows[0]["more_matches_in_session"] == 2

    def test_context_budget_truncates_the_fts_path(self, tmp_path, monkeypatch):
        store_db = tmp_path / "store.db"
        sessions = [(f"s-{i}", ["shared needle token"]) for i in range(5)]
        _seed_store(store_db, sessions)
        _build_index(store_db, sessions)
        monkeypatch.setattr(deps, "store_path", store_db)

        r = CliRunner().invoke(
            cli, ["memory", "decisions", "needle", "--context-budget", "1", "--json"],
        )
        assert r.exit_code == 0, r.output
        body = json.loads(r.output)
        assert body["truncated"] is True
        assert body["result_count"] == 0


# ── operator query handled literally, not as syntax ──────────────────────────


class TestOperatorQuery:
    def test_ask_use_not_null_searches_literally(self, tmp_path, monkeypatch):
        store_db = tmp_path / "store.db"
        sessions = [
            ("s-null", ["use NOT null constraints on the id column"]),
            ("s-other", ["completely unrelated note about colours"]),
        ]
        _seed_store(store_db, sessions)
        _build_index(store_db, sessions)
        monkeypatch.setattr(deps, "store_path", store_db)

        # No Ollama mock — the vector half no-ops; FTS + substring carry it.
        r = CliRunner().invoke(cli, ["memory", "ask", "use NOT null", "--json"])
        assert r.exit_code == 0, r.output
        assert r.exception is None
        body = json.loads(r.output)
        sids = {row["session_id"] for row in body["results"]}
        assert "s-null" in sids

    def test_decisions_operator_query_does_not_crash(self, tmp_path, monkeypatch):
        store_db = tmp_path / "store.db"
        sessions = [("s-null", ["you should use NOT null here"])]
        _seed_store(store_db, sessions)
        _build_index(store_db, sessions)
        monkeypatch.setattr(deps, "store_path", store_db)

        r = CliRunner().invoke(cli, ["memory", "decisions", "use NOT null", "--json"])
        assert r.exit_code == 0, r.output
        assert r.exception is None
        body = json.loads(r.output)
        assert {row["session_id"] for row in body["results"]} == {"s-null"}
