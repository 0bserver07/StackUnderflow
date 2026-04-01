"""
Bookmark service for saving and managing session/message bookmarks.

Stores bookmarks in ~/.stackunderflow/bookmarks.json as a simple JSON file.
"""

import json
import logging
import uuid
from datetime import UTC, datetime
from pathlib import Path

logger = logging.getLogger(__name__)


class BookmarkService:
    """Service for managing bookmarks stored in a local JSON file."""

    def __init__(self):
        self.storage_dir = Path.home() / ".stackunderflow"
        self.bookmarks_file = self.storage_dir / "bookmarks.json"

        # Ensure storage directory exists
        self.storage_dir.mkdir(parents=True, exist_ok=True)

    def _load_bookmarks(self) -> list[dict]:
        """Load all bookmarks from the JSON file."""
        if not self.bookmarks_file.exists():
            return []

        try:
            data = json.loads(self.bookmarks_file.read_text())
            if isinstance(data, list):
                return data
            return []
        except (OSError, json.JSONDecodeError) as e:
            logger.warning(f"Error loading bookmarks: {e}")
            return []

    def _save_bookmarks(self, bookmarks: list[dict]):
        """Save all bookmarks to the JSON file."""
        try:
            self.bookmarks_file.write_text(json.dumps(bookmarks, indent=2))
        except OSError as e:
            logger.error(f"Error saving bookmarks: {e}")
            raise

    def list_all(self, tag: str | None = None, sort_by: str = "created_at") -> list[dict]:
        """List all bookmarks, optionally filtered by tag.

        Args:
            tag: Optional tag to filter by
            sort_by: Sort field (created_at, updated_at, title)

        Returns:
            List of bookmark dicts
        """
        bookmarks = self._load_bookmarks()

        if tag:
            bookmarks = [b for b in bookmarks if tag in b.get("tags", [])]

        # Sort
        reverse = sort_by in ("created_at", "updated_at")
        bookmarks.sort(key=lambda b: b.get(sort_by, ""), reverse=reverse)

        return bookmarks

    def get_by_session(self, session_id: str) -> list[dict]:
        """Get all bookmarks for a specific session.

        Args:
            session_id: The session ID to look up

        Returns:
            List of bookmark dicts for this session
        """
        bookmarks = self._load_bookmarks()
        return [b for b in bookmarks if b.get("session_id") == session_id]

    def get_by_id(self, bookmark_id: str) -> dict | None:
        """Get a single bookmark by ID.

        Args:
            bookmark_id: The bookmark ID

        Returns:
            Bookmark dict or None
        """
        bookmarks = self._load_bookmarks()
        for b in bookmarks:
            if b.get("id") == bookmark_id:
                return b
        return None

    def add(
        self,
        session_id: str,
        title: str,
        message_index: int | None = None,
        notes: str = "",
        tags: list[str] | None = None,
    ) -> dict:
        """Add a new bookmark.

        Args:
            session_id: Session ID to bookmark
            title: Title for the bookmark
            message_index: Optional message index within the session
            notes: Optional notes
            tags: Optional list of tags

        Returns:
            The created bookmark dict
        """
        bookmarks = self._load_bookmarks()

        now = datetime.now(UTC).isoformat()
        bookmark = {
            "id": str(uuid.uuid4()),
            "session_id": session_id,
            "message_index": message_index,
            "title": title,
            "notes": notes,
            "tags": tags or [],
            "created_at": now,
            "updated_at": now,
        }

        bookmarks.append(bookmark)
        self._save_bookmarks(bookmarks)

        return bookmark

    def remove(self, bookmark_id: str) -> bool:
        """Remove a bookmark by ID.

        Args:
            bookmark_id: The bookmark ID to remove

        Returns:
            True if removed, False if not found
        """
        bookmarks = self._load_bookmarks()
        original_len = len(bookmarks)
        bookmarks = [b for b in bookmarks if b.get("id") != bookmark_id]

        if len(bookmarks) == original_len:
            return False

        self._save_bookmarks(bookmarks)
        return True

    def update(
        self,
        bookmark_id: str,
        title: str | None = None,
        notes: str | None = None,
        tags: list[str] | None = None,
    ) -> dict | None:
        """Update an existing bookmark.

        Args:
            bookmark_id: The bookmark ID to update
            title: New title (if provided)
            notes: New notes (if provided)
            tags: New tags (if provided)

        Returns:
            Updated bookmark dict, or None if not found
        """
        bookmarks = self._load_bookmarks()

        for bookmark in bookmarks:
            if bookmark.get("id") == bookmark_id:
                if title is not None:
                    bookmark["title"] = title
                if notes is not None:
                    bookmark["notes"] = notes
                if tags is not None:
                    bookmark["tags"] = tags
                bookmark["updated_at"] = datetime.now(UTC).isoformat()

                self._save_bookmarks(bookmarks)
                return bookmark

        return None

    def toggle(self, session_id: str, title: str, message_index: int | None = None) -> dict:
        """Toggle a bookmark for a session. If it exists, remove it; otherwise add it.

        Args:
            session_id: Session ID
            title: Title to use when adding
            message_index: Optional message index

        Returns:
            Dict with 'action' ('added' or 'removed') and 'bookmark' (if added)
        """
        bookmarks = self._load_bookmarks()

        # Check if this session is already bookmarked
        existing = None
        for b in bookmarks:
            if b.get("session_id") == session_id:
                if message_index is not None:
                    if b.get("message_index") == message_index:
                        existing = b
                        break
                else:
                    if b.get("message_index") is None:
                        existing = b
                        break

        if existing:
            self.remove(existing["id"])
            return {"action": "removed", "bookmark": existing}
        else:
            bookmark = self.add(session_id, title, message_index)
            return {"action": "added", "bookmark": bookmark}
