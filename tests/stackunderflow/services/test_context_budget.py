"""Tests for the context-budget estimator."""

from __future__ import annotations

import json
import unittest
from pathlib import Path
from tempfile import TemporaryDirectory

from stackunderflow.services.context_budget import (
    CHARS_PER_TOKEN,
    DEFAULT_INPUT_USD_PER_MILLION,
    DEFAULT_SESSIONS_PER_MONTH,
    DEFAULT_SYSTEM_PROMPT_TOKENS,
    MCP_BASE_TOKENS,
    MCP_PER_TOOL_TOKENS,
    MCP_UNKNOWN_TOOLS_FALLBACK,
    ContextBudget,
    ContextSlice,
    estimate_context_budget,
    estimate_global_budget,
    estimate_tokens,
)


class TestTokenEstimate(unittest.TestCase):
    """``estimate_tokens`` is the load-bearing primitive — pin its math."""

    def test_empty_string_returns_zero(self):
        self.assertEqual(estimate_tokens(""), 0)

    def test_none_returns_zero(self):
        # The function must not raise on falsy input; callers feed it
        # ``read_text`` results that may be empty.
        self.assertEqual(estimate_tokens(None), 0)  # type: ignore[arg-type]

    def test_floor_division_by_4(self):
        # 16 chars → 4 tokens; 17 chars → 4 tokens (floor division)
        self.assertEqual(estimate_tokens("a" * 16), 4)
        self.assertEqual(estimate_tokens("a" * 17), 4)
        self.assertEqual(estimate_tokens("a" * 20), 5)

    def test_chars_per_token_constant_drives_math(self):
        # 4 * CHARS_PER_TOKEN characters → exactly 4 tokens.
        self.assertEqual(estimate_tokens("a" * (4 * CHARS_PER_TOKEN)), 4)


