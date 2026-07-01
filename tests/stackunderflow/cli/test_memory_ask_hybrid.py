"""``memory ask`` hybrid path — provenance shape + semantic augmentation.

CRITICAL: no network. The semantic half is driven by fake vectors written
straight into an ``embeddings.db`` beside the tmp store, plus a fake
``embed_texts`` for the query side and a forced-``True`` reachability
probe. This exercises the RRF fusion of the store's substring hits with
the local vector search, and proves every returned chunk carries session
/ date / cost provenance — all offline, so it holds on CI too.

The FTS-only fallback (no Ollama, no index) is covered in
``test_memory_cli.py::TestMemoryAsk``; here we cover the *with-vectors*
half that those tests deliberately don't trigger.
"""

from __future__ import annotations

import json
from pathlib import Path
from unittest import mock

from click.testing import CliRunner

import stackunderflow.deps as deps
from stackunderflow.cli import cli
from stackunderflow.services import embeddings as emb
from stackunderflow.services.embeddings import EmbeddingStore
from stackunderflow.services.search_service import SearchService
from stackunderflow.store import db, schema

_MODEL = "nomic-embed-text"


def _seed_store(store_db: Path) -> None:
    """Two sessions: one the substring query hits, one only vectors reach."""
    conn = db.connect(store_db)
    schema.apply(conn)
    cur = conn.execute(
        "INSERT INTO projects (provider, slug, path, display_name, "
        " first_seen, last_modified) VALUES "
        "('claude', '-Users-yad-dev-foo', NULL, 'foo', 0.0, 0.0)"
    )
    pid = int(cur.lastrowid)
    for sid, day, text in [
        ("s-auth", "2026-05-01",
         "we fixed the flaky authentication test by adding a retry loop"),
        ("s-login", "2026-05-02",
         "reworked the sign-in flow so sessions persist across restarts"),
    ]:
        c = conn.execute(
            "INSERT INTO sessions (project_id, session_id, first_ts, last_ts, "
            " message_count) VALUES (?, ?, ?, ?, 1)",
            (pid, sid, f"{day}T00:00:00+00:00", f"{day}T01:00:00+00:00"),
        )
        sfk = int(c.lastrowid)
        conn.execute(
            "INSERT INTO messages (session_fk, seq, timestamp, role, model, "
            " input_tokens, output_tokens, cache_create_tokens, "
            " cache_read_tokens, content_text, tools_json, raw_json, "
            " is_sidechain) VALUES (?, 0, ?, 'assistant', NULL, 0, 0, 0, 0, ?, "
            " '[]', '{}', 0)",
            (sfk, f"{day}T00:00:00+00:00", text),
        )
    conn.commit()
    conn.close()


def _seed_index_beside_store(store_db: Path) -> Path:
    """Index the same two sessions into ``search_index.db`` beside the store.

    ``memory ask`` derives the index path from ``deps.store_path``'s parent,
    so writing here is what the CLI will actually read.
    """
    index_path = store_db.parent / "search_index.db"
    svc = SearchService(index_path)
    svc.index_project(
        "-Users-yad-dev-foo",
        [
            {"content": "we fixed the flaky authentication test by adding a "
                        "retry loop", "type": "assistant", "session_id": "s-auth",
             "timestamp": "2026-05-01T00:00:00Z", "model": "m"},
            {"content": "reworked the sign-in flow so sessions persist across "
                        "restarts", "type": "assistant", "session_id": "s-login",
             "timestamp": "2026-05-02T00:00:00Z", "model": "m"},
        ],
    )
    return index_path


class TestMemoryAskHybridProvenance:
    def test_semantic_only_hit_surfaces_with_provenance(
        self, tmp_path, monkeypatch,
    ):
        store_db = tmp_path / "store.db"
        _seed_store(store_db)
        _seed_index_beside_store(store_db)

        # search-index ids: 1 = s-auth, 2 = s-login. Seed vectors so the
        # query points at s-login (id 2) — a session the substring query
        # 'authentication' would NOT match. The vector half must pull it in.
        store = EmbeddingStore(tmp_path / "embeddings.db")
        store.upsert_many(
            [(1, [1.0, 0.0]), (2, [0.0, 1.0])], model=_MODEL,
        )

        monkeypatch.setattr(deps, "store_path", store_db)
        with mock.patch.object(emb, "ollama_reachable", return_value=True), \
             mock.patch.object(emb, "embed_texts", return_value=[[0.0, 1.0]]), \
             mock.patch.object(emb, "EmbeddingStore", return_value=store):
            r = CliRunner().invoke(
                cli, ["memory", "ask", "authentication", "--json"],
            )

        assert r.exit_code == 0, r.output
        body = json.loads(r.output)
        assert body["vector_used"] is True
        sids = {row["session_id"] for row in body["results"]}
        # s-auth from the substring hit, s-login from the vector hit.
        assert "s-login" in sids
        # Provenance present on every chunk.
        for row in body["results"]:
            assert "session_id" in row
            assert row["last_ts"].startswith("2026-05")   # date
            assert "cost_usd" in row                       # cost

    def test_note_reports_hybrid_when_vectors_used(self, tmp_path, monkeypatch):
        store_db = tmp_path / "store.db"
        _seed_store(store_db)
        _seed_index_beside_store(store_db)
        store = EmbeddingStore(tmp_path / "embeddings.db")
        store.upsert_many([(1, [1.0, 0.0]), (2, [0.0, 1.0])], model=_MODEL)

        monkeypatch.setattr(deps, "store_path", store_db)
        with mock.patch.object(emb, "ollama_reachable", return_value=True), \
             mock.patch.object(emb, "embed_texts", return_value=[[1.0, 0.0]]), \
             mock.patch.object(emb, "EmbeddingStore", return_value=store):
            r = CliRunner().invoke(
                cli, ["memory", "ask", "authentication", "--json"],
            )
        body = json.loads(r.output)
        assert body["vector_used"] is True
        assert "hybrid" in body["note"].lower()

    def test_ollama_down_is_pure_substring(self, tmp_path, monkeypatch):
        # Same fixtures, but Ollama unreachable → vector half no-ops and the
        # result is exactly the substring hit (s-auth only).
        store_db = tmp_path / "store.db"
        _seed_store(store_db)
        _seed_index_beside_store(store_db)

        monkeypatch.setattr(deps, "store_path", store_db)
        with mock.patch.object(emb, "ollama_reachable", return_value=False):
            r = CliRunner().invoke(
                cli, ["memory", "ask", "authentication", "--json"],
            )
        assert r.exit_code == 0, r.output
        body = json.loads(r.output)
        assert body["vector_used"] is False
        sids = {row["session_id"] for row in body["results"]}
        assert sids == {"s-auth"}
