"""FTS5/bm25 routing for ``search_past_decisions`` (spec #9).

The structured ``memory`` commands used to run a leading-wildcard
``content_text LIKE '%needle%'`` full scan. These tests prove the lexical
path now goes through ``SearchService``'s FTS5 + bm25 index when one is
injected and populated, that it clusters chatty sessions, that it degrades
to the LIKE scan only when the index isn't populated, and that the query
sanitiser neutralises FTS5 operators.

No network: the search index is a plain on-disk SQLite FTS5 table built
with ``SearchService.index_project`` (no embeddings involved), beside a
real store built with ``db.connect`` + ``schema.apply``.
"""

from __future__ import annotations

import sqlite3
from pathlib import Path

from stackunderflow.services.discovery import (
    BudgetedResult,
    search_past_decisions,
)
from stackunderflow.services.search_service import (
    SearchService,
    search_has_intent,
)
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


def _add_message(
    conn: sqlite3.Connection, sfk: int, seq: int, content: str,
    day: str = "2026-05-01",
) -> None:
    conn.execute(
        "INSERT INTO messages (session_fk, seq, timestamp, role, model, "
        " input_tokens, output_tokens, cache_create_tokens, cache_read_tokens, "
        " content_text, tools_json, raw_json, is_sidechain) "
        "VALUES (?, ?, ?, 'assistant', NULL, 0, 0, 0, 0, ?, '[]', '{}', 0)",
        (sfk, seq, f"{day}T00:{seq:02d}:00+00:00", content),
    )


def _build_index(store_db: Path, entries: list[dict], slug: str = "-Users-yad-dev-foo") -> SearchService:
    """Index ``entries`` into ``search_index.db`` beside the store.

    Each entry is ``{"session_id", "content"}`` (+ optional day). Returns
    the populated SearchService pointed at that index.
    """
    index_path = store_db.parent / "search_index.db"
    svc = SearchService(index_path)
    svc.index_project(slug, [
        {
            "content": e["content"],
            "type": "assistant",
            "session_id": e["session_id"],
            "timestamp": f"{e.get('day', '2026-05-01')}T00:00:00Z",
            "model": "m",
        }
        for e in entries
    ])
    return svc


class _RecordingConn:
    """sqlite3 connection proxy that records every ``execute`` SQL string.

    Lets a test prove the FTS path issues **no** ``content_text LIKE`` scan.
    Delegates everything else (commit/close/cursor) to the real connection;
    ``row_factory`` is proxied both ways so ``_ensure_row_factory`` works.
    """

    def __init__(self, real: sqlite3.Connection):
        self._real = real
        self.sql: list[str] = []

    def execute(self, sql, params=()):  # noqa: ANN001
        self.sql.append(sql)
        return self._real.execute(sql, params)

    @property
    def row_factory(self):
        return self._real.row_factory

    @row_factory.setter
    def row_factory(self, value):  # noqa: ANN001
        self._real.row_factory = value

    def __getattr__(self, name):  # noqa: ANN001
        return getattr(self._real, name)


# ── search_has_intent ────────────────────────────────────────────────────────


class TestSearchHasIntent:
    def test_empty_and_punctuation_have_no_intent(self):
        for q in ("", "   ", "!!!", "***", '"', "()", "- , .", None):
            assert search_has_intent(q) is False

    def test_words_have_intent(self):
        for q in ("a", "sqlite", "use NOT null", "cost.py", "café"):
            assert search_has_intent(q) is True


# ── _sanitize_fts_query neutralisation ───────────────────────────────────────


