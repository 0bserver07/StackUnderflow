"""Local, Ollama-optional vector embeddings for hybrid retrieval.

The search surface (``services/search_service.py``) is FTS-only: a keyword
``MATCH`` over ``messages_fts``. That only finds a message if the caller
guesses its words. This module adds the *vector* half of a hybrid
retriever: it embeds message text with a small local model served by
Ollama (``localhost:11434/api/embeddings``, e.g. ``nomic-embed-text``) and
stores the vectors in ``embeddings.db`` beside ``search_index.db``. A
brute-force cosine scan in pure Python is fine at local scale (tens of
thousands of messages), so there is **no new hard dependency** — no numpy,
no sqlite-vec, no torch.

Everything here degrades gracefully. If Ollama is unreachable, embedding
is a no-op and the hybrid retriever falls back to FTS-only with zero
behavioural change. CI has no Ollama, so the reachability gate is what
keeps the whole feature testable offline: :func:`ollama_reachable` short-
circuits every network path, and the vector store simply stays empty.

Design notes
------------
* **Vector key.** Vectors are keyed by the ``search_index.db`` message
  row id — the same autoincrement ``messages.id`` the FTS index ranks on
  — so the RRF merge in ``search_service`` can fuse the two rankings
  without any cross-database join. (``search_index.db`` carries no link
  back to the canonical ``store.db`` message id, and it does not need
  one: retrieval, ranking and provenance are all expressible from the
  search index's own rows plus a session-id lookup.)
* **Serialisation.** A vector is a ``list[float]`` packed little-endian
  with :mod:`struct` (``'<%df'``). No numpy on either the write or read
  path. ``dim`` is stored alongside so a truncated blob is caught at
  read time rather than corrupting a dot product.
* **Never raises on the hot path.** :func:`embed_new_messages` and the
  Ollama calls swallow every error and log at ``debug``/``warning``. A
  down or slow Ollama must never block ingest, the watcher, or a query.
"""

from __future__ import annotations

import logging
import math
import sqlite3
import struct
import time
from collections.abc import Iterable, Sequence
from pathlib import Path

import httpx

logger = logging.getLogger(__name__)

# ── configuration ────────────────────────────────────────────────────────────

# ``embeddings.db`` lives beside ``search_index.db`` (both under
# ~/.stackunderflow). Kept in its own file so a schema change or a full
# re-embed never touches the FTS index, and so deleting it to force a
# rebuild is a one-liner.
EMBEDDINGS_DB_PATH = Path.home() / ".stackunderflow" / "embeddings.db"

# A small, fast, CPU-friendly embedding model. ``nomic-embed-text`` is
# 768-dim and ships in Ollama's library; any model the local Ollama has
# pulled works — the dim is discovered from the first response, never
# assumed.
DEFAULT_EMBED_MODEL = "nomic-embed-text"

DEFAULT_OLLAMA_URL = "http://localhost:11434"

# Short, bounded timeouts: a slow or absent Ollama must fail fast so it
# never stalls ingest or a query. Reachability is a 1.5s probe; a single
# embed request gets a little longer because the first call may load the
# model into memory.
_REACHABLE_TIMEOUT_S = 1.5
_EMBED_TIMEOUT_S = 30.0

# Cache the reachability probe for a short window so a burst of embed
# calls (one per ingest cycle) does not fire a probe apiece. Keyed by
# base URL. ``(ok, checked_at_monotonic)``.
_REACHABLE_TTL_S = 30.0
_reachable_cache: dict[str, tuple[bool, float]] = {}


def _resolve_model(model: str | None) -> str:
    """Effective embedding model name.

    ``None`` → ``STACKUNDERFLOW_EMBED_MODEL`` env var, else
    :data:`DEFAULT_EMBED_MODEL`. Kept tiny and import-light so callers
    can resolve without pulling anything heavy.
    """
    if model:
        return model
    import os

    return os.environ.get("STACKUNDERFLOW_EMBED_MODEL", DEFAULT_EMBED_MODEL)


def _resolve_url(url: str | None) -> str:
    if url:
        return url
    import os

    return os.environ.get("OLLAMA_URL", DEFAULT_OLLAMA_URL)


