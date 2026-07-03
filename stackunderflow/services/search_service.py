"""
Full-text search service using SQLite FTS5.

Indexes message content from Claude Code sessions and supports
querying with filters, highlighted snippets, and pagination.
"""

import logging
import re
import sqlite3
from datetime import UTC
from pathlib import Path

logger = logging.getLogger(__name__)

# Location of the search index database
SEARCH_DB_PATH = Path.home() / ".stackunderflow" / "search_index.db"

# A single word character (unicode-aware) — the presence of one is what
# distinguishes a real search from empty / punctuation-only input.
_WORD_RE = re.compile(r"\w", re.UNICODE)


def search_has_intent(query: str | None) -> bool:
    """True when ``query`` carries at least one searchable term.

    The gate the ``memory`` commands run *before* opening the store: empty,
    whitespace-only, or pure-punctuation input (``""``, ``"   "``,
    ``"!!!"``, ``"***"``) has no term to match and is rejected up front
    rather than opening the store to return nothing. Any word character
    (alphanumeric or ``_``, incl. non-ASCII) counts as intent.

    Pure — never opens a connection, never raises.
    """
    if not query:
        return False
    return bool(_WORD_RE.search(query))


class SearchService:
    """Service for full-text search across all Claude Code sessions."""

    def __init__(self, db_path: Path | None = None):
        self.db_path = db_path or SEARCH_DB_PATH
        self.db_path.parent.mkdir(parents=True, exist_ok=True)
        self._ensure_schema()

    def _get_conn(self) -> sqlite3.Connection:
        """Get a database connection with WAL mode for better concurrency."""
        conn = sqlite3.connect(str(self.db_path))
        conn.execute("PRAGMA journal_mode=WAL")
        conn.execute("PRAGMA synchronous=NORMAL")
        conn.row_factory = sqlite3.Row
        return conn

    def _ensure_schema(self):
        """Create the FTS5 virtual table and metadata table if they don't exist."""
        conn = self._get_conn()
        try:
            # Regular table to hold message data and allow filtering
            conn.execute("""
                CREATE TABLE IF NOT EXISTS messages (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    session_id TEXT NOT NULL,
                    project TEXT NOT NULL,
                    role TEXT NOT NULL,
                    content TEXT NOT NULL,
                    timestamp TEXT,
                    model TEXT,
                    tokens_input INTEGER DEFAULT 0,
                    tokens_output INTEGER DEFAULT 0
                )
            """)

            # FTS5 virtual table linked to messages via content= sync
            conn.execute("""
                CREATE VIRTUAL TABLE IF NOT EXISTS messages_fts USING fts5(
                    content,
                    content='messages',
                    content_rowid='id',
                    tokenize='porter unicode61'
                )
            """)

            # Triggers to keep FTS index in sync with the messages table
            conn.execute("""
                CREATE TRIGGER IF NOT EXISTS messages_ai AFTER INSERT ON messages BEGIN
                    INSERT INTO messages_fts(rowid, content) VALUES (new.id, new.content);
                END
            """)
            conn.execute("""
                CREATE TRIGGER IF NOT EXISTS messages_ad AFTER DELETE ON messages BEGIN
                    INSERT INTO messages_fts(messages_fts, rowid, content) VALUES('delete', old.id, old.content);
                END
            """)
            conn.execute("""
                CREATE TRIGGER IF NOT EXISTS messages_au AFTER UPDATE ON messages BEGIN
                    INSERT INTO messages_fts(messages_fts, rowid, content) VALUES('delete', old.id, old.content);
                    INSERT INTO messages_fts(rowid, content) VALUES (new.id, new.content);
                END
            """)

            # Metadata table to track which projects have been indexed
            conn.execute("""
                CREATE TABLE IF NOT EXISTS index_metadata (
                    project TEXT PRIMARY KEY,
                    indexed_at TEXT NOT NULL,
                    message_count INTEGER DEFAULT 0
                )
            """)

            # Index for faster filtering
            conn.execute("CREATE INDEX IF NOT EXISTS idx_messages_project ON messages(project)")
            conn.execute("CREATE INDEX IF NOT EXISTS idx_messages_timestamp ON messages(timestamp)")
            conn.execute("CREATE INDEX IF NOT EXISTS idx_messages_role ON messages(role)")
            conn.execute("CREATE INDEX IF NOT EXISTS idx_messages_model ON messages(model)")

            conn.commit()
        finally:
            conn.close()

    def index_project(self, project_name: str, messages: list[dict]):
        """Index messages from a single project.

        Removes any existing messages for the project first (full re-index).

        Args:
            project_name: The project directory name (e.g., "-Users-john-dev-myapp")
            messages: List of message dicts from the processor
        """
        conn = self._get_conn()
        try:
            # Remove old data for this project
            conn.execute("DELETE FROM messages WHERE project = ?", (project_name,))

            # Insert new messages
            count = 0
            for msg in messages:
                content = msg.get("content", "")
                if not content or not content.strip():
                    continue

                role = msg.get("type", "unknown")
                session_id = msg.get("session_id", "")
                timestamp = msg.get("timestamp", "")
                model = msg.get("model", "")
                tokens_input = msg.get("tokens", {}).get("input", 0)
                tokens_output = msg.get("tokens", {}).get("output", 0)

                conn.execute(
                    """INSERT INTO messages
                       (session_id, project, role, content, timestamp, model, tokens_input, tokens_output)
                       VALUES (?, ?, ?, ?, ?, ?, ?, ?)""",
                    (session_id, project_name, role, content, timestamp, model, tokens_input, tokens_output),
                )
                count += 1

            # Update metadata
            from datetime import datetime

            conn.execute(
                """INSERT OR REPLACE INTO index_metadata (project, indexed_at, message_count)
                   VALUES (?, ?, ?)""",
                (project_name, datetime.now(UTC).isoformat(), count),
            )

            conn.commit()
            logger.info(f"Indexed {count} messages for project {project_name}")
        except Exception as e:
            conn.rollback()
            logger.error(f"Error indexing project {project_name}: {e}")
            raise
        finally:
            conn.close()

    def reindex_all(self, memory_cache=None, cache_service=None, projects=None) -> dict:
        """Rebuild the entire search index from the session store.

        `memory_cache` / `cache_service` parameters are retained for backwards
        compatibility but are no longer consulted — the SQLite session store is
        the single source of truth. Pass them as `None`.

        Args:
            memory_cache: ignored (legacy param)
            cache_service: ignored (legacy param)
            projects: Optional list of `{dir_name, log_path}` dicts; when None
                we walk the store's projects table directly.

        Returns:
            Dict with reindex results.
        """
        import stackunderflow.deps as deps

        from ..store import db, queries

        store_path = getattr(deps, 'store_path', None)
        if store_path is None:
            return {
                "projects_indexed": 0,
                "total_messages_indexed": 0,
                "errors": [{"project": "(all)", "error": "store_path unavailable"}],
            }

        total_messages = 0
        projects_indexed = 0
        errors: list[dict] = []

        # Load project rows once: we need the project_id to call
        # queries.get_project_stats. The `projects` arg from the caller only
        # carries dir_name / log_path which is insufficient.
        #
        # The schema has UNIQUE(provider, slug), so the same slug can have
        # multiple rows (e.g. claude + codex). `index_project` does a DELETE
        # by slug before inserting, so naively iterating rows would let the
        # second iteration wipe the first's messages. Group by slug and
        # concatenate before indexing.
        conn = db.connect(store_path)
        try:
            project_rows = queries.list_projects(conn)
            wanted_slugs = (
                {p.get("dir_name") for p in (projects or []) if p.get("dir_name")}
                if projects
                else None
            )

            from collections import defaultdict
            slug_groups: dict[str, list[int]] = defaultdict(list)
            for prow in project_rows:
                if wanted_slugs is not None and prow.slug not in wanted_slugs:
                    continue
                slug_groups[prow.slug].append(prow.id)

            for slug, ids in slug_groups.items():
                merged: list[dict] = []
                try:
                    for pid in ids:
                        msgs, _ = queries.get_project_stats(conn, project_id=pid)
                        merged.extend(msgs)
                    if merged:
                        self.index_project(slug, merged)
                        total_messages += len(merged)
                        projects_indexed += 1
                except Exception as e:
                    logger.error(f"Error indexing project {slug}: {e}")
                    errors.append({"project": slug, "error": str(e)})
        finally:
            conn.close()

        return {
            "projects_indexed": projects_indexed,
            "total_messages_indexed": total_messages,
            "errors": errors,
        }

    def search(
        self,
        query: str,
        project: str | None = None,
        date_from: str | None = None,
        date_to: str | None = None,
        model: str | None = None,
        role: str | None = None,
        page: int = 1,
        per_page: int = 20,
    ) -> dict:
        """Search indexed messages with full-text search and filters.

        Args:
            query: Search text (FTS5 query syntax supported)
            project: Optional project name filter
            date_from: Optional start date (ISO format, inclusive)
            date_to: Optional end date (ISO format, inclusive)
            model: Optional model name filter
            role: Optional role filter (user, assistant, etc.)
            page: Page number (1-indexed)
            per_page: Results per page

        Returns:
            Dict with results, pagination info, and metadata
        """
        if not query or not query.strip():
            return {
                "results": [],
                "total": 0,
                "page": page,
                "per_page": per_page,
                "total_pages": 0,
                "query": query,
            }

        conn = self._get_conn()
        try:
            # Sanitize the query for FTS5
            safe_query = self._sanitize_fts_query(query)

            # Build WHERE clauses for filtering
            where_clauses = []
            params = []

            if project:
                where_clauses.append("m.project = ?")
                params.append(project)

            if date_from:
                where_clauses.append("m.timestamp >= ?")
                params.append(date_from)

            if date_to:
                # Add time component to make it inclusive of the entire day
                if len(date_to) == 10:  # YYYY-MM-DD format
                    date_to = date_to + "T23:59:59"
                where_clauses.append("m.timestamp <= ?")
                params.append(date_to)

            if model:
                where_clauses.append("m.model = ?")
                params.append(model)

            if role:
                where_clauses.append("m.role = ?")
                params.append(role)

            where_sql = ""
            if where_clauses:
                where_sql = "AND " + " AND ".join(where_clauses)

            try:
                # Count total results
                count_sql = f"""
                    SELECT COUNT(*) as total
                    FROM messages_fts
                    JOIN messages m ON messages_fts.rowid = m.id
                    WHERE messages_fts MATCH ?
                    {where_sql}
                """
                count_params = [safe_query] + params
                total = conn.execute(count_sql, count_params).fetchone()["total"]
            except sqlite3.OperationalError:
                return {
                    "results": [],
                    "total": 0,
                    "page": page,
                    "per_page": per_page,
                    "total_pages": 0,
                    "query": query,
                }

            total_pages = (total + per_page - 1) // per_page if total > 0 else 0

            # Clamp page
            if page < 1:
                page = 1
            if page > total_pages and total_pages > 0:
                page = total_pages

            offset = (page - 1) * per_page

            # Fetch results with relevance ranking
            try:
                results_sql = f"""
                    SELECT
                        m.id,
                        m.session_id,
                        m.project,
                        m.role,
                        m.content,
                        m.timestamp,
                        m.model,
                        m.tokens_input,
                        m.tokens_output,
                        snippet(messages_fts, 0, '<mark>', '</mark>', '...', 48) as snippet,
                        rank
                    FROM messages_fts
                    JOIN messages m ON messages_fts.rowid = m.id
                    WHERE messages_fts MATCH ?
                    {where_sql}
                    ORDER BY rank
                    LIMIT ? OFFSET ?
                """
                results_params = [safe_query] + params + [per_page, offset]
                rows = conn.execute(results_sql, results_params).fetchall()
            except sqlite3.OperationalError:
                return {
                    "results": [],
                    "total": 0,
                    "page": page,
                    "per_page": per_page,
                    "total_pages": 0,
                    "query": query,
                }

            results = []
            for row in rows:
                results.append({
                    "id": row["id"],
                    "session_id": row["session_id"],
                    "project": row["project"],
                    "role": row["role"],
                    "content": row["content"][:500],  # Limit content size
                    "timestamp": row["timestamp"],
                    "model": row["model"],
                    "tokens_input": row["tokens_input"],
                    "tokens_output": row["tokens_output"],
                    "snippet": row["snippet"],
                    "relevance": row["rank"],
                })

            return {
                "results": results,
                "total": total,
                "page": page,
                "per_page": per_page,
                "total_pages": total_pages,
                "query": query,
            }

        finally:
            conn.close()

    def _fts_ranked_ids(
        self,
        conn: sqlite3.Connection,
        safe_query: str,
        where_sql: str,
        params: list,
        limit: int,
    ) -> list[int]:
        """Return message ids for ``safe_query``, best-relevance first.

        The lexical half of the hybrid retriever. Shares the exact FTS5
        ``MATCH`` + filter path :meth:`search` uses, but projects only the
        row id (RRF fuses on ids, then :meth:`_rows_for_ids` rehydrates).
        An FTS5 syntax error yields ``[]`` — same swallow as ``search``.
        """
        sql = f"""
            SELECT m.id AS id
            FROM messages_fts
            JOIN messages m ON messages_fts.rowid = m.id
            WHERE messages_fts MATCH ?
            {where_sql}
            ORDER BY rank
            LIMIT ?
        """
        try:
            rows = conn.execute(sql, [safe_query, *params, limit]).fetchall()
        except sqlite3.OperationalError:
            return []
        return [int(r["id"]) for r in rows]

    def _rows_for_ids(
        self, conn: sqlite3.Connection, ids: list[int]
    ) -> dict[int, dict]:
        """Fetch full message rows for ``ids`` → ``{id: result_dict}``.

        One query, ``IN (...)`` over the (small) fused candidate set. The
        dict shape mirrors :meth:`search`'s ``results`` rows minus the
        FTS-only ``snippet``/``relevance`` (the hybrid caller attaches its
        own ``relevance`` = fused score).
        """
        if not ids:
            return {}
        placeholders = ",".join("?" for _ in ids)
        sql = f"""
            SELECT id, session_id, project, role, content, timestamp,
                   model, tokens_input, tokens_output
            FROM messages
            WHERE id IN ({placeholders})
        """
        out: dict[int, dict] = {}
        for row in conn.execute(sql, ids).fetchall():
            out[int(row["id"])] = {
                "id": row["id"],
                "session_id": row["session_id"],
                "project": row["project"],
                "role": row["role"],
                "content": row["content"][:500],
                "timestamp": row["timestamp"],
                "model": row["model"],
                "tokens_input": row["tokens_input"],
                "tokens_output": row["tokens_output"],
            }
        return out

    def hybrid_search(
        self,
        query: str,
        *,
        project: str | None = None,
        date_from: str | None = None,
        date_to: str | None = None,
        model: str | None = None,
        role: str | None = None,
        limit: int = 20,
        candidate_k: int = 50,
        embed_model: str | None = None,
        ollama_url: str | None = None,
    ) -> dict:
        """Hybrid FTS + vector retrieval, merged by reciprocal-rank fusion.

        Runs the lexical FTS5 ``MATCH`` **and** a brute-force cosine scan
        over ``embeddings.db``, then fuses the two rankings with RRF
        (:func:`services.embeddings.rrf_merge`). Filters (project / date /
        model / role) are applied to the FTS half in SQL and to the vector
        half by post-filtering the fused rows against the same predicates.

        Graceful degradation is the whole point:

        * Ollama unreachable, or ``embeddings.db`` empty, or the query
          fails to embed → the vector ranking is empty and the fused
          result is **exactly** the FTS ranking. Zero regression: the same
          rows in the same order today's ``search`` would return.
        * FTS finds nothing but the vector half does → semantic-only hits
          still surface (the win: "how did I fix the flaky auth test"
          matching without the keyword).

        Returns the same envelope shape as :meth:`search` (``results`` /
        ``total`` / ``query`` / ``limit``), plus ``vector_used`` so a
        caller can tell whether the semantic half actually contributed.
        Each result's ``relevance`` is the fused RRF score (higher is
        better — note this is the opposite sign convention to ``search``'s
        raw FTS ``rank``, where lower is better).
        """
        empty = {
            "results": [],
            "total": 0,
            "query": query,
            "limit": limit,
            "vector_used": False,
        }
        if not query or not query.strip():
            return empty

        # Import here so callers that never hit the hybrid path don't pay
        # the (tiny, but principled) import, and so a partially-installed
        # env degrades to FTS via the except below.
        try:
            from stackunderflow.services import embeddings as _emb
        except Exception:  # noqa: BLE001
            _emb = None  # type: ignore[assignment]

        conn = self._get_conn()
        try:
            safe_query = self._sanitize_fts_query(query)
            where_clauses, params = self._build_filter_clauses(
                project=project, date_from=date_from, date_to=date_to,
                model=model, role=role,
            )
            where_sql = ("AND " + " AND ".join(where_clauses)) if where_clauses else ""

            # -- lexical half -------------------------------------------------
            fts_ids = self._fts_ranked_ids(
                conn, safe_query, where_sql, params, candidate_k
            )

            # -- vector half (best-effort, gated) -----------------------------
            vector_ids: list[int] = []
            if _emb is not None:
                vector_ids = self._vector_ranked_ids(
                    query, emb=_emb, candidate_k=candidate_k,
                    embed_model=embed_model, ollama_url=ollama_url,
                )

            vector_used = bool(vector_ids)

            # -- fuse ---------------------------------------------------------
            rankings = [r for r in (fts_ids, vector_ids) if r]
            if not rankings:
                return empty
            if _emb is not None:
                fused = _emb.rrf_merge(rankings, limit=None)
            else:
                # No embeddings module at all → FTS ids re-scored in order.
                fused = [(mid, 1.0 / (60 + i)) for i, mid in enumerate(fts_ids)]

            fused_ids = [mid for mid, _ in fused]
            rows_by_id = self._rows_for_ids(conn, fused_ids)

            # The vector half is not filtered in SQL, so post-filter every
            # fused row against the same predicates before returning. FTS
            # rows already satisfy them; vector-only rows may not.
            results: list[dict] = []
            for mid, score in fused:
                row = rows_by_id.get(mid)
                if row is None:
                    continue
                if not self._row_matches_filters(
                    row, project=project, date_from=date_from,
                    date_to=date_to, model=model, role=role,
                ):
                    continue
                row = dict(row)
                row["relevance"] = score
                results.append(row)
                if len(results) >= limit:
                    break

            return {
                "results": results,
                "total": len(results),
                "query": query,
                "limit": limit,
                "vector_used": vector_used,
            }
        finally:
            conn.close()

    def _vector_ranked_ids(
        self,
        query: str,
        *,
        emb,
        candidate_k: int,
        embed_model: str | None,
        ollama_url: str | None,
    ) -> list[int]:
        """Embed ``query`` and cosine-scan ``embeddings.db`` → id list.

        Returns ``[]`` on any miss (Ollama down, empty store, embed
        failure) so the caller falls back to FTS-only. Never raises.
        """
        try:
            model_name = emb._resolve_model(embed_model)
            store = emb.EmbeddingStore()
            if store.count(model_name) == 0:
                return []
            if not emb.ollama_reachable(ollama_url):
                return []
            qvecs = emb.embed_texts(
                [query], model=model_name, url=ollama_url, check_reachable=False,
            )
            if not qvecs:
                return []
            hits = store.search(qvecs[0], model=model_name, top_k=candidate_k)
            return [mid for mid, _ in hits]
        except Exception as exc:  # noqa: BLE001 — never break the query path
            logger.debug("hybrid_search: vector half failed: %s", exc)
            return []

    def _build_filter_clauses(
        self,
        *,
        project: str | None,
        date_from: str | None,
        date_to: str | None,
        model: str | None,
        role: str | None,
    ) -> tuple[list[str], list]:
        """Build the shared ``WHERE`` fragments + params (FTS SQL half)."""
        where_clauses: list[str] = []
        params: list = []
        if project:
            where_clauses.append("m.project = ?")
            params.append(project)
        if date_from:
            where_clauses.append("m.timestamp >= ?")
            params.append(date_from)
        if date_to:
            if len(date_to) == 10:
                date_to = date_to + "T23:59:59"
            where_clauses.append("m.timestamp <= ?")
            params.append(date_to)
        if model:
            where_clauses.append("m.model = ?")
            params.append(model)
        if role:
            where_clauses.append("m.role = ?")
            params.append(role)
        return where_clauses, params

    @staticmethod
    def _row_matches_filters(
        row: dict,
        *,
        project: str | None,
        date_from: str | None,
        date_to: str | None,
        model: str | None,
        role: str | None,
    ) -> bool:
        """Python mirror of :meth:`_build_filter_clauses` for vector rows."""
        if project and row.get("project") != project:
            return False
        if model and row.get("model") != model:
            return False
        if role and row.get("role") != role:
            return False
        ts = row.get("timestamp") or ""
        if date_from and ts < date_from:
            return False
        if date_to:
            hi = date_to + "T23:59:59" if len(date_to) == 10 else date_to
            if ts > hi:
                return False
        return True

    def get_indexed_projects(self) -> list[dict]:
        """Get list of projects that have been indexed with their metadata."""
        conn = self._get_conn()
        try:
            rows = conn.execute(
                "SELECT project, indexed_at, message_count FROM index_metadata ORDER BY project"
            ).fetchall()
            return [dict(row) for row in rows]
        finally:
            conn.close()

    def get_index_stats(self) -> dict:
        """Get statistics about the search index."""
        conn = self._get_conn()
        try:
            total_messages = conn.execute("SELECT COUNT(*) as c FROM messages").fetchone()["c"]
            total_projects = conn.execute("SELECT COUNT(*) as c FROM index_metadata").fetchone()["c"]
            distinct_models = conn.execute(
                "SELECT DISTINCT model FROM messages WHERE model IS NOT NULL AND model != '' AND model != 'N/A'"
            ).fetchall()

            return {
                "total_messages": total_messages,
                "total_projects": total_projects,
                "models": [row["model"] for row in distinct_models],
            }
        finally:
            conn.close()

    def _sanitize_fts_query(self, query: str) -> str:
        """Neutralise user input into a safe FTS5 ``MATCH`` expression.

        Every run of word characters becomes a quoted prefix term; FTS5
        operators and punctuation — ``AND`` / ``OR`` / ``NOT`` / ``NEAR``,
        ``*``, ``"``, ``(`` / ``)``, ``:``, ``-`` … — are treated as
        **literal search text**, never as query syntax. So agent free text
        like ``use NOT null`` searches for the words ``use``, ``not`` and
        ``null`` instead of reaching the FTS5 parser as a ``NOT`` operator
        (which would silently change the meaning) — and a stray bare ``*``
        or an unbalanced ``"`` can no longer raise
        ``sqlite3.OperationalError`` back at the agent.

        This deliberately drops FTS5 operator *syntax* from the query
        surface: the ``memory`` CLI (and the Search tab that shares this
        method) is free-text-first, so safety wins over a power-user DSL.

        Empty / punctuation-only input returns ``'""'`` (matches nothing);
        callers gate that earlier with :func:`search_has_intent`, but the
        floor keeps this method total.

        The output for an operator-free query is byte-identical to the old
        per-word ``"word"*`` form, so existing plain-text callers are
        unaffected.
        """
        tokens = re.findall(r"\w+", query or "")
        if not tokens:
            return '""'
        return " ".join(f'"{t}"*' for t in tokens)

    def lexical_session_hits(
        self,
        query: str,
        *,
        project: str | None = None,
        date_from: str | None = None,
        date_to: str | None = None,
        candidate_k: int = 200,
    ) -> list[dict] | None:
        """Best-bm25-first, one representative hit per session.

        The lexical retriever the structured ``memory`` commands
        (``decisions`` and friends) use in place of a leading-wildcard
        ``content_text LIKE '%needle%'`` full scan. Runs the same FTS5
        ``MATCH`` + bm25 ``rank`` + project/date filter path as
        :meth:`search`, then **clusters** to one row per session (the
        best-ranked message) and counts the further hits that session had,
        so a chatty session can't fill the page.

        Returns:

        * ``None`` when the index isn't populated (no rows in ``messages``)
          — the caller (``discovery.search_past_decisions``) then falls
          back to its LIKE scan. This is the *only* "index not populated"
          signal; a populated index that simply matched nothing returns
          ``[]`` so the caller never silently reintroduces the full scan.
        * ``[]`` when the index is populated but nothing matched (incl. an
          FTS5 syntax hiccup that survived sanitising).
        * otherwise a list of dicts, best-bm25-first, each::

              {"session_id": str,
               "content": str,      # representative message's full text
               "bm25": float,       # SQLite raw rank (lower = better)
               "more_matches_in_session": int}  # further hits, 0+

        ``content`` is the raw message text (not an FTS ``snippet()``) so
        the caller builds the same Python snippet the LIKE path does,
        keeping the snippet format identical across both paths.

        Never raises: any operational error degrades to ``[]`` (or ``None``
        for the populated check), matching the swallow in :meth:`search`.
        """
        if not query or not query.strip():
            return []
        conn = self._get_conn()
        try:
            # Populated? One cheap probe. Empty/absent → signal LIKE fallback.
            try:
                populated = conn.execute(
                    "SELECT 1 FROM messages LIMIT 1"
                ).fetchone()
            except sqlite3.OperationalError:
                return None
            if populated is None:
                return None

            safe_query = self._sanitize_fts_query(query)
            where_clauses, params = self._build_filter_clauses(
                project=project, date_from=date_from, date_to=date_to,
                model=None, role=None,
            )
            where_sql = ("AND " + " AND ".join(where_clauses)) if where_clauses else ""
            sql = f"""
                SELECT m.session_id AS session_id, m.content AS content, rank AS bm25
                FROM messages_fts
                JOIN messages m ON messages_fts.rowid = m.id
                WHERE messages_fts MATCH ?
                {where_sql}
                ORDER BY rank
                LIMIT ?
            """
            try:
                rows = conn.execute(
                    sql, [safe_query, *params, max(1, int(candidate_k))]
                ).fetchall()
            except sqlite3.OperationalError:
                # Index IS populated but the MATCH failed — return [] (a
                # genuine "no lexical hits"), never None: falling back to
                # the LIKE full scan on a syntax hiccup is the anti-pattern
                # this method exists to remove.
                return []

            best: dict[str, dict] = {}
            for r in rows:
                sid = r["session_id"]
                if not sid:
                    continue
                if sid in best:
                    best[sid]["more_matches_in_session"] += 1
                    continue
                best[sid] = {
                    "session_id": sid,
                    "content": r["content"] or "",
                    "bm25": float(r["bm25"]),
                    "more_matches_in_session": 0,
                }
            # dict preserves insertion order == bm25-best-first (ORDER BY rank).
            return list(best.values())
        finally:
            conn.close()
