"""Fixture tests for ``reports.benchmark`` — the observational engine (spec 26 §9).

Deterministic in-``tmp_path`` stores, never the user's real store. These cover
the honesty contract: the confounder guard (refuse a pooled winner), the sample
floor ("insufficient evidence", never a rank), coverage honesty (NULLs excluded
not imputed), success-tier precedence, the happy-path winner, and empty /
single-model degeneracy.
"""

from __future__ import annotations

import json
import sqlite3
from pathlib import Path

import pytest

from stackunderflow.reports import benchmark
from stackunderflow.store import db, schema

# ── seeding helpers ──────────────────────────────────────────────────────────


def _make_conn(tmp_path: Path) -> sqlite3.Connection:
    conn = db.connect(tmp_path / "store.db")
    schema.apply(conn)
    return conn


def _seed_project(conn: sqlite3.Connection, slug: str = "p1") -> int:
    conn.execute(
        "INSERT INTO projects (provider, slug, display_name, first_seen, last_modified) "
        "VALUES ('claude', ?, ?, 0, 0)",
        (slug, slug),
    )
    return conn.execute("SELECT id FROM projects WHERE slug = ?", (slug,)).fetchone()[0]


def _seed_session(
    conn: sqlite3.Connection,
    *,
    project_id: int,
    session_id: str,
    model: str,
    cost: float,
    in_tok: int = 300,
    out_tok: int = 200,
    one_shot: bool = False,
    turns: int = 3,
    first_text: str = "fix the bug in foo.py",
    grade: float | None = None,
    first_ts: str = "2026-04-01T10:00:00Z",
) -> None:
    """Seed a session + first user message + session_mart row (+ optional grade)."""
    cur = conn.execute(
        "INSERT INTO sessions (project_id, session_id, first_ts, last_ts, message_count) "
        "VALUES (?, ?, ?, ?, ?)",
        (project_id, session_id, first_ts, first_ts, turns + 1),
    )
    sfk = cur.lastrowid
    conn.execute(
        "INSERT INTO messages (session_fk, seq, timestamp, role, content_text, raw_json) "
        "VALUES (?, 0, ?, 'user', ?, '{}')",
        (sfk, first_ts, first_text),
    )
    conn.execute(
        "INSERT INTO session_mart "
        "(session_id, project_id, provider, primary_model, first_ts, last_ts, "
        " message_count, user_message_count, assistant_message_count, "
        " input_tokens, output_tokens, cost_usd, is_one_shot) "
        "VALUES (?, ?, 'claude', ?, ?, ?, ?, 1, ?, ?, ?, ?, ?)",
        (
            session_id, project_id, model, first_ts, first_ts,
            turns + 1, turns, in_tok, out_tok, cost, 1 if one_shot else 0,
        ),
    )
    if grade is not None:
        conn.execute(
            "INSERT INTO session_quality_metrics "
            "(session_id, overall_score, grades_json, rationale, suggestions_json, graded_at) "
            "VALUES (?, ?, ?, 'ok', '[]', ?)",
            (session_id, grade, json.dumps({"success": grade}), first_ts),
        )


# tiny: in+out < 200 ; small: 200..799 ; med: 800..2999 ; large: >=3000
_SIZE_TOKENS = {
    "tiny": (50, 50),
    "small": (300, 200),
    "med": (1000, 500),
    "large": (3000, 1000),
}


def _seed_winner_fixture(conn: sqlite3.Connection, pid: int) -> None:
    """Two strata (fix×small, fix×med), 10 sessions per model per stratum.

    ``sonnet`` is one-shot + cheap (success via Tier-4, cost 0.05); ``opus`` is
    high-retry + dear (fail via Tier-4, cost 0.50). A clean, significant, two-
    stratum win with enough balanced n to headline.
    """
    for band in ("small", "med"):
        in_tok, out_tok = _SIZE_TOKENS[band]
        for i in range(10):
            _seed_session(
                conn, project_id=pid, session_id=f"son-{band}-{i}",
                model="sonnet", cost=0.05, in_tok=in_tok, out_tok=out_tok,
                one_shot=True, turns=1,
            )
            _seed_session(
                conn, project_id=pid, session_id=f"opus-{band}-{i}",
                model="opus", cost=0.50, in_tok=in_tok, out_tok=out_tok,
                one_shot=False, turns=10,
            )


# ── empty / degenerate ───────────────────────────────────────────────────────


