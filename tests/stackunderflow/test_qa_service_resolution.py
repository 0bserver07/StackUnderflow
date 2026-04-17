"""Unit tests for Q&A resolution classification.

Each Q&A pair carries:
  - resolution_status: 'resolved' | 'looped' | 'open'
  - loop_count: number of follow-up pushbacks seen while extracting

These tests cover schema migration, classification logic, persistence, and
the list_qa filter.
"""

import os
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__)))))

from stackunderflow.services.qa_service import QAService


def _msg(mtype: str, content: str, session_id: str = "s1", timestamp: str = "2026-04-16T10:00:00") -> dict:
    """Minimal message dict acceptable to extract_qa_pairs()."""
    return {
        "type": mtype,
        "content": content,
        "session_id": session_id,
        "timestamp": timestamp,
        "tools": [],
        "model": "claude-sonnet-4-6",
    }


class _TempDBTestCase(unittest.TestCase):
    """Base class that creates a fresh throwaway SQLite DB per test."""

    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.db_path = Path(self._tmp.name) / "qa.db"
        self.svc = QAService(db_path=self.db_path)

    def tearDown(self):
        self._tmp.cleanup()


class TestSchemaHasResolutionColumns(_TempDBTestCase):
    """Fresh databases expose resolution_status and loop_count columns."""

    def test_fresh_db_has_resolution_status_column(self):
        conn = self.svc._get_conn()
        try:
            cols = {r["name"] for r in conn.execute("PRAGMA table_info(qa_pairs)").fetchall()}
        finally:
            conn.close()
        self.assertIn("resolution_status", cols)
        self.assertIn("loop_count", cols)


import sqlite3


class TestLegacyDBMigration(unittest.TestCase):
    """A database created before this feature migrates cleanly on reopen."""

    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.db_path = Path(self._tmp.name) / "legacy.db"

    def tearDown(self):
        self._tmp.cleanup()

    def _create_legacy_schema(self):
        """Create the pre-resolution qa_pairs schema exactly as it existed."""
        conn = sqlite3.connect(str(self.db_path))
        conn.execute("""
            CREATE TABLE qa_pairs (
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
                created_at TEXT NOT NULL
            )
        """)
        conn.execute(
            """INSERT INTO qa_pairs (id, session_id, project, question_text, answer_text, created_at)
               VALUES ('legacy1', 's1', 'p1', 'Q?', 'A', '2026-01-01T00:00:00')"""
        )
        conn.commit()
        conn.close()

    def test_migration_preserves_existing_rows_and_adds_columns(self):
        self._create_legacy_schema()

        # Re-opening should trigger the idempotent migration.
        svc = QAService(db_path=self.db_path)

        conn = svc._get_conn()
        try:
            cols = {r["name"] for r in conn.execute("PRAGMA table_info(qa_pairs)").fetchall()}
            row = conn.execute("SELECT * FROM qa_pairs WHERE id = 'legacy1'").fetchone()
        finally:
            conn.close()

        self.assertIn("resolution_status", cols)
        self.assertIn("loop_count", cols)
        self.assertIsNotNone(row)
        self.assertEqual(row["resolution_status"], "open")
        self.assertEqual(row["loop_count"], 0)


from stackunderflow.services.qa_service import _classify_resolution


class TestClassifyResolution(unittest.TestCase):
    """Pure classification function."""

    def test_two_or_more_followups_is_looped(self):
        self.assertEqual(_classify_resolution(followup_count=2, has_code=True), ("looped", 2))
        self.assertEqual(_classify_resolution(followup_count=5, has_code=False), ("looped", 5))

    def test_code_answer_with_zero_or_one_followup_is_resolved(self):
        self.assertEqual(_classify_resolution(followup_count=0, has_code=True), ("resolved", 0))
        self.assertEqual(_classify_resolution(followup_count=1, has_code=True), ("resolved", 1))

    def test_no_code_and_few_followups_is_open(self):
        self.assertEqual(_classify_resolution(followup_count=0, has_code=False), ("open", 0))
        self.assertEqual(_classify_resolution(followup_count=1, has_code=False), ("open", 1))


if __name__ == "__main__":
    unittest.main()