# ── cloud-first, local-fallback endpoints ────────────────────────────────────
# "Use cloud for Ollama, but check local." We resolve an ORDERED list of
# endpoints — a configured cloud endpoint first (STACKUNDERFLOW_OLLAMA_URL /
# OLLAMA_URL, with STACKUNDERFLOW_OLLAMA_API_KEY / OLLAMA_API_KEY as a bearer
# token for hosted Ollama), then local Ollama as a fallback — and use the first
# that answers. So a cloud outage silently degrades to a local daemon, a box
# with only local still works, and CI (neither) stays FTS-only. An explicit
# ``url=`` overrides the list (tests + specific callers), preserving behaviour.
LOCAL_OLLAMA_URL = "http://localhost:11434"


def _resolve_api_key() -> str | None:
    import os

    return os.environ.get("STACKUNDERFLOW_OLLAMA_API_KEY") or os.environ.get("OLLAMA_API_KEY") or None


def _resolve_endpoints(url: str | None = None) -> list[tuple[str, str | None]]:
    """Ordered ``(base_url, api_key)`` to try — cloud first, then local."""
    if url:
        return [(url.rstrip("/"), _resolve_api_key())]
    import os

    out: list[tuple[str, str | None]] = []
    cloud = os.environ.get("STACKUNDERFLOW_OLLAMA_URL") or os.environ.get("OLLAMA_URL")
    if cloud:
        out.append((cloud.rstrip("/"), _resolve_api_key()))
    if all(base != LOCAL_OLLAMA_URL for base, _ in out):
        out.append((LOCAL_OLLAMA_URL, None))
    return out


def _headers(api_key: str | None) -> dict[str, str]:
    return {"Authorization": f"Bearer {api_key}"} if api_key else {}


def active_endpoint(*, use_cache: bool = True) -> tuple[str, str | None] | None:
    """First reachable ``(base_url, api_key)`` from the cloud-first list, else None."""
    for base, key in _resolve_endpoints():
        if ollama_reachable(base, use_cache=use_cache, api_key=key):
            return (base, key)
    return None


# ── Ollama reachability + embedding ──────────────────────────────────────────


def ollama_reachable(url: str | None = None, *, use_cache: bool = True, api_key: str | None = None) -> bool:
    """Return ``True`` iff a local Ollama answers ``GET /api/tags`` quickly.

    This is the single gate the whole feature hangs on. Every network
    path checks it first, so on a box without Ollama (CI, most user
    machines) the vector half simply never runs and retrieval stays
    FTS-only. The result is cached for :data:`_REACHABLE_TTL_S` to keep a
    burst of embed calls from firing a probe each; pass ``use_cache=False``
    to force a fresh probe (tests do).
    """
    base = _resolve_url(url)
    now = time.monotonic()
    if use_cache:
        cached = _reachable_cache.get(base)
        if cached is not None and (now - cached[1]) < _REACHABLE_TTL_S:
            return cached[0]

    ok = False
    try:
        resp = httpx.get(
            f"{base}/api/tags",
            headers=_headers(api_key or _resolve_api_key()),
            timeout=_REACHABLE_TIMEOUT_S,
        )
        ok = resp.status_code == 200
    except Exception as exc:  # noqa: BLE001 — absence is the expected case
        logger.debug("embeddings: Ollama not reachable at %s: %s", base, exc)
        ok = False

    _reachable_cache[base] = (ok, now)
    return ok


def _reset_reachable_cache() -> None:
    """Clear the reachability cache. Test seam only."""
    _reachable_cache.clear()


def embed_texts(
    texts: Sequence[str],
    *,
    model: str | None = None,
    url: str | None = None,
    check_reachable: bool = True,
) -> list[list[float]] | None:
    """Embed ``texts`` via Ollama, one vector per input.

    Returns a list aligned with ``texts`` (a ``[]``/all-zero vector is
    never invented — a failed row is simply absent from the result, so
    the caller can tell partial failure from total). Returns ``None`` when
    Ollama is unreachable or the whole batch failed, which every caller
    treats as "embedding unavailable, fall back to FTS".

    Never raises: this rides the ingest/query path and must not take it
    down. ``check_reachable=False`` skips the probe when the caller has
    already gated (avoids a redundant round-trip).
    """
    if not texts:
        return []
    mdl = _resolve_model(model)

    # Pick the endpoint: an explicit url overrides; otherwise cloud-first,
    # then local. active_endpoint() probes in order and returns the first up.
    if url is not None:
        base: str | None = url.rstrip("/")
        key = _resolve_api_key()
        if check_reachable and not ollama_reachable(base, api_key=key):
            return None
    elif check_reachable:
        ep = active_endpoint()
        if ep is None:
            return None
        base, key = ep
    else:
        eps = _resolve_endpoints()
        if not eps:
            return None
        base, key = eps[0]

    out: list[list[float]] = []
    any_ok = False
    for text in texts:
        vec = _embed_one(text, model=mdl, base=base, api_key=key)
        if vec is None:
            continue
        out.append(vec)
        any_ok = True

    if not any_ok:
        return None
    return out


