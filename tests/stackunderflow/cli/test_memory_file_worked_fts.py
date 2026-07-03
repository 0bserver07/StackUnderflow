"""CLI coverage for the FTS content-half routing on ``memory file`` /
``memory worked`` (spec #15).

The ``memory`` namespace now injects the FTS index into
``find_sessions_touching_file`` / ``find_sessions_where_action_worked`` so the
free-text mention half gains bm25 ranking + clustering, while the exact
tool-arg half stays LIKE/exact. These drive the real CLI with ``CliRunner``,
monkeypatching ``deps.store_path`` to a tmp store and building the search index
beside it (the same derivation ``_lexical_search_service`` uses). No network.

Envelope shape (``pack_within_budget`` / row keys) is owned elsewhere and is
left untouched: these assert only which rows surface and their clustering.
"""

from __future__ import annotations

import json
from pathlib import Path

from click.testing import CliRunner

import stackunderflow.deps as deps
from stackunderflow.cli import cli
from stackunderflow.services.search_service import SearchService
from stackunderflow.store import db, schema

_PATH = "/Users/yad/dev/foo/src/cost.py"


def _edit_tools(path: str) -> str:
    return json.dumps([{"name": "Edit", "input": {
        "file_path": path, "old_string": "a", "new_string": "b",
    }}])


def _add_session(conn, pid: int, sid: str, day: str, turns: list[tuple]) -> None:
    """``turns`` = list of ``(role, content_text, tools_json)``."""
    cur = conn.execute(
        "INSERT INTO sessions (project_id, session_id, first_ts, last_ts, "
        " message_count) VALUES (?, ?, ?, ?, ?)",
        (pid, sid, f"{day}T00:00:00+00:00", f"{day}T01:00:00+00:00", len(turns)),
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


def _seed(store_db: Path, sessions: list[tuple[str, str, list[tuple]]]) -> None:
    """``sessions`` = list of ``(session_id, day, turns)``."""
    conn = db.connect(store_db)
    schema.apply(conn)
    cur = conn.execute(
        "INSERT INTO projects (provider, slug, path, display_name, "
        " first_seen, last_modified) VALUES "
        "('claude', '-Users-yad-dev-foo', NULL, 'foo', 0.0, 0.0)"
    )
    pid = int(cur.lastrowid)
    for sid, day, turns in sessions:
        _add_session(conn, pid, sid, day, turns)
    conn.commit()
    conn.close()


def _build_index(store_db: Path) -> None:
    """Index every non-empty content_text message beside the store."""
    conn = db.connect(store_db)
    rows = conn.execute(
        "SELECT s.session_id AS sid, m.content_text AS content, m.timestamp AS ts "
        "FROM messages m JOIN sessions s ON s.id = m.session_fk "
        "ORDER BY m.session_fk, m.seq"
    ).fetchall()
    conn.close()
    svc = SearchService(store_db.parent / "search_index.db")
    svc.index_project("-Users-yad-dev-foo", [
        {"content": r["content"], "type": "assistant",
         "session_id": r["sid"], "timestamp": r["ts"], "model": "m"}
        for r in rows
    ])


# ── memory file ──────────────────────────────────────────────────────────────


class TestMemoryFileContentHalf:
    def test_content_mention_surfaces_with_clustering(self, tmp_path, monkeypatch):
        store_db = tmp_path / "store.db"
        _seed(store_db, [
            ("s-mention", "2026-05-01", [
                ("assistant", f"note one: {_PATH} is where the bug lived", "[]"),
                ("assistant", f"note two: revisit {_PATH} after the refactor", "[]"),
            ]),
        ])
        _build_index(store_db)
        monkeypatch.setattr(deps, "store_path", store_db)

        r = CliRunner().invoke(cli, ["memory", "file", _PATH, "--json"])
        assert r.exit_code == 0, r.output
        body = json.loads(r.output)
        rows = [row for row in body["results"] if row["session_id"] == "s-mention"]
        assert len(rows) == 1  # clustered to a single row
        assert rows[0]["more_matches_in_session"] == 1
        assert rows[0]["kind"] == "touched"

    def test_exact_edit_ranks_before_content_mention(self, tmp_path, monkeypatch):
        store_db = tmp_path / "store.db"
        _seed(store_db, [
            ("s-mention", "2026-05-05", [
                ("assistant", f"chatting about {_PATH} in passing", "[]"),
            ]),
            ("s-edit", "2026-05-01", [  # older, but an exact tool match
                ("assistant", "applied the edit", _edit_tools(_PATH)),
            ]),
        ])
        _build_index(store_db)
        monkeypatch.setattr(deps, "store_path", store_db)

        r = CliRunner().invoke(cli, ["memory", "file", _PATH, "--json"])
        assert r.exit_code == 0, r.output
        body = json.loads(r.output)
        sids = [row["session_id"] for row in body["results"]]
        assert set(sids) == {"s-edit", "s-mention"}
        # Exact tool match leads despite being older (exact ranks first).
        assert sids.index("s-edit") < sids.index("s-mention")

    def test_unpopulated_index_still_reports(self, tmp_path, monkeypatch):
        # No index built → the LIKE fallback still finds the content mention.
        store_db = tmp_path / "store.db"
        _seed(store_db, [
            ("s-only", "2026-05-01", [("assistant", f"only in text: {_PATH}", "[]")]),
        ])
        monkeypatch.setattr(deps, "store_path", store_db)

        r = CliRunner().invoke(cli, ["memory", "file", _PATH, "--json"])
        assert r.exit_code == 0, r.output
        body = json.loads(r.output)
        assert "s-only" in {row["session_id"] for row in body["results"]}


# ── memory worked ────────────────────────────────────────────────────────────


class TestMemoryWorkedContentHalf:
    def test_content_action_mention_surfaces(self, tmp_path, monkeypatch):
        store_db = tmp_path / "store.db"
        _seed(store_db, [
            ("s-work", "2026-05-01", [
                ("user", "let's add caching to the cost route", "[]"),
                ("assistant", "done", _edit_tools(_PATH)),
                ("user", "also caching the second route", "[]"),
                ("assistant", "done too", "[]"),
                ("user", "thanks, that worked perfectly", "[]"),
            ]),
        ])
        _build_index(store_db)
        monkeypatch.setattr(deps, "store_path", store_db)

        # 'caching' never appears in a tool arg — only free-text content.
        r = CliRunner().invoke(cli, ["memory", "worked", "caching", "--json"])
        assert r.exit_code == 0, r.output
        body = json.loads(r.output)
        rows = [row for row in body["results"] if row["session_id"] == "s-work"]
        assert len(rows) == 1
        assert rows[0]["outcome"] == "worked"
        assert rows[0]["more_matches_in_session"] == 1

    def test_unpopulated_index_falls_back(self, tmp_path, monkeypatch):
        store_db = tmp_path / "store.db"
        _seed(store_db, [
            ("s-work", "2026-05-01", [
                ("user", "let's add caching here", "[]"),
                ("assistant", "done", _edit_tools(_PATH)),
                ("user", "thanks, that worked", "[]"),
            ]),
        ])
        monkeypatch.setattr(deps, "store_path", store_db)  # no index built

        r = CliRunner().invoke(cli, ["memory", "worked", "caching", "--json"])
        assert r.exit_code == 0, r.output
        body = json.loads(r.output)
        assert "s-work" in {row["session_id"] for row in body["results"]}
