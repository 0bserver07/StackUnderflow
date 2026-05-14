"""Opt-in semantic-search embeddings for the discovery surface.

This module is the pull-through cache + scoring backend for the
``--use-embeddings`` mode on ``search-past-decisions``. It is **never
imported at top level** by anything in the hot path — every entry point
defers ``import`` until the caller actually flags semantic mode on,
because the underlying sentence-transformers + torch import is heavy
(seconds, hundreds of MB resident).

Design
------
The substring filter in ``search_past_decisions`` still runs first. The
results of that filter — a small candidate set of message ids — are then
embedded (with caching) and re-ranked by cosine similarity against the
query embedding. So the embedding cost is bounded by the candidate set
size, not the whole store, and the cache means a second invocation
against the same set is just a SELECT.

Optional dependency
-------------------
``sentence-transformers`` and its transitive ``numpy`` / ``torch`` are
**optional** — they ride in via ``pip install stackunderflow[embeddings]``.
A user who never passes ``--use-embeddings`` pays nothing. A user who
does pay gets the model load on first call; subsequent calls in the same
process reuse the loaded model.

``_require_sentence_transformers`` (called inside every entry point that
needs it) raises a ``MissingEmbeddingsDependencyError`` with the exact
install hint when the import fails. The CLI catches that and emits a
clean ``SystemExit`` so the user never sees a bare traceback.

Storage shape
-------------
See ``store/migrations/v014_discovery_embeddings.sql`` for the full
``discovery_embeddings`` table. The vector goes in as a raw
``numpy.float32`` byte buffer (``arr.tobytes()`` / ``np.frombuffer``);
``embedding_dim`` is recorded separately so a corrupt blob can be caught
at read time.

Re-ranking math
---------------
Cosine similarity, normalised to ``[0, 1]``. sentence-transformers
``encode(..., normalize_embeddings=True)`` returns unit-length vectors,
so the cosine reduces to a plain dot product. We then map ``[-1, 1] →
[0, 1]`` with ``(x + 1) / 2`` so the score plugs cleanly into the
existing ``pack_within_budget`` rank fn (which expects each component in
``[0, 1]``). When ``--use-embeddings`` is set this score **replaces** the
LIKE-match-density relevance term; recency and cost weights are unchanged.
"""

from __future__ import annotations

import logging
import os
import sqlite3
from datetime import UTC, datetime
from typing import TYPE_CHECKING, Any

if TYPE_CHECKING:
    import numpy as np  # noqa: F401  # only for type checkers; runtime stays lazy

__all__ = [
    "DEFAULT_MODEL_NAME",
    "INSTALL_HINT",
    "MissingEmbeddingsDependencyError",
    "compute_or_load",
    "embed_query",
    "load_model",
    "resolve_model_name",
    "score_against_query",
]


_log = logging.getLogger(__name__)


# ── public constants ────────────────────────────────────────────────────────


# 90 MB, 384-dim, well-trusted general-purpose sentence model. Override
# via the ``STACKUNDERFLOW_EMBED_MODEL`` env var if you want a different
# model — the table is keyed on ``model_name`` so vectors from different
# models live side-by-side without collision.
DEFAULT_MODEL_NAME = "sentence-transformers/all-MiniLM-L6-v2"


# Single source of truth for the "you need the extra" message. Used in
# the exception, in the CLI's SystemExit message, and in the MCP error
# response. Keep it minimal — one line, one command.
INSTALL_HINT = (
    "Semantic search requires the optional embeddings extra. "
    "Install with `pip install stackunderflow[embeddings]` to use "
    "--use-embeddings."
)


# ── optional-dep gate ───────────────────────────────────────────────────────


class MissingEmbeddingsDependencyError(ImportError):
    """Raised when ``--use-embeddings`` is on but the extra isn't installed.

    Subclasses ``ImportError`` because that's what callers expect from a
    failed import gate, but carries the canonical install hint so the
    CLI / MCP layer can surface it without rewording.
    """

    def __init__(self, message: str = INSTALL_HINT) -> None:
        super().__init__(message)


def _require_sentence_transformers() -> Any:
    """Lazy import of ``sentence_transformers`` — raises if missing.

    Returns the imported module object. Callers cache it locally rather
    than re-importing. The import itself is the slow path (seconds on
    cold start because torch comes with it); we don't try to dodge that
    — there's no useful fallback for "no embeddings".
    """
    try:
        import sentence_transformers  # type: ignore[import-not-found]
    except ImportError as exc:
        raise MissingEmbeddingsDependencyError() from exc
    return sentence_transformers