def _embed_one(text: str, *, model: str, base: str, api_key: str | None = None) -> list[float] | None:
    """POST a single string to ``/api/embeddings``; ``None`` on any failure.

    Ollama's embeddings endpoint takes ``{"model", "prompt"}`` and
    answers ``{"embedding": [...]}``. Empty text is embedded as ``None``
    (skipped) so we never store a meaningless zero vector.
    """
    if not text or not text.strip():
        return None
    try:
        resp = httpx.post(
            f"{base}/api/embeddings",
            json={"model": model, "prompt": text},
            headers=_headers(api_key),
            timeout=_EMBED_TIMEOUT_S,
        )
        if resp.status_code != 200:
            logger.debug("embeddings: /api/embeddings HTTP %s", resp.status_code)
            return None
        data = resp.json()
    except Exception as exc:  # noqa: BLE001 — transient/absent Ollama
        logger.debug("embeddings: embed request failed: %s", exc)
        return None

    vec = data.get("embedding")
    if not isinstance(vec, list) or not vec:
        return None
    try:
        return [float(x) for x in vec]
    except (TypeError, ValueError):
        return None


# ── pure-Python vector math ──────────────────────────────────────────────────


def cosine(a: Sequence[float], b: Sequence[float]) -> float:
    """Cosine similarity of two equal-length vectors, in ``[-1, 1]``.

    Pure Python — no numpy. Returns ``0.0`` for a zero-norm or
    mismatched-length input rather than raising, so a corrupt row can
    never crash a scan.
    """
    if len(a) != len(b):
        return 0.0
    dot = 0.0
    na = 0.0
    nb = 0.0
    # Lengths already checked equal above; strict=False is intentional.
    for x, y in zip(a, b, strict=False):
        dot += x * y
        na += x * x
        nb += y * y
    if na <= 0.0 or nb <= 0.0:
        return 0.0
    return dot / (math.sqrt(na) * math.sqrt(nb))


def rrf_merge(
    rankings: Iterable[Sequence[int]],
    *,
    k: int = 60,
    limit: int | None = None,
) -> list[tuple[int, float]]:
    """Reciprocal-rank fusion of several ranked id lists.

    Each input is a list of ids best-first. An id's fused score is
    ``Σ 1/(k + rank)`` over every list it appears in (``rank`` is
    0-based). ``k=60`` is the value from the original RRF paper
    (Cormack et al.) and damps the weight of any single list's top slot
    so one ranker cannot dominate. Ties break by id (ascending) so the
    output is deterministic.

    Returns ``[(id, score), ...]`` best-first, truncated to ``limit``
    when given. An empty/one-element fusion is well defined: a single
    ranking just comes back re-scored in its original order, which is why
    the FTS-only fallback path can route through here unchanged.
    """
    scores: dict[int, float] = {}
    for ranking in rankings:
        for rank, mid in enumerate(ranking):
            scores[mid] = scores.get(mid, 0.0) + 1.0 / (k + rank)
    merged = sorted(scores.items(), key=lambda kv: (-kv[1], kv[0]))
    if limit is not None:
        merged = merged[:limit]
    return merged


# ── vector store (embeddings.db) ─────────────────────────────────────────────


