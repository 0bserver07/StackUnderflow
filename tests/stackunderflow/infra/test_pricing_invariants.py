"""Pricing CI invariants — the gates that keep cost numbers honest.

Three store-level contracts, locked here so a regression in a mart
builder, a normalizer, or the rate card fails CI instead of silently
shipping wrong dollars:

(a) **No materialization drift.** ``SUM`` over the marts equals ``SUM``
    over ``usage_events`` for cost AND every token column. ``daily_mart``
    and ``model_day_mart`` are full rollups of the fact table; if a mart
    builder drops or double-counts rows, the sums diverge.

(b) **Nothing silently unpriced.** Every model carrying a billable
    ``cost_source`` (``rate_card`` / ``live``) resolves to a rate card
    entry. A billable row against an unresolvable model means the
    normalizer priced something it couldn't actually price.

(c) **``unknown`` ⇒ $0.0.** No row pairs ``cost_source='unknown'`` with a
    nonzero ``cost_usd`` (``docs/specs/session-schema-v1.md``). This is the
    contract commit d2d4eb9 fixed in ``etl/normalize/base._compute_cost_usd``
    — locked here at the normalizer layer (the file that fix touched) and
    at the store layer.

Plus unit coverage for the read-only introspection helpers the
``pricing doctor`` surface relies on.
"""

from __future__ import annotations

import itertools
import json
from datetime import UTC, datetime, timedelta

import pytest

from stackunderflow.etl.marts.daily import DailyMartBuilder
from stackunderflow.etl.marts.model_day import ModelDayMartBuilder
from stackunderflow.etl.normalize.claude import ClaudeNormalizer
from stackunderflow.infra.costs import estimate_cost, is_rate_card_model
from stackunderflow.routes.pricing import assemble_pricing_health
from stackunderflow.services.pricing_service import PricingService
from stackunderflow.store import db, schema
from tests.conftest import set_home_env

# Globally increasing seq so the ``UNIQUE(session_fk, seq)`` index is never
# tripped within a single store (each test uses its own fresh DB).
_SEQ = itertools.count()


# ── seeding helpers ───────────────────────────────────────────────────────────


def _new_store(tmp_path):
    conn = db.connect(tmp_path / "store.db")
    schema.apply(conn)
    return conn


def _seed_project_session(conn, *, provider="claude", slug="-a"):
    pid = int(
        conn.execute(
            "INSERT INTO projects (provider, slug, display_name, first_seen, last_modified) "
            "VALUES (?, ?, ?, 0.0, 0.0)",
            (provider, slug, slug),
        ).lastrowid
    )
    sfk = int(
        conn.execute(
            "INSERT INTO sessions (project_id, session_id, first_ts, last_ts, message_count) "
            "VALUES (?, 's1', '2026-04-01T00:00:00Z', '2026-04-01T00:00:00Z', 1)",
            (pid,),
        ).lastrowid
    )
    return pid, sfk


def _insert_event(
    conn,
    *,
    project_id,
    session_fk,
    model,
    cost_usd,
    cost_source="rate_card",
    provider="claude",
    input_tokens=0,
    output_tokens=0,
    cache_read=0,
    cache_create=0,
    day="2026-04-01",
    speed="standard",
    session_id="s1",
):
    """Insert a backing message (FK target) + its usage_events row."""
    seq = next(_SEQ)
    ts = f"{day}T00:00:00Z"
    conn.execute(
        "INSERT INTO messages "
        "(session_fk, seq, timestamp, role, model, "
        " input_tokens, output_tokens, cache_create_tokens, cache_read_tokens, "
        " content_text, tools_json, raw_json, is_sidechain) "
        "VALUES (?, ?, ?, 'assistant', ?, ?, ?, ?, ?, '', '[]', '{}', 0)",
        (session_fk, seq, ts, model, input_tokens, output_tokens, cache_create, cache_read),
    )
    mid = int(
        conn.execute(
            "SELECT next_id - 1 FROM _messages_id_seq WHERE rowid_kind = 1"
        ).fetchone()[0]
    )
    conn.execute(
        "INSERT INTO usage_events "
        "(source_message_fk, provider, account, project_id, session_id, ts, day, "
        " model, speed, input_tokens, output_tokens, cache_read_tokens, "
        " cache_create_tokens, cost_usd, cost_source, role) "
        "VALUES (?, ?, 'default', ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 'assistant')",
        (
            mid, provider, project_id, session_id, ts, day, model, speed,
            input_tokens, output_tokens, cache_read, cache_create, cost_usd, cost_source,
        ),
    )


