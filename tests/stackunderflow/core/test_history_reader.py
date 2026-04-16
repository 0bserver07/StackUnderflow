"""Tests for the legacy history.jsonl reader."""

import json
import os
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from stackunderflow.pipeline.history_reader import (
    _assign_sessions,
    _epoch_ms_to_iso,
    _path_to_slug,
    _to_raw_entry,
    clear_cache,
    entries_for_slug,
    known_slugs,
)
from stackunderflow.pipeline.reader import RawEntry, scan


class TestPathToSlug(unittest.TestCase):
    def test_basic_path(self):
        self.assertEqual(
            _path_to_slug("/Users/me/code/app"),
            "-Users-me-code-app",
        )

    def test_underscores_replaced(self):
        self.assertEqual(
            _path_to_slug("/Users/me/dev_dev/project"),
            "-Users-me-dev-dev-project",
        )

    def test_trailing_slash_stripped(self):
        self.assertEqual(
            _path_to_slug("/Users/me/app/"),
            "-Users-me-app",
        )


class TestEpochToIso(unittest.TestCase):
    def test_conversion(self):
        iso = _epoch_ms_to_iso(1700000000000)
        self.assertIn("2023-11-14", iso)
        self.assertIn("+00:00", iso)


class TestAssignSessions(unittest.TestCase):
    def test_entries_with_session_id(self):
        entries = [
            {"timestamp": 1000, "sessionId": "abc"},
            {"timestamp": 2000, "sessionId": "abc"},
        ]
        result = _assign_sessions(entries)
        self.assertEqual(len(result), 2)
        self.assertEqual(result[0][1], "abc")
        self.assertEqual(result[1][1], "abc")

    def test_entries_without_session_id_same_session(self):
        # Entries within 2-hour gap → same session
        entries = [
            {"timestamp": 1000},
            {"timestamp": 2000},  # 1 second later
        ]
        result = _assign_sessions(entries)
        self.assertEqual(result[0][1], result[1][1])

    def test_entries_without_session_id_different_sessions(self):
        # Entries more than 2 hours apart → different sessions
        gap = 3 * 60 * 60 * 1000  # 3 hours in ms
        entries = [
            {"timestamp": 1000},
            {"timestamp": 1000 + gap},
        ]
        result = _assign_sessions(entries)
        self.assertNotEqual(result[0][1], result[1][1])


class TestToRawEntry(unittest.TestCase):
    def test_produces_raw_entry(self):
        entry = {"display": "hello world", "timestamp": 1700000000000}
        result = _to_raw_entry(entry, "sess-1")
        self.assertIsInstance(result, RawEntry)
        self.assertEqual(result.session_id, "sess-1")
        self.assertEqual(result.origin, "history")

    def test_payload_structure(self):
        entry = {"display": "test prompt", "timestamp": 1700000000000}
        result = _to_raw_entry(entry, "sess-1")
        p = result.payload
        self.assertEqual(p["type"], "user")
        self.assertEqual(p["source"], "history")
        self.assertEqual(p["message"]["role"], "user")
        self.assertEqual(p["message"]["content"][0]["text"], "test prompt")


class TestHistoryIndex(unittest.TestCase):
    """Test the full index with a temporary history.jsonl file."""

    def setUp(self):
        clear_cache()
        self.tmpdir = tempfile.mkdtemp()
        self.history_file = os.path.join(self.tmpdir, "history.jsonl")

        # Write fake history entries
        entries = [
            {"display": "msg 1", "timestamp": 1700000000000, "project": "/Users/test/proj_a"},
            {"display": "msg 2", "timestamp": 1700001000000, "project": "/Users/test/proj_a"},
            {"display": "msg 3", "timestamp": 1700000000000, "project": "/Users/test/proj-b", "sessionId": "s1"},
        ]
        with open(self.history_file, "w") as f:
            for e in entries:
                f.write(json.dumps(e) + "\n")

    def tearDown(self):
        clear_cache()
        import shutil
        shutil.rmtree(self.tmpdir, ignore_errors=True)

    @patch("stackunderflow.pipeline.history_reader._HISTORY_FILE")
    def test_entries_for_slug(self, mock_path):
        mock_path.__class__ = Path
        with patch("stackunderflow.pipeline.history_reader._HISTORY_FILE", Path(self.history_file)):
            clear_cache()
            entries = entries_for_slug("-Users-test-proj-a")
            self.assertEqual(len(entries), 2)
            for e in entries:
                self.assertEqual(e.payload["type"], "user")

    @patch("stackunderflow.pipeline.history_reader._HISTORY_FILE")
    def test_known_slugs(self, mock_path):
        with patch("stackunderflow.pipeline.history_reader._HISTORY_FILE", Path(self.history_file)):
            clear_cache()
            slugs = known_slugs()
            self.assertIn("-Users-test-proj-a", slugs)
            self.assertIn("-Users-test-proj-b", slugs)

    @patch("stackunderflow.pipeline.history_reader._HISTORY_FILE")
    def test_missing_slug_returns_empty(self, mock_path):
        with patch("stackunderflow.pipeline.history_reader._HISTORY_FILE", Path(self.history_file)):
            clear_cache()
            entries = entries_for_slug("-nonexistent-project")
            self.assertEqual(entries, [])


class TestReaderFallback(unittest.TestCase):
    """Test that reader.scan() falls back to history_reader for legacy projects."""

    def setUp(self):
        clear_cache()
        self.tmpdir = tempfile.mkdtemp()
        # Create a project dir with no JSONL files
        self.project_dir = os.path.join(self.tmpdir, "-Users-test-proj-a")
        os.makedirs(self.project_dir)

        # Write fake history
        self.history_file = os.path.join(self.tmpdir, "history.jsonl")
        entries = [
            {"display": "prompt 1", "timestamp": 1700000000000, "project": "/Users/test/proj_a"},
            {"display": "prompt 2", "timestamp": 1700001000000, "project": "/Users/test/proj_a"},
        ]
        with open(self.history_file, "w") as f:
            for e in entries:
                f.write(json.dumps(e) + "\n")

    def tearDown(self):
        clear_cache()
        import shutil
        shutil.rmtree(self.tmpdir, ignore_errors=True)

    def test_scan_falls_back_to_history(self):
        with patch("stackunderflow.pipeline.history_reader._HISTORY_FILE", Path(self.history_file)):
            clear_cache()
            entries = scan(self.project_dir)
            self.assertEqual(len(entries), 2)
            self.assertEqual(entries[0].origin, "history")

    def test_scan_empty_dir_no_history(self):
        """Empty dir with no matching history entries returns empty list."""
        empty_dir = os.path.join(self.tmpdir, "-nonexistent-slug")
        os.makedirs(empty_dir)
        with patch("stackunderflow.pipeline.history_reader._HISTORY_FILE", Path(self.history_file)):
            clear_cache()
            entries = scan(empty_dir)
            self.assertEqual(entries, [])


if __name__ == "__main__":
    unittest.main()
