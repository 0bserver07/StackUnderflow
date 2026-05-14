"""Integration tests for ``search_past_decisions(use_embeddings=True)``.

These tests verify that the embedding re-rank path:

1. Returns the same rows as the substring filter (it never widens the set);
2. Reorders them according to cosine similarity;
3. Surfaces ``embedding_score`` on each returned ``SessionMatch``;
4. Co-exists cleanly with ``context_budget`` (rank fn uses cosine).

The sentence-transformers model is stubbed via monkey-patching
``stackunderflow.services.discovery_embeddings.load_model`` — the real
90 MB MiniLM model is never loaded.
"""

from __future__ import annotations

import sqlite3

import numpy as np
import pytest

from stackunderflow.services import discovery_embeddings
from stackunderflow.services.discovery import (
    BudgetedResult,
    search_past_decisions,
)
from stackunderflow.store import db, schema

# ── deterministic stub model ────────────────────────────────────────────────


class _OrderedStub:
    """Stub model whose embeddings are crafted to enforce a known ranking.

    The encoder maps each text to a vector via a tiny lookup table. Texts
    not in the table get a near-orthogonal vector (low cosine to the
    query). This lets a single test assert "session A ranks above session
    B under embeddings" without depending on the real model's behaviour.
    """

    DIM = 4

    def __init__(self, table: dict[str, list[float]]) -> None:
        self.table = table
        self.encode_calls: list[list[str]] = []

    def encode(
        self,
        texts: list[str],
        *,
        normalize_embeddings: bool = False,  # noqa: ARG002
        convert_to_numpy: bool = True,       # noqa: ARG002
        show_progress_bar: bool = False,     # noqa: ARG002
        **_kw,
    ) -> np.ndarray:
        self.encode_calls.append(list(texts))
        out = np.zeros((len(texts), self.DIM), dtype=np.float32)
        for i, text in enumerate(texts):
            vec = self._lookup(text)
            out[i] = vec
        return out

    def _lookup(self, text: str) -> np.ndarray:
        # Exact-match by stripping whitespace + lower — keeps the table
        # readable in the test. Anything not registered gets a fallback
        # vector that's near-orthogonal to every registered one.
        key = text.strip().lower()
        for haystack, vec in self.table.items():
            if haystack in key:
                v = np.array(vec, dtype=np.float32)
                n = float(np.linalg.norm(v))
                return v / n if n > 0 else v
        # Fallback: a vector pointing in a "noise" direction.
        v = np.zeros(self.DIM, dtype=np.float32)
        v[-1] = 1.0
        return v


@pytest.fixture(autouse=True)
def _clear_model_cache() -> None:
    discovery_embeddings._MODEL_CACHE.clear()
    yield
    discovery_embeddings._MODEL_CACHE.clear()


def _make_conn(tmp_path) -> sqlite3.Connection:
    """Open a real store at tmp_path and apply migrations (so v014 lands)."""
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


def _stub_load_model(monkeypatch: pytest.MonkeyPatch, stub: _OrderedStub) -> None:
    """Replace the lazy model loader so the test never imports torch."""
    monkeypatch.setattr(
        discovery_embeddings, "load_model", lambda _name: stub,
    )


# ── re-rank assertions ──────────────────────────────────────────────────────


