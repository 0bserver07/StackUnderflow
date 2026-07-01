"""Tests for the Ollama-optional embeddings backend + hybrid retrieval.

CRITICAL: not one test here touches the network. The whole feature is
gated on :func:`embeddings.ollama_reachable`, which every test either
points at a dead port (real "absent" behaviour) or monkeypatches. The
vector half is exercised with **injected fake vectors** written straight
into the store, and a fake ``embed_texts`` for the query side — so RRF,
the cosine scan, and the FTS+vector fusion are all verified deterministic
and offline. CI has no Ollama; every assertion below still holds there.
"""

from __future__ import annotations

from pathlib import Path
from unittest import mock

import pytest

from stackunderflow.services import embeddings as emb
from stackunderflow.services.embeddings import EmbeddingStore
from stackunderflow.services.search_service import SearchService

_MODEL = "nomic-embed-text"


# ── RRF merge math ───────────────────────────────────────────────────────────


class TestRRFMerge:
    def test_single_ranking_preserves_order(self):
        # RRF of one list re-scores it in place — this is the property the
        # FTS-only fallback relies on (fuse of one ranking == that ranking).
        merged = emb.rrf_merge([[10, 20, 30]])
        assert [mid for mid, _ in merged] == [10, 20, 30]

    def test_score_is_sum_of_reciprocal_ranks(self):
        # id 1: rank 0 in A, rank 1 in B → 1/60 + 1/61.
        merged = dict(emb.rrf_merge([[1, 2], [2, 1]], k=60))
        assert merged[1] == pytest.approx(1 / 60 + 1 / 61)
        assert merged[2] == pytest.approx(1 / 61 + 1 / 60)

    def test_agreement_wins(self):
        # An id ranked high in both lists beats ids each list ranks once.
        merged = emb.rrf_merge([[1, 2, 3], [1, 4, 5]])
        assert merged[0][0] == 1

    def test_ties_break_by_id_deterministically(self):
        # 2 and 3 both appear once at rank 1 → equal score → id order.
        merged = emb.rrf_merge([[1, 2], [1, 3]])
        ids = [mid for mid, _ in merged]
        assert ids[0] == 1
        assert ids[1:] == [2, 3]

    def test_limit_truncates(self):
        merged = emb.rrf_merge([[1, 2, 3, 4, 5]], limit=2)
        assert len(merged) == 2

    def test_empty_input(self):
        assert emb.rrf_merge([]) == []
        assert emb.rrf_merge([[], []]) == []


# ── cosine (pure python, no numpy) ───────────────────────────────────────────


class TestCosine:
    def test_identical_is_one(self):
        assert emb.cosine([1.0, 2.0, 3.0], [1.0, 2.0, 3.0]) == pytest.approx(1.0)

    def test_orthogonal_is_zero(self):
        assert emb.cosine([1.0, 0.0], [0.0, 1.0]) == pytest.approx(0.0)

    def test_opposite_is_minus_one(self):
        assert emb.cosine([1.0, 0.0], [-1.0, 0.0]) == pytest.approx(-1.0)

    def test_zero_vector_is_zero_not_error(self):
        assert emb.cosine([0.0, 0.0], [1.0, 1.0]) == 0.0

    def test_length_mismatch_is_zero_not_error(self):
        assert emb.cosine([1.0, 2.0], [1.0, 2.0, 3.0]) == 0.0


# ── EmbeddingStore (embeddings.db) round-trip ────────────────────────────────