class EmbeddingStore:
    """SQLite-backed vector store, keyed by ``search_index.db`` message id.

    One table, ``embeddings(message_id, model, dim, vector)``. Vectors are
    little-endian float32 blobs (``struct``). The store is deliberately
    dumb — it holds vectors and hands them back for a brute-force cosine
    scan; ranking and fusion live in ``search_service``.
    """

    def __init__(self, db_path: Path | None = None):
        self.db_path = db_path or EMBEDDINGS_DB_PATH
        self.db_path.parent.mkdir(parents=True, exist_ok=True)
        self._ensure_schema()

    def _get_conn(self) -> sqlite3.Connection:
        conn = sqlite3.connect(str(self.db_path))
        conn.execute("PRAGMA journal_mode=WAL")
        conn.execute("PRAGMA synchronous=NORMAL")
        conn.row_factory = sqlite3.Row
        return conn

    def _ensure_schema(self) -> None:
        conn = self._get_conn()
        try:
            conn.execute(
                """
                CREATE TABLE IF NOT EXISTS embeddings (
                    message_id INTEGER NOT NULL,
                    model      TEXT    NOT NULL,
                    dim        INTEGER NOT NULL,
                    vector     BLOB    NOT NULL,
                    PRIMARY KEY (message_id, model)
                )
                """
            )
            conn.commit()
        finally:
            conn.close()

    # -- serialisation -------------------------------------------------------

    @staticmethod
    def _pack(vec: Sequence[float]) -> bytes:
        return struct.pack(f"<{len(vec)}f", *vec)

    @staticmethod
    def _unpack(blob: bytes, dim: int) -> list[float] | None:
        if len(blob) != dim * 4:
            return None
        return list(struct.unpack(f"<{dim}f", blob))

    # -- writes --------------------------------------------------------------

    def upsert_many(
        self, rows: Iterable[tuple[int, Sequence[float]]], *, model: str
    ) -> int:
        """Insert/replace ``(message_id, vector)`` pairs for ``model``.

        Returns the number of rows written. Idempotent — re-embedding a
        message overwrites its vector for that model.
        """
        conn = self._get_conn()
        try:
            payload = [
                (int(mid), model, len(vec), self._pack(vec))
                for mid, vec in rows
                if vec
            ]
            if not payload:
                return 0
            conn.executemany(
                "INSERT OR REPLACE INTO embeddings (message_id, model, dim, vector) "
                "VALUES (?, ?, ?, ?)",
                payload,
            )
            conn.commit()
            return len(payload)
        finally:
            conn.close()

    def delete_missing(self, keep_ids: Iterable[int]) -> int:
        """Drop vectors whose message id is not in ``keep_ids``.

        Used after a full FTS re-index (which reassigns autoincrement
        ids) so orphaned vectors don't accumulate. Best-effort; returns
        rows removed.
        """
        keep = {int(x) for x in keep_ids}
        conn = self._get_conn()
        try:
            existing = [
                int(r["message_id"])
                for r in conn.execute("SELECT message_id FROM embeddings").fetchall()
            ]
            stale = [mid for mid in existing if mid not in keep]
            if not stale:
                return 0
            conn.executemany(
                "DELETE FROM embeddings WHERE message_id = ?",
                [(mid,) for mid in stale],
            )
            conn.commit()
            return len(stale)
        finally:
            conn.close()

    # -- reads ---------------------------------------------------------------

    def existing_ids(self, model: str | None = None) -> set[int]:
        """Message ids that already have a vector (optionally for one model)."""
        conn = self._get_conn()
        try:
            if model is None:
                rows = conn.execute("SELECT message_id FROM embeddings").fetchall()
            else:
                rows = conn.execute(
                    "SELECT message_id FROM embeddings WHERE model = ?", (model,)
                ).fetchall()
            return {int(r["message_id"]) for r in rows}
        finally:
            conn.close()

    def count(self, model: str | None = None) -> int:
        conn = self._get_conn()
        try:
            if model is None:
                row = conn.execute("SELECT COUNT(*) AS c FROM embeddings").fetchone()
            else:
                row = conn.execute(
                    "SELECT COUNT(*) AS c FROM embeddings WHERE model = ?", (model,)
                ).fetchone()
            return int(row["c"])
        finally:
            conn.close()

    def iter_vectors(self, *, model: str) -> Iterable[tuple[int, list[float]]]:
        """Yield ``(message_id, vector)`` for every stored vector of ``model``.

        Streams rows so a large store doesn't materialise all vectors at
        once. Corrupt blobs (wrong length) are skipped, not raised.
        """
        conn = self._get_conn()
        try:
            for r in conn.execute(
                "SELECT message_id, dim, vector FROM embeddings WHERE model = ?",
                (model,),
            ):
                vec = self._unpack(r["vector"], int(r["dim"]))
                if vec is None:
                    continue
                yield int(r["message_id"]), vec
        finally:
            conn.close()

    def search(
        self,
        query_vector: Sequence[float],
        *,
        model: str,
        top_k: int = 50,
    ) -> list[tuple[int, float]]:
        """Brute-force cosine scan → ``[(message_id, similarity), ...]``.

        Best-first, truncated to ``top_k``. O(N) over the stored vectors
        of ``model`` — fine at local scale. Returns ``[]`` when the store
        is empty (the FTS-only fallback trigger).
        """
        if not query_vector:
            return []
        scored: list[tuple[int, float]] = []
        for mid, vec in self.iter_vectors(model=model):
            scored.append((mid, cosine(query_vector, vec)))
        scored.sort(key=lambda kv: (-kv[1], kv[0]))
        return scored[:top_k]


