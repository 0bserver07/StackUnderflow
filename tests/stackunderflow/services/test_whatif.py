"""Unit tests for ``stackunderflow.services.whatif``.

Covers:
* ``TokenTotals`` aggregation + the ``compute_cost`` token-shape mapping.
* ``reprice`` — every candidate priced, sorted cheapest-first, deltas signed.
* ``build_whatif`` — the full payload shape + ``cheapest`` pointer.
* The repricing is a black-box delegation to ``compute_cost`` (verified by
  swapping the candidate set and checking the numbers track the real rates).
* Zero-workload + zero-actual-cost edge cases (delta_pct is None).
"""

from __future__ import annotations

import pytest

from stackunderflow.infra.costs import compute_cost
from stackunderflow.services.whatif import (
    CANDIDATES,
    TokenTotals,
    build_whatif,
    reprice,
)


# ── TokenTotals ──────────────────────────────────────────────────────────────


class TestTokenTotals:
    def test_total_sums_all_four(self):
        t = TokenTotals(input=1, output=2, cache_read=4, cache_create=8)
        assert t.total == 15

    def test_cost_token_shape_uses_cache_creation_key(self):
        t = TokenTotals(input=1, output=2, cache_read=4, cache_create=8)
        shape = t.as_cost_tokens()
        assert shape == {
            "input": 1,
            "output": 2,
            "cache_creation": 8,  # note the key rename for compute_cost
            "cache_read": 4,
        }


# ── reprice ──────────────────────────────────────────────────────────────────


class TestReprice:
    def test_every_candidate_priced(self):
        totals = TokenTotals(input=1_000_000, output=1_000_000)
        rows = reprice(totals, actual_cost_usd=10.0)
        assert len(rows) == len(CANDIDATES)
        # All candidates resolve to a real positive rate.
        assert all(r["cost_usd"] > 0 for r in rows)

    def test_sorted_cheapest_first(self):
        totals = TokenTotals(input=1_000_000, output=1_000_000)
        rows = reprice(totals, actual_cost_usd=10.0)
        costs = [r["cost_usd"] for r in rows]
        assert costs == sorted(costs)

    def test_delta_sign(self):
        """delta_usd is negative when the candidate is cheaper than actual."""
        totals = TokenTotals(input=1_000_000, output=1_000_000)
        actual = 15.0
        rows = reprice(totals, actual_cost_usd=actual)
        for r in rows:
            assert r["delta_usd"] == pytest.approx(r["cost_usd"] - actual)
            if r["cost_usd"] < actual:
                assert r["delta_usd"] < 0

    def test_delta_pct_none_when_no_actual_spend(self):
        totals = TokenTotals(input=1_000_000, output=1_000_000)
        rows = reprice(totals, actual_cost_usd=0.0)
        assert all(r["delta_pct"] is None for r in rows)

    def test_delta_pct_computed_with_actual_spend(self):
        totals = TokenTotals(input=1_000_000, output=1_000_000)
        actual = 10.0
        rows = reprice(totals, actual_cost_usd=actual)
        for r in rows:
            assert r["delta_pct"] == pytest.approx(
                (r["cost_usd"] - actual) / actual * 100.0
            )

    def test_black_box_matches_compute_cost(self):
        """Each row's cost equals ``compute_cost`` on the same token shape.

        This is the contract: the service reprices via ``compute_cost`` as a
        black box and must not transform the number.
        """
        totals = TokenTotals(
            input=500_000, output=250_000, cache_read=1_000_000, cache_create=50_000
        )
        rows = reprice(totals, actual_cost_usd=0.0)
        shape = totals.as_cost_tokens()
        by_model = {(r["provider"], r["model"]): r for r in rows}
        for provider, model, _label in CANDIDATES:
            expected = compute_cost(shape, model, provider=provider)["total_cost"]
            assert by_model[(provider, model)]["cost_usd"] == pytest.approx(expected)

    def test_custom_candidate_set(self):
        totals = TokenTotals(input=1_000_000, output=0)
        custom = (("anthropic", "claude-opus-4-8", "Opus"),)
        rows = reprice(totals, actual_cost_usd=0.0, candidates=custom)
        assert len(rows) == 1
        assert rows[0]["label"] == "Opus"
        assert rows[0]["cost_usd"] > 0

    def test_unknown_candidate_does_not_crash(self):
        """A candidate the rate tables can't resolve still yields a row."""
        totals = TokenTotals(input=1_000_000, output=1_000_000)
        custom = (("anthropic", "totally-made-up-model-xyz", "Bogus"),)
        rows = reprice(totals, actual_cost_usd=0.0, candidates=custom)
        assert len(rows) == 1
        # Cost is whatever the fallback family prices it at (>= 0), never raises.
        assert rows[0]["cost_usd"] >= 0.0


# ── build_whatif ─────────────────────────────────────────────────────────────


class TestBuildWhatif:
    def test_payload_shape(self):
        totals = TokenTotals(input=1_000_000, output=500_000)
        out = build_whatif(
            totals, actual_cost_usd=12.0, actual_models=["claude-opus-4-8"]
        )
        assert out["tokens"] == {
            "input": 1_000_000,
            "output": 500_000,
            "cache_read": 0,
            "cache_create": 0,
            "total": 1_500_000,
        }
        assert out["actual"]["cost_usd"] == 12.0
        assert out["actual"]["models"] == ["claude-opus-4-8"]
        assert len(out["candidates"]) == len(CANDIDATES)
        # cheapest is the first candidate.
        assert out["cheapest"] == out["candidates"][0]
        assert out["cheapest"]["cost_usd"] == min(
            r["cost_usd"] for r in out["candidates"]
        )

    def test_empty_candidate_set_yields_null_cheapest(self):
        out = build_whatif(TokenTotals(), actual_cost_usd=0.0, candidates=())
        assert out["candidates"] == []
        assert out["cheapest"] is None

    def test_models_sorted(self):
        out = build_whatif(
            TokenTotals(input=10),
            actual_cost_usd=1.0,
            actual_models=["zebra", "alpha", "mid"],
        )
        assert out["actual"]["models"] == ["alpha", "mid", "zebra"]

    def test_zero_workload(self):
        out = build_whatif(TokenTotals(), actual_cost_usd=0.0)
        assert out["tokens"]["total"] == 0
        # Every candidate prices a zero workload at $0.
        assert all(r["cost_usd"] == 0.0 for r in out["candidates"])