def _sum_row(conn, table, *, cache_read_col, cache_create_col):
    return conn.execute(
        f"SELECT COALESCE(SUM(cost_usd), 0.0) AS c, "
        f"       COALESCE(SUM(input_tokens), 0) AS i, "
        f"       COALESCE(SUM(output_tokens), 0) AS o, "
        f"       COALESCE(SUM({cache_read_col}), 0) AS cr, "
        f"       COALESCE(SUM({cache_create_col}), 0) AS cc "
        f"FROM {table}"  # noqa: S608 — table/col names are test-controlled literals
    ).fetchone()


# ── (a) no materialization drift ──────────────────────────────────────────────


class TestMartSumInvariant:
    def test_daily_and_model_day_marts_sum_to_usage_events(self, tmp_path):
        conn = _new_store(tmp_path)
        pid, sfk = _seed_project_session(conn)
        # Varied across day / model / speed / cost_source so the rollup
        # actually fans into multiple mart keys.
        _insert_event(
            conn, project_id=pid, session_fk=sfk, model="claude-opus-4-8",
            cost_usd=1.2345, input_tokens=1000, output_tokens=500,
            cache_read=200, cache_create=50, day="2026-04-01", speed="fast",
        )
        _insert_event(
            conn, project_id=pid, session_fk=sfk, model="claude-sonnet-4-5-20250929",
            cost_usd=0.5, input_tokens=300, output_tokens=100, day="2026-04-02",
        )
        _insert_event(
            conn, project_id=pid, session_fk=sfk, model="claude-opus-4-8",
            cost_usd=0.75, input_tokens=400, output_tokens=900,
            cache_read=10, cache_create=5, day="2026-04-02",
        )
        # An unknown-model row (cost 0) still carries tokens — it must be
        # included in the token sums.
        _insert_event(
            conn, project_id=pid, session_fk=sfk, model="exotic-model-x",
            cost_usd=0.0, cost_source="unknown", input_tokens=999, output_tokens=1,
            day="2026-04-02",
        )
        conn.commit()

        DailyMartBuilder().rebuild_from_scratch(conn)
        ModelDayMartBuilder().rebuild_from_scratch(conn)

        ev = _sum_row(
            conn, "usage_events",
            cache_read_col="cache_read_tokens", cache_create_col="cache_create_tokens",
        )
        for table in ("daily_mart", "model_day_mart"):
            m = _sum_row(
                conn, table, cache_read_col="cache_read", cache_create_col="cache_create"
            )
            assert m["c"] == pytest.approx(ev["c"]), f"{table} cost drift"
            assert m["i"] == ev["i"], f"{table} input_tokens drift"
            assert m["o"] == ev["o"], f"{table} output_tokens drift"
            assert m["cr"] == ev["cr"], f"{table} cache_read drift"
            assert m["cc"] == ev["cc"], f"{table} cache_create drift"
        conn.close()

    def test_empty_store_marts_match_trivially(self, tmp_path):
        conn = _new_store(tmp_path)
        DailyMartBuilder().rebuild_from_scratch(conn)
        ModelDayMartBuilder().rebuild_from_scratch(conn)
        ev = _sum_row(
            conn, "usage_events",
            cache_read_col="cache_read_tokens", cache_create_col="cache_create_tokens",
        )
        assert ev["c"] == 0.0 and ev["i"] == 0
        for table in ("daily_mart", "model_day_mart"):
            m = _sum_row(
                conn, table, cache_read_col="cache_read", cache_create_col="cache_create"
            )
            assert m["c"] == 0.0 and m["i"] == 0
        conn.close()


# ── (b) nothing silently unpriced ─────────────────────────────────────────────


