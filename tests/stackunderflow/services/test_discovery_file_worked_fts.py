"""FTS5/bm25 routing for the *content half* of ``find_sessions_touching_file``
(``memory file``) and ``find_sessions_where_action_worked`` (``memory worked``)
— spec #15.

Spec #9 routed ``search_past_decisions`` through the FTS index. These two
commands stayed on exact/LIKE because their inputs are file paths / tool-arg
fragments. #15 finishes the job: the *exact* half (a Read/Edit/Write tool arg,
or the ``tools_json`` match) stays LIKE/exact, while the *free-text mention*
half — the path or action appearing in ``messages.content_text`` — is gathered,
bm25-ranked and clustered through ``SearchService.lexical_session_hits``.

Proven here:

* a content-only mention now surfaces via bm25, with per-session clustering
  (``more_matches_in_session``);
* exact tool-arg matches are unchanged and rank first;
* the ``memory file`` <100ms budget is protected — the FTS content-half is
  *not* consulted when the exact half already fills the page (spy asserts zero
  ``lexical_session_hits`` calls on the fast path);
* an unpopulated index degrades to the pre-#15 LIKE behaviour.

No network: the index is a plain on-disk FTS5 table built with
``SearchService.index_project``; Ollama is never reached.
"""

from __future__ import annotations

import json
import sqlite3
from pathlib import Path

from stackunderflow.services.discovery import (
    BudgetedResult,
    OutcomeMatch,
    find_sessions_touching_file,
    find_sessions_where_action_worked,
)
from stackunderflow.services.search_service import SearchService
from stackunderflow.store import db, schema

# ── seeding helpers ──────────────────────────────────────────────────────────


def _make_store(tmp_path: Path) -> tuple[sqlite3.Connection, Path]:
    store_db = tmp_path / "store.db"
    conn = db.connect(store_db)
    schema.apply(conn)
    return conn, store_db


def _seed_project(conn: sqlite3.Connection, slug: str = "-Users-yad-dev-foo") -> int:
    cur = conn.execute(
        "INSERT INTO projects (provider, slug, path, display_name, "
        " first_seen, last_modified) VALUES ('claude', ?, NULL, 'foo', 0.0, 0.0)",
        (slug,),
    )
    return int(cur.lastrowid)


def _seed_session(
    conn: sqlite3.Connection, pid: int, sid: str, day: str = "2026-05-01",
) -> int:
    cur = conn.execute(
        "INSERT INTO sessions (project_id, session_id, first_ts, last_ts, "
        " message_count) VALUES (?, ?, ?, ?, 1)",
        (pid, sid, f"{day}T00:00:00+00:00", f"{day}T00:00:00+00:00"),
    )
    return int(cur.lastrowid)


def _add_msg(
    conn: sqlite3.Connection, sfk: int, seq: int, *,
    role: str = "assistant", content: str = "", tools_json: str = "[]",
    day: str = "2026-05-01",
) -> None:
    conn.execute(
        "INSERT INTO messages (session_fk, seq, timestamp, role, model, "
        " input_tokens, output_tokens, cache_create_tokens, cache_read_tokens, "
        " content_text, tools_json, raw_json, is_sidechain) "
        "VALUES (?, ?, ?, ?, NULL, 0, 0, 0, 0, ?, ?, '{}', 0)",
        (sfk, seq, f"{day}T00:{seq:02d}:00+00:00", role, content, tools_json),
    )


def _edit_blob(file_path: str) -> str:
    return json.dumps([{"name": "Edit", "input": {
        "file_path": file_path, "old_string": "x", "new_string": "y",
    }}])


def _index_from_store(store_db: Path, conn: sqlite3.Connection,
                      slug: str = "-Users-yad-dev-foo") -> SearchService:
    """Index every non-empty ``content_text`` message in the store into the
    FTS index beside it — exactly the projection ``reindex_all`` would build,
    keyed by the provider-facing ``session_id``."""
    rows = conn.execute(
        "SELECT s.session_id AS sid, m.content_text AS content, m.timestamp AS ts "
        "FROM messages m JOIN sessions s ON s.id = m.session_fk "
        "ORDER BY m.session_fk, m.seq"
    ).fetchall()
    svc = SearchService(store_db.parent / "search_index.db")
    svc.index_project(slug, [
        {"content": r["content"], "type": "assistant",
         "session_id": r["sid"], "timestamp": r["ts"], "model": "m"}
        for r in rows
    ])
    return svc


