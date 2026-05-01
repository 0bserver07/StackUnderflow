"""Tests for ``GET /api/context-budget``."""

from __future__ import annotations

import unittest
from pathlib import Path
from tempfile import TemporaryDirectory
from unittest.mock import patch

from fastapi import FastAPI
from fastapi.testclient import TestClient

import stackunderflow.deps as deps
from stackunderflow.routes.context_budget import router as ctx_router
from stackunderflow.store import db, schema


class TestContextBudgetRoute(unittest.TestCase):
    def setUp(self):
        self._tmp = TemporaryDirectory()
        self.tmp = Path(self._tmp.name)
        # Set up a project on disk that the store will reference.
        self.project_path = self.tmp / "demo_project"
        self.project_path.mkdir()
        (self.project_path / "CLAUDE.md").write_text("a" * 400)  # 100 tokens

        # Build a minimal store with one project row pointing at the dir.
        store_db = self.tmp / "store.db"
        conn = db.connect(store_db)
        schema.apply(conn)
        conn.execute(
            "INSERT INTO projects (provider, slug, path, display_name, first_seen, last_modified) "
            "VALUES (?, ?, ?, ?, ?, ?)",
            ("claude", "demo", str(self.project_path), "demo", 0.0, 0.0),
        )
        conn.commit()
        conn.close()

        self._original_path = deps.store_path
        deps.store_path = store_db

        app = FastAPI()
        app.include_router(ctx_router)
        self.client = TestClient(app)

    def tearDown(self):
        deps.store_path = self._original_path
        self._tmp.cleanup()

    def test_global_budget_when_no_project(self):
        resp = self.client.get("/api/context-budget")
        self.assertEqual(resp.status_code, 200)
        body = resp.json()
        self.assertIn("total_tokens", body)
        self.assertIn("slices", body)
        names = {s["name"] for s in body["slices"]}
        self.assertIn("system_prompt", names)
        # Global budget never carries a project-memory slice.
        self.assertNotIn("memory:project_CLAUDE.md", names)

    def test_unknown_slug_returns_404(self):
        resp = self.client.get("/api/context-budget", params={"project": "nope"})
        self.assertEqual(resp.status_code, 404)

    def test_known_slug_returns_project_budget(self):
        resp = self.client.get("/api/context-budget", params={"project": "demo"})
        self.assertEqual(resp.status_code, 200)
        body = resp.json()
        names = {s["name"]: s for s in body["slices"]}
        # Project memory slice present and pinned to the on-disk file.
        self.assertIn("memory:project_CLAUDE.md", names)
        proj_slice = names["memory:project_CLAUDE.md"]
        self.assertEqual(proj_slice["tokens"], 100)
        self.assertTrue(proj_slice["source_path"].endswith("CLAUDE.md"))

    def test_project_with_missing_path_falls_back_to_global(self):
        # Simulate a project row whose on-disk path no longer exists
        store_db = self.tmp / "store.db"
        conn = db.connect(store_db)
        conn.execute(
            "INSERT INTO projects (provider, slug, path, display_name, first_seen, last_modified) "
            "VALUES (?, ?, ?, ?, ?, ?)",
            ("claude", "ghost", str(self.tmp / "does-not-exist"), "ghost", 0.0, 0.0),
        )
        conn.commit()
        conn.close()
        resp = self.client.get("/api/context-budget", params={"project": "ghost"})
        self.assertEqual(resp.status_code, 200)
        body = resp.json()
        names = {s["name"] for s in body["slices"]}
        # Falls through to global shape (no project memory slice).
        self.assertNotIn("memory:project_CLAUDE.md", names)


class TestContextBudgetReportFinding(unittest.TestCase):
    """``find_context_budget_findings`` integration with the optimize report."""

    def setUp(self):
        self._tmp = TemporaryDirectory()
        self.tmp = Path(self._tmp.name)
        store_db = self.tmp / "store.db"
        self.conn = db.connect(store_db)
        schema.apply(self.conn)

        self.proj_dir = self.tmp / "bloated"
        self.proj_dir.mkdir()
        # Write a huge CLAUDE.md so the per-project budget pops over threshold
        (self.proj_dir / "CLAUDE.md").write_text("x" * 200_000)  # 50,000 tokens

        self.conn.execute(
            "INSERT INTO projects (provider, slug, path, display_name, first_seen, last_modified) "
            "VALUES (?, ?, ?, ?, ?, ?)",
            ("claude", "bloated", str(self.proj_dir), "bloated", 0.0, 0.0),
        )
        self.conn.commit()

    def tearDown(self):
        self.conn.close()
        self._tmp.cleanup()

    def test_finding_emitted_when_budget_exceeds_threshold(self):
        from stackunderflow.reports.optimize import find_context_budget_findings

        # Pin the global budget to something tiny so it doesn't pollute
        # the project assertion. We patch estimate_global_budget at the
        # optimize module's import site.
        with patch("stackunderflow.reports.optimize.estimate_global_budget") as mock_global:
            from stackunderflow.services.context_budget import ContextBudget
            mock_global.return_value = ContextBudget(total_tokens=0)
            findings = find_context_budget_findings(self.conn)

        self.assertGreaterEqual(len(findings), 1)
        proj_findings = [f for f in findings if f["project"] == "bloated"]
        self.assertEqual(len(proj_findings), 1)
        f = proj_findings[0]
        self.assertEqual(f["kind"], "context_budget_bloat")
        self.assertEqual(f["severity"], "medium")
        self.assertGreater(f["total_tokens"], 20_000)
        self.assertIn("top_slices", f)
        # The CLAUDE.md slice should be one of the largest.
        slice_names = {s["name"] for s in f["top_slices"]}
        self.assertIn("memory:project_CLAUDE.md", slice_names)

    def test_finding_silent_when_under_threshold(self):
        from stackunderflow.reports.optimize import find_context_budget_findings

        # Override threshold high so no finding emitted
        with patch("stackunderflow.reports.optimize.estimate_global_budget") as mock_global:
            from stackunderflow.services.context_budget import ContextBudget
            mock_global.return_value = ContextBudget(total_tokens=0)
            findings = find_context_budget_findings(self.conn, threshold=10_000_000)
        self.assertEqual(findings, [])


if __name__ == "__main__":
    unittest.main()