class TestEmbeddingStore:
    def test_upsert_and_search(self, tmp_path: Path):
        store = EmbeddingStore(tmp_path / "embeddings.db")
        store.upsert_many([(1, [1.0, 0.0, 0.0]), (2, [0.0, 1.0, 0.0])], model=_MODEL)
        assert store.count(_MODEL) == 2
        hits = store.search([0.9, 0.1, 0.0], model=_MODEL, top_k=2)
        assert hits[0][0] == 1  # nearest to the [1,0,0] vector
        assert hits[0][1] > hits[1][1]

    def test_upsert_is_idempotent(self, tmp_path: Path):
        store = EmbeddingStore(tmp_path / "embeddings.db")
        store.upsert_many([(1, [1.0, 0.0])], model=_MODEL)
        store.upsert_many([(1, [0.0, 1.0])], model=_MODEL)  # overwrite
        assert store.count(_MODEL) == 1
        got = dict(store.iter_vectors(model=_MODEL))[1]
        assert got == pytest.approx([0.0, 1.0])

    def test_existing_ids(self, tmp_path: Path):
        store = EmbeddingStore(tmp_path / "embeddings.db")
        store.upsert_many([(5, [1.0]), (7, [1.0])], model=_MODEL)
        assert store.existing_ids(_MODEL) == {5, 7}

    def test_search_empty_store_is_empty(self, tmp_path: Path):
        store = EmbeddingStore(tmp_path / "embeddings.db")
        assert store.search([1.0, 0.0], model=_MODEL) == []

    def test_per_model_isolation(self, tmp_path: Path):
        store = EmbeddingStore(tmp_path / "embeddings.db")
        store.upsert_many([(1, [1.0])], model="a")
        store.upsert_many([(2, [1.0])], model="b")
        assert store.existing_ids("a") == {1}
        assert store.existing_ids("b") == {2}

    def test_delete_missing_prunes_orphans(self, tmp_path: Path):
        store = EmbeddingStore(tmp_path / "embeddings.db")
        store.upsert_many([(1, [1.0]), (2, [1.0]), (3, [1.0])], model=_MODEL)
        removed = store.delete_missing([1, 3])  # 2 is now orphaned
        assert removed == 1
        assert store.existing_ids(_MODEL) == {1, 3}

    def test_corrupt_blob_is_skipped_not_raised(self, tmp_path: Path):
        store = EmbeddingStore(tmp_path / "embeddings.db")
        conn = store._get_conn()
        try:
            # dim says 3 but the blob is only 3 bytes — corrupt.
            conn.execute(
                "INSERT INTO embeddings (message_id, model, dim, vector) "
                "VALUES (1, ?, 3, ?)",
                (_MODEL, b"abc"),
            )
            conn.commit()
        finally:
            conn.close()
        assert list(store.iter_vectors(model=_MODEL)) == []
        assert store.search([1.0, 0.0, 0.0], model=_MODEL) == []


# ── Ollama-absent behaviour (the tested default) ─────────────────────────────


class TestOllamaAbsent:
    def test_reachable_false_on_dead_port(self):
        # A real probe at a dead port: fast, offline, deterministically False.
        assert emb.ollama_reachable("http://127.0.0.1:1", use_cache=False) is False

    def test_embed_texts_returns_none_when_unreachable(self):
        assert (
            emb.embed_texts(["hello"], url="http://127.0.0.1:1") is None
        )

    def test_embed_texts_empty_input_is_empty_list(self):
        # No inputs → no work, no probe, empty list (not None).
        assert emb.embed_texts([]) == []

    def test_embed_new_messages_noop_without_ollama(self, tmp_path: Path):
        svc = SearchService(tmp_path / "search_index.db")
        svc.index_project(
            "proj",
            [{"content": "hi", "type": "user", "session_id": "s1",
              "timestamp": "2026-05-01T00:00:00Z", "model": "m"}],
        )
        conn = svc._get_conn()
        try:
            with mock.patch.object(emb, "ollama_reachable", return_value=False):
                written = emb.embed_new_messages(
                    conn, store=EmbeddingStore(tmp_path / "embeddings.db"),
                )
        finally:
            conn.close()
        assert written == 0

    def test_embed_new_messages_swallows_ollama_error(self, tmp_path: Path):
        # Reachable says yes, but embed_texts blows up → still returns 0.
        svc = SearchService(tmp_path / "search_index.db")
        svc.index_project(
            "proj",
            [{"content": "hi there", "type": "user", "session_id": "s1",
              "timestamp": "2026-05-01T00:00:00Z", "model": "m"}],
        )
        conn = svc._get_conn()
        try:
            with mock.patch.object(emb, "ollama_reachable", return_value=True), \
                 mock.patch.object(
                     emb, "embed_texts", side_effect=RuntimeError("boom")):
                written = emb.embed_new_messages(
                    conn, store=EmbeddingStore(tmp_path / "embeddings.db"),
                )
        finally:
            conn.close()
        assert written == 0


