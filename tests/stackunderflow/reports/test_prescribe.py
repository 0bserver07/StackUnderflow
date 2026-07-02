"""Prescriptive cost — :mod:`stackunderflow.reports.prescribe`.

Two surfaces under test:

* ``generate_claudemd_preview`` — fixed synthetic CLAUDE.md text in, the
  EXACT slimmed text + unified diff + per-rule rationale + savings math out.
  Every dollar figure is asserted against a direct ``compute_cost`` call in
  the test (the same black box the module prices through) — nothing is
  hardcoded from a rate table and nothing is invented.

* ``build_routing_recommendations`` — deterministic ``usage_events``
  fixtures (incl. the v026 ``reasoning_tokens`` column and, where a case
  needs it, ``session_quality_metrics``) with an injected fake pricer so
  the expected candidate costs / deltas are exact by construction.

A source-scan test locks (a)'s never-writes guarantee: the module must not
import any filesystem API nor call any write primitive.
"""

from __future__ import annotations

import ast
import difflib
import inspect

import pytest

from stackunderflow.infra.costs import compute_cost
from stackunderflow.reports import prescribe
from stackunderflow.reports.optimize import WASTE_PRICING_MODEL
from stackunderflow.reports.prescribe import (
    build_routing_recommendations,
    generate_claudemd_preview,
)
from stackunderflow.reports.scope import Scope
from stackunderflow.store import db, schema

# ── shared fixture bits ──────────────────────────────────────────────────────

# 65 normalised chars — over the 60-char dedupe floor, and small enough that
# the "## Tail" section body stays under the 20-token extraction threshold.
_DUP = "Duplicated paragraph for the dedupe rule to catch, 65 chars long"

_LONG_BODY = "A" * 100  # 102-char section body → ~25 tokens ≥ threshold 20

_FIXTURE_MD = (
    "# Notes\n"
    "\n"
    "Keep this intro.\n"
    "\n"
    "<!-- secret author note -->\n"
    "\n"
    "## Long Part\n"
    "\n"
    f"{_LONG_BODY}\n"
    "\n"
    "## Tail\n"
    "\n"
    f"{_DUP}\n"
    "\n"
    f"{_DUP}\n"
)

# The slimmed text the fixture must produce, constructed by hand from the
# documented rules: comment dropped, second duplicate dropped, "Long Part"
# body swapped for the pointer, no 3+ blank runs to collapse.
_EXPECTED_SLIM = (
    "# Notes\n"
    "\n"
    "Keep this intro.\n"
    "\n"
    "\n"
    "## Long Part\n"
    "\n"
    "> Body moved to docs/claude-md/long-part.md (~25 tokens) — slimmed by "
    "StackUnderflow; move the original text there before adopting this file.\n"
    "\n"
    "## Tail\n"
    "\n"
    f"{_DUP}\n"
    "\n"
)


def _expected_diff(original: str, slimmed: str, label: str = "CLAUDE.md") -> str:
    return "".join(
        difflib.unified_diff(
            original.splitlines(keepends=True),
            slimmed.splitlines(keepends=True),
            fromfile=label,
            tofile=f"{label} (slim preview)",
        )
    )


# ═════════════════════════════════════════════════════════════════════════
# (a) CLAUDE.md slim preview
# ═════════════════════════════════════════════════════════════════════════