class _SpyService:
    """Wraps a SearchService, counting ``lexical_session_hits`` calls so a
    test can prove the perf gate skipped the FTS open on the fast path."""

    def __init__(self, inner: SearchService):
        self.inner = inner
        self.calls = 0

    def lexical_session_hits(self, *args, **kwargs):  # noqa: ANN002, ANN003
        self.calls += 1
        return self.inner.lexical_session_hits(*args, **kwargs)


_PATH = "/Users/yad/dev/foo/src/cost.py"


# ── find_sessions_touching_file — content half via FTS ───────────────────────


class TestTouchingFileContentHalf:
    def test_content_mention_surfaces_via_bm25_with_clustering(self, tmp_path):
        conn, store_db = _make_store(tmp_path)
        pid = _seed_project(conn)
        # A session that only *mentions* the path in message content, three
        # times — no tool call. This is the free-text half the LIKE path
        # found but never clustered.
        mention = _seed_session(conn, pid, "s-mention", day="2026-05-01")
        for i in range(3):
            _add_msg(conn, mention, i, content=f"note {i}: see {_PATH} for the fix")
        # A session that edited the file via a tool call — the exact half.
        tool = _seed_session(conn, pid, "s-tool", day="2026-05-02")
        _add_msg(conn, tool, 0, content="applied the edit", tools_json=_edit_blob(_PATH))
        conn.commit()
        svc = _index_from_store(store_db, conn)

        out = find_sessions_touching_file(conn, _PATH, mode="any", search_service=svc)
        sids = [m.session_id for m in out]
        assert set(sids) == {"s-tool", "s-mention"}
        # Exact tool match ranks first; the content mention follows.
        assert sids[0] == "s-tool"
        # The content-only session is clustered to one row + the 2 further hits.
        mrow = next(m for m in out if m.session_id == "s-mention")
        assert mrow.more_matches_in_session == 2
        assert mrow.to_dict()["more_matches_in_session"] == 2
        # The exact tool row carries no content-clustering signal.
        assert next(m for m in out if m.session_id == "s-tool").more_matches_in_session is None

    def test_bm25_orders_the_content_mentions(self, tmp_path):
        conn, store_db = _make_store(tmp_path)
        pid = _seed_project(conn)
        strong = _seed_session(conn, pid, "s-strong", day="2026-05-01")
        _add_msg(conn, strong, 0, content=f"{_PATH} {_PATH} {_PATH} is the file we changed")
        weak = _seed_session(conn, pid, "s-weak", day="2026-05-01")
        _add_msg(
            conn, weak, 0,
            content=(
                "a long rambling paragraph about many other unrelated matters "
                f"that only in passing happens to mention {_PATH} once among a "
                "great deal of other surrounding prose and commentary entirely"
            ),
        )
        conn.commit()
        svc = _index_from_store(store_db, conn)

        out = find_sessions_touching_file(conn, _PATH, mode="any", search_service=svc)
        # Both are content-only; bm25 puts the denser mention first.
        assert [m.session_id for m in out] == ["s-strong", "s-weak"]

    def test_exact_tool_match_unchanged_with_service(self, tmp_path):
        # A path that matched before (tool arg) still matches, and read/write
        # modes never touch the content half at all.
        conn, store_db = _make_store(tmp_path)
        pid = _seed_project(conn)
        sfk = _seed_session(conn, pid, "s-edit")
        _add_msg(conn, sfk, 0, content="applied", tools_json=_edit_blob(_PATH))
        conn.commit()
        svc = _index_from_store(store_db, conn)

        out = find_sessions_touching_file(conn, _PATH, mode="any", search_service=svc)
        assert [m.session_id for m in out] == ["s-edit"]
        # write mode: the FTS branch is bypassed entirely (mode != 'any').
        wout = find_sessions_touching_file(conn, _PATH, mode="write", search_service=svc)
        assert [m.session_id for m in wout] == ["s-edit"]

    def test_perf_gate_skips_fts_when_exact_half_is_full(self, tmp_path):
        # The <100ms budget guard: when the exact tool-arg half already fills
        # the page (>= limit sessions), the second-DB FTS open must NOT happen.
        conn, store_db = _make_store(tmp_path)
        pid = _seed_project(conn)
        for i in range(3):
            sfk = _seed_session(conn, pid, f"s-edit-{i}", day=f"2026-05-0{i + 1}")
            _add_msg(conn, sfk, 0, content="edit", tools_json=_edit_blob(_PATH))
        # Also a content-only mention that WOULD surface if FTS ran.
        m = _seed_session(conn, pid, "s-mention", day="2026-05-09")
        _add_msg(conn, m, 0, content=f"we talked about {_PATH} here")
        conn.commit()
        spy = _SpyService(_index_from_store(store_db, conn))

        # limit == exact count → not thin → FTS skipped entirely.
        full = find_sessions_touching_file(
            conn, _PATH, mode="any", limit=3, search_service=spy,
        )
        assert spy.calls == 0
        assert {m.session_id for m in full} == {"s-edit-0", "s-edit-1", "s-edit-2"}

        # A roomier limit makes the exact half thin → FTS is consulted, and
        # the content-only mention surfaces.
        spy.calls = 0
        thin = find_sessions_touching_file(
            conn, _PATH, mode="any", limit=20, search_service=spy,
        )
        assert spy.calls >= 1
        assert "s-mention" in {m.session_id for m in thin}

    def test_unpopulated_index_falls_back_to_like(self, tmp_path):
        conn, store_db = _make_store(tmp_path)
        pid = _seed_project(conn)
        sfk = _seed_session(conn, pid, "s-only")
        _add_msg(conn, sfk, 0, content=f"only mentioned in text: {_PATH}")
        conn.commit()
        # Created-but-empty index → lexical_session_hits returns None → the
        # original combined LIKE scan must still find the content mention.
        empty = SearchService(store_db.parent / "search_index.db")

        out = find_sessions_touching_file(conn, _PATH, mode="any", search_service=empty)
        assert [m.session_id for m in out] == ["s-only"]

    def test_budget_still_governs_the_fts_path(self, tmp_path):
        conn, store_db = _make_store(tmp_path)
        pid = _seed_project(conn)
        for i in range(4):
            sfk = _seed_session(conn, pid, f"s-{i}", day=f"2026-05-0{i + 1}")
            _add_msg(conn, sfk, 0, content=f"see {_PATH} in session {i}", day=f"2026-05-0{i + 1}")
        conn.commit()
        svc = _index_from_store(store_db, conn)

        tight = find_sessions_touching_file(
            conn, _PATH, mode="any", search_service=svc, context_budget=1,
        )
        assert isinstance(tight, BudgetedResult)
        assert tight.truncated is True
        assert len(tight.sessions) == 0

        roomy = find_sessions_touching_file(
            conn, _PATH, mode="any", search_service=svc, context_budget=100_000,
        )
        assert isinstance(roomy, BudgetedResult)
        assert len(roomy.sessions) == 4