class TestSanitizeFtsQuery:
    def test_operators_become_literal_terms(self, tmp_path):
        svc = SearchService(tmp_path / "idx.db")
        # NOT is quoted as a literal term, never left as a bare operator.
        assert svc._sanitize_fts_query("use NOT null") == '"use"* "NOT"* "null"*'

    def test_stray_star_and_quote_are_dropped(self, tmp_path):
        svc = SearchService(tmp_path / "idx.db")
        # A bare ``*`` / unbalanced ``"`` can no longer reach the parser.
        assert svc._sanitize_fts_query('cache *') == '"cache"*'
        assert svc._sanitize_fts_query('a"b') == '"a"* "b"*'

    def test_punctuation_only_is_match_nothing(self, tmp_path):
        svc = SearchService(tmp_path / "idx.db")
        assert svc._sanitize_fts_query("!!!") == '""'
        assert svc._sanitize_fts_query("   ") == '""'

    def test_operator_free_query_is_byte_identical_to_old_form(self, tmp_path):
        svc = SearchService(tmp_path / "idx.db")
        assert svc._sanitize_fts_query("authentication test") == (
            '"authentication"* "test"*'
        )

    def test_operator_query_searches_literally_not_as_syntax(self, tmp_path):
        # A doc containing all three words must MATCH "use NOT null". Under
        # the old pass-through, FTS5 read it as ``use AND NOT null`` and
        # EXCLUDED the doc (total 0). Neutralised, it matches (total >= 1).
        idx = tmp_path / "search_index.db"
        svc = SearchService(idx)
        svc.index_project("-Users-yad-dev-foo", [{
            "content": "remember to use NOT null constraints on the id column",
            "type": "assistant", "session_id": "s-null",
            "timestamp": "2026-05-01T00:00:00Z", "model": "m",
        }])
        assert svc.search("use NOT null")["total"] >= 1


# ── lexical_session_hits ─────────────────────────────────────────────────────


class TestLexicalSessionHits:
    def test_unpopulated_index_returns_none(self, tmp_path):
        svc = SearchService(tmp_path / "search_index.db")  # created, empty
        assert svc.lexical_session_hits("anything") is None

    def test_populated_no_match_returns_empty_list(self, tmp_path):
        store_db = tmp_path / "store.db"
        svc = _build_index(store_db, [
            {"session_id": "s1", "content": "totally unrelated content"},
        ])
        assert svc.lexical_session_hits("flywheel") == []

    def test_clusters_and_counts_per_session(self, tmp_path):
        store_db = tmp_path / "store.db"
        svc = _build_index(store_db, [
            {"session_id": "s-chatty", "content": "widget one"},
            {"session_id": "s-chatty", "content": "widget two"},
            {"session_id": "s-chatty", "content": "widget three"},
        ])
        hits = svc.lexical_session_hits("widget")
        assert len(hits) == 1
        assert hits[0]["session_id"] == "s-chatty"
        assert hits[0]["more_matches_in_session"] == 2


# ── search_past_decisions routed through FTS ─────────────────────────────────