class TestDegenerate:
    def test_schemaless_store_is_well_formed(self):
        raw = sqlite3.connect(":memory:")
        raw.row_factory = sqlite3.Row
        r = benchmark.analyze_benchmark(raw)
        assert r["verdict"]["headline"] == "insufficient evidence"
        assert r["verdict"]["winning_model"] is None
        assert r["strata"] == []
        assert r["weights"] == {"success": 0.45, "cost": 0.35, "effort": 0.20}
        assert r["rubric_version"] == 1
        assert r["success_threshold"] == 7.0
        assert r["ci_level"] == 0.90

    def test_empty_schema_store(self, tmp_path):
        conn = _make_conn(tmp_path)
        r = benchmark.analyze_benchmark(conn)
        assert r["verdict"]["headline"] == "insufficient evidence"
        assert r["coverage"]["sessions_total"] == 0

    def test_single_model_store_names_no_winner(self, tmp_path):
        conn = _make_conn(tmp_path)
        pid = _seed_project(conn)
        for i in range(8):
            _seed_session(conn, project_id=pid, session_id=f"s{i}",
                          model="sonnet", cost=0.05, one_shot=True, turns=1)
        r = benchmark.analyze_benchmark(conn)
        assert r["verdict"]["winning_model"] is None
        # one stratum, one model → nothing to compare
        assert all(s["winner"] is None for s in r["strata"])


# ── confounder guard (the core threat) ───────────────────────────────────────


class TestConfounderGuard:
    def test_refuses_pooled_winner_when_models_never_share_a_stratum(self, tmp_path):
        conn = _make_conn(tmp_path)
        pid = _seed_project(conn)
        # sonnet only draws TINY tasks; opus only draws LARGE tasks.
        for i in range(8):
            it, ot = _SIZE_TOKENS["tiny"]
            _seed_session(conn, project_id=pid, session_id=f"son{i}", model="sonnet",
                          cost=0.02, in_tok=it, out_tok=ot, one_shot=True, turns=1)
            it, ot = _SIZE_TOKENS["large"]
            _seed_session(conn, project_id=pid, session_id=f"op{i}", model="opus",
                          cost=0.80, in_tok=it, out_tok=ot, one_shot=False, turns=10)
        r = benchmark.analyze_benchmark(conn)

        # No stratum has two models → no winner can be named.
        assert r["verdict"]["winning_model"] is None
        assert r["verdict"]["headline"] == "insufficient evidence"
        # Each stratum reports exactly one model and discloses the imbalance.
        for s in r["strata"]:
            assert len(s["assignment_balance"]) == 1
            assert s["cell_verdict"] == "insufficient evidence"


# ── sample floor ─────────────────────────────────────────────────────────────


class TestSampleFloor:
    def test_below_floor_is_insufficient_never_a_rank(self, tmp_path):
        conn = _make_conn(tmp_path)
        pid = _seed_project(conn)
        # n=3 for the only model in the stratum — below MIN_SESSIONS_PER_CELL.
        for i in range(3):
            _seed_session(conn, project_id=pid, session_id=f"s{i}", model="sonnet",
                          cost=0.05, one_shot=True, turns=1)
        r = benchmark.analyze_benchmark(conn)
        assert len(r["strata"]) == 1
        cell = r["strata"][0]
        assert cell["cell_verdict"] == "insufficient evidence"
        assert cell["winner"] is None
        assert cell["models"][0]["qualified"] is False
        assert cell["models"][0]["n"] == 3


# ── coverage honesty ─────────────────────────────────────────────────────────


class TestCoverageHonesty:
    def test_unmeasured_sessions_excluded_from_rate_counted_in_coverage(self, tmp_path):
        conn = _make_conn(tmp_path)
        pid = _seed_project(conn)
        # 3 measured (graded success) + 3 unmeasured (no signal at all).
        for i in range(3):
            _seed_session(conn, project_id=pid, session_id=f"g{i}", model="sonnet",
                          cost=0.05, grade=9.0, one_shot=False, turns=3)
        for i in range(3):
            # not one-shot, low turns, no grade/static/outcome → success = None
            _seed_session(conn, project_id=pid, session_id=f"n{i}", model="sonnet",
                          cost=0.05, grade=None, one_shot=False, turns=3)
        r = benchmark.analyze_benchmark(conn)
        assert r["coverage"]["sessions_total"] == 6
        assert r["coverage"]["sessions_scored"] == 3  # NULLs excluded
        assert r["coverage"]["grade_coverage"] == pytest.approx(0.5)
        row = r["strata"][0]["models"][0]
        assert row["n"] == 6
        assert row["success_measured_n"] == 3
        assert row["coverage"] == pytest.approx(0.5)
        assert row["success_rate"]["point"] == pytest.approx(1.0)  # 3/3 measured