def _require_numpy() -> Any:
    """Lazy import of ``numpy`` — raises with the embeddings hint if missing.

    ``numpy`` is a transitive of sentence-transformers, so in practice
    the only way this fails is "user installed numpy-less torch by hand"
    — vanishingly rare, but we still want the same install hint rather
    than a bare ``ImportError: No module named numpy``.
    """
    try:
        import numpy  # type: ignore[import-not-found]
    except ImportError as exc:
        raise MissingEmbeddingsDependencyError() from exc
    return numpy


# ── model loading ───────────────────────────────────────────────────────────


# Process-wide model cache. Loading is expensive (seconds + ~90 MB
# resident for the default model); a single agent invocation can fire
# multiple ``search-past-decisions`` calls and we don't want to reload
# the model each time. Keyed by ``model_name`` so a switch via env var
# triggers a fresh load and an old model is GC'd on next access.
_MODEL_CACHE: dict[str, Any] = {}


def resolve_model_name(override: str | None = None) -> str:
    """Pick the model name to use.

    Precedence: explicit ``override`` arg > ``STACKUNDERFLOW_EMBED_MODEL``
    env var > :data:`DEFAULT_MODEL_NAME`. Empty strings are treated as
    "not set" so a shell that exports an empty value doesn't sink the
    feature.
    """
    if override:
        return override
    env = os.environ.get("STACKUNDERFLOW_EMBED_MODEL", "").strip()
    if env:
        return env
    return DEFAULT_MODEL_NAME


def load_model(model_name: str) -> Any:
    """Return a loaded ``SentenceTransformer`` instance (cached).

    Raises :class:`MissingEmbeddingsDependencyError` if the extra isn't
    installed. The returned object is whatever ``SentenceTransformer``
    exposes — we only call ``.encode(...)`` on it from this module, so
    tests can monkey-patch the ``_MODEL_CACHE`` directly with a stub
    that has an ``encode`` method (see ``test_discovery_embeddings.py``).
    """
    cached = _MODEL_CACHE.get(model_name)
    if cached is not None:
        return cached
    sentence_transformers = _require_sentence_transformers()
    _log.info("Loading sentence-transformers model %s (first use)", model_name)
    model = sentence_transformers.SentenceTransformer(model_name)
    _MODEL_CACHE[model_name] = model
    return model


# ── pull-through cache ──────────────────────────────────────────────────────


def _row_to_vector(blob: bytes, dim: int, np_module: Any) -> Any:
    """Decode one cached blob back into a 1-D float32 ndarray.

    Validates the declared ``dim`` matches the actual buffer length —
    a mismatch means either a corrupt write or a model change without
    a cache invalidation. Either way the right call is to skip the
    row and recompute, but here we surface the inconsistency loudly
    (caller catches and re-embeds) rather than returning garbage.
    """
    arr = np_module.frombuffer(blob, dtype=np_module.float32)
    if arr.shape[0] != dim:
        raise ValueError(
            f"Cached embedding dim mismatch: declared {dim}, actual {arr.shape[0]}"
        )
    return arr


def _load_cached(
    conn: sqlite3.Connection,
    message_ids: list[int],
    model_name: str,
    np_module: Any,
) -> dict[int, Any]:
    """Read whatever rows are already in ``discovery_embeddings``.

    Returns a ``{message_id: ndarray}`` for hits. Missing message ids
    are simply absent from the dict — the caller computes those.
    """
    if not message_ids:
        return {}
    out: dict[int, Any] = {}
    # Chunk the IN clause to dodge SQLite's parameter limit (default
    # 999) on large candidate sets — discovery seldom passes more than a
    # few dozen ids but the chunking is cheap insurance.
    chunk_size = 500
    for start in range(0, len(message_ids), chunk_size):
        chunk = message_ids[start:start + chunk_size]
        placeholders = ",".join("?" for _ in chunk)
        rows = conn.execute(
            "SELECT message_id, embedding, embedding_dim FROM discovery_embeddings "  # noqa: S608 — placeholders are bound; model_name parameterised below
            f"WHERE model_name = ? AND message_id IN ({placeholders})",
            [model_name, *chunk],
        ).fetchall()
        for r in rows:
            mid = int(r["message_id"] if hasattr(r, "keys") else r[0])
            blob = r["embedding"] if hasattr(r, "keys") else r[1]
            dim = int(r["embedding_dim"] if hasattr(r, "keys") else r[2])
            try:
                out[mid] = _row_to_vector(blob, dim, np_module)
            except ValueError as exc:
                _log.warning(
                    "Skipping corrupt cached embedding for message_id=%d: %s",
                    mid, exc,
                )
    return out


