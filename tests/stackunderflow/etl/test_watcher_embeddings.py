"""The watcher's Step 6 embedding hook — best-effort, Ollama-gated.

CRITICAL: no network. The hook (:func:`etl.watcher._embed_new_messages_best_effort`)
is gated on :func:`embeddings.ollama_reachable`; these tests either let
that probe fail naturally (no Ollama on CI) or monkeypatch it, and inject
a fake ``embed_texts`` for the write side. The contract under test is
"never blocks, never raises, and does nothing when Ollama is absent" —
mirroring how the FTS triggers keep the index current without ever
failing an ingest cycle.
"""

from __future__ import annotations

from pathlib import Path
from unittest import mock

from stackunderflow.etl import watcher as _watcher
from stackunderflow.services import embeddings as emb
from stackunderflow.services.embeddings import EmbeddingStore
from stackunderflow.services.search_service import SearchService

_MODEL = "nomic-embed-text"


def _seed_index(path: Path) -> SearchService:
    svc = SearchService(path)
    svc.index_project(
        "proj",
        [{"content": "a message worth embedding", "type": "assistant",
          "session_id": "s1", "timestamp": "2026-05-01T00:00:00Z", "model": "m"}],
    )
    return svc


class TestWatcherEmbeddingHook:
    def test_noop_returns_zero_without_ollama(self):
        # Real probe at whatever the default URL is: on CI there is no
        # Ollama, so this short-circuits to 0 without touching anything.
        with mock.patch.object(emb, "ollama_reachable", return_value=False):
            assert _watcher._embed_new_messages_best_effort() == 0

    def test_never_raises_on_internal_failure(self):
        # Even if reachability lies and the downstream blows up, the hook
        # swallows it and returns 0 — the cycle log line must never crash.
        # SearchService is imported inside the function, so patch it at its
        # source module.
        with mock.patch.object(emb, "ollama_reachable", return_value=True), \
             mock.patch(
                 "stackunderflow.services.search_service.SearchService",
                 side_effect=RuntimeError("boom")):
            assert _watcher._embed_new_messages_best_effort() == 0

    def test_embeds_new_messages_when_available(self, tmp_path: Path, monkeypatch):
        # Point SearchService + EmbeddingStore at tmp files and inject a
        # fake embedder → the hook writes a vector for the new message.
        sidx = tmp_path / "search_index.db"
        edb = tmp_path / "embeddings.db"
        _seed_index(sidx)
        store = EmbeddingStore(edb)

        real_search_service = SearchService

        def _fake_search_service(*a, **k):
            return real_search_service(sidx)

        with mock.patch.object(emb, "ollama_reachable", return_value=True), \
             mock.patch.object(emb, "embed_texts", return_value=[[0.1, 0.2, 0.3]]), \
             mock.patch.object(emb, "EmbeddingStore", return_value=store), \
             mock.patch(
                 "stackunderflow.services.search_service.SearchService",
                 side_effect=_fake_search_service):
            written = _watcher._embed_new_messages_best_effort()

        assert written == 1
        assert store.existing_ids(_MODEL) == {1}