class TestSearchPastDecisionsFts:
    def test_bm25_orders_and_no_like_scan_on_hot_path(self, tmp_path):
        conn, store_db = _make_store(tmp_path)
        pid = _seed_project(conn)
        # Two sessions, SAME last_ts (recency ties) and no cost — so the
        # only rank differentiator is bm25 relevance.
        strong = _seed_session(conn, pid, "s-strong")
        weak = _seed_session(conn, pid, "s-weak")
        _add_message(conn, strong, 0, "sqlite sqlite sqlite is the store")
        _add_message(
            conn, weak, 0,
            "we discussed many unrelated topics one of which touched on "
            "sqlite among a long rambling list of other matters entirely",
        )
        conn.commit()
        svc = _build_index(store_db, [
            {"session_id": "s-strong", "content": "sqlite sqlite sqlite is the store"},
            {"session_id": "s-weak",
             "content": "we discussed many unrelated topics one of which "
                        "touched on sqlite among a long rambling list of "
                        "other matters entirely"},
        ])

        rec = _RecordingConn(conn)
        result = search_past_decisions(
            rec, "sqlite", search_service=svc, context_budget=100_000,
        )
        assert isinstance(result, BudgetedResult)
        assert [m.session_id for m in result.sessions] == ["s-strong", "s-weak"]
        # The whole point: the leading-wildcard scan is gone from this path.
        assert not any("content_text LIKE" in s for s in rec.sql), rec.sql

    def test_clustering_surfaces_more_matches(self, tmp_path):
        conn, store_db = _make_store(tmp_path)
        pid = _seed_project(conn)
        sfk = _seed_session(conn, pid, "s-chatty")
        for i in range(3):
            _add_message(conn, sfk, i, f"the widget number {i} broke")
        conn.commit()
        svc = _build_index(store_db, [
            {"session_id": "s-chatty", "content": f"the widget number {i} broke"}
            for i in range(3)
        ])

        out = search_past_decisions(conn, "widget", search_service=svc)
        assert len(out) == 1
        assert out[0].session_id == "s-chatty"
        assert out[0].more_matches_in_session == 2
        assert out[0].to_dict()["more_matches_in_session"] == 2

    def test_unpopulated_index_falls_back_to_like(self, tmp_path):
        conn, store_db = _make_store(tmp_path)
        pid = _seed_project(conn)
        sfk = _seed_session(conn, pid, "s-only")
        _add_message(conn, sfk, 0, "we chose the flywheel design")
        conn.commit()
        # Empty (created-but-not-indexed) service → None from lexical hits
        # → LIKE fallback must still find the store row.
        empty_svc = SearchService(store_db.parent / "search_index.db")

        out = search_past_decisions(conn, "flywheel", search_service=empty_svc)
        assert [m.session_id for m in out] == ["s-only"]

    def test_populated_but_no_match_does_not_rescan_store(self, tmp_path):
        # The index is populated (with an unrelated session) and does NOT
        # contain the needle — even though the STORE does. FTS is
        # authoritative: return empty, never fall back to the LIKE scan.
        conn, store_db = _make_store(tmp_path)
        pid = _seed_project(conn)
        sfk = _seed_session(conn, pid, "s-store-only")
        _add_message(conn, sfk, 0, "the flywheel lives only in the store")
        conn.commit()
        svc = _build_index(store_db, [
            {"session_id": "s-indexed", "content": "an unrelated indexed note"},
        ])

        rec = _RecordingConn(conn)
        out = search_past_decisions(rec, "flywheel", search_service=svc)
        assert out == []
        assert not any("content_text LIKE" in s for s in rec.sql), rec.sql

    def test_budget_still_governs_the_fts_path(self, tmp_path):
        conn, store_db = _make_store(tmp_path)
        pid = _seed_project(conn)
        entries = []
        for i in range(5):
            sfk = _seed_session(conn, pid, f"s-{i}", day=f"2026-05-0{i + 1}")
            _add_message(conn, sfk, 0, "shared needle token here", day=f"2026-05-0{i + 1}")
            entries.append({"session_id": f"s-{i}", "content": "shared needle token here",
                            "day": f"2026-05-0{i + 1}"})
        conn.commit()
        svc = _build_index(store_db, entries)

        tight = search_past_decisions(conn, "needle", search_service=svc, context_budget=1)
        assert isinstance(tight, BudgetedResult)
        assert tight.truncated is True
        assert len(tight.sessions) == 0
        assert tight.more_available == 5

        roomy = search_past_decisions(
            conn, "needle", search_service=svc, context_budget=100_000,
        )
        assert len(roomy.sessions) == 5
        assert roomy.truncated is False

    def test_embeddings_mode_ignores_injected_service(self, tmp_path, monkeypatch):
        # use_embeddings=True must keep its own substring+cosine pipeline —
        # the injected FTS service is bypassed (search_service is only the
        # lexical path). With Ollama unreachable it degrades to substring,
        # so the store row still surfaces via the LIKE scan.
        from stackunderflow.services import embeddings as emb

        conn, store_db = _make_store(tmp_path)
        pid = _seed_project(conn)
        sfk = _seed_session(conn, pid, "s-emb")
        _add_message(conn, sfk, 0, "the caching decision we made")
        conn.commit()
        svc = _build_index(store_db, [
            {"session_id": "s-emb", "content": "the caching decision we made"},
        ])
        monkeypatch.setattr(emb, "embed_texts", lambda *a, **k: None)

        out = search_past_decisions(
            conn, "caching", search_service=svc, use_embeddings=True,
        )
        assert [m.session_id for m in out] == ["s-emb"]