class TestEstimateContextBudget(unittest.TestCase):
    """End-to-end estimator tests against synthetic project + home dirs."""

    def setUp(self):
        self._home_tmp = TemporaryDirectory()
        self._proj_tmp = TemporaryDirectory()
        self.home = Path(self._home_tmp.name)
        self.project = Path(self._proj_tmp.name)
        # Set up the canonical layout the estimator looks for.
        (self.home / ".claude").mkdir(parents=True)
        (self.home / ".claude" / "skills").mkdir()
        (self.home / ".claude" / "agents").mkdir()
        (self.project / ".claude").mkdir(parents=True)
        (self.project / ".claude" / "agents").mkdir()

    def tearDown(self):
        self._home_tmp.cleanup()
        self._proj_tmp.cleanup()

    # ── empty environment ─────────────────────────────────────────────

    def test_empty_environment_returns_only_system_prompt_and_zero_memories(self):
        """No CLAUDE.md, no MCP, no skills, no agents → only the system
        prompt slice carries non-zero tokens (memory slices are present
        with 0 tokens to make the missing-file state visible)."""
        budget = estimate_context_budget(self.project, home_dir=self.home)
        names = {s.name for s in budget.slices}
        # System prompt must be present and pinned to the constant.
        sys_slice = next(s for s in budget.slices if s.name == "system_prompt")
        self.assertEqual(sys_slice.tokens, DEFAULT_SYSTEM_PROMPT_TOKENS)
        # Memory slices appear even when missing — with zero tokens.
        self.assertIn("memory:project_CLAUDE.md", names)
        self.assertIn("memory:global_CLAUDE.md", names)
        for s in budget.slices:
            if s.name.startswith("memory:"):
                self.assertEqual(s.tokens, 0)
        # No mcp / skill / agent slices should have been added.
        self.assertFalse(any(s.name.startswith("mcp:") for s in budget.slices))
        self.assertFalse(any(s.name.startswith("skill:") for s in budget.slices))
        self.assertFalse(any(s.name.startswith("agent:") for s in budget.slices))
        # Total is exactly the system prompt.
        self.assertEqual(budget.total_tokens, DEFAULT_SYSTEM_PROMPT_TOKENS)

    # ── memory files ──────────────────────────────────────────────────

    def test_project_claude_md_charged_via_4_char_heuristic(self):
        body = "a" * 400  # 100 tokens
        (self.project / "CLAUDE.md").write_text(body)
        budget = estimate_context_budget(self.project, home_dir=self.home)
        slice_ = next(s for s in budget.slices if s.name == "memory:project_CLAUDE.md")
        self.assertEqual(slice_.tokens, 100)
        self.assertEqual(slice_.source_path, str(self.project / "CLAUDE.md"))

    def test_global_claude_md_charged_separately(self):
        (self.home / ".claude" / "CLAUDE.md").write_text("b" * 800)  # 200 tokens
        budget = estimate_context_budget(self.project, home_dir=self.home)
        global_slice = next(s for s in budget.slices if s.name == "memory:global_CLAUDE.md")
        self.assertEqual(global_slice.tokens, 200)

    # ── MCP servers ───────────────────────────────────────────────────

    def test_mcp_servers_from_global_claude_json(self):
        config = {
            "mcpServers": {
                "alpha": {"command": "alpha-mcp"},
                "beta": {"command": "beta-mcp", "tools": ["t1", "t2", "t3"]},
            }
        }
        (self.home / ".claude.json").write_text(json.dumps(config))
        budget = estimate_context_budget(self.project, home_dir=self.home)
        # alpha: no tool list → base + UNKNOWN_TOOLS_FALLBACK
        alpha = next(s for s in budget.slices if s.name == "mcp:alpha")
        self.assertEqual(alpha.tokens, MCP_BASE_TOKENS + MCP_UNKNOWN_TOOLS_FALLBACK)
        # beta: 3 tools → base + 3 * PER_TOOL
        beta = next(s for s in budget.slices if s.name == "mcp:beta")
        self.assertEqual(beta.tokens, MCP_BASE_TOKENS + 3 * MCP_PER_TOOL_TOKENS)

    def test_project_settings_mcp_does_not_double_charge(self):
        global_cfg = {"mcpServers": {"shared": {"command": "x"}}}
        (self.home / ".claude.json").write_text(json.dumps(global_cfg))
        proj_cfg = {"mcpServers": {"shared": {"command": "x"}, "extra": {"command": "y"}}}
        (self.project / ".claude" / "settings.json").write_text(json.dumps(proj_cfg))
        budget = estimate_context_budget(self.project, home_dir=self.home)
        names = [s.name for s in budget.slices]
        # ``shared`` appears exactly once (skipped from project settings).
        self.assertEqual(names.count("mcp:shared"), 1)
        # ``extra`` (project-only) is included.
        self.assertEqual(names.count("mcp:extra"), 1)

    def test_malformed_claude_json_is_silent(self):
        (self.home / ".claude.json").write_text("{not valid json")
        # Must not raise.
        budget = estimate_context_budget(self.project, home_dir=self.home)
        # No mcp slices materialise from the broken file.
        self.assertFalse(any(s.name.startswith("mcp:") for s in budget.slices))

    # ── skills ────────────────────────────────────────────────────────

    def test_skill_md_charged_per_skill(self):
        skill_a = self.home / ".claude" / "skills" / "alpha"
        skill_a.mkdir()
        (skill_a / "SKILL.md").write_text("x" * 400)  # 100 tokens
        skill_b = self.home / ".claude" / "skills" / "beta"
        skill_b.mkdir()
        (skill_b / "SKILL.md").write_text("y" * 200)  # 50 tokens
        # A directory with no SKILL.md is silently skipped.
        (self.home / ".claude" / "skills" / "no-skill-md").mkdir()

        budget = estimate_context_budget(self.project, home_dir=self.home)
        slices_by_name = {s.name: s for s in budget.slices}
        self.assertEqual(slices_by_name["skill:alpha"].tokens, 100)
        self.assertEqual(slices_by_name["skill:beta"].tokens, 50)
        self.assertNotIn("skill:no-skill-md", slices_by_name)

    # ── agents ────────────────────────────────────────────────────────

    def test_project_and_global_agents_distinguished_by_scope(self):
        (self.project / ".claude" / "agents" / "linter.md").write_text("p" * 80)  # 20 tok
        (self.home / ".claude" / "agents" / "reviewer.md").write_text("g" * 160)  # 40 tok
        budget = estimate_context_budget(self.project, home_dir=self.home)
        names = {s.name: s for s in budget.slices}
        self.assertEqual(names["agent:project:linter"].tokens, 20)
        self.assertEqual(names["agent:global:reviewer"].tokens, 40)

    # ── totals + cost ─────────────────────────────────────────────────

    def test_total_equals_sum_of_slice_tokens(self):
        (self.project / "CLAUDE.md").write_text("x" * 400)  # 100 tok
        budget = estimate_context_budget(self.project, home_dir=self.home)
        self.assertEqual(budget.total_tokens, sum(s.tokens for s in budget.slices))
        self.assertGreater(budget.total_tokens, DEFAULT_SYSTEM_PROMPT_TOKENS)

    def test_cost_projection_uses_default_anthropic_rate(self):
        # With nothing but the system prompt, total = DEFAULT tokens.
        budget = estimate_context_budget(self.project, home_dir=self.home)
        expected_per_session = (
            budget.total_tokens / 1_000_000.0
        ) * DEFAULT_INPUT_USD_PER_MILLION
        self.assertAlmostEqual(budget.cost_per_session_usd, expected_per_session, places=8)
        self.assertAlmostEqual(
            budget.estimated_monthly_cost_usd,
            expected_per_session * DEFAULT_SESSIONS_PER_MONTH,
            places=8,
        )

    # ── serialization ─────────────────────────────────────────────────

    def test_to_dict_round_trip_keys(self):
        budget = estimate_context_budget(self.project, home_dir=self.home)
        d = budget.to_dict()
        self.assertIn("total_tokens", d)
        self.assertIn("slices", d)
        self.assertIn("cost_per_session_usd", d)
        self.assertIn("estimated_monthly_cost_usd", d)
        self.assertIn("heuristic", d)
        self.assertIsInstance(d["slices"], list)
        for s in d["slices"]:
            self.assertIn("name", s)
            self.assertIn("tokens", s)
            self.assertIn("source_path", s)


