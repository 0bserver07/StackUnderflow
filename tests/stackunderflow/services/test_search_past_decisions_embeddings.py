"""Integration tests for ``search_past_decisions(use_embeddings=True)``.

These tests verify that the embedding re-rank path:

1. Returns the same rows as the substring filter (it never widens the set);
2. Reorders them according to cosine similarity;
3. Surfaces ``embedding_score`` on each returned ``SessionMatch``;
4. Co-exists cleanly with ``context_budget`` (rank fn uses cosine);
5. Degrades *silently* to substring ranking when Ollama is unreachable.

The Ollama backend (``services/embeddings.py``) is stubbed by
monkey-patching ``embeddings.embed_texts`` with a deterministic
text→vector table — no network, no Ollama, no numpy. This mirrors how
``memory ask`` / ``hybrid_search`` embed via Ollama and fall back to
lexical ranking when it is down.
"""

from __future__ import annotations

import sqlite3

import pytest

from stackunderflow.services import embeddings
from stackunderflow.services.discovery import (
    BudgetedResult,
    search_past_decisions,
)
from stackunderflow.store import db, schema

# ── deterministic embed stub ─────────────────────────────────────────────────


class _EmbedStub:
    """Deterministic stand-in for ``embeddings.embed_texts``.

    Maps each input string to a fixed vector via a substring lookup table
    (so the test can pin "session A ranks above session B"). Anything not
    registered gets a noise vector near-orthogonal to every registered
    one. Records the batches it was asked to embed so a test can assert
    that off-topic (non-substring-matching) text never reached the
    encoder. Aligned 1:1 with the input — one vector per input string,
    including the leading query.
    """

    DIM = 4

    def __init__(self, table: dict[str, list[float]]) -> None:
        self.table = table
        self.calls: list[list[str]] = []

    def __call__(self, texts, *, model=None, **_kw):  # noqa: ARG002 — API compat
        self.calls.append(list(texts))
        return [self._lookup(t) for t in texts]

    def _lookup(self, text: str) -> list[float]:
        key = text.strip().lower()
        for needle, vec in self.table.items():
            if needle in key:
                return list(vec)
        # Fallback: a vector on the last axis, orthogonal to axis-0 hits.
        v = [0.0] * self.DIM
        v[-1] = 1.0
        return v


def _install_embed_stub(
    monkeypatch: pytest.MonkeyPatch, stub: _EmbedStub,
) -> None:
    """Patch the Ollama embed fn so the test never touches the network."""
    monkeypatch.setattr(embeddings, "embed_texts", stub)


def _make_conn(tmp_path) -> sqlite3.Connection:
    """Open a real store at tmp_path and apply migrations."""
    conn = db.connect(tmp_path / "store.db")
    schema.apply(conn)
    return conn


def _seed_project(conn: sqlite3.Connection, slug: str = "-Users-x-app") -> int:
    conn.execute(
        "INSERT INTO projects (provider, slug, path, display_name, "
        " first_seen, last_modified) VALUES ('claude', ?, NULL, ?, 0, 0)",
        (slug, slug),
    )
    return int(conn.execute(
        "SELECT id FROM projects WHERE slug = ?", (slug,),
    ).fetchone()[0])


def _seed_session(
    conn: sqlite3.Connection,
    project_id: int,
    session_id: str,
    *,
    first_ts: str = "2026-05-01T00:00:00+00:00",
    last_ts: str = "2026-05-02T00:00:00+00:00",
) -> int:
    conn.execute(
        "INSERT INTO sessions (project_id, session_id, first_ts, last_ts, "
        " message_count) VALUES (?, ?, ?, ?, 1)",
        (project_id, session_id, first_ts, last_ts),
    )
    return int(conn.execute(
        "SELECT id FROM sessions WHERE session_id = ?", (session_id,),
    ).fetchone()[0])


def _seed_message(
    conn: sqlite3.Connection,
    session_fk: int,
    seq: int,
    *,
    content_text: str,
    timestamp: str = "2026-05-01T12:00:00+00:00",
) -> int:
    conn.execute(
        "INSERT INTO messages (session_fk, seq, timestamp, role, model, "
        " input_tokens, output_tokens, cache_create_tokens, cache_read_tokens, "
        " content_text, tools_json, raw_json, is_sidechain) "
        "VALUES (?, ?, ?, 'assistant', 'claude-x', 0, 0, 0, 0, ?, '[]', '{}', 0)",
        (session_fk, seq, timestamp, content_text),
    )
    return int(conn.execute(
        "SELECT next_id - 1 AS mid FROM _messages_id_seq WHERE rowid_kind = 1"
    ).fetchone()[0])


