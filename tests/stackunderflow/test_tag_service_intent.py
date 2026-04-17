"""Unit tests for intent-tag detection in TagService.

Intents are auto-tags with the "intent:" prefix. They classify what the user
was trying to do in a session (build, fix, explore, refactor, test, ops).
These tests exercise pattern matching, metadata exposure, and the tag cloud.
"""

import json
import os
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__)))))

from stackunderflow.services.tag_service import TagService


def _msg(content: str, session_id: str = "s1") -> dict:
    """Minimal message dict for auto_tag_session()."""
    return {
        "session_id": session_id,
        "content": content,
        "tools": [],
    }


class _IsolatedTagService(TagService):
    """TagService that never touches the user's real ~/.stackunderflow dir."""

    def __init__(self, storage_dir: Path):
        self.storage_dir = storage_dir
        self.tags_file = storage_dir / "tags.json"
        self.storage_dir.mkdir(parents=True, exist_ok=True)


class TestIntentDetection(unittest.TestCase):
    """auto_tag_session emits intent:* tags based on content patterns."""

    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.svc = _IsolatedTagService(Path(self._tmp.name))

    def tearDown(self):
        self._tmp.cleanup()

    def test_empty_messages_produce_no_intent(self):
        tags = self.svc.auto_tag_session("s1", [])
        self.assertEqual([t for t in tags if t.startswith("intent:")], [])

    def test_content_with_no_intent_keywords_produces_no_intent(self):
        tags = self.svc.auto_tag_session("s1", [_msg("hello world")])
        self.assertEqual([t for t in tags if t.startswith("intent:")], [])

    def test_build_intent_from_add_keyword(self):
        tags = self.svc.auto_tag_session(
            "s1",
            [_msg("Can you add a new /api/users endpoint?")],
        )
        self.assertIn("intent:build", tags)

    def test_build_intent_from_implement_keyword(self):
        tags = self.svc.auto_tag_session(
            "s1",
            [_msg("Implement a retry wrapper around this function")],
        )
        self.assertIn("intent:build", tags)

    def test_fix_intent_from_bug_keyword(self):
        tags = self.svc.auto_tag_session(
            "s1",
            [_msg("There's a bug in the token counter — can you investigate?")],
        )
        self.assertIn("intent:fix", tags)

    def test_fix_intent_from_error_keyword(self):
        tags = self.svc.auto_tag_session(
            "s1",
            [_msg("I'm getting a traceback when I run this script")],
        )
        self.assertIn("intent:fix", tags)

    def test_fix_intent_from_doesnt_work_phrase(self):
        tags = self.svc.auto_tag_session(
            "s1",
            [_msg("The login flow doesn't work on Safari")],
        )
        self.assertIn("intent:fix", tags)

    def test_explore_intent_from_explain_keyword(self):
        tags = self.svc.auto_tag_session(
            "s1",
            [_msg("Can you explain how the cache warming works?")],
        )
        self.assertIn("intent:explore", tags)

    def test_explore_intent_from_how_does_phrase(self):
        tags = self.svc.auto_tag_session(
            "s1",
            [_msg("How does the dedup step decide what's a duplicate?")],
        )
        self.assertIn("intent:explore", tags)

    def test_refactor_intent_from_refactor_keyword(self):
        tags = self.svc.auto_tag_session(
            "s1",
            [_msg("Let's refactor this into smaller functions")],
        )
        self.assertIn("intent:refactor", tags)

    def test_refactor_intent_from_simplify_keyword(self):
        tags = self.svc.auto_tag_session(
            "s1",
            [_msg("Simplify this nested conditional")],
        )
        self.assertIn("intent:refactor", tags)


if __name__ == "__main__":
    unittest.main()