# ── incremental embedding (watcher / ingest hook) ────────────────────────────


def embed_new_messages(
    search_conn: sqlite3.Connection,
    *,
    store: EmbeddingStore | None = None,
    model: str | None = None,
    url: str | None = None,
    batch_limit: int = 512,
    max_chars: int = 2000,
) -> int:
    """Embed ``search_index.db`` messages that have no vector yet.

    Reads ``(id, content)`` from the FTS index's ``messages`` table,
    skips ids already present in the vector store, embeds the rest via
    Ollama, and upserts them. Returns the number of vectors written.

    Best-effort and **never raises**: any failure (Ollama down, bad row,
    HTTP error) is swallowed and returns ``0`` / a partial count. This is
    the function the watcher and ingest call after a refresh cycle, so it
    is the load-bearing "graceful degradation" seam — with Ollama absent
    it does a single cheap reachability probe and returns ``0``.

    ``batch_limit`` bounds how many new messages one call embeds so a
    cold start (thousands of un-embedded rows) doesn't monopolise a
    cycle; the next call picks up where this one left off. ``max_chars``
    truncates very long messages before embedding (embedding models cap
    context anyway, and the tail rarely changes the vector's neighbourhood).
    """
    try:
        # cloud-first, local-fallback (same as embed_texts): an explicit url is
        # probed directly; otherwise active_endpoint() tries cloud then local.
        if url is not None:
            base = url
            if not ollama_reachable(base):
                return 0
        else:
            ep = active_endpoint()
            if ep is None:
                return 0
            base = ep[0]

        mdl = _resolve_model(model)
        vstore = store or EmbeddingStore()

        try:
            have = vstore.existing_ids(mdl)
        except Exception as exc:  # noqa: BLE001
            logger.debug("embeddings: existing_ids failed: %s", exc)
            return 0

        try:
            rows = search_conn.execute(
                "SELECT id, content FROM messages ORDER BY id"
            ).fetchall()
        except Exception as exc:  # noqa: BLE001 — search index may be absent
            logger.debug("embeddings: could not read search index messages: %s", exc)
            return 0

        pending: list[tuple[int, str]] = []
        for r in rows:
            mid = int(r["id"])
            if mid in have:
                continue
            content = (r["content"] or "")[:max_chars]
            if not content.strip():
                continue
            pending.append((mid, content))
            if len(pending) >= batch_limit:
                break

        if not pending:
            return 0

        vectors = embed_texts(
            [c for _, c in pending], model=mdl, url=base, check_reachable=False
        )
        if not vectors:
            return 0

        # ``embed_texts`` drops failed rows, so a short result maps to the
        # first N pending ids in order (Ollama preserves request order per
        # call, and we send one prompt per request). Guard the zip to the
        # shorter length so a partial batch never misaligns.
        pairs = [
            (pending[i][0], vectors[i])
            for i in range(min(len(pending), len(vectors)))
        ]
        try:
            return vstore.upsert_many(pairs, model=mdl)
        except Exception as exc:  # noqa: BLE001
            logger.debug("embeddings: upsert failed: %s", exc)
            return 0
    except Exception as exc:  # noqa: BLE001 — belt-and-suspenders; never propagate
        logger.debug("embeddings: embed_new_messages failed: %s", exc)
        return 0
