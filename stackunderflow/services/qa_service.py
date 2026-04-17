"""
Q&A extraction service for Claude Code sessions.

Extracts question-answer pairs from parsed session data using heuristics:
- User messages with question indicators (?, keywords like how/why/fix/error)
- User messages followed by Claude responses containing code blocks
- Multi-turn grouping when users indicate the answer didn't work

Stores extracted Q&A pairs in SQLite at ~/.stackunderflow/qa_pairs.db.
"""

import hashlib
import logging
import re
import sqlite3
from datetime import UTC, datetime
from pathlib import Path

logger = logging.getLogger(__name__)

# Location of the Q&A database
QA_DB_PATH = Path.home() / ".stackunderflow" / "qa_pairs.db"

# Keywords that indicate a question
QUESTION_KEYWORDS = [
    "how", "why", "fix", "error", "help", "what", "can you", "is there",
    "could you", "where", "when", "which", "should", "would", "does",
    "doesn't work", "not working", "broken", "issue", "problem", "bug",
    "implement", "create", "add", "make", "build", "set up", "configure",
    "explain", "show me", "tell me",
]

# Patterns that indicate the user is saying the answer didn't work (continuation)
FOLLOWUP_PATTERNS = [
    "that didn't work",
    "that doesn't work",
    "still not working",
    "still broken",
    "still getting",
    "same error",
    "same issue",
    "didn't fix",
    "doesn't fix",
    "try again",
    "that's not right",
    "that's wrong",
    "not quite",
    "almost but",
    "close but",
    "nope",
    "no, ",
    "no that",
    "actually,",
    "wait,",
    "but ",
    "however ",
]


def _is_question(content: str) -> bool:
    """Determine if a user message looks like a question."""
    if not content or not content.strip():
        return False

    content_lower = content.lower().strip()

    # Contains a question mark
    if "?" in content:
        return True

    # Starts with or contains question keywords
    for keyword in QUESTION_KEYWORDS:
        # Check if keyword appears at the start of any sentence
        if content_lower.startswith(keyword):
            return True
        # Check if keyword appears after a newline or sentence boundary
        if f"\n{keyword}" in content_lower or f". {keyword}" in content_lower:
            return True

    return False


def _is_followup(content: str) -> bool:
    """Determine if a user message is a follow-up to a previous answer."""
    if not content or not content.strip():
        return False

    content_lower = content.lower().strip()

    for pattern in FOLLOWUP_PATTERNS:
        if content_lower.startswith(pattern) or pattern in content_lower[:100]:
            return True

    return False


def _has_code_blocks(content: str) -> bool:
    """Check if content contains code blocks (markdown fenced or indented)."""
    if not content:
        return False
    # Check for fenced code blocks
    if "```" in content:
        return True
    # Check for indented code blocks (4+ spaces at line start, multiple lines)
    lines = content.split("\n")
    code_lines = sum(1 for line in lines if line.startswith("    ") and line.strip())
    return code_lines >= 3


def _extract_code_snippets(content: str) -> list[str]:
    """Extract code snippets from content."""
    snippets = []
    if not content:
        return snippets

    # Extract fenced code blocks
    pattern = r"```(?:\w*)\n(.*?)```"
    matches = re.findall(pattern, content, re.DOTALL)
    for match in matches:
        snippet = match.strip()
        if snippet and len(snippet) > 10:
            snippets.append(snippet[:2000])  # Limit snippet size

    return snippets


def _extract_tools_used(messages: list[dict]) -> list[str]:
    """Extract unique tool names from a list of messages."""
    tools = set()
    for msg in messages:
        for tool in msg.get("tools", []):
            name = tool.get("name", "")
            if name:
                tools.add(name)
    return sorted(tools)


def _generate_qa_id(session_id: str, timestamp: str, content_preview: str) -> str:
    """Generate a stable ID for a Q&A pair."""
    raw = f"{session_id}:{timestamp}:{content_preview[:200]}"
    return hashlib.sha256(raw.encode()).hexdigest()[:16]