class TestClaudeMdPreviewExact:
    def _preview(self, **kw):
        kw.setdefault("section_token_threshold", 20)
        kw.setdefault("bloat_token_threshold", 10)
        return generate_claudemd_preview(_FIXTURE_MD, **kw)

    def test_fixture_preconditions(self):
        # The dedupe floor and section threshold the fixture is tuned for.
        assert len(_DUP) >= prescribe.DEDUPE_MIN_CHARS
        assert len(_LONG_BODY) == 100

    def test_exact_slimmed_text(self):
        p = self._preview()
        assert p["changed"] is True
        assert p["slimmed_text"] == _EXPECTED_SLIM

    def test_exact_unified_diff(self):
        p = self._preview()
        assert p["preview_diff"] == _expected_diff(_FIXTURE_MD, _EXPECTED_SLIM)
        # Sanity on the shape: the removed lines show as deletions.
        assert "-<!-- secret author note -->\n" in p["preview_diff"]
        assert f"-{_LONG_BODY}\n" in p["preview_diff"]
        assert "+> Body moved to docs/claude-md/long-part.md" in p["preview_diff"]

    def test_rationale_rules_in_order_and_sums(self):
        p = self._preview()
        rules = [r["rule"] for r in p["rationale"]]
        # No 3+ blank runs in the fixture → the collapse rule contributes
        # nothing and must NOT appear.
        assert rules == [
            "strip_html_comments",
            "dedupe_paragraphs",
            "extract_oversized_sections",
        ]
        # Per-rule token savings sum exactly to the headline number.
        assert sum(r["tokens_saved"] for r in p["rationale"]) == p["tokens_saved"]
        # Each rule carries its evidence.
        by_rule = {r["rule"]: r for r in p["rationale"]}
        assert by_rule["strip_html_comments"]["detail"]["removed_comments"] == 1
        assert by_rule["dedupe_paragraphs"]["detail"]["removed_duplicates"] == 1
        extracted = by_rule["extract_oversized_sections"]["detail"]["extracted_sections"]
        assert extracted == [
            {
                "heading": "Long Part",
                "suggested_path": "docs/claude-md/long-part.md",
                "body_tokens": 25,
            }
        ]

    def test_token_math_traces_to_text(self):
        p = self._preview()
        assert p["original_tokens"] == len(_FIXTURE_MD) // 4
        assert p["slimmed_tokens"] == len(_EXPECTED_SLIM) // 4
        assert p["tokens_saved"] == p["original_tokens"] - p["slimmed_tokens"]
        assert p["tokens_saved"] > 0

    def test_savings_priced_through_compute_cost(self):
        """The $ figures are the black-box price of tokens_saved — nothing else."""
        p = self._preview()
        expected_per_session = round(
            float(
                compute_cost(
                    {"input": p["tokens_saved"], "output": 0, "cache_creation": 0, "cache_read": 0},
                    WASTE_PRICING_MODEL,
                )["total_cost"]
            ),
            4,
        )
        assert p["estimated_savings_usd_per_session"] == expected_per_session
        assert p["estimated_savings_usd_monthly"] == round(
            expected_per_session * p["sessions_per_month"], 4
        )

    def test_sessions_per_month_scales_monthly(self):
        p100 = self._preview(sessions_per_month=100)
        p200 = self._preview(sessions_per_month=200)
        assert p200["sessions_per_month"] == 200
        assert p200["estimated_savings_usd_monthly"] == round(
            p100["estimated_savings_usd_per_session"] * 200, 4
        )


class TestClaudeMdPreviewEdges:
    def test_empty_text_clean_empty_result(self):
        p = generate_claudemd_preview("")
        assert p["changed"] is False
        assert p["preview_diff"] == ""
        assert p["slimmed_text"] == ""
        assert p["rationale"] == []
        assert p["tokens_saved"] == 0
        assert p["estimated_savings_usd_per_session"] == 0.0
        assert p["estimated_savings_usd_monthly"] == 0.0

    def test_clean_text_unchanged(self):
        text = "# Small\n\nNothing to trim here.\n"
        p = generate_claudemd_preview(text)
        assert p["changed"] is False
        assert p["preview_diff"] == ""
        assert p["rationale"] == []
        assert p["tokens_saved"] == 0

    def test_fenced_code_is_untouchable(self):
        """Comments/duplicates inside code fences must survive verbatim."""
        text = (
            "# Doc\n"
            "\n"
            "```html\n"
            "<!-- this is example code, not an author note -->\n"
            f"{_DUP}\n"
            f"{_DUP}\n"
            "```\n"
        )
        p = generate_claudemd_preview(text)
        assert p["changed"] is False

    def test_rule_titled_sections_never_extracted(self):
        text = (
            "## Hard Rules — Never Break These\n"
            "\n"
            f"{'B' * 200}\n"
        )
        p = generate_claudemd_preview(
            text, section_token_threshold=20, bloat_token_threshold=10
        )
        rules = [r["rule"] for r in p["rationale"]]
        assert "extract_oversized_sections" not in rules

    def test_extraction_gated_off_under_bloat_threshold(self):
        """No bloat flag + under threshold → the safe rules still run but
        sections stay inline."""
        text = f"## Big Section\n\n{'C' * 200}\n"
        # Text is ~52 tokens; default bloat threshold is 5000 → not bloated.
        p = generate_claudemd_preview(text, section_token_threshold=20)
        assert p["changed"] is False

    def test_bloat_finding_enables_extraction(self):
        """A passed bloated_claude_md finding force-enables extraction even
        when the text sits under the numeric threshold."""
        text = f"## Big Section\n\n{'C' * 200}\n"
        finding = {"pattern_id": "bloated_claude_md"}
        p = generate_claudemd_preview(
            text, findings=[finding], section_token_threshold=20
        )
        assert p["changed"] is True
        assert [r["rule"] for r in p["rationale"]] == ["extract_oversized_sections"]

    def test_blank_run_collapse(self):
        text = "# T\n\n\n\n\nBody after a 4-blank run.\n"
        p = generate_claudemd_preview(text)
        assert [r["rule"] for r in p["rationale"]] == ["collapse_blank_runs"]
        assert p["slimmed_text"] == "# T\n\nBody after a 4-blank run.\n"
        assert p["rationale"][0]["detail"]["blank_lines_removed"] == 3

    def test_parse_render_round_trip(self):
        """The block parser must be lossless on untouched documents."""
        assert prescribe._render(prescribe._parse_blocks(_FIXTURE_MD)) == _FIXTURE_MD
        gnarly = "no trailing newline\n```\nunclosed fence\n<!-- unclosed"
        assert prescribe._render(prescribe._parse_blocks(gnarly)) == gnarly