def _session_ids_for_messages(
    conn: sqlite3.Connection, message_ids: list[int],
) -> dict[int, str]:
    """Map ``message_id → sessions.session_id`` for the persist write.

    The ``discovery_embeddings`` table stores ``session_id`` (TEXT, stable
    across rebuilds) rather than the synthetic ``session_fk``. We resolve
    via the ``messages`` view + ``sessions`` join. Messages whose session
    can't be resolved (deleted upstream) are dropped from the persist
    write — the embedding is still returned for in-memory scoring.
    """
    if not message_ids:
        return {}
    out: dict[int, str] = {}
    chunk_size = 500
    for start in range(0, len(message_ids), chunk_size):
        chunk = message_ids[start:start + chunk_size]
        placeholders = ",".join("?" for _ in chunk)
        rows = conn.execute(
            "SELECT m.id AS mid, s.session_id AS sid "  # noqa: S608 — placeholders bound
            "FROM messages m JOIN sessions s ON s.id = m.session_fk "
            f"WHERE m.id IN ({placeholders})",
            chunk,
        ).fetchall()
        for r in rows:
            mid = int(r["mid"] if hasattr(r, "keys") else r[0])
            sid = r["sid"] if hasattr(r, "keys") else r[1]
            if sid:
                out[mid] = str(sid)
    return out


def _persist(
    conn: sqlite3.Connection,
    *,
    model_name: str,
    vectors: dict[int, Any],
    sid_by_mid: dict[int, str],
) -> None:
    """Write fresh ``(session_id, message_id, model_name, embedding, …)`` rows.

    ``INSERT OR REPLACE`` so a stale cache row from an old buggy write
    (or a manual edit) is corrected on the next compute. Writes are
    wrapped in a single transaction; a partial failure rolls back the
    whole batch rather than leaving half the candidate set persisted.
    """
    if not vectors:
        return
    now_iso = datetime.now(UTC).isoformat()
    rows = []
    for mid, vec in vectors.items():
        sid = sid_by_mid.get(mid)
        if sid is None:
            # Couldn't resolve session_id — skip the persist write
            # (vector is still returned to the caller for in-memory use).
            continue
        rows.append(
            (
                sid,
                mid,
                model_name,
                vec.tobytes(),
                int(vec.shape[0]),
                now_iso,
            )
        )
    if not rows:
        return
    conn.executemany(
        "INSERT OR REPLACE INTO discovery_embeddings "
        "(session_id, message_id, model_name, embedding, embedding_dim, created_ts) "
        "VALUES (?, ?, ?, ?, ?, ?)",
        rows,
    )
    conn.commit()


def _load_texts(
    conn: sqlite3.Connection, message_ids: list[int],
) -> dict[int, str]:
    """Fetch ``content_text`` for the given message ids.

    Returns ``{message_id: content_text}``; missing rows are absent.
    Empty / whitespace-only content stays an empty string here — the
    caller decides whether to embed empty strings (the default model
    handles them fine; they end up near the origin and rank low).
    """
    if not message_ids:
        return {}
    out: dict[int, str] = {}
    chunk_size = 500
    for start in range(0, len(message_ids), chunk_size):
        chunk = message_ids[start:start + chunk_size]
        placeholders = ",".join("?" for _ in chunk)
        rows = conn.execute(
            "SELECT id, content_text FROM messages "  # noqa: S608 — placeholders bound
            f"WHERE id IN ({placeholders})",
            chunk,
        ).fetchall()
        for r in rows:
            mid = int(r["id"] if hasattr(r, "keys") else r[0])
            content = r["content_text"] if hasattr(r, "keys") else r[1]
            out[mid] = content or ""
    return out


def _encode_batch(model: Any, texts: list[str], np_module: Any) -> Any:
    """Single-batch encode with ``normalize_embeddings=True``.

    Returns a 2-D float32 ndarray (rows = texts, cols = embedding dim).
    Normalisation here means the downstream cosine becomes a dot
    product, and saves us a per-row normalisation step at score time.
    """
    raw = model.encode(
        texts,
        normalize_embeddings=True,
        convert_to_numpy=True,
        show_progress_bar=False,
    )
    arr = np_module.asarray(raw, dtype=np_module.float32)
    if arr.ndim != 2:
        raise ValueError(
            f"Expected 2-D encode output, got shape {arr.shape}"
        )
    return arr