class TestBillableModelsResolvable:
    def test_every_billable_model_has_a_resolvable_rate(self, tmp_path):
        conn = _new_store(tmp_path)
        pid, sfk = _seed_project_session(conn)
        _insert_event(
            conn, project_id=pid, session_fk=sfk, model="claude-opus-4-8",
            cost_usd=1.0, cost_source="rate_card",
        )
        _insert_event(
            conn, project_id=pid, session_fk=sfk, model="gpt-5-codex",
            cost_usd=0.3, cost_source="rate_card", provider="codex",
        )
        # An unknown-source exotic model is fine — it's not "billable".
        _insert_event(
            conn, project_id=pid, session_fk=sfk, model="exotic-model-x",
            cost_usd=0.0, cost_source="unknown",
        )
        conn.commit()

        billable_models = [
            r["model"]
            for r in conn.execute(
                "SELECT DISTINCT model FROM usage_events "
                "WHERE cost_source IN ('rate_card', 'live') AND model <> ''"
            )
        ]
        unresolved = [m for m in billable_models if not is_rate_card_model(m)]
        assert unresolved == [], f"billable models with no resolvable rate: {unresolved}"
        conn.close()

    def test_doctor_flags_a_billable_unpriced_model(self, tmp_path, monkeypatch):
        """The negative case: a ``rate_card`` row against a model the rate
        card doesn't know is a defect — the doctor must surface it and flip
        ``ok`` to False."""
        set_home_env(monkeypatch, tmp_path / "home")
        conn = _new_store(tmp_path)
        pid, sfk = _seed_project_session(conn)
        _insert_event(
            conn, project_id=pid, session_fk=sfk, model="bogus-priced-model",
            cost_usd=2.5, cost_source="rate_card", input_tokens=1000, output_tokens=500,
        )
        conn.commit()

        payload = assemble_pricing_health(conn)
        assert payload["ok"] is False
        assert payload["summary"]["billable_unpriced_model_count"] == 1
        flagged = [u for u in payload["unpriced_models"] if u["billable"]]
        assert len(flagged) == 1
        assert flagged[0]["model"] == "bogus-priced-model"
        conn.close()


# ── (c) unknown ⇒ $0.0 (d2d4eb9 regression lock) ──────────────────────────────


class TestUnknownCostContract:
    def test_store_has_no_unknown_row_with_nonzero_cost(self, tmp_path):
        conn = _new_store(tmp_path)
        pid, sfk = _seed_project_session(conn)
        _insert_event(
            conn, project_id=pid, session_fk=sfk, model="claude-opus-4-8",
            cost_usd=1.0, cost_source="rate_card",
        )
        _insert_event(
            conn, project_id=pid, session_fk=sfk, model="exotic-model-x",
            cost_usd=0.0, cost_source="unknown", input_tokens=500, output_tokens=100,
        )
        conn.commit()
        n = conn.execute(
            "SELECT COUNT(*) AS n FROM usage_events "
            "WHERE cost_source = 'unknown' AND cost_usd <> 0"
        ).fetchone()["n"]
        assert n == 0
        conn.close()

    def test_compute_cost_usd_honors_unknown_flag_even_for_known_model(self):
        """The exact d2d4eb9 fix: ``_compute_cost_usd`` returns 0.0 when
        ``cost_source='unknown'`` regardless of the model — without this
        the Anthropic family fallback would price it with phantom dollars.
        """
        n = ClaudeNormalizer()
        unknown_cost = n._compute_cost_usd(
            input_tokens=1000, output_tokens=500, cache_read_tokens=0,
            cache_create_tokens=0, model="claude-opus-4-8", speed="standard",
            cost_source="unknown", at_ts="2026-04-01T00:00:00Z",
        )
        assert unknown_cost == 0.0
        # Control: identical tokens priced as rate_card are nonzero, proving
        # the zero above is the flag's doing, not zero tokens.
        priced = n._compute_cost_usd(
            input_tokens=1000, output_tokens=500, cache_read_tokens=0,
            cache_create_tokens=0, model="claude-opus-4-8", speed="standard",
            cost_source="rate_card", at_ts="2026-04-01T00:00:00Z",
        )
        assert priced > 0.0

    def test_claude_normalizer_stamps_unknown_and_zero_for_unmapped_model(self):
        events = list(
            ClaudeNormalizer().normalize(
                {
                    "id": 1, "role": "assistant",
                    "model": "totally-unknown-model-xyz",
                    "input_tokens": 1000, "output_tokens": 500,
                    "timestamp": "2026-04-01T00:00:00Z",
                    "project_id": 1, "session_id": "s1", "provider": "claude",
                }
            )
        )
        assert len(events) == 1
        assert events[0]["cost_source"] == "unknown"
        assert events[0]["cost_usd"] == 0.0

    def test_claude_normalizer_prices_a_known_model(self):
        events = list(
            ClaudeNormalizer().normalize(
                {
                    "id": 1, "role": "assistant", "model": "claude-opus-4-8",
                    "input_tokens": 1000, "output_tokens": 500,
                    "timestamp": "2026-04-01T00:00:00Z",
                    "project_id": 1, "session_id": "s1", "provider": "claude",
                }
            )
        )
        assert events[0]["cost_source"] == "rate_card"
        assert events[0]["cost_usd"] > 0.0

    def test_doctor_detects_an_unknown_nonzero_cost_violation(self, tmp_path, monkeypatch):
        """Hand-write the contract violation the normalizer can't produce and
        confirm the doctor catches it (the store-level half of the gate)."""
        set_home_env(monkeypatch, tmp_path / "home")
        conn = _new_store(tmp_path)
        pid, sfk = _seed_project_session(conn)
        _insert_event(
            conn, project_id=pid, session_fk=sfk, model="exotic-model-x",
            cost_usd=4.0, cost_source="unknown", input_tokens=1000,
        )
        conn.commit()
        payload = assemble_pricing_health(conn)
        assert payload["summary"]["unknown_nonzero_cost_rows"] == 1
        assert payload["ok"] is False
        conn.close()


