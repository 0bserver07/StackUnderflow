"""Unit tests for ``stackunderflow.services.mode_recommender`` (Spec 18 v1).

Covers:

* feature-extraction snapshots (intent / band / language / counts)
* hash stability (same features → same hash; different features → diff)
* recommendation shape on a seeded store with 5+ historical sessions
* cache miss → store, hit → bumped ``last_used_ts``, TTL eviction
* empty-store path returns ``confidence=0.0`` with a clean message
* the ``recommend()`` service entry point round-trips the full payload
* meta-agent dispatcher routes ``recommend_mode`` correctly

All tests use ``tmp_path``; the user's real
``~/.stackunderflow/store.db`` is never touched.
"""

from __future__ import annotations

import json
import sqlite3
from datetime import UTC, datetime, timedelta
from pathlib import Path

import pytest

from stackunderflow.services import mode_recommender as mr
from stackunderflow.store import db, schema

# ── helpers ─────────────────────────────────────────────────────────────────


def _make_conn(tmp_path: Path) -> sqlite3.Connection:
    conn = db.connect(tmp_path / "store.db")
    schema.apply(conn)
    return conn


def _seed_session(
    conn: sqlite3.Connection,
    *,
    project_id: int,
    session_id: str,
    primary_model: str,
    cost_usd: float,
    first_user_text: str,
    last_ts: str = "2026-04-01T11:00:00Z",
) -> None:
    """Seed a session + first-user-message + session_mart row."""
    cur = conn.execute(
        "INSERT INTO sessions (project_id, session_id, first_ts, last_ts, message_count) "
        "VALUES (?, ?, ?, ?, ?)",
        (project_id, session_id, "2026-04-01T10:00:00Z", last_ts, 2),
    )
    sfk = cur.lastrowid
    conn.execute(
        "INSERT INTO messages (session_fk, seq, timestamp, role, content_text, raw_json) "
        "VALUES (?, 0, ?, ?, ?, ?)",
        (sfk, "2026-04-01T10:00:00Z", "user", first_user_text, "{}"),
    )
    conn.execute(
        "INSERT INTO session_mart "
        "(session_id, project_id, provider, primary_model, first_ts, last_ts, "
        " message_count, cost_usd) "
        "VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        (session_id, project_id, "claude", primary_model,
         "2026-04-01T10:00:00Z", last_ts, 2, cost_usd),
    )


def _seed_project(conn: sqlite3.Connection, slug: str = "p1") -> int:
    conn.execute(
        "INSERT INTO projects (provider, slug, display_name, first_seen, last_modified) "
        "VALUES ('claude', ?, ?, 0, 0)",
        (slug, slug),
    )
    return conn.execute("SELECT id FROM projects WHERE slug = ?", (slug,)).fetchone()[0]


# ── feature extraction ──────────────────────────────────────────────────────


class TestExtractFeatures:
    def test_fix_intent_python_tiny(self):
        f = mr.extract_features("fix the failing test in tests/foo.py")
        assert f == {
            "intent": "fix",
            "token_band": "tiny",
            "languages": ["python"],
            "file_mentions": 1,
            "code_blocks": 0,
        }

    def test_build_intent_typescript(self):
        f = mr.extract_features(
            "implement a new React component for the sidebar in components/Sidebar.tsx"
        )
        assert f["intent"] == "build"
        assert "typescript" in f["languages"]
        assert f["file_mentions"] == 1
        assert f["token_band"] == "tiny"

    def test_refactor_intent(self):
        f = mr.extract_features("refactor and rename the utility function")
        assert f["intent"] == "refactor"

    def test_test_intent(self):
        f = mr.extract_features("write unit tests for the parser, target 90% coverage")
        assert f["intent"] == "test"

    def test_explore_intent_default(self):
        f = mr.extract_features("what does the cache module do")
        assert f["intent"] == "explore"

    def test_empty_prompt_defaults_to_explore_tiny(self):
        f = mr.extract_features("")
        assert f["intent"] == "explore"
        assert f["token_band"] == "tiny"
        assert f["languages"] == []

    def test_token_bands_progression(self):
        assert mr.extract_features("a" * 100)["token_band"] == "tiny"   # ~25 toks
        assert mr.extract_features("a" * 1000)["token_band"] == "small"  # ~250 toks
        assert mr.extract_features("a" * 5000)["token_band"] == "med"    # ~1250 toks
        assert mr.extract_features("a" * 50000)["token_band"] == "large"  # ~12500 toks

    def test_language_overlap_multiple(self):
        f = mr.extract_features(
            "convert the python script (foo.py) to a typescript module foo.ts"
        )
        assert "python" in f["languages"]
        assert "typescript" in f["languages"]
        assert f["languages"] == sorted(f["languages"])  # always sorted

    def test_code_block_count(self):
        prompt = "fix this:\n```python\ndef f(): pass\n```\nand this:\n```js\nlet x;\n```"
        f = mr.extract_features(prompt)
        assert f["code_blocks"] == 2

    def test_file_mentions_count(self):
        f = mr.extract_features("touch a.py b.py c.ts and run pytest")
        assert f["file_mentions"] >= 3