class TestNeverWritesGuarantee:
    """Mechanical enforcement of (a)'s NEVER-writes contract."""

    def test_module_imports_no_filesystem_api(self):
        tree = ast.parse(inspect.getsource(prescribe))
        banned = {"pathlib", "os", "shutil", "io", "tempfile"}
        for node in ast.walk(tree):
            if isinstance(node, ast.Import):
                for alias in node.names:
                    assert alias.name.split(".")[0] not in banned, (
                        f"prescribe.py imports {alias.name} — the module must "
                        "stay filesystem-free"
                    )
            elif isinstance(node, ast.ImportFrom):
                mod = (node.module or "").split(".")[0]
                assert mod not in banned, (
                    f"prescribe.py imports from {node.module} — the module "
                    "must stay filesystem-free"
                )

    def test_module_calls_no_write_primitive(self):
        tree = ast.parse(inspect.getsource(prescribe))
        banned_calls = {"open", "exec", "eval"}
        banned_attrs = {
            "write_text", "write_bytes", "write", "unlink", "remove",
            "rmdir", "mkdir", "makedirs", "rename", "replace", "touch",
        }
        for node in ast.walk(tree):
            if not isinstance(node, ast.Call):
                continue
            fn = node.func
            if isinstance(fn, ast.Name):
                assert fn.id not in banned_calls, f"prescribe.py calls {fn.id}()"
            elif isinstance(fn, ast.Attribute):
                assert fn.attr not in banned_attrs, f"prescribe.py calls .{fn.attr}()"

    def test_preview_api_takes_text_not_path(self):
        sig = inspect.signature(generate_claudemd_preview)
        assert "claude_md_text" in sig.parameters
        assert not any("path" in name for name in sig.parameters)


# ═════════════════════════════════════════════════════════════════════════
# (b) model-routing recommendations
# ═════════════════════════════════════════════════════════════════════════

# Injected pricer: cost = total tokens × rate(model) / 1M. Unknown model →
# KeyError → the module's defensive skip must drop the candidate.
_FAKE_RATES = {"cheap-model": 1.0, "mid-model": 3.0, "big-model": 5.0}


def _fake_cost(tokens, model, provider="anthropic", *, speed="standard"):
    rate = _FAKE_RATES[model]
    total = sum(int(v or 0) for v in tokens.values())
    return {"total_cost": total * rate / 1_000_000}


_CANDIDATES = (
    ("anthropic", "cheap-model", "Cheap"),
    ("anthropic", "big-model", "Big"),
)


def _fresh_store(tmp_path):
    conn = db.connect(tmp_path / "store.db")
    schema.apply(conn)
    return conn


def _add_project(conn, *, provider="claude", slug="demo") -> int:
    cur = conn.execute(
        "INSERT INTO projects (provider, slug, display_name, first_seen, last_modified) "
        "VALUES (?, ?, ?, 0, 0)",
        (provider, slug, slug),
    )
    return int(cur.lastrowid)