class TestEmbedNewMessagesWithFakeVectors:
    """The write side with a fake ``embed_texts`` — no network."""

    def test_writes_vectors_for_new_messages(self, tmp_path: Path):
        svc = SearchService(tmp_path / "search_index.db")
        svc.index_project(
            "proj",
            [
                {"content": "first message", "type": "user", "session_id": "s1",
                 "timestamp": "2026-05-01T00:00:00Z", "model": "m"},
                {"content": "second message", "type": "assistant",
                 "session_id": "s1", "timestamp": "2026-05-01T00:01:00Z",
                 "model": "m"},
            ],
        )
        store = EmbeddingStore(tmp_path / "embeddings.db")
        conn = svc._get_conn()
        try:
            with mock.patch.object(emb, "ollama_reachable", return_value=True), \
                 mock.patch.object(
                     emb, "embed_texts",
                     return_value=[[0.1, 0.2], [0.3, 0.4]]):
                written = emb.embed_new_messages(conn, store=store)
        finally:
            conn.close()
        assert written == 2
        assert store.existing_ids(_MODEL) == {1, 2}

    def test_second_call_skips_already_embedded(self, tmp_path: Path):
        svc = SearchService(tmp_path / "search_index.db")
        svc.index_project(
            "proj",
            [{"content": "only message", "type": "user", "session_id": "s1",
              "timestamp": "2026-05-01T00:00:00Z", "model": "m"}],
        )
        store = EmbeddingStore(tmp_path / "embeddings.db")
        store.upsert_many([(1, [0.5, 0.5])], model=_MODEL)  # pre-seed id 1
        conn = svc._get_conn()
        try:
            with mock.patch.object(emb, "ollama_reachable", return_value=True), \
                 mock.patch.object(emb, "embed_texts") as embed_mock:
                written = emb.embed_new_messages(conn, store=store)
        finally:
            conn.close()
        assert written == 0
        embed_mock.assert_not_called()  # nothing new → no embed call at all


# ── hybrid_search: FTS-only fallback == today, + injected-vector augmentation ─


def _seed_index(path: Path) -> SearchService:
    svc = SearchService(path)
    svc.index_project(
        "proj",
        [
            {"content": "we fixed the flaky authentication test with a retry",
             "type": "assistant", "session_id": "sess-auth",
             "timestamp": "2026-05-01T00:00:00Z", "model": "m"},
            {"content": "refactored the payment gateway module",
             "type": "assistant", "session_id": "sess-pay",
             "timestamp": "2026-05-02T00:00:00Z", "model": "m"},
            {"content": "the login page CSS was broken on mobile",
             "type": "user", "session_id": "sess-css",
             "timestamp": "2026-05-03T00:00:00Z", "model": "m"},
        ],
    )
    return svc