class TestHashFeatures:
    def test_same_features_same_hash(self):
        a = mr.extract_features("fix the failing test in foo.py")
        b = mr.extract_features("fix the failing test in foo.py")
        assert mr.hash_features(a) == mr.hash_features(b)

    def test_different_features_different_hash(self):
        a = mr.extract_features("fix the failing test")
        b = mr.extract_features("implement a new feature")
        assert mr.hash_features(a) != mr.hash_features(b)

    def test_hash_stable_across_dict_orders(self):
        a = {"intent": "fix", "token_band": "tiny", "languages": [],
             "file_mentions": 0, "code_blocks": 0}
        # Same keys, different insertion order
        b = {"code_blocks": 0, "file_mentions": 0, "languages": [],
             "token_band": "tiny", "intent": "fix"}
        assert mr.hash_features(a) == mr.hash_features(b)


# ── recommendation on seeded store ──────────────────────────────────────────


class TestRecommendSeeded:
    @pytest.fixture
    def seeded_conn(self, tmp_path):
        """5 sessions: 3 cheap on sonnet, 2 expensive on opus, all 'fix' intent."""
        conn = _make_conn(tmp_path)
        pid = _seed_project(conn)
        for sid, model, cost, prompt in [
            ("s1", "sonnet", 0.05, "fix the failing test in foo.py"),
            ("s2", "sonnet", 0.04, "debug the broken pytest in bar.py"),
            ("s3", "sonnet", 0.06, "fix bug in baz.py"),
            ("s4", "opus",   0.45, "fix the failing test in x.py"),
            ("s5", "opus",   0.55, "fix bug in y.py pytest"),
        ]:
            _seed_session(conn, project_id=pid, session_id=sid,
                          primary_model=model, cost_usd=cost,
                          first_user_text=prompt)
        return conn

    def test_recommend_picks_cheapest(self, seeded_conn):
        result = mr.recommend(
            seeded_conn,
            "fix the broken test in qux.py pytest",
            current_model="opus",
            use_cache=False,
        )
        assert result["recommended_model"] == "sonnet"
        assert result["current_model"] == "opus"
        assert result["confidence"] > 0.0
        assert result["cost_delta_usd"] > 0.0  # opus median 0.50 - sonnet median 0.05
        assert result["similar_session_count"] == 5
        assert len(result["evidence_session_ids"]) >= 1

    def test_returns_full_payload_shape(self, seeded_conn):
        result = mr.recommend(seeded_conn, "fix the bug in foo.py")
        # The exact contract — every key the CLI / MCP / meta-agent depends on.
        for key in (
            "recommended_model", "current_model", "confidence",
            "cost_delta_usd", "similar_session_count",
            "evidence_session_ids", "features", "task_pattern_hash",
            "rationale", "cache_hit",
        ):
            assert key in result

    def test_cost_delta_is_zero_when_no_current_model(self, seeded_conn):
        result = mr.recommend(
            seeded_conn,
            "fix the broken test in qux.py pytest",
            use_cache=False,
        )
        assert result["cost_delta_usd"] == 0.0

    def test_cost_delta_is_zero_when_current_equals_pick(self, seeded_conn):
        result = mr.recommend(
            seeded_conn,
            "fix the broken test in qux.py pytest",
            current_model="sonnet",
            use_cache=False,
        )
        # Pick is 'sonnet', current is 'sonnet' → no delta
        assert result["cost_delta_usd"] == 0.0


# ── empty store path ────────────────────────────────────────────────────────


