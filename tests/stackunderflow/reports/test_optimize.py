"""Tests for the waste-finding heuristic (store-backed)."""

import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from stackunderflow.reports.optimize import find_waste
from stackunderflow.reports.scope import Scope
from stackunderflow.services.qa_service import QAService
from stackunderflow.store import db, schema


def _msg(mtype: str, content: str, timestamp: str, session_id: str = "s1") -> dict:
    return {
        "type": mtype,
        "content": content,
        "session_id": session_id,
        "timestamp": timestamp,
        "tools": [],
        "model": "claude-sonnet-4-6",
    }


class TestFindWaste(unittest.TestCase):
    def setUp(self):
        self._qa_tmp = tempfile.TemporaryDirectory()
        self._store_tmp = tempfile.TemporaryDirectory()
        qa_path = Path(self._qa_tmp.name) / "qa.db"
        store_path = Path(self._store_tmp.name) / "store.db"

        self.svc = QAService(db_path=qa_path)
        self.svc.index_project("proj-a", [
            _msg("user", "How do I fix the import?", "2026-04-16T10:00:00"),
            _msg("assistant", "Try:\n```bash\npip install foo\n```", "2026-04-16T10:00:01"),
            _msg("user", "that doesn't work", "2026-04-16T10:00:02"),
            _msg("assistant", "Try:\n```bash\npip install foo --upgrade\n```", "2026-04-16T10:00:03"),
            _msg("user", "still not working", "2026-04-16T10:00:04"),
            _msg("assistant", "Check:\n```bash\npython --version\n```", "2026-04-16T10:00:05"),
        ])
        self.svc.index_project("proj-a-second-loop", [
            _msg("user", "Why is my build failing?", "2026-04-16T11:00:00", session_id="s2"),
            _msg("assistant", "Try:\n```bash\nrm -rf node_modules\n```", "2026-04-16T11:00:01", session_id="s2"),
            _msg("user", "that doesn't work", "2026-04-16T11:00:02", session_id="s2"),
            _msg("assistant", "Try:\n```bash\nnpm cache clean\n```", "2026-04-16T11:00:03", session_id="s2"),
            _msg("user", "still broken", "2026-04-16T11:00:04", session_id="s2"),
            _msg("assistant", "Check:\n```bash\nnode --version\n```", "2026-04-16T11:00:05", session_id="s2"),
        ])
        self.svc.index_project("proj-b", [
            _msg("user", "How do I read a file?", "2026-04-16T12:00:00", session_id="s3"),
            _msg("assistant", "Use:\n```python\nopen('x.txt').read()\n```", "2026-04-16T12:00:01", session_id="s3"),
        ])

        # Seed the session store with the same projects
        self.conn = db.connect(store_path)
        schema.apply(self.conn)
        for slug in ("proj-a", "proj-a-second-loop", "proj-b"):
            self.conn.execute(
                "INSERT INTO projects (provider, slug, display_name, first_seen, last_modified) "
                "VALUES (?, ?, ?, ?, ?)",
                ("claude", slug, slug, 0.0, 0.0),
            )
        self.conn.commit()

    def tearDown(self):
        self.conn.close()
        self._qa_tmp.cleanup()
        self._store_tmp.cleanup()

    def test_find_waste_ranks_looped_projects_first(self):
        scope = Scope(since=None, until=None, label="all")
        with patch("stackunderflow.reports.optimize._qa_service_factory", return_value=self.svc):
            waste = find_waste(self.conn, scope=scope)
        names = {w["project"] for w in waste}
        self.assertIn("proj-a", names)
        self.assertIn("proj-a-second-loop", names)
        self.assertNotIn("proj-b", names)
        for row in waste:
            self.assertGreaterEqual(row["looped_pairs"], 1)

    def test_find_waste_respects_exclude(self):
        scope = Scope(since=None, until=None, label="all")
        with patch("stackunderflow.reports.optimize._qa_service_factory", return_value=self.svc):
            waste = find_waste(self.conn, scope=scope, exclude=["proj-a-second-loop"])
        self.assertEqual({w["project"] for w in waste}, {"proj-a"})

    def test_find_waste_respects_include(self):
        scope = Scope(since=None, until=None, label="all")
        with patch("stackunderflow.reports.optimize._qa_service_factory", return_value=self.svc):
            waste = find_waste(self.conn, scope=scope, include=["proj-a"])
        self.assertEqual({w["project"] for w in waste}, {"proj-a"})


if __name__ == "__main__":
    unittest.main()