# ── re-rank assertions ──────────────────────────────────────────────────────


class TestEmbeddingsReRanking:
    def test_use_embeddings_reorders_by_cosine(self, tmp_path, monkeypatch):
        """With the stub crafted so session A's content matches the query
        closely (cosine ≈ 1) and session B's matches loosely (cosine ≈ 0),
        the budgeted re-rank must put A first.
        """
        conn = _make_conn(tmp_path)
        pid = _seed_project(conn)
        sfk_a = _seed_session(conn, pid, "sess-A")
        sfk_b = _seed_session(conn, pid, "sess-B")
        _seed_message(conn, sfk_a, 0,
                      content_text="The watcher exact match decision was clear")
        _seed_message(conn, sfk_b, 0,
                      content_text="The watcher off-topic discussion went elsewhere")
        conn.commit()

        # query "watcher" → axis 0; sess A's text also lands on axis 0;
        # sess B's text lands on axis 3 (orthogonal → cosine 0 → 0.5).
        stub = _EmbedStub({
            "watcher exact match": [1.0, 0.0, 0.0, 0.0],
            "watcher off-topic": [0.0, 0.0, 0.0, 1.0],
            "watcher": [1.0, 0.0, 0.0, 0.0],
        })
        _install_embed_stub(monkeypatch, stub)

        result = search_past_decisions(
            conn, "watcher",
            context_budget=2000,
            use_embeddings=True,
            model_name="stub",
        )
        assert isinstance(result, BudgetedResult)
        ids = [m.session_id for m in result.sessions]
        assert set(ids) == {"sess-A", "sess-B"}
        # A ranks above B under cosine.
        assert ids[0] == "sess-A"
        for m in result.sessions:
            assert m.embedding_score is not None
            assert 0.0 <= m.embedding_score <= 1.0
        score_by_id = {m.session_id: m.embedding_score for m in result.sessions}
        assert score_by_id["sess-A"] > score_by_id["sess-B"]

    def test_default_off_keeps_substring_ranking(self, tmp_path, monkeypatch):
        """Without ``use_embeddings``, the LIKE-density path runs and the
        Ollama backend is never touched."""
        conn = _make_conn(tmp_path)
        pid = _seed_project(conn)
        sfk = _seed_session(conn, pid, "sess-X")
        _seed_message(conn, sfk, 0, content_text="watcher decision worth keeping")
        conn.commit()

        # If we accidentally hit the embedding path this stub records it.
        stub = _EmbedStub({})
        _install_embed_stub(monkeypatch, stub)

        out = search_past_decisions(conn, "watcher")
        assert len(out) == 1
        assert out[0].session_id == "sess-X"
        assert out[0].embedding_score is None
        # No embed call fired.
        assert stub.calls == []

    def test_embedding_score_surfaces_in_to_dict(self, tmp_path, monkeypatch):
        conn = _make_conn(tmp_path)
        pid = _seed_project(conn)
        sfk = _seed_session(conn, pid, "sess-X")
        _seed_message(conn, sfk, 0, content_text="the watcher landed")
        conn.commit()

        stub = _EmbedStub({"watcher": [1.0, 0.0, 0.0, 0.0]})
        _install_embed_stub(monkeypatch, stub)

        out = search_past_decisions(conn, "watcher", use_embeddings=True,
                                    model_name="stub")
        assert isinstance(out, list)  # no context_budget → plain list
        assert len(out) == 1
        d = out[0].to_dict()
        assert "embedding_score" in d
        assert 0.0 <= d["embedding_score"] <= 1.0

    def test_substring_mode_to_dict_omits_score(self, tmp_path):
        conn = _make_conn(tmp_path)
        pid = _seed_project(conn)
        sfk = _seed_session(conn, pid, "sess-X")
        _seed_message(conn, sfk, 0, content_text="watcher again")
        conn.commit()

        out = search_past_decisions(conn, "watcher")
        d = out[0].to_dict()
        # The original 9-key shape stays intact for substring callers.
        assert "embedding_score" not in d

    def test_does_not_widen_candidate_set(self, tmp_path, monkeypatch):
        """An embedding-mode call must still filter on the substring; rows
        with no substring hit never reach the encoder.
        """
        conn = _make_conn(tmp_path)
        pid = _seed_project(conn)
        sfk_a = _seed_session(conn, pid, "sess-match")
        sfk_b = _seed_session(conn, pid, "sess-nomatch")
        _seed_message(conn, sfk_a, 0, content_text="the watcher fired")
        _seed_message(conn, sfk_b, 0, content_text="something unrelated entirely")
        conn.commit()

        stub = _EmbedStub({})
        _install_embed_stub(monkeypatch, stub)

        out = search_past_decisions(conn, "watcher", use_embeddings=True,
                                    model_name="stub")
        ids = [m.session_id for m in out]
        assert ids == ["sess-match"]
        # The non-matching session's text never reached the encoder.
        for batch in stub.calls:
            for text in batch:
                assert "unrelated" not in text

    def test_empty_query_short_circuits(self, tmp_path, monkeypatch):
        conn = _make_conn(tmp_path)
        stub = _EmbedStub({})
        _install_embed_stub(monkeypatch, stub)
        out = search_past_decisions(conn, "", use_embeddings=True,
                                    model_name="stub")
        assert out == []
        # And no embed call was ever fired.
        assert stub.calls == []


