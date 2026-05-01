"""Tests for the waste-finding heuristic (store-backed)."""

import json
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from stackunderflow.reports import optimize as optimize_mod
from stackunderflow.reports.optimize import (
    Finding,
    find_patterns,
    find_waste,
)
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


# ── helpers for direct store seeding ───────────────────────────────────────


def _open_test_store() -> tuple[Path, "object"]:
    """Open a fresh store DB in a tempdir; return (tmp, conn)."""
    tmp = tempfile.TemporaryDirectory()
    conn = db.connect(Path(tmp.name) / "store.db")
    schema.apply(conn)
    # One default project + session for fixtures.
    conn.execute(
        "INSERT INTO projects (provider, slug, display_name, first_seen, last_modified) "
        "VALUES (?, ?, ?, ?, ?)",
        ("claude", "fixture-proj", "fixture-proj", 0.0, 0.0),
    )
    pid = conn.execute("SELECT id FROM projects WHERE slug = 'fixture-proj'").fetchone()[0]
    conn.execute(
        "INSERT INTO sessions (project_id, session_id) VALUES (?, ?)",
        (pid, "sess-A"),
    )
    sid = conn.execute("SELECT id FROM sessions WHERE session_id = 'sess-A'").fetchone()[0]
    conn.commit()
    return tmp, conn, sid


def _seed_message(
    conn,
    *,
    session_fk: int,
    seq: int,
    role: str,
    timestamp: str = "2026-04-25T10:00:00Z",
    tools: list[str] | None = None,
    raw: dict | None = None,
    content_text: str = "",
    input_tokens: int = 0,
    cache_create: int = 0,
) -> None:
    conn.execute(
        "INSERT INTO messages (session_fk, seq, timestamp, role, model, "
        " input_tokens, output_tokens, cache_create_tokens, cache_read_tokens, "
        " content_text, tools_json, raw_json, is_sidechain, uuid, parent_uuid) "
        "VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        (
            session_fk, seq, timestamp, role, "claude-sonnet-4-6",
            input_tokens, 0, cache_create, 0,
            content_text, json.dumps(tools or []), json.dumps(raw or {}),
            0, f"u{seq}", None,
        ),
    )
    conn.commit()


def _assistant_tool_use_raw(name: str, input_obj: dict) -> dict:
    """Synthesise a Claude raw_json payload for an assistant tool_use."""
    return {
        "message": {
            "role": "assistant",
            "content": [{"type": "tool_use", "name": name, "input": input_obj}],
        }
    }


# ── legacy find_waste tests (unchanged) ────────────────────────────────────


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


# ── Pattern 1: bloated CLAUDE.md ────────────────────────────────────────────


class TestBloatedClaudeMd(unittest.TestCase):
    def setUp(self):
        self.tmp, self.conn, self.session_fk = _open_test_store()
        # Redirect ~/.claude to a tempdir so we control the filesystem
        self._home = tempfile.TemporaryDirectory()
        self._home_patch = patch.object(
            Path, "home", classmethod(lambda cls: Path(self._home.name))  # noqa: ARG005
        )
        self._home_patch.start()
        (Path(self._home.name) / ".claude").mkdir()
        (Path(self._home.name) / ".claude" / "projects").mkdir()

    def tearDown(self):
        self._home_patch.stop()
        self.conn.close()
        self.tmp.cleanup()
        self._home.cleanup()

    def test_no_claude_md_no_finding(self):
        findings = optimize_mod._detect_bloated_claude_md(self.conn)
        self.assertEqual(findings, [])

    def test_small_claude_md_no_finding(self):
        md = Path(self._home.name) / ".claude" / "CLAUDE.md"
        md.write_text("short note")
        findings = optimize_mod._detect_bloated_claude_md(self.conn)
        self.assertEqual(findings, [])

    def test_bloated_claude_md_emits_high_for_3x(self):
        # 4 chars per token approximation → 4x threshold * 4 chars
        chars_for_4x = 4 * optimize_mod.CLAUDE_MD_TOKEN_THRESHOLD * 4 + 100
        md = Path(self._home.name) / ".claude" / "CLAUDE.md"
        md.write_text("x" * chars_for_4x)
        findings = optimize_mod._detect_bloated_claude_md(self.conn)
        self.assertEqual(len(findings), 1)
        self.assertEqual(findings[0].pattern_id, "bloated_claude_md")
        self.assertEqual(findings[0].severity, "high")
        self.assertGreater(findings[0].estimated_waste_tokens or 0, 0)

    def test_bloated_claude_md_just_over_is_low(self):
        # Just barely over the threshold → severity "low"
        chars = (optimize_mod.CLAUDE_MD_TOKEN_THRESHOLD + 100) * 4
        md = Path(self._home.name) / ".claude" / "CLAUDE.md"
        md.write_text("y" * chars)
        findings = optimize_mod._detect_bloated_claude_md(self.conn)
        self.assertEqual(len(findings), 1)
        self.assertEqual(findings[0].severity, "low")