class TestEmptyStore:
    def test_no_sessions_returns_zero_confidence(self, tmp_path):
        conn = _make_conn(tmp_path)
        result = mr.recommend(conn, "fix the broken test", current_model="opus")
        assert result["confidence"] == 0.0
        assert result["similar_session_count"] == 0
        assert "no historical data" in result["rationale"].lower()
        assert result["recommended_model"] == "opus"  # falls back to current

    def test_no_current_model_no_history_returns_empty_string(self, tmp_path):
        conn = _make_conn(tmp_path)
        result = mr.recommend(conn, "fix the broken test")
        assert result["recommended_model"] == ""
        assert result["confidence"] == 0.0


# ── cache behaviour ─────────────────────────────────────────────────────────


class TestCache:
    @pytest.fixture
    def seeded_conn(self, tmp_path):
        conn = _make_conn(tmp_path)
        pid = _seed_project(conn)
        for sid, model, cost in [
            ("s1", "sonnet", 0.05), ("s2", "sonnet", 0.04),
            ("s3", "sonnet", 0.06), ("s4", "opus", 0.45),
        ]:
            _seed_session(conn, project_id=pid, session_id=sid,
                          primary_model=model, cost_usd=cost,
                          first_user_text="fix the failing test in foo.py")
        return conn

    def test_first_call_misses_second_call_hits(self, seeded_conn):
        r1 = mr.recommend(seeded_conn, "fix the test in foo.py")
        assert r1["cache_hit"] is False
        r2 = mr.recommend(seeded_conn, "fix the test in foo.py")
        assert r2["cache_hit"] is True
        assert r2["recommended_model"] == r1["recommended_model"]

    def test_cache_row_present_after_recommendation(self, seeded_conn):
        mr.recommend(seeded_conn, "fix the test in foo.py")
        rows = seeded_conn.execute(
            "SELECT recommended_model, confidence, evidence_session_ids "
            "FROM mode_recommendations"
        ).fetchall()
        assert len(rows) == 1
        assert rows[0]["recommended_model"]  # non-empty
        evidence = json.loads(rows[0]["evidence_session_ids"])
        assert isinstance(evidence, list)

    def test_no_cache_flag_skips_cache(self, seeded_conn):
        mr.recommend(seeded_conn, "fix the test in foo.py")
        # Even though there's a cache row, use_cache=False bypasses it.
        r2 = mr.recommend(seeded_conn, "fix the test in foo.py", use_cache=False)
        assert r2["cache_hit"] is False

    def test_ttl_eviction(self, seeded_conn):
        # Manually insert a stale row beyond the TTL.
        stale_ts = (datetime.now(UTC) - timedelta(hours=mr.CACHE_TTL_HOURS + 1)).isoformat()
        features = mr.extract_features("fix the test in foo.py")
        h = mr.hash_features(features)
        seeded_conn.execute(
            "INSERT INTO mode_recommendations "
            "(task_pattern_hash, recommended_model, confidence, "
            " evidence_session_ids, created_ts, last_used_ts) "
            "VALUES (?, ?, ?, ?, ?, ?)",
            (h, "stale-model", 0.9, "[]", stale_ts, stale_ts),
        )
        # Recommendation should ignore the stale row and recompute.
        r = mr.recommend(seeded_conn, "fix the test in foo.py")
        assert r["recommended_model"] != "stale-model"
        assert r["cache_hit"] is False

    def test_cache_hit_bumps_last_used_ts(self, seeded_conn):
        mr.recommend(seeded_conn, "fix the test in foo.py")
        first = seeded_conn.execute(
            "SELECT last_used_ts FROM mode_recommendations"
        ).fetchone()["last_used_ts"]
        # Sleep would slow tests; instead, mutate created/last_used to a
        # known earlier value, then re-query and confirm last_used moved.
        seeded_conn.execute(
            "UPDATE mode_recommendations SET last_used_ts = '2020-01-01T00:00:00+00:00'"
        )
        mr.recommend(seeded_conn, "fix the test in foo.py")
        bumped = seeded_conn.execute(
            "SELECT last_used_ts FROM mode_recommendations"
        ).fetchone()["last_used_ts"]
        assert bumped != "2020-01-01T00:00:00+00:00"
        assert bumped >= first


# ── similarity filter ──────────────────────────────────────────────────────