class TestHybridSearchFallback:
    def test_empty_query_returns_empty(self, tmp_path: Path):
        svc = _seed_index(tmp_path / "search_index.db")
        res = svc.hybrid_search("   ")
        assert res["results"] == []
        assert res["vector_used"] is False

    def test_fts_only_matches_plain_search(self, tmp_path: Path):
        # With no Ollama (dead port), hybrid == FTS: same winning session.
        svc = _seed_index(tmp_path / "search_index.db")
        with mock.patch.object(emb, "ollama_reachable", return_value=False):
            hybrid = svc.hybrid_search("authentication test")
        plain = svc.search("authentication test")
        assert hybrid["vector_used"] is False
        assert hybrid["results"][0]["session_id"] == plain["results"][0]["session_id"]
        assert hybrid["results"][0]["session_id"] == "sess-auth"

    def test_fts_relevance_is_fused_score_float(self, tmp_path: Path):
        svc = _seed_index(tmp_path / "search_index.db")
        with mock.patch.object(emb, "ollama_reachable", return_value=False):
            res = svc.hybrid_search("payment gateway")
        assert res["results"]
        assert isinstance(res["results"][0]["relevance"], float)

    def test_project_filter_applies(self, tmp_path: Path):
        svc = _seed_index(tmp_path / "search_index.db")
        with mock.patch.object(emb, "ollama_reachable", return_value=False):
            res = svc.hybrid_search("authentication", project="no-such-project")
        assert res["results"] == []


class TestHybridSearchWithInjectedVectors:
    """Vector half with fake vectors + fake query embed — no network."""

    def test_semantic_only_hit_surfaces(self, tmp_path: Path):
        # Query has ZERO lexical overlap with the auth message, so FTS
        # finds nothing; the vector half must surface it anyway.
        sidx = tmp_path / "search_index.db"
        edb = tmp_path / "embeddings.db"
        svc = _seed_index(sidx)
        store = EmbeddingStore(edb)
        # search-index ids: 1=auth, 2=pay, 3=css. Point the query vector
        # at id 1's neighbourhood.
        store.upsert_many(
            [(1, [1.0, 0.0, 0.0]), (2, [0.0, 1.0, 0.0]), (3, [0.0, 0.0, 1.0])],
            model=_MODEL,
        )
        with mock.patch.object(emb, "ollama_reachable", return_value=True), \
             mock.patch.object(emb, "embed_texts", return_value=[[0.98, 0.02, 0.0]]), \
             mock.patch.object(emb, "EmbeddingStore", return_value=store):
            res = svc.hybrid_search("zzz nonlexical query", limit=5)
        assert res["vector_used"] is True
        assert "sess-auth" in [r["session_id"] for r in res["results"]]

    def test_vector_agreement_boosts_rank(self, tmp_path: Path):
        # Both FTS and vector favour auth → it ranks first under fusion.
        sidx = tmp_path / "search_index.db"
        edb = tmp_path / "embeddings.db"
        svc = _seed_index(sidx)
        store = EmbeddingStore(edb)
        store.upsert_many(
            [(1, [1.0, 0.0, 0.0]), (2, [0.0, 1.0, 0.0]), (3, [0.0, 0.0, 1.0])],
            model=_MODEL,
        )
        with mock.patch.object(emb, "ollama_reachable", return_value=True), \
             mock.patch.object(emb, "embed_texts", return_value=[[1.0, 0.0, 0.0]]), \
             mock.patch.object(emb, "EmbeddingStore", return_value=store):
            res = svc.hybrid_search("authentication test", limit=5)
        assert res["vector_used"] is True
        assert res["results"][0]["session_id"] == "sess-auth"

    def test_empty_vector_store_degrades_to_fts(self, tmp_path: Path):
        # Ollama up but embeddings.db empty → vector half no-ops, FTS only.
        sidx = tmp_path / "search_index.db"
        edb = tmp_path / "embeddings.db"
        svc = _seed_index(sidx)
        store = EmbeddingStore(edb)  # empty
        with mock.patch.object(emb, "ollama_reachable", return_value=True), \
             mock.patch.object(emb, "embed_texts", return_value=[[1.0, 0.0, 0.0]]), \
             mock.patch.object(emb, "EmbeddingStore", return_value=store):
            res = svc.hybrid_search("authentication test")
        assert res["vector_used"] is False
        assert res["results"][0]["session_id"] == "sess-auth"
