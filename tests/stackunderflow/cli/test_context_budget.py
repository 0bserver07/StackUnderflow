"""Tests for ``stackunderflow context-budget``."""

from __future__ import annotations

import json
import unittest
from pathlib import Path
from tempfile import TemporaryDirectory

from click.testing import CliRunner

from stackunderflow.cli import cli


class TestContextBudgetCli(unittest.TestCase):
    def setUp(self):
        self._home_tmp = TemporaryDirectory()
        self._proj_tmp = TemporaryDirectory()
        self.home = Path(self._home_tmp.name)
        self.project = Path(self._proj_tmp.name)
        (self.home / ".claude").mkdir(parents=True)
        (self.home / ".claude" / "skills").mkdir()
        (self.home / ".claude" / "agents").mkdir()
        # Project + global CLAUDE.md so the budget exceeds bare system prompt
        (self.project / "CLAUDE.md").write_text("p" * 800)  # 200 tokens
        (self.home / ".claude" / "CLAUDE.md").write_text("g" * 400)  # 100 tokens

    def tearDown(self):
        self._home_tmp.cleanup()
        self._proj_tmp.cleanup()

    def test_text_output_lists_slices_and_total(self):
        runner = CliRunner()
        # Patch home to our temp dir via monkey-equivalent: the CLI uses
        # estimate_context_budget with default home; we use --project to
        # point at the project dir but rely on the real ~ for global.
        # To keep this hermetic, monkey-patch Path.home via an env? The
        # cleaner path is to invoke the service directly elsewhere; here
        # we just test text formatting end-to-end with the real home dir.
        result = runner.invoke(cli, ["context-budget", "--project", str(self.project), "--format", "text"])
        self.assertEqual(result.exit_code, 0, msg=result.output)
        self.assertIn("Context budget", result.output)
        self.assertIn("system_prompt", result.output)
        self.assertIn("total:", result.output)
        self.assertIn("cost per session:", result.output)

    def test_json_output_parses(self):
        runner = CliRunner()
        result = runner.invoke(cli, ["context-budget", "--project", str(self.project), "--format", "json"])
        self.assertEqual(result.exit_code, 0, msg=result.output)
        data = json.loads(result.output)
        self.assertIn("total_tokens", data)
        self.assertIn("slices", data)
        self.assertIn("cost_per_session_usd", data)
        self.assertIn("estimated_monthly_cost_usd", data)
        self.assertIn("heuristic", data)
        names = {s["name"] for s in data["slices"]}
        self.assertIn("system_prompt", names)

    def test_global_flag_excludes_project_memory(self):
        runner = CliRunner()
        result = runner.invoke(cli, ["context-budget", "--global", "--format", "json"])
        self.assertEqual(result.exit_code, 0, msg=result.output)
        data = json.loads(result.output)
        names = {s["name"] for s in data["slices"]}
        self.assertNotIn("memory:project_CLAUDE.md", names)
        self.assertIn("system_prompt", names)


if __name__ == "__main__":
    unittest.main()
