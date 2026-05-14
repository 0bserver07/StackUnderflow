"""Unit tests for ``stackunderflow.services.discovery_embeddings``.

These tests exercise the pull-through cache + scoring backend without
actually loading the 90 MB sentence-transformers model. A
``_DeterministicStub`` model class produces reproducible vectors from
text content so the cache hit/miss + cosine math can be asserted
exactly.
"""

from __future__ import annotations

import sqlite3
from datetime import UTC, datetime

import pytest

np = pytest.importorskip("numpy")

from stackunderflow.services import discovery_embeddings as emb
from stackunderflow.store import db, schema

# ── stub model ──────────────────────────────────────────────────────────────


class _DeterministicStub:
    """Tiny stand-in for ``sentence_transformers.SentenceTransformer``.

    Produces a fixed-dim float32 vector for each input text by hashing
    short token features. Deterministic across runs so the cosine
    asserts can pin exact values. Vectors are L2-normalised after
    construction so the dot product reduces to cosine, matching what
    the real ``normalize_embeddings=True`` flag delivers.

    The ``encode`` signature mirrors the real class's interface that
    this module touches — extra kwargs are accepted and ignored so the
    stub keeps working if upstream adds parameters.
    """

    DIM = 8

    def __init__(self, name: str = "stub") -> None:
        self.name = name
        self.encode_calls: list[list[str]] = []

    def encode(
        self,
        texts: list[str],
        *,
        normalize_embeddings: bool = False,  # noqa: ARG002 — interface compat
        convert_to_numpy: bool = True,       # noqa: ARG002 — interface compat
        show_progress_bar: bool = False,     # noqa: ARG002 — interface compat
        **_kwargs,
    ) -> np.ndarray:
        self.encode_calls.append(list(texts))
        out = np.zeros((len(texts), self.DIM), dtype=np.float32)
        for i, text in enumerate(texts):
            vec = self._vec_for(text)
            out[i] = vec
        return out

    def _vec_for(self, text: str) -> np.ndarray:
        """Deterministic mapping ``text → unit vector``.

        Hashes word tokens into the vector dimensions so synonyms with
        overlapping words land near each other (good enough for an
        ordering assertion) and unrelated text lands far apart. Empty
        input maps to a fixed non-zero vector so divide-by-zero doesn't
        bite the normalisation step.
        """
        vec = np.zeros(self.DIM, dtype=np.float32)
        words = (text.lower() or "_empty_").split()
        if not words:
            words = ["_empty_"]
        for word in words:
            idx = hash(word) % self.DIM
            vec[idx] += 1.0
        norm = float(np.linalg.norm(vec))
        if norm == 0.0:
            vec[0] = 1.0
            return vec
        return vec / norm


@pytest.fixture(autouse=True)
def _clear_model_cache() -> None:
    """Each test gets a fresh empty model cache.

    The module-level ``_MODEL_CACHE`` persists across tests within the
    same process; without this fixture a test that monkey-patches the
    stub model would leak it into the next test's load_model call.
    """
    emb._MODEL_CACHE.clear()
    yield
    emb._MODEL_CACHE.clear()


def _make_conn(tmp_path) -> sqlite3.Connection:
    """Open a real store at tmp_path and apply migrations (so v014 lands)."""
    conn = db.connect(tmp_path / "store.db")
    schema.apply(conn)
    return conn