class TestSimilarityFilter:
    def test_intent_mismatch_filtered_out(self, tmp_path):
        conn = _make_conn(tmp_path)
        pid = _seed_project(conn)
        # Seed lots of "build" sessions
        for i in range(10):
            _seed_session(conn, project_id=pid, session_id=f"b{i}",
                          primary_model="sonnet", cost_usd=0.10,
                          first_user_text="implement a new feature in foo.py")
        # Ask about a "fix" task — none should match.
        result = mr.recommend(conn, "fix the bug in foo.py")
        assert result["similar_session_count"] == 0
        assert result["confidence"] == 0.0

    def test_token_band_mismatch_filtered_out(self, tmp_path):
        conn = _make_conn(tmp_path)
        pid = _seed_project(conn)
        # Seed 'fix'/'tiny' band sessions
        for i in range(5):
            _seed_session(conn, project_id=pid, session_id=f"t{i}",
                          primary_model="sonnet", cost_usd=0.10,
                          first_user_text="fix the bug")
        # Ask with a med-band prompt: ~3000 chars / 4 ≈ 750 tokens? Wait,
        # 'med' band is 800-3000 tokens i.e. >=3200 chars. Use 4000 chars.
        big_prompt = "fix the bug. " + ("Context: " + "x " * 50 + ". ") * 30
        result = mr.recommend(conn, big_prompt)
        # The big prompt has band 'med', seeded sessions are 'tiny' →
        # zero matches.
        assert result["similar_session_count"] == 0


# ── recommend() service entry point ─────────────────────────────────────────


class TestRecommendServiceEntryPoint:
    """``mode_recommender.recommend`` is the single service entry point
    behind the recommendation surface. A thin wrapper used to sit in front
    of it (it was retired with the standalone MCP server), so these
    exercise the service directly — the payload it returns and how an
    empty prompt is handled at the service layer.
    """

    def test_recommend_round_trips_full_payload(self, tmp_path):
        conn = _make_conn(tmp_path)
        pid = _seed_project(conn)
        for sid, model, cost in [
            ("a", "sonnet", 0.05), ("b", "sonnet", 0.05),
            ("c", "sonnet", 0.05), ("d", "opus", 0.50),
        ]:
            _seed_session(conn, project_id=pid, session_id=sid,
                          primary_model=model, cost_usd=cost,
                          first_user_text="fix the failing test in foo.py")
        result = mr.recommend(
            conn, "fix the broken test in bar.py", current_model="opus",
        )
        assert result["recommended_model"] == "sonnet"
        assert result["confidence"] > 0.0
        assert result["cost_delta_usd"] > 0.0
        assert "features" in result

    def test_empty_prompt_rejected_at_the_service_layer(self, tmp_path):
        # ``recommend`` itself tolerates any prompt — it never raises on
        # empty data (see TestEmptyStore). The empty/whitespace-prompt
        # *rejection* lives in the meta-agent executor, the service-layer
        # caller of recommend(); that is where this contract now sits.
        from stackunderflow.services.meta_agent import execute_tool

        conn = _make_conn(tmp_path)
        result = execute_tool(conn, "recommend_mode", {"prompt": "   "})
        assert result.ok is False
        assert "error" in result.data


# ── meta-agent dispatcher ───────────────────────────────────────────────────


class TestMetaAgentDispatcher:
    def test_recommend_mode_in_tool_catalog(self):
        from stackunderflow.services.meta_agent import TOOL_CATALOG, tool_names

        names = tool_names()
        assert "recommend_mode" in names
        spec = next(t for t in TOOL_CATALOG
                    if t["function"]["name"] == "recommend_mode")
        assert "prompt" in spec["function"]["parameters"]["required"]

    def test_executor_returns_full_payload(self, tmp_path):
        from stackunderflow.services.meta_agent import execute_tool

        conn = _make_conn(tmp_path)
        result = execute_tool(conn, "recommend_mode",
                              {"prompt": "fix the broken test"})
        assert result.name == "recommend_mode"
        assert result.ok  # empty store is still a "valid" result
        assert "rationale" in result.data

    def test_executor_rejects_empty_prompt(self, tmp_path):
        from stackunderflow.services.meta_agent import execute_tool

        conn = _make_conn(tmp_path)
        result = execute_tool(conn, "recommend_mode", {"prompt": ""})
        assert result.ok is False
        assert "error" in result.data
