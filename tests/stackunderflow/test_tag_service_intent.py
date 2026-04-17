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

    def test_test_intent_from_pytest_keyword(self):
        tags = self.svc.auto_tag_session(
            "s1",
            [_msg("Write a pytest for the new endpoint")],
        )
        self.assertIn("intent:test", tags)

    def test_test_intent_from_coverage_keyword(self):
        tags = self.svc.auto_tag_session(
            "s1",
            [_msg("We need coverage on the dedup branch")],
        )
        self.assertIn("intent:test", tags)

    def test_ops_intent_from_deploy_keyword(self):
        tags = self.svc.auto_tag_session(
            "s1",
            [_msg("Deploy this to staging and check the logs")],
        )
        self.assertIn("intent:ops", tags)

    def test_ops_intent_from_docker_keyword(self):
        tags = self.svc.auto_tag_session(
            "s1",
            [_msg("Why is the docker build hanging on apt-get?")],
        )
        self.assertIn("intent:ops", tags)

    def test_ops_intent_from_env_file_reference(self):
        tags = self.svc.auto_tag_session(
            "s1",
            [_msg("Missing value in .env — causes auth to fail")],
        )
        self.assertIn("intent:ops", tags)

    def test_multi_intent_build_and_fix(self):
        tags = self.svc.auto_tag_session(
            "s1",
            [
                _msg("Add a new /users endpoint"),
                _msg("Wait, there's an error — can you fix it?"),
            ],
        )
        self.assertIn("intent:build", tags)
        self.assertIn("intent:fix", tags)

    def test_intents_are_sorted_in_output(self):
        # auto_tag_session returns sorted tags; intents should slot alphabetically
        # within the overall list without breaking sort order.
        tags = self.svc.auto_tag_session(
            "s1",
            [_msg("Fix the bug and add a test")],
        )
        intent_tags = [t for t in tags if t.startswith("intent:")]
        self.assertEqual(intent_tags, sorted(intent_tags))


class TestIntentMetadata(unittest.TestCase):
    """Tag metadata must expose the 'intent' category for all 6 intent tags."""

    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.svc = _IsolatedTagService(Path(self._tmp.name))

    def tearDown(self):
        self._tmp.cleanup()

    def test_all_intents_have_metadata(self):
        metadata = self.svc._build_tag_metadata()
        for intent in [
            "intent:build",
            "intent:fix",
            "intent:explore",
            "intent:refactor",
            "intent:test",
            "intent:ops",
        ]:
            self.assertIn(intent, metadata, f"{intent} missing from metadata")
            self.assertEqual(metadata[intent]["category"], "intent")
            self.assertTrue(metadata[intent]["color"].startswith("#"))

    def test_tag_cloud_reports_intent_category(self):
        # Index a session with a clear build intent
        self.svc.index_project([
            {"session_id": "s1", "content": "Add a new feature", "tools": []},
        ])
        cloud = self.svc.get_tag_cloud()
        intent_entries = [t for t in cloud["tags"] if t["name"] == "intent:build"]
        self.assertEqual(len(intent_entries), 1)
        self.assertEqual(intent_entries[0]["category"], "intent")
        self.assertEqual(intent_entries[0]["count"], 1)


class TestIntentBrowse(unittest.TestCase):
    """get_sessions_by_tag('intent:build') returns matching sessions."""

    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.svc = _IsolatedTagService(Path(self._tmp.name))

    def tearDown(self):
        self._tmp.cleanup()

    def test_browse_by_intent_tag(self):
        self.svc.index_project([
            {"session_id": "s1", "content": "Add a login page", "tools": []},
            {"session_id": "s2", "content": "Fix the crash on logout", "tools": []},
        ])
        build_sessions = self.svc.get_sessions_by_tag("intent:build")
        fix_sessions = self.svc.get_sessions_by_tag("intent:fix")
        self.assertEqual([s["session_id"] for s in build_sessions], ["s1"])
        self.assertEqual([s["session_id"] for s in fix_sessions], ["s2"])
        self.assertEqual(build_sessions[0]["source"], ["auto"])


if __name__ == "__main__":
    unittest.main()