def _seed_messages(conn: sqlite3.Connection, texts: dict[int, str]) -> dict[int, int]:
    """Seed ``texts`` under one project + session; return ``{seq: message_id}``.

    Tests want a stable mapping from "the message about X" to the
    actual generated message_id. The seq number is the test-facing key.
    """
    conn.execute(
        "INSERT INTO projects (provider, slug, path, display_name, "
        " first_seen, last_modified) VALUES ('claude', '-Users-x', NULL, '-Users-x', 0, 0)"
    )
    pid = conn.execute("SELECT id FROM projects WHERE slug='-Users-x'").fetchone()[0]
    conn.execute(
        "INSERT INTO sessions (project_id, session_id, first_ts, last_ts, "
        " message_count) VALUES (?, 's-1', '2026-05-01T00:00:00+00:00', "
        " '2026-05-01T01:00:00+00:00', ?)",
        (pid, len(texts)),
    )
    sfk = conn.execute("SELECT id FROM sessions WHERE session_id='s-1'").fetchone()[0]
    out: dict[int, int] = {}
    for seq, text in texts.items():
        conn.execute(
            "INSERT INTO messages (session_fk, seq, timestamp, role, model, "
            " input_tokens, output_tokens, cache_create_tokens, cache_read_tokens, "
            " content_text, tools_json, raw_json, is_sidechain) "
            "VALUES (?, ?, '2026-05-01T00:00:00+00:00', 'assistant', 'claude-x', "
            "  0, 0, 0, 0, ?, '[]', '{}', 0)",
            (sfk, seq, text),
        )
        mid = conn.execute(
            "SELECT next_id - 1 AS mid FROM _messages_id_seq WHERE rowid_kind = 1"
        ).fetchone()[0]
        out[seq] = int(mid)
    conn.commit()
    return out


# ── resolve_model_name ──────────────────────────────────────────────────────


class TestResolveModelName:
    def test_default(self, monkeypatch):
        monkeypatch.delenv("STACKUNDERFLOW_EMBED_MODEL", raising=False)
        assert emb.resolve_model_name() == emb.DEFAULT_MODEL_NAME

    def test_explicit_override_wins(self, monkeypatch):
        monkeypatch.setenv("STACKUNDERFLOW_EMBED_MODEL", "ignored")
        assert emb.resolve_model_name("explicit-id") == "explicit-id"

    def test_env_var_overrides_default(self, monkeypatch):
        monkeypatch.setenv("STACKUNDERFLOW_EMBED_MODEL", "custom/model")
        assert emb.resolve_model_name() == "custom/model"

    def test_empty_env_is_ignored(self, monkeypatch):
        monkeypatch.setenv("STACKUNDERFLOW_EMBED_MODEL", "")
        assert emb.resolve_model_name() == emb.DEFAULT_MODEL_NAME

    def test_whitespace_env_is_ignored(self, monkeypatch):
        monkeypatch.setenv("STACKUNDERFLOW_EMBED_MODEL", "   ")
        assert emb.resolve_model_name() == emb.DEFAULT_MODEL_NAME


# ── missing-dep gate ────────────────────────────────────────────────────────


class TestMissingDep:
    def test_load_model_raises_with_install_hint_when_missing(self, monkeypatch):
        # Force the lazy import to fail by patching the helper.
        def _boom():
            raise emb.MissingEmbeddingsDependencyError()
        monkeypatch.setattr(emb, "_require_sentence_transformers", _boom)
        with pytest.raises(emb.MissingEmbeddingsDependencyError) as exc:
            emb.load_model("any-model")
        assert "pip install stackunderflow[embeddings]" in str(exc.value)

    def test_compute_or_load_raises_when_numpy_missing(self, monkeypatch):
        def _boom():
            raise emb.MissingEmbeddingsDependencyError()
        monkeypatch.setattr(emb, "_require_numpy", _boom)
        # Even an empty message_ids list still trips the numpy gate
        # because we don't want a silent no-op disguising a missing dep.
        with pytest.raises(emb.MissingEmbeddingsDependencyError):
            emb.compute_or_load(sqlite3.connect(":memory:"), [1], "stub")

    def test_missing_error_subclasses_importerror(self):
        # The CLI's ``except ImportError`` and the MCP's mirroring rely
        # on the inheritance chain — pin it.
        assert issubclass(emb.MissingEmbeddingsDependencyError, ImportError)


# ── pull-through cache ──────────────────────────────────────────────────────