# ── Pattern 2: unused MCP servers ──────────────────────────────────────────


class TestUnusedMcpServers(unittest.TestCase):
    def setUp(self):
        self.tmp, self.conn, self.session_fk = _open_test_store()
        self._home = tempfile.TemporaryDirectory()
        self._home_patch = patch.object(
            Path, "home", classmethod(lambda cls: Path(self._home.name))  # noqa: ARG005
        )
        self._home_patch.start()

    def tearDown(self):
        self._home_patch.stop()
        self.conn.close()
        self.tmp.cleanup()
        self._home.cleanup()

    def _write_claude_json(self, servers: dict[str, dict]) -> None:
        cfg = Path(self._home.name) / ".claude.json"
        cfg.write_text(json.dumps({"mcpServers": servers}))

    def test_no_servers_registered_no_finding(self):
        findings = optimize_mod._detect_unused_mcp_servers(self.conn)
        self.assertEqual(findings, [])

    def test_all_servers_used_no_finding(self):
        self._write_claude_json({"taco": {"command": "x"}})
        # Seed a recent message that calls a taco MCP tool
        _seed_message(
            self.conn, session_fk=self.session_fk, seq=1, role="assistant",
            timestamp="2099-01-01T10:00:00Z",
            tools=["mcp__taco__order"],
        )
        findings = optimize_mod._detect_unused_mcp_servers(self.conn)
        self.assertEqual(findings, [])

    def test_unused_server_emits_finding(self):
        self._write_claude_json({
            "abandoned1": {"command": "x"},
            "abandoned2": {"command": "y"},
        })
        findings = optimize_mod._detect_unused_mcp_servers(self.conn)
        self.assertEqual(len(findings), 1)
        self.assertEqual(findings[0].pattern_id, "unused_mcp_servers")
        self.assertEqual(findings[0].affected_count, 2)
        self.assertIn("abandoned1", findings[0].details["unused_servers"])

    def test_severity_scales_with_count(self):
        self._write_claude_json({f"srv{i}": {} for i in range(7)})
        findings = optimize_mod._detect_unused_mcp_servers(self.conn)
        self.assertEqual(findings[0].severity, "high")


# ── Pattern 3: ghost agents ────────────────────────────────────────────────


