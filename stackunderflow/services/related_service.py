"""
Related sessions discovery service.

Given a session_id, finds related sessions using a similarity score based on:
- Shared tags (languages, frameworks, topics)
- Same project
- Similar tool usage patterns
- Recency preference

Leverages the existing TagService for tag data.
"""

import logging
import time
from collections import defaultdict
from datetime import UTC, datetime

from .tag_service import TagService

logger = logging.getLogger(__name__)

# Tag category weights for similarity scoring
TAG_CATEGORY_WEIGHTS = {
    "language": 2,
    "framework": 2,
    "topic": 1,
    "tool": 0,      # Tools are scored separately via tool overlap
    "custom": 1,
}

# Score boost for sessions in the same project
SAME_PROJECT_BOOST = 3

# Score boost for sessions within last 30 days
RECENCY_BOOST = 1

# Days threshold for recency boost
RECENCY_DAYS = 30


class RelatedService:
    """Service for finding related sessions based on similarity scoring."""

    def __init__(self, tag_service: TagService | None = None):
        self.tag_service = tag_service or TagService()

    def find_related(
        self,
        session_id: str,
        messages: list[dict] | None = None,
        limit: int = 5,
    ) -> list[dict]:
        """Find sessions related to the given session_id.

        Uses a weighted similarity score based on shared tags, project overlap,
        tool usage patterns, and recency.

        Args:
            session_id: The session to find related sessions for
            messages: Optional list of all messages (used for project/tool/timestamp info)
            limit: Maximum number of related sessions to return

        Returns:
            List of related session dicts sorted by score descending, each containing:
            - session_id: The related session's ID
            - project: The project directory name (if known)
            - score: Similarity score
            - shared_tags: List of tags in common
            - timestamp: Latest timestamp of the related session
            - preview_text: Short preview of the session content
        """
        start_time = time.time()

        # Load tag data
        tag_data = self.tag_service._load_tags()
        auto_tags = tag_data.get("auto_tags", {})
        manual_tags = tag_data.get("manual_tags", {})
        metadata = tag_data.get("tag_metadata", {})

        # Get tags for the target session
        target_auto = set(auto_tags.get(session_id, []))
        target_manual = set(manual_tags.get(session_id, []))
        target_all_tags = target_auto | target_manual

        if not target_all_tags and not messages:
            # No tags and no messages to work with
            logger.debug(f"No tags found for session {session_id}, cannot find related sessions")
            return []

        # Build session index from messages if available
        session_index = self._build_session_index(messages) if messages else {}

        # Get target session info
        target_info = session_index.get(session_id, {})
        target_project = target_info.get("project", "")
        target_tools = set(target_info.get("tools", []))

        # Collect all sessions that have tags
        all_session_ids = set()
        for sid in auto_tags:
            all_session_ids.add(sid)
        for sid in manual_tags:
            all_session_ids.add(sid)
        # Also include sessions from messages
        for sid in session_index:
            all_session_ids.add(sid)

        # Remove the target session
        all_session_ids.discard(session_id)

        if not all_session_ids:
            return []

        # Score each candidate session
        scored_sessions = []

        for candidate_id in all_session_ids:
            score = 0.0
            shared_tags = []

            # Get candidate tags
            candidate_auto = set(auto_tags.get(candidate_id, []))
            candidate_manual = set(manual_tags.get(candidate_id, []))
            candidate_all_tags = candidate_auto | candidate_manual

            # Score shared tags by category
            common_tags = target_all_tags & candidate_all_tags
            for tag in common_tags:
                tag_meta = metadata.get(tag, {})
                category = tag_meta.get("category", "custom")
                weight = TAG_CATEGORY_WEIGHTS.get(category, 1)
                score += weight
                # Only include non-tool tags as "shared tags" in the result
                if category != "tool":
                    shared_tags.append(tag)

            # Get candidate session info
            candidate_info = session_index.get(candidate_id, {})
            candidate_project = candidate_info.get("project", "")
            candidate_tools = set(candidate_info.get("tools", []))
            candidate_timestamp = candidate_info.get("timestamp", "")

            # Same project boost
            if target_project and candidate_project and target_project == candidate_project:
                score += SAME_PROJECT_BOOST

            # Tool overlap scoring (separate from tag-based tools)
            if target_tools and candidate_tools:
                tool_overlap = len(target_tools & candidate_tools)
                total_tools = len(target_tools | candidate_tools)
                if total_tools > 0:
                    # Jaccard-like similarity, scaled
                    score += (tool_overlap / total_tools) * 2

            # Recency boost
            if candidate_timestamp:
                try:
                    candidate_dt = datetime.fromisoformat(
                        candidate_timestamp.replace("Z", "+00:00")
                    )
                    now = datetime.now(UTC)
                    days_ago = (now - candidate_dt).days
                    if days_ago <= RECENCY_DAYS:
                        score += RECENCY_BOOST
                except (ValueError, AttributeError):
                    pass

            # Only include sessions with a positive score
            if score > 0:
                # Build preview text
                preview_text = candidate_info.get("preview", "")

                scored_sessions.append({
                    "session_id": candidate_id,
                    "project": candidate_project,
                    "score": round(score, 2),
                    "shared_tags": sorted(shared_tags),
                    "timestamp": candidate_timestamp,
                    "preview_text": preview_text[:200] if preview_text else "",
                })

        # Sort by score descending, then by timestamp descending (most recent first)
        scored_sessions.sort(
            key=lambda s: (s["score"], s["timestamp"] or ""),
            reverse=True,
        )

        # Limit results
        results = scored_sessions[:limit]

        elapsed_ms = (time.time() - start_time) * 1000
        logger.debug(
            f"Found {len(results)} related sessions for {session_id} "
            f"in {elapsed_ms:.1f}ms (scored {len(all_session_ids)} candidates)"
        )

        return results

    def _build_session_index(self, messages: list[dict]) -> dict[str, dict]:
        """Build an index of session metadata from messages.

        Extracts project, tools used, timestamps, and a content preview
        for each session.

        Args:
            messages: List of message dicts from the processor

        Returns:
            Dict mapping session_id -> {project, tools, timestamp, preview}
        """
        sessions = defaultdict(lambda: {
            "project": "",
            "tools": set(),
            "timestamp": "",
            "preview": "",
        })

        for msg in messages:
            sid = msg.get("session_id", "")
            if not sid:
                continue

            session = sessions[sid]

            # Track project from cwd (use the first non-empty cwd seen)
            if not session["project"] and msg.get("cwd"):
                session["project"] = msg["cwd"]

            # Track latest timestamp
            ts = msg.get("timestamp", "")
            if ts and (not session["timestamp"] or ts > session["timestamp"]):
                session["timestamp"] = ts

            # Track tools used
            for tool in msg.get("tools", []):
                tool_name = tool.get("name", "")
                if tool_name:
                    session["tools"].add(tool_name)

            # Get preview from first user message
            if not session["preview"] and msg.get("type") == "user":
                content = msg.get("content", "").strip()
                if content and not content.startswith("[Tool Result:"):
                    session["preview"] = content[:200]

        # Convert sets to lists for JSON serialization
        result = {}
        for sid, data in sessions.items():
            result[sid] = {
                "project": data["project"],
                "tools": list(data["tools"]),
                "timestamp": data["timestamp"],
                "preview": data["preview"],
            }

        return result