# ── introspection helpers ─────────────────────────────────────────────────────


class TestIntrospectionHelpers:
    def test_is_rate_card_model_exact_membership(self):
        assert is_rate_card_model("claude-opus-4-8") is True
        assert is_rate_card_model("gpt-5-codex") is True
        assert is_rate_card_model("totally-made-up-zzz") is False
        assert is_rate_card_model("") is False

    def test_estimate_cost_prices_via_fallback_for_unknown_claude_id(self):
        # Anthropic fallback prices an unrecognised claude-shape id, so the
        # exposure of an unpriced row is quantifiable (> 0).
        est = estimate_cost(
            {"input": 1_000_000, "output": 0, "cache_creation": 0, "cache_read": 0},
            "claude-made-up-model",
        )
        assert est > 0.0

    def test_estimate_cost_is_zero_for_empty_tokens(self):
        est = estimate_cost(
            {"input": 0, "output": 0, "cache_creation": 0, "cache_read": 0},
            "claude-opus-4-8",
        )
        assert est == 0.0

    def test_read_cache_status_no_cache(self, tmp_path, monkeypatch):
        set_home_env(monkeypatch, tmp_path / "home")
        status = PricingService.read_cache_status()
        assert status["source"] == "none"
        assert status["is_stale"] is True
        assert status["age_days"] is None
        assert status["model_count"] == 0

    def test_read_cache_status_fresh_cache(self, tmp_path, monkeypatch):
        set_home_env(monkeypatch, tmp_path / "home")
        cache_dir = tmp_path / "home" / ".stackunderflow" / "cache"
        cache_dir.mkdir(parents=True)
        (cache_dir / "pricing.json").write_text(
            json.dumps(
                {
                    "timestamp": datetime.now(UTC).isoformat(),
                    "source": "litellm",
                    "pricing": {"claude-opus-4-8": {"input_cost_per_token": 1e-6}},
                }
            )
        )
        status = PricingService.read_cache_status()
        assert status["source"] == "litellm"
        assert status["is_stale"] is False
        assert status["age_days"] is not None and status["age_days"] < 1
        assert status["model_count"] == 1

    def test_read_cache_status_stale_cache(self, tmp_path, monkeypatch):
        set_home_env(monkeypatch, tmp_path / "home")
        cache_dir = tmp_path / "home" / ".stackunderflow" / "cache"
        cache_dir.mkdir(parents=True)
        old_ts = (datetime.now(UTC) - timedelta(days=30)).isoformat()
        (cache_dir / "pricing.json").write_text(
            json.dumps({"timestamp": old_ts, "source": "cache", "pricing": {}})
        )
        status = PricingService.read_cache_status()
        assert status["is_stale"] is True
        assert status["age_days"] is not None and status["age_days"] > 7