def compute_or_load(
    conn: sqlite3.Connection,
    message_ids: list[int],
    model_name: str,
) -> dict[int, Any]:
    """Pull-through cache: read what's there, compute what's missing, persist.

    Parameters
    ----------
    conn:
        Main store connection. Must have the v014 migration applied.
    message_ids:
        Candidate ``messages.id`` values to embed. Duplicates are
        de-duped before any work happens; order is irrelevant to the
        return shape.
    model_name:
        Which sentence-transformers model to use. Pass through
        :func:`resolve_model_name` if the caller has a CLI override.

    Returns
    -------
    ``{message_id: numpy.ndarray}`` with one entry per id whose text was
    findable in the store. Ids whose text could not be resolved (orphan
    rows / deleted sessions) are simply absent from the dict — the
    caller treats them as un-embeddable and skips them in the re-rank.

    Raises
    ------
    :class:`MissingEmbeddingsDependencyError`
        If sentence-transformers (or its transitive numpy) is not
        installed.
    """
    np_module = _require_numpy()
    if not message_ids:
        return {}

    # Dedup first — same id passed twice (e.g. duplicate hits across
    # the LIKE-density pre-filter) shouldn't double-compute.
    unique_ids = sorted({int(mid) for mid in message_ids})

    cached = _load_cached(conn, unique_ids, model_name, np_module)
    missing = [mid for mid in unique_ids if mid not in cached]

    if not missing:
        return cached

    texts = _load_texts(conn, missing)
    # Order matters for encode() — keep ``encode_ids`` aligned with the
    # rows of the resulting matrix. Skip ids with no text at all (deleted
    # rows): we don't want to ship empty strings into the encoder for them.
    encode_ids = [mid for mid in missing if mid in texts]
    if not encode_ids:
        return cached

    model = load_model(model_name)
    matrix = _encode_batch(model, [texts[mid] for mid in encode_ids], np_module)

    fresh: dict[int, Any] = {}
    for i, mid in enumerate(encode_ids):
        fresh[mid] = matrix[i]

    sid_by_mid = _session_ids_for_messages(conn, encode_ids)
    _persist(conn, model_name=model_name, vectors=fresh, sid_by_mid=sid_by_mid)

    merged = dict(cached)
    merged.update(fresh)
    return merged


def embed_query(query: str, model_name: str) -> Any:
    """Compute a single query embedding (1-D normalised float32 ndarray).

    Not cached: queries are unique per call and caching them would
    require a separate text-keyed table that's far less reusable than
    the per-message cache. The encode call is cheap (~10 ms for a short
    query on CPU once the model's loaded).
    """
    np_module = _require_numpy()
    model = load_model(model_name)
    raw = model.encode(
        [query],
        normalize_embeddings=True,
        convert_to_numpy=True,
        show_progress_bar=False,
    )
    arr = np_module.asarray(raw, dtype=np_module.float32)
    if arr.ndim != 2 or arr.shape[0] != 1:
        raise ValueError(
            f"Expected (1, dim) query embedding, got shape {arr.shape}"
        )
    return arr[0]


# ── scoring ─────────────────────────────────────────────────────────────────


def score_against_query(
    query_vector: Any,
    candidate_vectors: dict[int, Any],
) -> dict[int, float]:
    """Cosine similarity, mapped to ``[0, 1]``.

    Both ``query_vector`` and the values of ``candidate_vectors`` are
    expected to be unit-normalised (which they are if produced by
    :func:`embed_query` / :func:`compute_or_load`, both of which set
    ``normalize_embeddings=True``). Cosine then reduces to a dot
    product. We map ``[-1, 1] → [0, 1]`` with ``(x + 1) / 2`` so the
    score composes with the existing ``pack_within_budget`` rank fn
    (which expects each weighted component in ``[0, 1]``).

    Returns ``{message_id: score}``. Vectors with the wrong shape (e.g.
    a model change since the cache was populated) are scored ``0.0``
    rather than crashing the whole call — the row stays in the result
    set but ranks at the bottom, which is the right surface behaviour.
    """
    np_module = _require_numpy()
    out: dict[int, float] = {}
    q = np_module.asarray(query_vector, dtype=np_module.float32)
    for mid, vec in candidate_vectors.items():
        try:
            v = np_module.asarray(vec, dtype=np_module.float32)
            if v.shape != q.shape:
                out[mid] = 0.0
                continue
            dot = float(np_module.dot(q, v))
            # Numerical noise can push a near-1 dot just over 1.0; clamp.
            dot = max(-1.0, min(1.0, dot))
            out[mid] = (dot + 1.0) / 2.0
        except (ValueError, TypeError):
            out[mid] = 0.0
    return out
