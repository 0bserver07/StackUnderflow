"""
Social features service for StackUnderflow.

Manages AI agent personas, discussion threads on Q&A pairs, voting,
and agent run tracking. Stores data in SQLite at ~/.stackunderflow/social.db.
"""

import logging
import sqlite3
import uuid
from datetime import UTC, datetime
from pathlib import Path

logger = logging.getLogger(__name__)

SOCIAL_DB_PATH = Path.home() / ".stackunderflow" / "social.db"


class SocialService:
    """Service for managing social features: agents, discussions, votes, and agent runs."""

    def __init__(self, db_path: Path | None = None):
        self.db_path = db_path or SOCIAL_DB_PATH
        self.db_path.parent.mkdir(parents=True, exist_ok=True)
        self._ensure_schema()
        self.seed_default_agents()

    def _get_conn(self) -> sqlite3.Connection:
        """Get a database connection with WAL mode."""
        conn = sqlite3.connect(str(self.db_path))
        conn.execute("PRAGMA journal_mode=WAL")
        conn.execute("PRAGMA synchronous=NORMAL")
        conn.row_factory = sqlite3.Row
        return conn

    def _ensure_schema(self):
        """Create the social tables if they don't exist."""
        conn = self._get_conn()
        try:
            conn.execute("""
                CREATE TABLE IF NOT EXISTS agents (
                    id TEXT PRIMARY KEY,
                    name TEXT NOT NULL,
                    role TEXT NOT NULL,
                    avatar_emoji TEXT DEFAULT '🤖',
                    avatar_color TEXT DEFAULT '#6366f1',
                    system_prompt TEXT NOT NULL,
                    memory TEXT DEFAULT '',
                    expertise_tags TEXT DEFAULT '[]',
                    ollama_model TEXT DEFAULT '',
                    status TEXT DEFAULT 'active',
                    total_posts INTEGER DEFAULT 0,
                    total_likes_received INTEGER DEFAULT 0,
                    created_at TEXT NOT NULL,
                    updated_at TEXT NOT NULL
                )
            """)

            conn.execute("""
                CREATE TABLE IF NOT EXISTS discussions (
                    id TEXT PRIMARY KEY,
                    qa_id TEXT NOT NULL,
                    parent_id TEXT,
                    author_type TEXT NOT NULL,
                    author_id TEXT NOT NULL,
                    content TEXT NOT NULL,
                    created_at TEXT NOT NULL,
                    updated_at TEXT NOT NULL
                )
            """)
            conn.execute("CREATE INDEX IF NOT EXISTS idx_discussions_qa ON discussions(qa_id)")
            conn.execute("CREATE INDEX IF NOT EXISTS idx_discussions_parent ON discussions(parent_id)")

            conn.execute("""
                CREATE TABLE IF NOT EXISTS votes (
                    id TEXT PRIMARY KEY,
                    target_type TEXT NOT NULL,
                    target_id TEXT NOT NULL,
                    voter_type TEXT NOT NULL,
                    voter_id TEXT NOT NULL,
                    value INTEGER NOT NULL DEFAULT 1,
                    created_at TEXT NOT NULL,
                    UNIQUE(target_id, voter_id)
                )
            """)
            conn.execute("CREATE INDEX IF NOT EXISTS idx_votes_target ON votes(target_type, target_id)")

            conn.execute("""
                CREATE TABLE IF NOT EXISTS agent_runs (
                    id TEXT PRIMARY KEY,
                    qa_id TEXT NOT NULL,
                    status TEXT DEFAULT 'pending',
                    agents_involved TEXT DEFAULT '[]',
                    total_steps INTEGER DEFAULT 0,
                    completed_steps INTEGER DEFAULT 0,
                    error TEXT,
                    created_at TEXT NOT NULL,
                    completed_at TEXT
                )
            """)

            conn.commit()
        finally:
            conn.close()

    def seed_default_agents(self):
        """Insert default agent personas if they don't already exist."""
        import json

        now = datetime.now(UTC).isoformat()
        default_agents = [
            {
                "id": "security-reviewer",
                "name": "Security Reviewer",
                "role": "Security Analyst",
                "avatar_emoji": "\U0001f6e1\ufe0f",
                "avatar_color": "#ef4444",
                "system_prompt": (
                    "You are a meticulous security reviewer. You examine code for vulnerabilities "
                    "including injection attacks (SQL, XSS, command injection), authentication/"
                    "authorization flaws, secrets exposure, insecure cryptography, and OWASP Top 10 "
                    "issues. You're direct and specific about risks, always suggesting concrete fixes. "
                    "You occasionally reference CVEs and real-world breaches. Your tone is serious but "
                    "not alarmist."
                ),
                "expertise_tags": json.dumps(["security", "vulnerabilities", "auth", "crypto", "owasp"]),
            },
            {
                "id": "architecture-expert",
                "name": "Architecture Expert",
                "role": "Software Architect",
                "avatar_emoji": "\U0001f3d7\ufe0f",
                "avatar_color": "#8b5cf6",
                "system_prompt": (
                    "You are an experienced software architect who evaluates code structure, design "
                    "patterns, and system organization. You look for separation of concerns, dependency "
                    "management, API design, scalability considerations, and adherence to SOLID principles. "
                    "You appreciate clean abstractions but warn against over-engineering. Your reviews "
                    "reference well-known patterns (Factory, Observer, Repository) when relevant."
                ),
                "expertise_tags": json.dumps(["architecture", "design-patterns", "solid", "scalability", "api-design"]),
            },
            {
                "id": "performance-optimizer",
                "name": "Performance Optimizer",
                "role": "Performance Engineer",
                "avatar_emoji": "\u26a1",
                "avatar_color": "#f59e0b",
                "system_prompt": (
                    "You are a performance-focused engineer who spots inefficiencies in code. You look "
                    "for N+1 queries, missing indexes, unnecessary memory allocations, inefficient "
                    "algorithms, missing caching opportunities, and blocking I/O in async contexts. "
                    "You think in terms of Big-O complexity and production-scale load. You suggest "
                    "benchmarks and profiling approaches."
                ),
                "expertise_tags": json.dumps(["performance", "optimization", "caching", "algorithms", "database"]),
            },
            {
                "id": "code-mentor",
                "name": "Code Mentor",
                "role": "Senior Developer & Educator",
                "avatar_emoji": "\U0001f4da",
                "avatar_color": "#10b981",
                "system_prompt": (
                    "You are a friendly senior developer who focuses on code readability, maintainability, "
                    "and teaching opportunities. You explain WHY certain patterns are preferred, suggest "
                    "better naming conventions, identify missing documentation, and highlight learning "
                    "opportunities. You're encouraging but honest, and you always consider the perspective "
                    "of someone who'll maintain this code next year."
                ),
                "expertise_tags": json.dumps(["readability", "maintainability", "best-practices", "documentation", "mentoring"]),
            },
            {
                "id": "devils-advocate",
                "name": "Devil's Advocate",
                "role": "Critical Thinker",
                "avatar_emoji": "\U0001f608",
                "avatar_color": "#ec4899",
                "system_prompt": (
                    "You are a provocative critical thinker who challenges assumptions and explores edge "
                    "cases. You ask 'what happens when this fails?', 'what if the input is malicious?', "
                    "'does this handle the empty case?', and 'what happens at 10x scale?'. You play "
                    "devil's advocate to strengthen the code. Your tone is playfully skeptical, and you "
                    "often pose questions rather than make statements."
                ),
                "expertise_tags": json.dumps(["edge-cases", "error-handling", "resilience", "testing", "assumptions"]),
            },
        ]

        conn = self._get_conn()
        try:
            for agent in default_agents:
                conn.execute(
                    """INSERT OR IGNORE INTO agents
                       (id, name, role, avatar_emoji, avatar_color, system_prompt,
                        memory, expertise_tags, ollama_model, status,
                        total_posts, total_likes_received, created_at, updated_at)
                       VALUES (?, ?, ?, ?, ?, ?, '', ?, '', 'active', 0, 0, ?, ?)""",
                    (
                        agent["id"],
                        agent["name"],
                        agent["role"],
                        agent["avatar_emoji"],
                        agent["avatar_color"],
                        agent["system_prompt"],
                        agent["expertise_tags"],
                        now,
                        now,
                    ),
                )
            conn.commit()
        finally:
            conn.close()

    # ── Agent CRUD ──────────────────────────────────────────────────────

    def list_agents(self) -> list[dict]:
        """List all agents."""
        import json

        conn = self._get_conn()
        try:
            rows = conn.execute("SELECT * FROM agents ORDER BY name").fetchall()
            results = []
            for row in rows:
                agent = dict(row)
                agent["expertise_tags"] = json.loads(agent["expertise_tags"] or "[]")
                results.append(agent)
            return results
        finally:
            conn.close()

    def get_agent(self, agent_id: str) -> dict | None:
        """Get a single agent by ID."""
        import json

        conn = self._get_conn()
        try:
            row = conn.execute("SELECT * FROM agents WHERE id = ?", (agent_id,)).fetchone()
            if not row:
                return None
            agent = dict(row)
            agent["expertise_tags"] = json.loads(agent["expertise_tags"] or "[]")
            return agent
        finally:
            conn.close()

    def create_agent(self, data: dict) -> dict:
        """Create a new agent."""
        import json

        now = datetime.now(UTC).isoformat()
        agent_id = data.get("id") or str(uuid.uuid4())

        conn = self._get_conn()
        try:
            conn.execute(
                """INSERT INTO agents
                   (id, name, role, avatar_emoji, avatar_color, system_prompt,
                    memory, expertise_tags, ollama_model, status,
                    total_posts, total_likes_received, created_at, updated_at)
                   VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, 'active', 0, 0, ?, ?)""",
                (
                    agent_id,
                    data.get("name", "New Agent"),
                    data.get("role", "Assistant"),
                    data.get("avatar_emoji", "\U0001f916"),
                    data.get("avatar_color", "#6366f1"),
                    data.get("system_prompt", "You are a helpful assistant."),
                    data.get("memory", ""),
                    json.dumps(data.get("expertise_tags", [])),
                    data.get("ollama_model", ""),
                    now,
                    now,
                ),
            )
            conn.commit()

            # Re-fetch the newly created agent (will always exist since we just inserted)
            agent = self.get_agent(agent_id)
            if agent is None:
                # Should never happen since we just inserted
                return {
                    "id": agent_id,
                    "name": data.get("name", "New Agent"),
                    "role": data.get("role", "Assistant"),
                    "created_at": now,
                    "updated_at": now,
                }
            return agent
        finally:
            conn.close()

    def update_agent(self, agent_id: str, data: dict) -> dict | None:
        """Update an existing agent."""
        import json

        conn = self._get_conn()
        try:
            existing = conn.execute("SELECT * FROM agents WHERE id = ?", (agent_id,)).fetchone()
            if not existing:
                return None

            now = datetime.now(UTC).isoformat()
            existing = dict(existing)

            name = data.get("name", existing["name"])
            role = data.get("role", existing["role"])
            avatar_emoji = data.get("avatar_emoji", existing["avatar_emoji"])
            avatar_color = data.get("avatar_color", existing["avatar_color"])
            system_prompt = data.get("system_prompt", existing["system_prompt"])
            memory = data.get("memory", existing["memory"])
            expertise_tags = json.dumps(data.get("expertise_tags", json.loads(existing["expertise_tags"] or "[]")))
            ollama_model = data.get("ollama_model", existing["ollama_model"])
            status = data.get("status", existing["status"])

            conn.execute(
                """UPDATE agents SET
                       name=?, role=?, avatar_emoji=?, avatar_color=?,
                       system_prompt=?, memory=?, expertise_tags=?,
                       ollama_model=?, status=?, updated_at=?
                   WHERE id=?""",
                (
                    name, role, avatar_emoji, avatar_color,
                    system_prompt, memory, expertise_tags,
                    ollama_model, status, now,
                    agent_id,
                ),
            )
            conn.commit()
        finally:
            conn.close()

        return self.get_agent(agent_id)

    # ── Discussions ─────────────────────────────────────────────────────

    def get_discussion_tree(self, qa_id: str) -> dict:
        """Get the full discussion tree for a Q&A pair.

        Returns nested structure with author info and vote counts.
        """

        conn = self._get_conn()
        try:
            rows = conn.execute(
                """SELECT d.*,
                          a.name AS agent_name,
                          a.avatar_emoji AS agent_emoji,
                          a.avatar_color AS agent_color,
                          a.role AS agent_role,
                          COALESCE(v.vote_count, 0) AS vote_count
                   FROM discussions d
                   LEFT JOIN agents a ON d.author_type = 'agent' AND d.author_id = a.id
                   LEFT JOIN (
                       SELECT target_id, SUM(value) AS vote_count
                       FROM votes
                       WHERE target_type = 'discussion'
                       GROUP BY target_id
                   ) v ON v.target_id = d.id
                   WHERE d.qa_id = ?
                   ORDER BY d.created_at ASC""",
                (qa_id,),
            ).fetchall()

            posts_by_id = {}
            root_posts = []

            for row in rows:
                post = {
                    "id": row["id"],
                    "qa_id": row["qa_id"],
                    "parent_id": row["parent_id"],
                    "author_type": row["author_type"],
                    "author_id": row["author_id"],
                    "content": row["content"],
                    "created_at": row["created_at"],
                    "updated_at": row["updated_at"],
                    "vote_count": row["vote_count"],
                    "children": [],
                }

                if row["author_type"] == "agent":
                    post["author_name"] = row["agent_name"] or "Unknown Agent"
                    post["author_emoji"] = row["agent_emoji"] or "\U0001f916"
                    post["author_color"] = row["agent_color"] or "#6366f1"
                    post["author_role"] = row["agent_role"] or "Agent"
                else:
                    post["author_name"] = "You"
                    post["author_emoji"] = "\U0001f464"
                    post["author_color"] = "#6b7280"
                    post["author_role"] = "Human"

                posts_by_id[post["id"]] = post

            # Build tree
            for post in posts_by_id.values():
                if post["parent_id"] and post["parent_id"] in posts_by_id:
                    posts_by_id[post["parent_id"]]["children"].append(post)
                else:
                    root_posts.append(post)

            return {
                "qa_id": qa_id,
                "posts": root_posts,
                "total_count": len(posts_by_id),
            }
        finally:
            conn.close()

    def create_discussion(self, qa_id: str, author_type: str, author_id: str,
                          content: str, parent_id: str | None = None) -> dict:
        """Create a new discussion post on a Q&A pair."""
        now = datetime.now(UTC).isoformat()
        discussion_id = str(uuid.uuid4())

        conn = self._get_conn()
        try:
            conn.execute(
                """INSERT INTO discussions
                   (id, qa_id, parent_id, author_type, author_id, content, created_at, updated_at)
                   VALUES (?, ?, ?, ?, ?, ?, ?, ?)""",
                (discussion_id, qa_id, parent_id, author_type, author_id, content, now, now),
            )

            if author_type == "agent":
                conn.execute(
                    "UPDATE agents SET total_posts = total_posts + 1, updated_at = ? WHERE id = ?",
                    (now, author_id),
                )

            conn.commit()

            return {
                "id": discussion_id,
                "qa_id": qa_id,
                "parent_id": parent_id,
                "author_type": author_type,
                "author_id": author_id,
                "content": content,
                "created_at": now,
                "updated_at": now,
            }
        finally:
            conn.close()

    # ── Votes ───────────────────────────────────────────────────────────

    def toggle_vote(self, target_type: str, target_id: str,
                    voter_type: str = "human", voter_id: str = "human") -> dict:
        """Toggle a vote on a target (discussion or Q&A).

        Returns dict with 'voted' bool and 'new_count'.
        """
        now = datetime.now(UTC).isoformat()

        conn = self._get_conn()
        try:
            existing = conn.execute(
                "SELECT id FROM votes WHERE target_id = ? AND voter_id = ?",
                (target_id, voter_id),
            ).fetchone()

            if existing:
                # If un-voting a discussion by an agent, decrement their likes
                if target_type == "discussion":
                    discussion = conn.execute(
                        "SELECT author_type, author_id FROM discussions WHERE id = ?",
                        (target_id,),
                    ).fetchone()
                    if discussion and discussion["author_type"] == "agent":
                        conn.execute(
                            "UPDATE agents SET total_likes_received = total_likes_received - 1, updated_at = ? WHERE id = ?",
                            (now, discussion["author_id"]),
                        )

                conn.execute("DELETE FROM votes WHERE id = ?", (existing["id"],))
                voted = False
            else:
                vote_id = str(uuid.uuid4())
                conn.execute(
                    """INSERT INTO votes (id, target_type, target_id, voter_type, voter_id, value, created_at)
                       VALUES (?, ?, ?, ?, ?, 1, ?)""",
                    (vote_id, target_type, target_id, voter_type, voter_id, now),
                )
                voted = True

                # If voting up a discussion by an agent, increment their likes
                if target_type == "discussion":
                    discussion = conn.execute(
                        "SELECT author_type, author_id FROM discussions WHERE id = ?",
                        (target_id,),
                    ).fetchone()
                    if discussion and discussion["author_type"] == "agent":
                        conn.execute(
                            "UPDATE agents SET total_likes_received = total_likes_received + 1, updated_at = ? WHERE id = ?",
                            (now, discussion["author_id"]),
                        )

            conn.commit()

            count_row = conn.execute(
                "SELECT COALESCE(SUM(value), 0) AS cnt FROM votes WHERE target_type = ? AND target_id = ?",
                (target_type, target_id),
            ).fetchone()

            return {
                "voted": voted,
                "new_count": count_row["cnt"],
            }
        finally:
            conn.close()

    def get_vote_counts(self, target_type: str, target_ids: list) -> dict:
        """Get vote counts for a list of targets.

        Returns dict mapping target_id -> count.
        """
        if not target_ids:
            return {}

        conn = self._get_conn()
        try:
            placeholders = ",".join("?" for _ in target_ids)
            rows = conn.execute(
                f"""SELECT target_id, COALESCE(SUM(value), 0) AS cnt
                    FROM votes
                    WHERE target_type = ? AND target_id IN ({placeholders})
                    GROUP BY target_id""",
                [target_type] + list(target_ids),
            ).fetchall()

            result = dict.fromkeys(target_ids, 0)
            for row in rows:
                result[row["target_id"]] = row["cnt"]
            return result
        finally:
            conn.close()

    def get_user_votes(self, target_type: str, target_ids: list,
                       voter_id: str = "human") -> dict:
        """Get which targets a user has voted on.

        Returns dict mapping target_id -> bool.
        """
        if not target_ids:
            return {}

        conn = self._get_conn()
        try:
            placeholders = ",".join("?" for _ in target_ids)
            rows = conn.execute(
                f"""SELECT target_id
                    FROM votes
                    WHERE target_type = ? AND voter_id = ? AND target_id IN ({placeholders})""",
                [target_type, voter_id] + list(target_ids),
            ).fetchall()

            voted_set = {row["target_id"] for row in rows}
            return {tid: tid in voted_set for tid in target_ids}
        finally:
            conn.close()

    def get_qa_social_stats(self, qa_ids: list) -> dict:
        """Get social stats for a list of Q&A pairs.

        Returns dict mapping qa_id -> {discussion_count, vote_count, user_voted, agent_avatars}.
        """
        if not qa_ids:
            return {}

        conn = self._get_conn()
        try:
            placeholders = ",".join("?" for _ in qa_ids)

            # Discussion counts per qa_id
            disc_rows = conn.execute(
                f"""SELECT qa_id, COUNT(*) AS cnt
                    FROM discussions
                    WHERE qa_id IN ({placeholders})
                    GROUP BY qa_id""",
                list(qa_ids),
            ).fetchall()
            disc_counts = {row["qa_id"]: row["cnt"] for row in disc_rows}

            # Vote counts per qa_id (votes on the qa itself)
            vote_rows = conn.execute(
                f"""SELECT target_id, COALESCE(SUM(value), 0) AS cnt
                    FROM votes
                    WHERE target_type = 'qa' AND target_id IN ({placeholders})
                    GROUP BY target_id""",
                list(qa_ids),
            ).fetchall()
            vote_counts = {row["target_id"]: row["cnt"] for row in vote_rows}

            # User votes on qa_ids
            user_vote_rows = conn.execute(
                f"""SELECT target_id
                    FROM votes
                    WHERE target_type = 'qa' AND voter_id = 'human' AND target_id IN ({placeholders})""",
                list(qa_ids),
            ).fetchall()
            user_voted = {row["target_id"] for row in user_vote_rows}

            # Agent avatars that participated in discussions per qa_id
            avatar_rows = conn.execute(
                f"""SELECT DISTINCT d.qa_id, a.avatar_emoji, a.avatar_color
                    FROM discussions d
                    JOIN agents a ON d.author_id = a.id
                    WHERE d.author_type = 'agent' AND d.qa_id IN ({placeholders})""",
                list(qa_ids),
            ).fetchall()
            agent_avatars = {}
            for row in avatar_rows:
                qa_id = row["qa_id"]
                if qa_id not in agent_avatars:
                    agent_avatars[qa_id] = []
                agent_avatars[qa_id].append({
                    "emoji": row["avatar_emoji"],
                    "color": row["avatar_color"],
                })

            result = {}
            for qa_id in qa_ids:
                result[qa_id] = {
                    "discussion_count": disc_counts.get(qa_id, 0),
                    "vote_count": vote_counts.get(qa_id, 0),
                    "user_voted": qa_id in user_voted,
                    "agent_avatars": agent_avatars.get(qa_id, []),
                }
            return result
        finally:
            conn.close()

    # ── Agent Runs ──────────────────────────────────────────────────────

    def create_agent_run(self, qa_id: str, agent_ids: list) -> dict:
        """Create a new agent run record."""
        import json

        now = datetime.now(UTC).isoformat()
        run_id = str(uuid.uuid4())

        conn = self._get_conn()
        try:
            conn.execute(
                """INSERT INTO agent_runs
                   (id, qa_id, status, agents_involved, total_steps, completed_steps, error, created_at, completed_at)
                   VALUES (?, ?, 'pending', ?, ?, 0, NULL, ?, NULL)""",
                (run_id, qa_id, json.dumps(agent_ids), len(agent_ids), now),
            )
            conn.commit()

            return {
                "id": run_id,
                "qa_id": qa_id,
                "status": "pending",
                "agents_involved": agent_ids,
                "total_steps": len(agent_ids),
                "completed_steps": 0,
                "error": None,
                "created_at": now,
                "completed_at": None,
            }
        finally:
            conn.close()

    def update_agent_run(self, run_id: str, status: str | None = None,
                         completed_steps: int | None = None,
                         total_steps: int | None = None,
                         error: str | None = None,
                         completed_at: str | None = None) -> dict | None:
        """Update an agent run record."""
        import json

        conn = self._get_conn()
        try:
            existing = conn.execute("SELECT * FROM agent_runs WHERE id = ?", (run_id,)).fetchone()
            if not existing:
                return None

            updates = []
            params = []

            if status is not None:
                updates.append("status = ?")
                params.append(status)
            if completed_steps is not None:
                updates.append("completed_steps = ?")
                params.append(completed_steps)
            if total_steps is not None:
                updates.append("total_steps = ?")
                params.append(total_steps)
            if error is not None:
                updates.append("error = ?")
                params.append(error)
            if completed_at is not None:
                updates.append("completed_at = ?")
                params.append(completed_at)

            if not updates:
                row = existing
            else:
                params.append(run_id)
                conn.execute(
                    f"UPDATE agent_runs SET {', '.join(updates)} WHERE id = ?",
                    params,
                )
                conn.commit()
                row = conn.execute("SELECT * FROM agent_runs WHERE id = ?", (run_id,)).fetchone()

            return {
                "id": row["id"],
                "qa_id": row["qa_id"],
                "status": row["status"],
                "agents_involved": json.loads(row["agents_involved"] or "[]"),
                "total_steps": row["total_steps"],
                "completed_steps": row["completed_steps"],
                "error": row["error"],
                "created_at": row["created_at"],
                "completed_at": row["completed_at"],
            }
        finally:
            conn.close()

    def get_agent_run(self, run_id: str) -> dict | None:
        """Get an agent run by ID."""
        import json

        conn = self._get_conn()
        try:
            row = conn.execute("SELECT * FROM agent_runs WHERE id = ?", (run_id,)).fetchone()
            if not row:
                return None

            return {
                "id": row["id"],
                "qa_id": row["qa_id"],
                "status": row["status"],
                "agents_involved": json.loads(row["agents_involved"] or "[]"),
                "total_steps": row["total_steps"],
                "completed_steps": row["completed_steps"],
                "error": row["error"],
                "created_at": row["created_at"],
                "completed_at": row["completed_at"],
            }
        finally:
            conn.close()