class TestEmbeddingsReRanking:
    def test_use_embeddings_reorders_by_cosine(self, tmp_path, monkeypatch):
        """With the stub model crafted so session A's content matches the
        query closely (cosine ≈ 1) and session B's matches loosely
        (cosine ≈ 0.5), the budgeted re-rank must put A first.
        """
        conn = _make_conn(tmp_path)
        pid = _seed_project(conn)
        # Both sessions contain the substring needle but use it
        # differently. The stub aligns "watcher exact match" to the
        # query vector; "watcher off-topic" lands on a noise axis.
        sfk_a = _seed_session(
            conn, pid, "sess-A",
            first_ts="2026-05-01T00:00:00+00:00",
            last_ts="2026-05-02T00:00:00+00:00",
        )
        sfk_b = _seed_session(
            conn, pid, "sess-B",
            first_ts="2026-05-01T00:00:00+00:00",
            last_ts="2026-05-02T00:00:00+00:00",
        )
        _seed_message(conn, sfk_a, 0,
                      content_text="The watcher exact match decision was clear")
        _seed_message(conn, sfk_b, 0,
                      content_text="The watcher off-topic discussion went elsewhere")
        conn.commit()

        # Stub: query "watcher" → vector pointing at axis 0; sess A's
        # text also lands on axis 0; sess B's text lands on axis 3
        # (orthogonal → low cosine).
        stub = _OrderedStub({
            "watcher exact match": [1.0, 0.0, 0.0, 0.0],
            "watcher off-topic": [0.0, 0.0, 0.0, 1.0],
            "watcher": [1.0, 0.0, 0.0, 0.0],
        })
        _stub_load_model(monkeypatch, stub)

        # With context_budget — the budgeted result is rank-ordered.
        result = search_past_decisions(
            conn, "watcher",
            context_budget=2000,
            use_embeddings=True,
            model_name="stub",
        )
        assert isinstance(result, BudgetedResult)
        # Both sessions are kept (small enough to fit 2000-token budget).
        ids = [m.session_id for m in result.sessions]
        assert set(ids) == {"sess-A", "sess-B"}
        # A ranks above B under cosine.
        assert ids[0] == "sess-A"
        # Each surfaced row carries the cosine score.
        for m in result.sessions:
            assert m.embedding_score is not None
            assert 0.0 <= m.embedding_score <= 1.0
        # Session A's score is strictly higher than B's.
        score_by_id = {m.session_id: m.embedding_score for m in result.sessions}
        assert score_by_id["sess-A"] > score_by_id["sess-B"]

    def test_default_off_keeps_substring_ranking(self, tmp_path, monkeypatch):
        """Without ``use_embeddings``, the LIKE-density path runs and
        no encoder is touched."""
        conn = _make_conn(tmp_path)
        pid = _seed_project(conn)
        sfk = _seed_session(conn, pid, "sess-X")
        _seed_message(conn, sfk, 0, content_text="watcher decision worth keeping")
        conn.commit()

        # If we accidentally hit the embedding path the test fails on
        # the model load (no stub installed).
        out = search_past_decisions(conn, "watcher")
        assert len(out) == 1
        assert out[0].session_id == "sess-X"
        # No embedding score on substring-mode rows.
        assert out[0].embedding_score is None

    def test_embedding_score_surfaces_in_to_dict(self, tmp_path, monkeypatch):
        conn = _make_conn(tmp_path)
        pid = _seed_project(conn)
        sfk = _seed_session(conn, pid, "sess-X")
        _seed_message(conn, sfk, 0, content_text="the watcher landed")
        conn.commit()

        stub = _OrderedStub({"watcher": [1.0, 0.0, 0.0, 0.0]})
        _stub_load_model(monkeypatch, stub)

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
        """An embedding-mode call must still filter on the substring;
        rows with no substring hit never reach the encoder.
        """
        conn = _make_conn(tmp_path)
        pid = _seed_project(conn)
        sfk_a = _seed_session(conn, pid, "sess-match")
        sfk_b = _seed_session(conn, pid, "sess-nomatch")
        _seed_message(conn, sfk_a, 0, content_text="the watcher fired")
        _seed_message(conn, sfk_b, 0, content_text="something unrelated entirely")
        conn.commit()

        stub = _OrderedStub({})
        _stub_load_model(monkeypatch, stub)

        out = search_past_decisions(conn, "watcher", use_embeddings=True,
                                    model_name="stub")
        ids = [m.session_id for m in out]
        assert ids == ["sess-match"]
        # The non-matching session's text never reached the encoder.
        for batch in stub.encode_calls:
            for text in batch:
                assert "unrelated" not in text

    def test_empty_query_short_circuits(self, tmp_path, monkeypatch):
        conn = _make_conn(tmp_path)
        stub = _OrderedStub({})
        _stub_load_model(monkeypatch, stub)
        out = search_past_decisions(conn, "", use_embeddings=True,
                                    model_name="stub")
        assert out == []
        # And no encoder was ever invoked.
        assert stub.encode_calls == []


# ── missing dep at the service surface ──────────────────────────────────────


class TestMissingDepSurface:
    def test_missing_extra_propagates_importerror(self, tmp_path, monkeypatch):
        """When the user has not installed the embeddings extra and
        flags ``--use-embeddings``, the service raises
        ``MissingEmbeddingsDependencyError`` (subclass of ``ImportError``).
        """
        conn = _make_conn(tmp_path)
        pid = _seed_project(conn)
        sfk = _seed_session(conn, pid, "s")
        _seed_message(conn, sfk, 0, content_text="watcher here")
        conn.commit()

        # Simulate "the extra isn't installed" by making the lazy
        # require helper raise.
        def _no_st():
            raise discovery_embeddings.MissingEmbeddingsDependencyError()
        monkeypatch.setattr(
            discovery_embeddings, "_require_sentence_transformers", _no_st,
        )

        with pytest.raises(discovery_embeddings.MissingEmbeddingsDependencyError) as exc:
            search_past_decisions(conn, "watcher", use_embeddings=True,
                                  model_name="stub")
        assert "pip install stackunderflow[embeddings]" in str(exc.value)