class TestGhostAgents(unittest.TestCase):
    def setUp(self):
        self.tmp, self.conn, self.session_fk = _open_test_store()
        self._home = tempfile.TemporaryDirectory()
        self._home_patch = patch.object(
            Path, "home", classmethod(lambda cls: Path(self._home.name))  # noqa: ARG005
        )
        self._home_patch.start()

    def tearDown(self):
        self._home_patch.stop()
        self.conn.close()
        self.tmp.cleanup()
        self._home.cleanup()

    def _create_agent(self, name: str) -> None:
        agents_dir = Path(self._home.name) / ".claude" / "agents"
        agents_dir.mkdir(parents=True, exist_ok=True)
        (agents_dir / f"{name}.md").write_text(f"# {name}\nAgent definition.")

    def test_no_agents_no_finding(self):
        findings = optimize_mod._detect_ghost_agents(self.conn)
        self.assertEqual(findings, [])

    def test_unused_agent_emits_finding(self):
        self._create_agent("forgotten-helper")
        findings = optimize_mod._detect_ghost_agents(self.conn)
        self.assertEqual(len(findings), 1)
        self.assertEqual(findings[0].pattern_id, "ghost_agents")
        self.assertEqual(findings[0].affected_count, 1)

    def test_used_agent_skipped(self):
        self._create_agent("used-helper")
        self._create_agent("forgotten-helper")
        # Simulate a recent Task call invoking 'used-helper'
        raw = {"toolUse": {"subagent_type": "used-helper"}}
        _seed_message(
            self.conn, session_fk=self.session_fk, seq=1, role="assistant",
            timestamp="2099-01-01T10:00:00Z",
            tools=["Task"],
            raw=raw,
        )
        findings = optimize_mod._detect_ghost_agents(self.conn)
        self.assertEqual(len(findings), 1)
        names = [a["name"] for a in findings[0].details["agents"]]
        self.assertIn("forgotten-helper", names)
        self.assertNotIn("used-helper", names)


# ── Pattern 4: low read:edit ratio ─────────────────────────────────────────


class TestLowReadEditRatio(unittest.TestCase):
    def setUp(self):
        self.tmp, self.conn, self.session_fk = _open_test_store()

    def tearDown(self):
        self.conn.close()
        self.tmp.cleanup()

    def test_no_reads_no_finding(self):
        findings = optimize_mod._detect_low_read_edit_ratio(self.conn)
        self.assertEqual(findings, [])

    def test_many_reads_zero_edits_emits_finding(self):
        for i in range(optimize_mod.LOW_READ_EDIT_READ_FLOOR + 2):
            _seed_message(
                self.conn, session_fk=self.session_fk, seq=i, role="assistant",
                tools=["Read"],
            )
        findings = optimize_mod._detect_low_read_edit_ratio(self.conn)
        self.assertEqual(len(findings), 1)
        self.assertEqual(findings[0].pattern_id, "low_read_edit_ratio")
        self.assertEqual(findings[0].affected_count, 1)
        self.assertGreater(findings[0].estimated_waste_tokens or 0, 0)

    def test_reads_with_edit_no_finding(self):
        for i in range(optimize_mod.LOW_READ_EDIT_READ_FLOOR + 2):
            _seed_message(
                self.conn, session_fk=self.session_fk, seq=i, role="assistant",
                tools=["Read"],
            )
        _seed_message(
            self.conn, session_fk=self.session_fk, seq=999, role="assistant",
            tools=["Edit"],
        )
        findings = optimize_mod._detect_low_read_edit_ratio(self.conn)
        self.assertEqual(findings, [])

    def test_below_threshold_no_finding(self):
        for i in range(optimize_mod.LOW_READ_EDIT_READ_FLOOR - 1):
            _seed_message(
                self.conn, session_fk=self.session_fk, seq=i, role="assistant",
                tools=["Read"],
            )
        findings = optimize_mod._detect_low_read_edit_ratio(self.conn)
        self.assertEqual(findings, [])


# ── Pattern 5: junk reads ──────────────────────────────────────────────────


