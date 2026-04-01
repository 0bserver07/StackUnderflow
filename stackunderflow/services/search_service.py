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

    def reindex_all(self, memory_cache, cache_service) -> dict:
        """Rebuild the entire search index from all available project data.

        Args:
            memory_cache: The MemoryCache instance
            cache_service: The LocalCacheService instance

        Returns:
            Dict with reindex results
        """
        from ..infra.discovery import project_metadata as get_all_projects_with_metadata
        from ..pipeline import process as _run_pipeline

        projects = get_all_projects_with_metadata()
        total_messages = 0
        projects_indexed = 0
        errors = []

        for project in projects:
            project_name = project["dir_name"]
            log_path = project["log_path"]

            try:
                # Try memory cache first
                messages = None
                memory_result = memory_cache.fetch(log_path) if memory_cache else None
                if memory_result:
                    messages, _ = memory_result
                else:
                    # Try file cache
                    cached_messages = cache_service.load_messages(log_path) if cache_service else None
                    if cached_messages:
                        messages = cached_messages
                    else:
                        # Process from disk
                        messages, _ = _run_pipeline(log_path)

                if messages:
                    self.index_project(project_name, messages)
                    total_messages += len(messages)
                    projects_indexed += 1

            except Exception as e:
                logger.error(f"Error indexing project {project_name}: {e}")
                errors.append({"project": project_name, "error": str(e)})

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
        """Sanitize user input for safe FTS5 querying.

        Handles common FTS5 syntax issues by wrapping terms in quotes
        if the query contains special characters that would break FTS5.
        Simple queries (plain words) are left as-is for natural matching.
        """
        query = query.strip()

        if not query:
            return '""'

        # If user explicitly uses FTS5 operators, let them through
        # (AND, OR, NOT, NEAR, quotes, *)
        fts5_operators = re.compile(r'\b(AND|OR|NOT|NEAR)\b|[*"]', re.IGNORECASE)
        if fts5_operators.search(query):
            return query

        # For plain text queries, wrap each word as a prefix match for flexibility
        # This lets "fastapi" match "FastAPI" and "fast" match "fastapi"
        words = query.split()
        if len(words) == 1:
            # Single word: use prefix match
            escaped = words[0].replace('"', '""')
            return f'"{escaped}"*'
        else:
            # Multiple words: match all terms (implicit AND in FTS5)
            parts = []
            for word in words:
                escaped = word.replace('"', '""')
                parts.append(f'"{escaped}"*')
            return " ".join(parts)