def _add_session(conn, project_id: int, session_id: str) -> int:
    cur = conn.execute(
        "INSERT INTO sessions (project_id, session_id) VALUES (?, ?)",
        (project_id, session_id),
    )
    return int(cur.lastrowid)


_MSG_SEQ = {"n": 0}


def _add_event(
    conn,
    *,
    project_id: int,
    session_fk: int,
    session_id: str,
    day: str,
    model: str = "mid-model",
    provider: str = "anthropic",
    input_tokens: int = 0,
    output_tokens: int = 0,
    reasoning_tokens: int = 0,
    cost_usd: float = 0.0,
) -> None:
    """One usage_events row + the messages row its FK requires."""
    _MSG_SEQ["n"] += 1
    msg_id = _MSG_SEQ["n"]
    conn.execute(
        "INSERT INTO messages (id, session_fk, seq, timestamp, role, tools_json, "
        " content_text, raw_json) VALUES (?, ?, ?, ?, 'assistant', '[]', '', '{}')",
        (msg_id, session_fk, msg_id, f"{day}T10:00:00Z"),
    )
    conn.execute(
        "INSERT INTO usage_events ("
        " source_message_fk, provider, project_id, session_id, ts, day, model, speed,"
        " input_tokens, output_tokens, cache_read_tokens, cache_create_tokens,"
        " reasoning_tokens, cost_usd, cost_source, role"
        ") VALUES (?, ?, ?, ?, ?, ?, ?, 'standard', ?, ?, 0, 0, ?, ?, 'rate_card', 'assistant')",
        (
            msg_id, provider, project_id, session_id, f"{day}T10:00:00Z", day,
            model, input_tokens, output_tokens, reasoning_tokens, cost_usd,
        ),
    )
    conn.commit()


def _grade_session(conn, session_id: str, score: float) -> None:
    conn.execute(
        "INSERT INTO session_quality_metrics "
        "(session_id, overall_score, grades_json, rationale, suggestions_json, graded_at) "
        "VALUES (?, ?, '{}', 'fixture', '[]', '2026-06-02T00:00:00Z')",
        (session_id, score),
    )
    conn.commit()


def _build(conn, **kw):
    kw.setdefault("candidates", _CANDIDATES)
    kw.setdefault("compute_cost", _fake_cost)
    return build_routing_recommendations(conn, **kw)