class TestComputeOrLoad:
    def test_empty_message_ids_short_circuits(self, tmp_path, monkeypatch):
        conn = _make_conn(tmp_path)
        stub = _DeterministicStub()
        monkeypatch.setattr(emb, "load_model", lambda _name: stub)
        out = emb.compute_or_load(conn, [], "stub")
        assert out == {}
        # Confirm the stub was not invoked.
        assert stub.encode_calls == []

    def test_first_call_computes_and_persists(self, tmp_path, monkeypatch):
        conn = _make_conn(tmp_path)
        ids = _seed_messages(conn, {0: "watcher inotify decision",
                                    1: "unrelated topic"})
        stub = _DeterministicStub()
        monkeypatch.setattr(emb, "load_model", lambda _name: stub)

        result = emb.compute_or_load(
            conn, [ids[0], ids[1]], "stub-model-v1",
        )

        assert set(result.keys()) == {ids[0], ids[1]}
        assert result[ids[0]].shape == (_DeterministicStub.DIM,)
        # Each vector should be unit-normalised by the stub.
        for v in result.values():
            assert abs(float(np.linalg.norm(v)) - 1.0) < 1e-5
        # One batch encode, two texts.
        assert len(stub.encode_calls) == 1
        assert len(stub.encode_calls[0]) == 2
        # Persisted to the table.
        rows = conn.execute(
            "SELECT message_id, embedding_dim FROM discovery_embeddings"
        ).fetchall()
        assert sorted(int(r[0]) for r in rows) == sorted([ids[0], ids[1]])
        for r in rows:
            assert int(r[1]) == _DeterministicStub.DIM

    def test_second_call_loads_from_cache(self, tmp_path, monkeypatch):
        conn = _make_conn(tmp_path)
        ids = _seed_messages(conn, {0: "watcher inotify decision"})
        stub = _DeterministicStub()
        monkeypatch.setattr(emb, "load_model", lambda _name: stub)

        emb.compute_or_load(conn, [ids[0]], "stub-model-v1")
        first_calls = list(stub.encode_calls)

        # Second call against the same id should not re-encode.
        result = emb.compute_or_load(conn, [ids[0]], "stub-model-v1")
        assert ids[0] in result
        assert stub.encode_calls == first_calls  # no additional encode

    def test_mixed_cache_hit_and_miss(self, tmp_path, monkeypatch):
        conn = _make_conn(tmp_path)
        ids = _seed_messages(conn, {0: "first", 1: "second", 2: "third"})
        stub = _DeterministicStub()
        monkeypatch.setattr(emb, "load_model", lambda _name: stub)

        # Prime the cache with just id 0 and 1.
        emb.compute_or_load(conn, [ids[0], ids[1]], "model-a")
        prime_calls = list(stub.encode_calls)
        # The follow-up call asks for all three — only id 2 should be
        # batched into the encoder.
        emb.compute_or_load(conn, [ids[0], ids[1], ids[2]], "model-a")
        new_call = stub.encode_calls[len(prime_calls)]
        assert len(new_call) == 1  # one new text encoded

    def test_different_models_isolated_in_cache(self, tmp_path, monkeypatch):
        conn = _make_conn(tmp_path)
        ids = _seed_messages(conn, {0: "shared text"})
        stub = _DeterministicStub()
        monkeypatch.setattr(emb, "load_model", lambda _name: stub)

        emb.compute_or_load(conn, [ids[0]], "model-a")
        emb.compute_or_load(conn, [ids[0]], "model-b")
        # Both models recorded their own row keyed on model_name.
        rows = conn.execute(
            "SELECT DISTINCT model_name FROM discovery_embeddings"
        ).fetchall()
        names = sorted(r[0] for r in rows)
        assert names == ["model-a", "model-b"]
        # Two encodes (one per model) — cache didn't cross.
        assert len(stub.encode_calls) == 2

    def test_dedup_message_ids(self, tmp_path, monkeypatch):
        conn = _make_conn(tmp_path)
        ids = _seed_messages(conn, {0: "the same"})
        stub = _DeterministicStub()
        monkeypatch.setattr(emb, "load_model", lambda _name: stub)

        # Pass the same id three times.
        emb.compute_or_load(conn, [ids[0], ids[0], ids[0]], "model-x")
        # Only one text encoded.
        assert stub.encode_calls == [["the same"]]

    def test_missing_text_skipped_silently(self, tmp_path, monkeypatch):
        conn = _make_conn(tmp_path)
        ids = _seed_messages(conn, {0: "present"})
        stub = _DeterministicStub()
        monkeypatch.setattr(emb, "load_model", lambda _name: stub)

        # 999999 isn't a real message; it should be dropped.
        result = emb.compute_or_load(conn, [ids[0], 999999], "m")
        assert set(result.keys()) == {ids[0]}