class TestJunkReads(unittest.TestCase):
    def setUp(self):
        self.tmp, self.conn, self.session_fk = _open_test_store()

    def tearDown(self):
        self.conn.close()
        self.tmp.cleanup()

    def test_no_repeats_no_finding(self):
        findings = optimize_mod._detect_junk_reads(self.conn)
        self.assertEqual(findings, [])

    def test_same_path_read_repeatedly_emits_finding(self):
        path = "/tmp/foo.py"  # noqa: S108 — synthetic string, not a filesystem path
        for i in range(optimize_mod.JUNK_READ_REPEAT_THRESHOLD + 1):
            _seed_message(
                self.conn, session_fk=self.session_fk, seq=i, role="assistant",
                tools=["Read"],
                raw=_assistant_tool_use_raw("Read", {"file_path": path}),
            )
        findings = optimize_mod._detect_junk_reads(self.conn)
        self.assertEqual(len(findings), 1)
        self.assertEqual(findings[0].pattern_id, "junk_reads")
        self.assertEqual(findings[0].affected_count, 1)

    def test_different_paths_no_finding(self):
        for i in range(10):
            _seed_message(
                self.conn, session_fk=self.session_fk, seq=i, role="assistant",
                tools=["Read"],
                raw=_assistant_tool_use_raw("Read", {"file_path": f"/tmp/{i}.py"}),  # noqa: S108
            )
        findings = optimize_mod._detect_junk_reads(self.conn)
        self.assertEqual(findings, [])

    def test_exactly_at_threshold_emits(self):
        path = "/tmp/edge.py"  # noqa: S108 — synthetic string, not a filesystem path
        for i in range(optimize_mod.JUNK_READ_REPEAT_THRESHOLD):
            _seed_message(
                self.conn, session_fk=self.session_fk, seq=i, role="assistant",
                tools=["Read"],
                raw=_assistant_tool_use_raw("Read", {"file_path": path}),
            )
        findings = optimize_mod._detect_junk_reads(self.conn)
        self.assertEqual(len(findings), 1)


# ── Pattern 6: cache overhead ──────────────────────────────────────────────


class TestCacheOverhead(unittest.TestCase):
    def setUp(self):
        self.tmp, self.conn, self.session_fk = _open_test_store()

    def tearDown(self):
        self.conn.close()
        self.tmp.cleanup()

    def test_no_messages_no_finding(self):
        findings = optimize_mod._detect_cache_overhead(self.conn)
        self.assertEqual(findings, [])

    def test_high_cache_create_emits_finding(self):
        # Cache 1000, input 100 → ratio 0.91 > 0.5
        _seed_message(
            self.conn, session_fk=self.session_fk, seq=0, role="assistant",
            input_tokens=100, cache_create=1000,
        )
        findings = optimize_mod._detect_cache_overhead(self.conn)
        self.assertEqual(len(findings), 1)
        self.assertEqual(findings[0].pattern_id, "cache_overhead")

    def test_low_cache_ratio_no_finding(self):
        _seed_message(
            self.conn, session_fk=self.session_fk, seq=0, role="assistant",
            input_tokens=10_000, cache_create=100,
        )
        findings = optimize_mod._detect_cache_overhead(self.conn)
        self.assertEqual(findings, [])

    def test_zero_cache_no_finding(self):
        _seed_message(
            self.conn, session_fk=self.session_fk, seq=0, role="assistant",
            input_tokens=100, cache_create=0,
        )
        findings = optimize_mod._detect_cache_overhead(self.conn)
        self.assertEqual(findings, [])


# ── Pattern 7: bash output limits ──────────────────────────────────────────