# ── success-tier precedence ──────────────────────────────────────────────────


class TestSuccessTierPrecedence:
    def test_ground_truth_beats_grade(self):
        # CI passed (Tier 1 → success) but LLM grade is low (Tier 3 → fail).
        # Tier 1 must win and be recorded.
        gt = {"prs": [], "ci_runs": [{"status": "success"}]}
        val, tier = benchmark._compose_success(
            "s1",
            ground_truth={"s1": gt},
            static_outcome={},
            grade_success=2.0,  # low grade → would be a fail on Tier 3
            is_one_shot=False,
            num_turns=3,
        )
        assert val == 1
        assert tier == "ground_truth"

    def test_revert_is_a_failure(self):
        gt = {"prs": [{"state": "merged", "reverted_at": "2026-01-01T00:00:00Z"}], "ci_runs": []}
        val, tier = benchmark._compose_success(
            "s1", ground_truth={"s1": gt}, static_outcome={},
            grade_success=None, is_one_shot=True, num_turns=1,
        )
        assert val == 0
        assert tier == "ground_truth"

    def test_falls_through_to_behavioral(self):
        val, tier = benchmark._compose_success(
            "s1", ground_truth={}, static_outcome={},
            grade_success=None, is_one_shot=True, num_turns=1,
        )
        assert val == 1
        assert tier == "behavioral"


# ── happy-path winner ────────────────────────────────────────────────────────


class TestWinner:
    def test_names_a_winner_when_evidence_is_strong(self, tmp_path):
        conn = _make_conn(tmp_path)
        pid = _seed_project(conn)
        _seed_winner_fixture(conn, pid)
        r = benchmark.analyze_benchmark(conn)

        assert r["verdict"]["winning_model"] == "sonnet"
        assert "sonnet" in r["verdict"]["headline"]
        assert r["verdict"]["confidence"] in {"low", "medium", "high"}
        assert r["verdict"]["cost_per_outcome_usd"] == pytest.approx(0.05, abs=1e-6)
        # both strata are clear sonnet wins
        clears = [s for s in r["strata"] if s["cell_verdict"] == "clear"]
        assert len(clears) == 2
        for s in clears:
            assert s["winner"] == "sonnet"
            assert s["effect"]["statistically_separated"] is True

    def test_cost_read_from_session_mart_not_recomputed(self, tmp_path):
        conn = _make_conn(tmp_path)
        pid = _seed_project(conn)
        for i in range(6):
            _seed_session(conn, project_id=pid, session_id=f"s{i}", model="sonnet",
                          cost=0.123, one_shot=True, turns=1)
        r = benchmark.analyze_benchmark(conn)
        row = r["strata"][0]["models"][0]
        assert row["median_cost"]["point"] == pytest.approx(0.123)

    def test_deterministic_across_runs(self, tmp_path):
        conn = _make_conn(tmp_path)
        pid = _seed_project(conn)
        _seed_winner_fixture(conn, pid)
        a = benchmark.analyze_benchmark(conn)
        b = benchmark.analyze_benchmark(conn)
        assert a == b  # seeded bootstrap + pure classify → byte-identical


# ── recommend_from_history ───────────────────────────────────────────────────


class TestRecommend:
    def test_recommends_stratum_winner(self, tmp_path):
        conn = _make_conn(tmp_path)
        pid = _seed_project(conn)
        _seed_winner_fixture(conn, pid)
        rec = benchmark.recommend_from_history(conn, intent="fix", size="small")
        assert rec["recommended_model"] == "sonnet"
        assert rec["basis"] == "stratum"
        assert rec["stratum"]["size_band"] == "small"

    def test_insufficient_evidence_is_honest(self, tmp_path):
        conn = _make_conn(tmp_path)
        pid = _seed_project(conn)
        for i in range(3):
            _seed_session(conn, project_id=pid, session_id=f"s{i}", model="sonnet",
                          cost=0.05, one_shot=True, turns=1)
        rec = benchmark.recommend_from_history(conn, intent="fix", size="small")
        assert rec["recommended_model"] is None
        assert rec["basis"] == "insufficient_evidence"