# ── graceful degradation when Ollama is unreachable ─────────────────────────


class TestOllamaUnreachableDegrades:
    """``--use-embeddings`` with no Ollama must **not** raise — it silently
    falls back to substring ranking, exactly like ``hybrid_search`` degrades
    to FTS-only.
    """

    def test_embed_texts_none_degrades_to_substring(self, tmp_path, monkeypatch):
        """``embed_texts`` returns ``None`` (Ollama down). The query must
        still succeed, return the substring-matched rows, and carry no
        embedding score — proving the fallback, no exception.
        """
        conn = _make_conn(tmp_path)
        pid = _seed_project(conn)
        # Two sessions, different last_ts so substring ranking (recency)
        # gives a deterministic order we can assert against.
        sfk_recent = _seed_session(
            conn, pid, "sess-recent", last_ts="2026-05-10T00:00:00+00:00",
        )
        sfk_old = _seed_session(
            conn, pid, "sess-old", last_ts="2026-05-01T00:00:00+00:00",
        )
        _seed_message(conn, sfk_recent, 0, content_text="watcher fix, recent")
        _seed_message(conn, sfk_old, 0, content_text="watcher fix, older")
        conn.commit()

        # Simulate Ollama unreachable: embed_texts returns None.
        called = {"n": 0}

        def _down(texts, *, model=None, **_kw):  # noqa: ARG001 — API compat
            called["n"] += 1
            return None

        monkeypatch.setattr(embeddings, "embed_texts", _down)

        # No exception, both rows returned.
        out = search_past_decisions(
            conn, "watcher", context_budget=2000, use_embeddings=True,
        )
        assert isinstance(out, BudgetedResult)
        ids = [m.session_id for m in out.sessions]
        assert set(ids) == {"sess-recent", "sess-old"}
        # embed_texts *was* attempted (candidates had embeddable text) …
        assert called["n"] == 1
        # … but no score attached — the rank fell back to substring/recency.
        for m in out.sessions:
            assert m.embedding_score is None
        # Substring fallback rank = recency-led → recent session first.
        assert ids[0] == "sess-recent"

    def test_plain_list_mode_degrades_without_error(self, tmp_path, monkeypatch):
        """Same fallback in the non-budgeted (plain list) call shape."""
        conn = _make_conn(tmp_path)
        pid = _seed_project(conn)
        sfk = _seed_session(conn, pid, "sess-x")
        _seed_message(conn, sfk, 0, content_text="watcher note here")
        conn.commit()

        monkeypatch.setattr(
            embeddings, "embed_texts",
            lambda texts, *, model=None, **_kw: None,  # noqa: ARG005
        )

        out = search_past_decisions(conn, "watcher", use_embeddings=True)
        assert isinstance(out, list)
        assert [m.session_id for m in out] == ["sess-x"]
        assert out[0].embedding_score is None
