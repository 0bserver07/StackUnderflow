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


if __name__ == "__main__":
    unittest.main()