# ── find_sessions_where_action_worked — content half via FTS ─────────────────


class TestActionWorkedContentHalf:
    def test_content_mention_of_action_surfaces_with_outcome(self, tmp_path):
        conn, store_db = _make_store(tmp_path)
        pid = _seed_project(conn)
        sfk = _seed_session(conn, pid, "s-work")
        # The action ("caching") appears only in the free-text conversation,
        # never a tool arg. Two mentions → clustering count of 1.
        _add_msg(conn, sfk, 0, role="user", content="let's add caching to the cost route")
        _add_msg(conn, sfk, 1, role="assistant", content="done", tools_json=_edit_blob(_PATH))
        _add_msg(conn, sfk, 2, role="user", content="also caching the other route please")
        _add_msg(conn, sfk, 3, role="assistant", content="done too")
        _add_msg(conn, sfk, 4, role="user", content="thanks, that worked perfectly")
        conn.commit()
        svc = _index_from_store(store_db, conn)

        out = find_sessions_where_action_worked(conn, action="caching", search_service=svc)
        assert [m.session_id for m in out] == ["s-work"]
        m = out[0]
        assert isinstance(m, OutcomeMatch)
        assert m.outcome == "worked"
        # Clustering rides onto the outcome row (two content mentions → +1).
        assert m.more_matches_in_session == 1
        assert m.to_dict()["more_matches_in_session"] == 1

    def test_exact_tool_arg_match_still_works_with_service(self, tmp_path):
        # action == a file fragment matched via the tool-arg (exact) half;
        # the populated index has no content hit for it, so this proves the
        # exact half survives alongside FTS routing.
        conn, store_db = _make_store(tmp_path)
        pid = _seed_project(conn)
        sfk = _seed_session(conn, pid, "s-tool")
        _add_msg(conn, sfk, 0, role="assistant", content="applied the edit",
                 tools_json=_edit_blob("/Users/yad/dev/foo/util.py"))
        _add_msg(conn, sfk, 1, role="user", content="thanks, that worked")
        conn.commit()
        svc = _index_from_store(store_db, conn)

        out = find_sessions_where_action_worked(conn, action="util.py", search_service=svc)
        assert [m.session_id for m in out] == ["s-tool"]
        assert out[0].outcome == "worked"
        # No content clustering for a purely tool-matched session.
        assert out[0].more_matches_in_session is None

    def test_unpopulated_index_falls_back_cleanly(self, tmp_path):
        conn, store_db = _make_store(tmp_path)
        pid = _seed_project(conn)
        sfk = _seed_session(conn, pid, "s-content")
        _add_msg(conn, sfk, 0, role="user", content="let's add caching here")
        _add_msg(conn, sfk, 1, role="assistant", content="done", tools_json=_edit_blob(_PATH))
        _add_msg(conn, sfk, 2, role="user", content="thanks, that worked")
        conn.commit()
        empty = SearchService(store_db.parent / "search_index.db")  # unpopulated

        out = find_sessions_where_action_worked(conn, action="caching", search_service=empty)
        assert [m.session_id for m in out] == ["s-content"]
        assert out[0].outcome == "worked"

    def test_tool_and_later_content_match_anchors_on_the_later_seq(self, tmp_path):
        # A session matched by BOTH halves: an early tool edit (which the next
        # turn confirmed "worked") AND a *later* content mention the following
        # turn says broke. The union anchor must be MAX(seq) — the later
        # content message — so the outcome classifies as failed and the
        # session is NOT surfaced as "worked" (a naive "tool anchor wins"
        # would wrongly report worked).
        conn, store_db = _make_store(tmp_path)
        pid = _seed_project(conn)
        sfk = _seed_session(conn, pid, "s-mixed")
        _add_msg(conn, sfk, 0, role="assistant", content="applying the fix",
                 tools_json=_edit_blob("/x/cost.py"))
        _add_msg(conn, sfk, 1, role="user", content="thanks, that worked")
        _add_msg(conn, sfk, 2, role="assistant", content="I also had to change cost.py again")
        _add_msg(conn, sfk, 3, role="user", content="no, that broke it")
        conn.commit()
        svc = _index_from_store(store_db, conn)

        out = find_sessions_where_action_worked(conn, action="cost.py", search_service=svc)
        assert [m.session_id for m in out] == []

    def test_file_narrowing_still_applies_under_fts(self, tmp_path):
        conn, store_db = _make_store(tmp_path)
        pid = _seed_project(conn)
        here = _seed_session(conn, pid, "s-here")
        _add_msg(conn, here, 0, role="user", content="add caching")
        _add_msg(conn, here, 1, role="assistant", content="ok", tools_json=_edit_blob(_PATH))
        _add_msg(conn, here, 2, role="user", content="thanks, works great")
        other = _seed_session(conn, pid, "s-other", day="2026-05-04")
        _add_msg(conn, other, 0, role="user", content="add caching")
        _add_msg(conn, other, 1, role="assistant", content="ok",
                 tools_json=_edit_blob("/Users/yad/dev/foo/other.py"))
        _add_msg(conn, other, 2, role="user", content="thanks, works great")
        conn.commit()
        svc = _index_from_store(store_db, conn)

        out = find_sessions_where_action_worked(
            conn, action="caching", file_path=_PATH, search_service=svc,
        )
        assert [m.session_id for m in out] == ["s-here"]