class TestRoutingRecommendations:
    def test_downshift_low_reasoning_exact_math(self, tmp_path):
        conn = _fresh_store(tmp_path)
        pid = _add_project(conn)
        sfk = _add_session(conn, pid, "s1")
        # mid-model over 2 active days: 1M input + 200K output, 5K reasoning
        # (share 0.025 < 0.05 → low-reasoning), stored cost $6 total.
        _add_event(conn, project_id=pid, session_fk=sfk, session_id="s1",
                   day="2026-06-01", input_tokens=600_000, output_tokens=100_000,
                   reasoning_tokens=3_000, cost_usd=3.5)
        _add_event(conn, project_id=pid, session_fk=sfk, session_id="s1",
                   day="2026-06-02", input_tokens=400_000, output_tokens=100_000,
                   reasoning_tokens=2_000, cost_usd=2.5)

        out = _build(conn)
        conn.close()

        assert out["observed_days"] == 2
        assert out["monthly_factor"] == 15.0  # 30 / 2

        assert len(out["recommendations"]) == 1
        rec = out["recommendations"][0]
        assert rec["rec_id"] == "downshift_low_reasoning"
        assert rec["work_type"] == "low-reasoning"
        assert rec["from_model"] == "mid-model"
        assert rec["to_model"] == "cheap-model"
        assert rec["to_label"] == "Cheap"
        # Exact dollars, all traceable: actual = 3.5 + 2.5 stored; candidate
        # = 1.2M tokens × $1/M (fake pricer); big-model reprices to 6.0 — not
        # cheaper than actual, so cheap-model is the pick.
        assert rec["window_cost_usd"] == 6.0
        assert rec["candidate_window_cost_usd"] == 1.2
        assert rec["window_delta_usd"] == pytest.approx(-4.8)
        assert rec["estimated_monthly_delta_usd"] == pytest.approx(-72.0)  # −4.8 × 15
        assert rec["evidence"]["reasoning_share"] == pytest.approx(0.025)
        assert rec["evidence"]["reasoning_tokens"] == 5_000
        assert rec["evidence"]["events"] == 2
        assert rec["caveats"] == []

        # The evidence table carries the model row too.
        assert len(out["models"]) == 1
        row = out["models"][0]
        assert row["work_type"] == "low-reasoning"
        assert row["reasoning_attributed"] is True
        assert row["window_cost_usd"] == 6.0
        assert out["caveats"]  # global caveats present when models exist

    def test_downshift_short_output_unattributed_has_caveat(self, tmp_path):
        conn = _fresh_store(tmp_path)
        pid = _add_project(conn)
        sfk = _add_session(conn, pid, "s1")
        # 10 events, zero reasoning attribution, 200 output tokens/event.
        for i in range(10):
            _add_event(conn, project_id=pid, session_fk=sfk, session_id="s1",
                       day=f"2026-06-{i + 1:02d}", input_tokens=10_000,
                       output_tokens=200, reasoning_tokens=0, cost_usd=0.1)

        out = _build(conn)
        conn.close()

        assert len(out["recommendations"]) == 1
        rec = out["recommendations"][0]
        assert rec["rec_id"] == "downshift_short_output"
        assert rec["work_type"] == "short-output (reasoning unattributed)"
        # actual = 10 × $0.1 = $1.0; cheap = 102K tokens × $1/M = $0.102.
        assert rec["window_cost_usd"] == 1.0
        assert rec["candidate_window_cost_usd"] == pytest.approx(0.102)
        assert rec["estimated_monthly_delta_usd"] == pytest.approx(
            (0.102 - 1.0) * 3.0
        )  # 10 active days → factor 3
        assert any("attribution is unavailable" in c for c in rec["caveats"])

    def test_upshift_needs_quality_evidence(self, tmp_path):
        conn = _fresh_store(tmp_path)
        pid = _add_project(conn)
        sfk = _add_session(conn, pid, "s1")
        # Reasoning-heavy (share 0.5) with a poor grade → upshift to the
        # next-more-expensive candidate (big-model at $6 vs actual $3.6).
        _add_event(conn, project_id=pid, session_fk=sfk, session_id="s1",
                   day="2026-06-01", input_tokens=1_000_000, output_tokens=200_000,
                   reasoning_tokens=100_000, cost_usd=3.6)
        _grade_session(conn, "s1", 2.0)

        out = _build(conn)
        conn.close()

        assert len(out["recommendations"]) == 1
        rec = out["recommendations"][0]
        assert rec["rec_id"] == "upshift_reasoning_quality"
        assert rec["work_type"] == "reasoning-heavy"
        assert rec["to_model"] == "big-model"
        assert rec["window_delta_usd"] == pytest.approx(2.4)  # 6.0 − 3.6, positive
        assert rec["estimated_monthly_delta_usd"] == pytest.approx(72.0)  # × 30
        assert rec["evidence"]["avg_quality_score"] == 2.0
        assert rec["evidence"]["graded_sessions"] == 1

    def test_no_quality_rows_no_upshift_and_null_scores(self, tmp_path):
        """session_quality_metrics empty → no upshift, quality fields null."""
        conn = _fresh_store(tmp_path)
        pid = _add_project(conn)
        sfk = _add_session(conn, pid, "s1")
        _add_event(conn, project_id=pid, session_fk=sfk, session_id="s1",
                   day="2026-06-01", input_tokens=1_000_000, output_tokens=200_000,
                   reasoning_tokens=100_000, cost_usd=3.6)

        out = _build(conn)
        conn.close()

        assert out["recommendations"] == []
        assert out["models"][0]["avg_quality_score"] is None
        assert out["models"][0]["graded_sessions"] == 0

    def test_empty_store_clean_empty_payload(self, tmp_path):
        conn = _fresh_store(tmp_path)
        out = _build(conn)
        conn.close()
        assert out == {
            "recommendations": [],
            "models": [],
            "observed_days": 0,
            "monthly_factor": None,
            "caveats": [],
        }

    def test_project_filter_scopes_and_empty_filter_matches_nothing(self, tmp_path):
        conn = _fresh_store(tmp_path)
        p1 = _add_project(conn, slug="one")
        p2 = _add_project(conn, slug="two")
        s1 = _add_session(conn, p1, "s1")
        s2 = _add_session(conn, p2, "s2")
        _add_event(conn, project_id=p1, session_fk=s1, session_id="s1",
                   day="2026-06-01", model="mid-model", input_tokens=1_000_000,
                   output_tokens=100_000, reasoning_tokens=1_000, cost_usd=3.3)
        _add_event(conn, project_id=p2, session_fk=s2, session_id="s2",
                   day="2026-06-01", model="big-model", input_tokens=1_000,
                   output_tokens=100, cost_usd=0.01)

        scoped = _build(conn, project_ids=[p1])
        assert [m["model"] for m in scoped["models"]] == ["mid-model"]

        nothing = _build(conn, project_ids=[])
        assert nothing["recommendations"] == []
        assert nothing["models"] == []
        conn.close()

    def test_scope_window_excludes_out_of_range_events(self, tmp_path):
        conn = _fresh_store(tmp_path)
        pid = _add_project(conn)
        sfk = _add_session(conn, pid, "s1")
        _add_event(conn, project_id=pid, session_fk=sfk, session_id="s1",
                   day="2026-06-01", input_tokens=1_000_000, output_tokens=100_000,
                   reasoning_tokens=1_000, cost_usd=3.3)
        _add_event(conn, project_id=pid, session_fk=sfk, session_id="s1",
                   day="2020-01-01", input_tokens=9_000_000, output_tokens=900_000,
                   reasoning_tokens=9_000, cost_usd=99.0)

        scope = Scope(since="2026-06-01T00:00:00Z", until=None, label="test")
        out = _build(conn, scope=scope)
        conn.close()
        assert out["observed_days"] == 1
        assert out["models"][0]["window_cost_usd"] == 3.3

    def test_zero_cost_model_makes_no_dollar_claim(self, tmp_path):
        """cost_usd = 0 in window → evidence row, but NO recommendation."""
        conn = _fresh_store(tmp_path)
        pid = _add_project(conn)
        sfk = _add_session(conn, pid, "s1")
        _add_event(conn, project_id=pid, session_fk=sfk, session_id="s1",
                   day="2026-06-01", input_tokens=1_000_000, output_tokens=100_000,
                   reasoning_tokens=1_000, cost_usd=0.0)
        out = _build(conn)
        conn.close()
        assert len(out["models"]) == 1
        assert out["recommendations"] == []

    def test_same_model_candidate_skipped_date_suffix_aware(self, tmp_path):
        conn = _fresh_store(tmp_path)
        pid = _add_project(conn)
        sfk = _add_session(conn, pid, "s1")
        _add_event(conn, project_id=pid, session_fk=sfk, session_id="s1",
                   day="2026-06-01", model="cheap-model", input_tokens=1_000_000,
                   output_tokens=100_000, reasoning_tokens=1_000, cost_usd=1.1)
        # The only candidate is the dated alias of the model already in use.
        out = _build(
            conn, candidates=(("anthropic", "cheap-model-20250101", "Cheap dated"),)
        )
        conn.close()
        assert out["recommendations"] == []

    def test_unpriceable_candidate_skipped_not_fabricated(self, tmp_path):
        """A candidate the pricer can't resolve must be dropped, never $0."""
        conn = _fresh_store(tmp_path)
        pid = _add_project(conn)
        sfk = _add_session(conn, pid, "s1")
        _add_event(conn, project_id=pid, session_fk=sfk, session_id="s1",
                   day="2026-06-01", input_tokens=1_000_000, output_tokens=100_000,
                   reasoning_tokens=1_000, cost_usd=3.3)
        out = _build(
            conn,
            candidates=(("anthropic", "model-not-in-rates", "Ghost"),),
        )
        conn.close()
        assert out["recommendations"] == []

    def test_cross_provider_candidates_ignored(self, tmp_path):
        """Routing stays within the provider the workload already runs on."""
        conn = _fresh_store(tmp_path)
        pid = _add_project(conn)
        sfk = _add_session(conn, pid, "s1")
        _add_event(conn, project_id=pid, session_fk=sfk, session_id="s1",
                   day="2026-06-01", provider="openai", model="mid-model",
                   input_tokens=1_000_000, output_tokens=100_000,
                   reasoning_tokens=1_000, cost_usd=3.3)
        # Candidates are all anthropic → nothing applies.
        out = _build(conn)
        conn.close()
        assert out["recommendations"] == []