def _classify_resolution(followup_count: int, has_code: bool) -> tuple[str, int]:
    """Classify how the Q&A was resolved based on observed signals.

    Rules:
      - followup_count >= 2  -> 'looped'  (user pushed back repeatedly)
      - has_code and followup_count <= 1  -> 'resolved'  (concrete answer, no repeated pushback)
      - otherwise  -> 'open'  (no strong signal either way)

    Returns:
        (resolution_status, loop_count) — loop_count equals followup_count verbatim.
    """
    if followup_count >= 2:
        return "looped", followup_count
    if has_code and followup_count <= 1:
        return "resolved", followup_count
    return "open", followup_count


class QAService:
    """Service for extracting and managing Q&A pairs from Claude Code sessions."""

    def __init__(self, db_path: Path | None = None):
        self.db_path = db_path or QA_DB_PATH
        self.db_path.parent.mkdir(parents=True, exist_ok=True)
        self._ensure_schema()

    def _get_conn(self) -> sqlite3.Connection:
        """Get a database connection with WAL mode."""
        conn = sqlite3.connect(str(self.db_path))
        conn.execute("PRAGMA journal_mode=WAL")
        conn.execute("PRAGMA synchronous=NORMAL")
        conn.row_factory = sqlite3.Row
        return conn

    def _ensure_schema(self):
        """Create the Q&A tables if they don't exist."""
        conn = self._get_conn()
        try:
            conn.execute("""
                CREATE TABLE IF NOT EXISTS qa_pairs (
                    id TEXT PRIMARY KEY,
                    session_id TEXT NOT NULL,
                    project TEXT NOT NULL,
                    question_text TEXT NOT NULL,
                    answer_text TEXT NOT NULL,
                    code_snippets TEXT DEFAULT '[]',
                    tools_used TEXT DEFAULT '[]',
                    timestamp TEXT,
                    model TEXT,
                    num_attempts INTEGER DEFAULT 1,
                    created_at TEXT NOT NULL,
                    resolution_status TEXT NOT NULL DEFAULT 'open',
                    loop_count INTEGER NOT NULL DEFAULT 0
                )
            """)

            # Idempotent migration for databases created before resolution_status existed.
            existing_cols = {
                row[1] for row in conn.execute("PRAGMA table_info(qa_pairs)").fetchall()
            }
            if "resolution_status" not in existing_cols:
                conn.execute(
                    "ALTER TABLE qa_pairs ADD COLUMN resolution_status TEXT NOT NULL DEFAULT 'open'"
                )
            if "loop_count" not in existing_cols:
                conn.execute(
                    "ALTER TABLE qa_pairs ADD COLUMN loop_count INTEGER NOT NULL DEFAULT 0"
                )

            # FTS5 for full-text search within Q&A
            conn.execute("""
                CREATE VIRTUAL TABLE IF NOT EXISTS qa_fts USING fts5(
                    question_text,
                    answer_text,
                    content='qa_pairs',
                    content_rowid='rowid',
                    tokenize='porter unicode61'
                )
            """)

            # Triggers to keep FTS in sync
            conn.execute("""
                CREATE TRIGGER IF NOT EXISTS qa_ai AFTER INSERT ON qa_pairs BEGIN
                    INSERT INTO qa_fts(rowid, question_text, answer_text)
                    VALUES (new.rowid, new.question_text, new.answer_text);
                END
            """)
            conn.execute("""
                CREATE TRIGGER IF NOT EXISTS qa_ad AFTER DELETE ON qa_pairs BEGIN
                    INSERT INTO qa_fts(qa_fts, rowid, question_text, answer_text)
                    VALUES('delete', old.rowid, old.question_text, old.answer_text);
                END
            """)
            conn.execute("""
                CREATE TRIGGER IF NOT EXISTS qa_au AFTER UPDATE ON qa_pairs BEGIN
                    INSERT INTO qa_fts(qa_fts, rowid, question_text, answer_text)
                    VALUES('delete', old.rowid, old.question_text, old.answer_text);
                    INSERT INTO qa_fts(rowid, question_text, answer_text)
                    VALUES (new.rowid, new.question_text, new.answer_text);
                END
            """)

            # Metadata table to track indexed projects
            conn.execute("""
                CREATE TABLE IF NOT EXISTS qa_index_metadata (
                    project TEXT PRIMARY KEY,
                    indexed_at TEXT NOT NULL,
                    qa_count INTEGER DEFAULT 0
                )
            """)

            # Indexes for filtering
            conn.execute("CREATE INDEX IF NOT EXISTS idx_qa_project ON qa_pairs(project)")
            conn.execute("CREATE INDEX IF NOT EXISTS idx_qa_timestamp ON qa_pairs(timestamp)")
            conn.execute("CREATE INDEX IF NOT EXISTS idx_qa_session ON qa_pairs(session_id)")
            conn.execute("CREATE INDEX IF NOT EXISTS idx_qa_resolution ON qa_pairs(resolution_status)")

            conn.commit()
        finally:
            conn.close()

    def extract_qa_pairs(self, project_name: str, messages: list[dict]) -> list[dict]:
        """Extract Q&A pairs from a list of processed messages.

        Args:
            project_name: The project directory name
            messages: List of message dicts from the processor (sorted newest first)

        Returns:
            List of extracted Q&A pair dicts
        """
        # Sort messages chronologically for processing
        sorted_msgs = sorted(
            messages,
            key=lambda m: m.get("timestamp", "") if m.get("timestamp") else "",
        )

        # Filter to only user and assistant messages (skip summaries, etc.)
        relevant_msgs = [
            m for m in sorted_msgs
            if m.get("type") in ("user", "assistant")
            and m.get("content", "").strip()
        ]

        qa_pairs = []
        i = 0

        while i < len(relevant_msgs):
            msg = relevant_msgs[i]

            # Skip non-user messages
            if msg.get("type") != "user":
                i += 1
                continue

            content = msg.get("content", "").strip()

            # Skip tool result messages (they start with [Tool Result: or similar)
            if content.startswith("[Tool Result:") or content.startswith("[Tool Error:"):
                i += 1
                continue

            # Check if this looks like a question
            is_q = _is_question(content)

            # Also check if the next assistant message has code blocks
            # (indicates a coding question even without explicit question markers)
            next_assistant_idx = None
            for j in range(i + 1, min(i + 5, len(relevant_msgs))):
                if relevant_msgs[j].get("type") == "assistant":
                    next_assistant_idx = j
                    break

            has_code_answer = False
            if next_assistant_idx is not None:
                assistant_content = relevant_msgs[next_assistant_idx].get("content", "")
                has_code_answer = _has_code_blocks(assistant_content)

            if not is_q and not has_code_answer:
                i += 1
                continue

            # We have a question. Collect the answer (possibly multi-turn).
            question_text = content
            answer_parts = []
            all_answer_msgs = []
            num_attempts = 0
            followup_count = 0
            session_id = msg.get("session_id", "")
            timestamp = msg.get("timestamp", "")
            model = "N/A"

            j = i + 1
            while j < len(relevant_msgs):
                next_msg = relevant_msgs[j]
                next_type = next_msg.get("type", "")
                next_content = next_msg.get("content", "").strip()

                if next_type == "assistant":
                    # Collect this assistant response
                    if next_content and not next_content.startswith("[Tool Result:"):
                        answer_parts.append(next_content)
                        all_answer_msgs.append(next_msg)
                        num_attempts += 1

                        # Track model
                        msg_model = next_msg.get("model", "N/A")
                        if msg_model and msg_model != "N/A":
                            model = msg_model

                    j += 1

                elif next_type == "user":
                    # Check if this is a follow-up to the same question
                    if next_content.startswith("[Tool Result:") or next_content.startswith("[Tool Error:"):
                        # Tool result, keep going
                        j += 1
                        continue

                    if _is_followup(next_content):
                        # This is a continuation - include it as context
                        answer_parts.append(f"\n---\n[Follow-up]: {next_content}")
                        followup_count += 1
                        j += 1
                        continue
                    else:
                        # New question - stop here
                        break
                else:
                    j += 1

            # Only create Q&A if we have both question and answer
            if answer_parts:
                answer_text = "\n\n".join(answer_parts)
                code_snippets = _extract_code_snippets(answer_text)
                tools_used = _extract_tools_used(all_answer_msgs)

                qa_id = _generate_qa_id(session_id, timestamp, question_text)

                resolution_status, loop_count = _classify_resolution(
                    followup_count=followup_count,
                    has_code=bool(code_snippets),
                )

                qa_pairs.append({
                    "id": qa_id,
                    "session_id": session_id,
                    "project": project_name,
                    "question_text": question_text,
                    "answer_text": answer_text,
                    "code_snippets": code_snippets,
                    "tools_used": tools_used,
                    "timestamp": timestamp,
                    "model": model,
                    "num_attempts": max(1, num_attempts),
                    "resolution_status": resolution_status,
                    "loop_count": loop_count,
                })

            # Move to the next unprocessed message
            i = j if j > i + 1 else i + 1

        return qa_pairs

    def index_project(self, project_name: str, messages: list[dict]):
        """Extract and store Q&A pairs for a project.

        Removes existing Q&A for the project first (full re-index).

        Args:
            project_name: The project directory name
            messages: List of message dicts from the processor
        """
        import json

        conn = self._get_conn()
        try:
            # Remove old data for this project
            conn.execute("DELETE FROM qa_pairs WHERE project = ?", (project_name,))

            # Extract Q&A pairs
            qa_pairs = self.extract_qa_pairs(project_name, messages)

            # Insert into database
            count = 0
            now = datetime.now(UTC).isoformat()
            for qa in qa_pairs:
                conn.execute(
                    """INSERT OR REPLACE INTO qa_pairs
                       (id, session_id, project, question_text, answer_text,
                        code_snippets, tools_used, timestamp, model, num_attempts, created_at)
                       VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)""",
                    (
                        qa["id"],
                        qa["session_id"],
                        qa["project"],
                        qa["question_text"],
                        qa["answer_text"],
                        json.dumps(qa["code_snippets"]),
                        json.dumps(qa["tools_used"]),
                        qa["timestamp"],
                        qa["model"],
                        qa["num_attempts"],
                        now,
                    ),
                )
                count += 1

            # Update metadata
            conn.execute(
                """INSERT OR REPLACE INTO qa_index_metadata (project, indexed_at, qa_count)
                   VALUES (?, ?, ?)""",
                (project_name, now, count),
            )

            conn.commit()
            logger.info(f"Indexed {count} Q&A pairs for project {project_name}")
        except Exception as e:
            conn.rollback()
            logger.error(f"Error indexing Q&A for project {project_name}: {e}")
            raise
        finally:
            conn.close()

    def reindex_all(self, memory_cache, cache_service) -> dict:
        """Rebuild the entire Q&A index from all available project data.

        Args:
            memory_cache: The MemoryCache instance
            cache_service: The LocalCacheService instance

        Returns:
            Dict with reindex results
        """
        from ..infra.discovery import project_metadata as get_all_projects_with_metadata
        from ..pipeline import process as _run_pipeline

        projects = get_all_projects_with_metadata()
        total_qa = 0
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
                    # Count how many were indexed
                    qa_pairs = self.extract_qa_pairs(project_name, messages)
                    total_qa += len(qa_pairs)
                    projects_indexed += 1

            except Exception as e:
                logger.error(f"Error indexing Q&A for project {project_name}: {e}")
                errors.append({"project": project_name, "error": str(e)})

        return {
            "projects_indexed": projects_indexed,
            "total_qa_indexed": total_qa,
            "errors": errors,
        }

    def list_qa(
        self,
        project: str | None = None,
        date_from: str | None = None,
        date_to: str | None = None,
        search: str | None = None,
        page: int = 1,
        per_page: int = 20,
    ) -> dict:
        """List Q&A pairs with filtering and pagination.

        Args:
            project: Optional project name filter
            date_from: Optional start date (ISO format)
            date_to: Optional end date (ISO format)
            search: Optional search text within Q&A
            page: Page number (1-indexed)
            per_page: Results per page

        Returns:
            Dict with results, pagination info
        """
        import json

        conn = self._get_conn()
        try:
            where_clauses = []
            params = []

            if project:
                where_clauses.append("q.project = ?")
                params.append(project)

            if date_from:
                where_clauses.append("q.timestamp >= ?")
                params.append(date_from)

            if date_to:
                if len(date_to) == 10:
                    date_to = date_to + "T23:59:59"
                where_clauses.append("q.timestamp <= ?")
                params.append(date_to)

            # Handle full-text search
            if search and search.strip():
                safe_query = self._sanitize_fts_query(search)
                # Use FTS join
                where_sql = "WHERE qa_fts MATCH ?"
                fts_params = [safe_query]

                if where_clauses:
                    where_sql += " AND " + " AND ".join(where_clauses)
                    fts_params.extend(params)

                # Count
                count_sql = f"""
                    SELECT COUNT(*) as total
                    FROM qa_fts
                    JOIN qa_pairs q ON qa_fts.rowid = q.rowid
                    {where_sql}
                """
                try:
                    total = conn.execute(count_sql, fts_params).fetchone()["total"]
                except sqlite3.OperationalError:
                    return {
                        "results": [],
                        "total": 0,
                        "page": page,
                        "per_page": per_page,
                        "total_pages": 0,
                    }

                total_pages = (total + per_page - 1) // per_page if total > 0 else 0
                if page < 1:
                    page = 1
                if page > total_pages and total_pages > 0:
                    page = total_pages

                offset = (page - 1) * per_page

                results_sql = f"""
                    SELECT q.*,
                           snippet(qa_fts, 0, '<mark>', '</mark>', '...', 32) as question_snippet,
                           snippet(qa_fts, 1, '<mark>', '</mark>', '...', 48) as answer_snippet
                    FROM qa_fts
                    JOIN qa_pairs q ON qa_fts.rowid = q.rowid
                    {where_sql}
                    ORDER BY q.timestamp DESC
                    LIMIT ? OFFSET ?
                """
                try:
                    rows = conn.execute(results_sql, fts_params + [per_page, offset]).fetchall()
                except sqlite3.OperationalError:
                    return {
                        "results": [],
                        "total": 0,
                        "page": page,
                        "per_page": per_page,
                        "total_pages": 0,
                    }

            else:
                # No search query - simple filter
                where_sql = ""
                if where_clauses:
                    where_sql = "WHERE " + " AND ".join(where_clauses)

                count_sql = f"SELECT COUNT(*) as total FROM qa_pairs q {where_sql}"
                total = conn.execute(count_sql, params).fetchone()["total"]

                total_pages = (total + per_page - 1) // per_page if total > 0 else 0
                if page < 1:
                    page = 1
                if page > total_pages and total_pages > 0:
                    page = total_pages

                offset = (page - 1) * per_page

                results_sql = f"""
                    SELECT q.*, NULL as question_snippet, NULL as answer_snippet
                    FROM qa_pairs q
                    {where_sql}
                    ORDER BY q.timestamp DESC
                    LIMIT ? OFFSET ?
                """
                rows = conn.execute(results_sql, params + [per_page, offset]).fetchall()

            results = []
            for row in rows:
                results.append({
                    "id": row["id"],
                    "session_id": row["session_id"],
                    "project": row["project"],
                    "question_text": row["question_text"][:500],
                    "answer_text": row["answer_text"][:500],
                    "code_snippets": json.loads(row["code_snippets"] or "[]"),
                    "tools_used": json.loads(row["tools_used"] or "[]"),
                    "timestamp": row["timestamp"],
                    "model": row["model"],
                    "num_attempts": row["num_attempts"],
                    "question_snippet": row["question_snippet"],
                    "answer_snippet": row["answer_snippet"],
                })

            return {
                "results": results,
                "total": total,
                "page": page,
                "per_page": per_page,
                "total_pages": total_pages,
            }

        finally:
            conn.close()

    def get_qa_by_id(self, qa_id: str) -> dict | None:
        """Get a single Q&A pair by ID with full content.

        Args:
            qa_id: The Q&A pair ID

        Returns:
            Full Q&A pair dict, or None if not found
        """
        import json

        conn = self._get_conn()
        try:
            row = conn.execute(
                "SELECT * FROM qa_pairs WHERE id = ?", (qa_id,)
            ).fetchone()

            if not row:
                return None

            return {
                "id": row["id"],
                "session_id": row["session_id"],
                "project": row["project"],
                "question_text": row["question_text"],
                "answer_text": row["answer_text"],
                "code_snippets": json.loads(row["code_snippets"] or "[]"),
                "tools_used": json.loads(row["tools_used"] or "[]"),
                "timestamp": row["timestamp"],
                "model": row["model"],
                "num_attempts": row["num_attempts"],
                "created_at": row["created_at"],
            }
        finally:
            conn.close()

    def get_stats(self) -> dict:
        """Get statistics about the Q&A index.

        Returns:
            Dict with total pairs, by project, etc.
        """
        conn = self._get_conn()
        try:
            total = conn.execute("SELECT COUNT(*) as c FROM qa_pairs").fetchone()["c"]

            # By project
            by_project_rows = conn.execute(
                "SELECT project, COUNT(*) as count FROM qa_pairs GROUP BY project ORDER BY count DESC"
            ).fetchall()
            by_project = [{"project": row["project"], "count": row["count"]} for row in by_project_rows]

            # By date (last 30 days)
            by_date_rows = conn.execute(
                """SELECT substr(timestamp, 1, 10) as date, COUNT(*) as count
                   FROM qa_pairs
                   WHERE timestamp IS NOT NULL AND timestamp != ''
                   GROUP BY date
                   ORDER BY date DESC
                   LIMIT 30"""
            ).fetchall()
            by_date = [{"date": row["date"], "count": row["count"]} for row in by_date_rows]

            # Indexed projects
            indexed_projects_rows = conn.execute(
                "SELECT project, indexed_at, qa_count FROM qa_index_metadata ORDER BY project"
            ).fetchall()
            indexed_projects = [dict(row) for row in indexed_projects_rows]

            # With code snippets count
            with_code = conn.execute(
                "SELECT COUNT(*) as c FROM qa_pairs WHERE code_snippets != '[]'"
            ).fetchone()["c"]

            return {
                "total_pairs": total,
                "by_project": by_project,
                "by_date": by_date,
                "indexed_projects": indexed_projects,
                "with_code_snippets": with_code,
            }
        finally:
            conn.close()

    def _sanitize_fts_query(self, query: str) -> str:
        """Sanitize user input for safe FTS5 querying."""
        query = query.strip()
        if not query:
            return '""'

        fts5_operators = re.compile(r'\b(AND|OR|NOT|NEAR)\b|[*"]', re.IGNORECASE)
        if fts5_operators.search(query):
            return query

        words = query.split()
        if len(words) == 1:
            escaped = words[0].replace('"', '""')
            return f'"{escaped}"*'
        else:
            parts = []
            for word in words:
                escaped = word.replace('"', '""')
                parts.append(f'"{escaped}"*')
            return " ".join(parts)