class TestGlobalBudget(unittest.TestCase):
    """``estimate_global_budget`` excludes project-only artefacts."""

    def setUp(self):
        self._home_tmp = TemporaryDirectory()
        self.home = Path(self._home_tmp.name)
        (self.home / ".claude").mkdir(parents=True)
        (self.home / ".claude" / "skills").mkdir()
        (self.home / ".claude" / "agents").mkdir()

    def tearDown(self):
        self._home_tmp.cleanup()

    def test_global_budget_has_no_project_slices(self):
        budget = estimate_global_budget(home_dir=self.home)
        names = {s.name for s in budget.slices}
        self.assertIn("system_prompt", names)
        self.assertIn("memory:global_CLAUDE.md", names)
        # Project memory must not appear in the global budget.
        self.assertNotIn("memory:project_CLAUDE.md", names)
        # No project-scoped agent slice should appear.
        for n in names:
            self.assertFalse(n.startswith("agent:project:"), f"unexpected slice {n!r}")

    def test_global_budget_includes_skills(self):
        skill_dir = self.home / ".claude" / "skills" / "demo"
        skill_dir.mkdir()
        (skill_dir / "SKILL.md").write_text("z" * 80)  # 20 tokens
        budget = estimate_global_budget(home_dir=self.home)
        slice_ = next(s for s in budget.slices if s.name == "skill:demo")
        self.assertEqual(slice_.tokens, 20)


class TestDataclasses(unittest.TestCase):
    """Sanity checks on the public dataclass shapes."""

    def test_context_slice_default_source_path_none(self):
        s = ContextSlice(name="x", tokens=42)
        self.assertIsNone(s.source_path)

    def test_context_budget_default_slices_empty(self):
        b = ContextBudget(total_tokens=0)
        self.assertEqual(b.slices, [])
        self.assertIn("len(text)", b.heuristic)


if __name__ == "__main__":
    unittest.main()