# ── scoring math ────────────────────────────────────────────────────────────


class TestScoring:
    def test_identical_vectors_score_one(self):
        v = np.array([0.6, 0.0, 0.8], dtype=np.float32)
        out = emb.score_against_query(v, {7: v.copy()})
        assert abs(out[7] - 1.0) < 1e-5

    def test_orthogonal_vectors_score_half(self):
        # cosine = 0 → mapped to 0.5
        q = np.array([1.0, 0.0, 0.0], dtype=np.float32)
        v = np.array([0.0, 1.0, 0.0], dtype=np.float32)
        out = emb.score_against_query(q, {3: v})
        assert abs(out[3] - 0.5) < 1e-5

    def test_antiparallel_vectors_score_zero(self):
        # cosine = -1 → mapped to 0.0
        q = np.array([1.0, 0.0], dtype=np.float32)
        v = np.array([-1.0, 0.0], dtype=np.float32)
        out = emb.score_against_query(q, {1: v})
        assert abs(out[1] - 0.0) < 1e-5

    def test_returns_in_zero_to_one(self):
        q = np.array([0.8, 0.6, 0.0], dtype=np.float32)
        candidates = {
            1: np.array([0.8, 0.6, 0.0], dtype=np.float32),
            2: np.array([-0.6, 0.8, 0.0], dtype=np.float32),
            3: np.array([0.0, 0.0, 1.0], dtype=np.float32),
        }
        out = emb.score_against_query(q, candidates)
        for s in out.values():
            assert 0.0 <= s <= 1.0

    def test_shape_mismatch_scores_zero(self):
        # A cached vector from a different model (different dim) must
        # not blow up the whole call — that row scores 0.
        q = np.array([1.0, 0.0], dtype=np.float32)
        v_wrong = np.array([1.0, 0.0, 0.0], dtype=np.float32)
        out = emb.score_against_query(q, {1: v_wrong})
        assert out[1] == 0.0


# ── corrupt cache row ───────────────────────────────────────────────────────


class TestCorruptCache:
    def test_dim_mismatch_logged_and_skipped(self, tmp_path, monkeypatch, caplog):
        """A row whose blob length disagrees with embedding_dim is dropped
        from the cached set so the candidate gets re-embedded fresh.
        """
        conn = _make_conn(tmp_path)
        ids = _seed_messages(conn, {0: "stuff"})
        # Inject a corrupt row.
        now_iso = datetime.now(UTC).isoformat()
        conn.execute(
            "INSERT INTO discovery_embeddings "
            "(session_id, message_id, model_name, embedding, embedding_dim, created_ts) "
            "VALUES ('s-1', ?, 'mx', ?, 99, ?)",
            (ids[0], np.zeros(4, dtype=np.float32).tobytes(), now_iso),
        )
        conn.commit()
        stub = _DeterministicStub()
        monkeypatch.setattr(emb, "load_model", lambda _name: stub)
        with caplog.at_level("WARNING"):
            result = emb.compute_or_load(conn, [ids[0]], "mx")
        # The corrupt row gets skipped → fresh compute kicks in.
        assert ids[0] in result
        assert result[ids[0]].shape == (_DeterministicStub.DIM,)
        # And it was logged.
        assert any("corrupt" in r.message.lower() for r in caplog.records)