class TestBashOutputLimits(unittest.TestCase):
    def setUp(self):
        self.tmp, self.conn, self.session_fk = _open_test_store()

    def tearDown(self):
        self.conn.close()
        self.tmp.cleanup()

    def test_no_bash_no_finding(self):
        findings = optimize_mod._detect_bash_output_limits(self.conn)
        self.assertEqual(findings, [])

    def test_oversized_bash_output_emits_finding(self):
        # Assistant Bash call
        _seed_message(
            self.conn, session_fk=self.session_fk, seq=0, role="assistant",
            tools=["Bash"],
            raw=_assistant_tool_use_raw("Bash", {"command": "find /"}),
        )
        # User tool_result with > 50 KB content
        big_blob = "x" * (optimize_mod.BASH_OUTPUT_BYTES_THRESHOLD + 1)
        _seed_message(
            self.conn, session_fk=self.session_fk, seq=1, role="user",
            tools=[],
            content_text=big_blob,
        )
        findings = optimize_mod._detect_bash_output_limits(self.conn)
        self.assertEqual(len(findings), 1)
        self.assertEqual(findings[0].pattern_id, "bash_output_limits")
        self.assertEqual(findings[0].affected_count, 1)

    def test_small_bash_output_no_finding(self):
        _seed_message(
            self.conn, session_fk=self.session_fk, seq=0, role="assistant",
            tools=["Bash"],
            raw=_assistant_tool_use_raw("Bash", {"command": "ls"}),
        )
        _seed_message(
            self.conn, session_fk=self.session_fk, seq=1, role="user",
            tools=[],
            content_text="three lines\nof output\nhere",
        )
        findings = optimize_mod._detect_bash_output_limits(self.conn)
        self.assertEqual(findings, [])

    def test_user_output_without_preceding_bash_skipped(self):
        # Big user message, but no Bash call earlier — not our pattern.
        _seed_message(
            self.conn, session_fk=self.session_fk, seq=0, role="user",
            tools=[],
            content_text="x" * (optimize_mod.BASH_OUTPUT_BYTES_THRESHOLD + 1),
        )
        findings = optimize_mod._detect_bash_output_limits(self.conn)
        self.assertEqual(findings, [])


# ── orchestrator ───────────────────────────────────────────────────────────


class TestFindPatternsOrchestrator(unittest.TestCase):
    def setUp(self):
        self.tmp, self.conn, self.session_fk = _open_test_store()

    def tearDown(self):
        self.conn.close()
        self.tmp.cleanup()

    def test_returns_empty_when_nothing_to_find(self):
        # Force the filesystem detectors into "nothing here" land.
        with patch.object(optimize_mod, "_candidate_claude_md_paths", return_value=[]):
            with patch.object(optimize_mod, "_registered_mcp_servers", return_value=[]):
                with patch.object(optimize_mod, "_registered_agents", return_value=[]):
                    findings = find_patterns(self.conn)
        self.assertEqual(findings, [])

    def test_findings_sorted_by_severity_desc(self):
        # Seed cache_overhead (high if 10+ sessions); we'll get one finding
        # with severity 'low' since only 1 session.
        _seed_message(
            self.conn, session_fk=self.session_fk, seq=0, role="assistant",
            input_tokens=10, cache_create=1000,
        )
        with patch.object(optimize_mod, "_candidate_claude_md_paths", return_value=[]):
            with patch.object(optimize_mod, "_registered_mcp_servers", return_value=[]):
                with patch.object(optimize_mod, "_registered_agents", return_value=[]):
                    findings = find_patterns(self.conn)
        self.assertGreaterEqual(len(findings), 1)
        # severities are non-decreasing in (high → low) order
        ranks = [{"high": 0, "medium": 1, "low": 2}[f.severity] for f in findings]
        self.assertEqual(ranks, sorted(ranks))


# ── Finding shape ──────────────────────────────────────────────────────────


class TestFindingShape(unittest.TestCase):
    def test_to_dict_round_trip(self):
        f = Finding(
            pattern_id="test",
            severity="high",
            title="t",
            description="d",
            affected_count=1,
            suggested_fix="fix",
            estimated_waste_tokens=42,
        )
        d = f.to_dict()
        self.assertEqual(d["pattern_id"], "test")
        self.assertEqual(d["severity"], "high")
        self.assertEqual(d["estimated_waste_tokens"], 42)
        self.assertIn("details", d)


if __name__ == "__main__":
    unittest.main()
