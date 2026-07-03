"""Credibility tests for ``services.benchmark_stats`` (spec 26 §9).

These gate merge: Wilson vs hand-computed textbook values, seeded-bootstrap
reproducibility, BH-FDR against a known vector, and stratified standardization
vs a worked Simpson's-paradox fixture (pooled says A wins, standardized
correctly says B). If any of these drift, the benchmark's honesty guarantee is
broken.
"""

from __future__ import annotations

import statistics

import pytest

from stackunderflow.services import benchmark_stats as bs

# ── Wilson score interval vs textbook ────────────────────────────────────────


class TestWilson:
    def test_eight_of_ten_at_95pct(self):
        # Textbook Wilson 95% interval for 8/10 ≈ (0.4902, 0.9433).
        lo, hi = bs.wilson_interval(8, 10, ci_level=0.95)
        assert lo == pytest.approx(0.4902, abs=0.001)
        assert hi == pytest.approx(0.9433, abs=0.001)

    def test_zero_of_ten_at_95pct(self):
        # 0 successes must not produce a negative lower bound; upper ≈ 0.2775.
        lo, hi = bs.wilson_interval(0, 10, ci_level=0.95)
        assert lo == 0.0
        assert hi == pytest.approx(0.2775, abs=0.001)

    def test_full_success_upper_clamped_to_one(self):
        lo, hi = bs.wilson_interval(10, 10, ci_level=0.95)
        assert hi == 1.0
        assert 0.0 < lo < 1.0

    def test_n_zero_is_widest_interval(self):
        assert bs.wilson_interval(0, 0) == (0.0, 1.0)

    def test_ninety_pct_is_narrower_than_ninety_five(self):
        lo90, hi90 = bs.wilson_interval(8, 10, ci_level=0.90)
        lo95, hi95 = bs.wilson_interval(8, 10, ci_level=0.95)
        assert (hi90 - lo90) < (hi95 - lo95)

    def test_z_for_confidence_known_values(self):
        assert bs.z_for_confidence(0.95) == pytest.approx(1.959964, abs=1e-4)
        assert bs.z_for_confidence(0.90) == pytest.approx(1.644854, abs=1e-4)


# ── seeded percentile bootstrap ──────────────────────────────────────────────


class TestBootstrap:
    def test_reproducible_byte_identical(self):
        vals = [0.05, 0.04, 0.06, 0.45, 0.55, 0.03, 0.08, 0.51]
        a = bs.percentile_bootstrap_ci(vals)
        b = bs.percentile_bootstrap_ci(vals)
        assert a == b  # exact equality — same seed, same draws

    def test_default_seed_is_pinned(self):
        # The default path must use the pinned SEED so two runs on the same
        # store agree; passing SEED explicitly is identical to the default.
        vals = [0.05, 0.04, 0.06, 0.45, 0.55, 0.03, 0.08, 0.51]
        assert bs.percentile_bootstrap_ci(vals) == bs.percentile_bootstrap_ci(
            vals, seed=bs.SEED
        )

    def test_ci_brackets_the_median(self):
        vals = [0.10, 0.12, 0.11, 0.13, 0.09, 0.14, 0.10, 0.12]
        med = statistics.median(vals)
        lo, hi = bs.percentile_bootstrap_ci(vals)
        assert lo <= med <= hi

    def test_single_value_degenerate(self):
        assert bs.percentile_bootstrap_ci([0.42]) == (0.42, 0.42)

    def test_empty(self):
        assert bs.percentile_bootstrap_ci([]) == (0.0, 0.0)

    def test_mean_statistic_supported(self):
        vals = [1.0, 2.0, 3.0, 4.0, 100.0]
        lo, hi = bs.percentile_bootstrap_ci(vals, statistic="mean")
        assert lo <= hi


# ── percentile helper ────────────────────────────────────────────────────────


class TestPercentile:
    def test_linear_interpolation(self):
        vals = [0.0, 1.0, 2.0, 3.0, 4.0]
        assert bs.percentile(vals, 0.0) == 0.0
        assert bs.percentile(vals, 1.0) == 4.0
        assert bs.percentile(vals, 0.5) == 2.0
        assert bs.percentile(vals, 0.25) == 1.0

    def test_empty_and_single(self):
        assert bs.percentile([], 0.5) == 0.0
        assert bs.percentile([7.0], 0.5) == 7.0


# ── Benjamini–Hochberg FDR ───────────────────────────────────────────────────


class TestBenjaminiHochberg:
    def test_step_up_rejects_below_the_carrying_rank(self):
        # p_(1)=0.02 fails its own 0.0167 threshold, but p_(2)=0.03 passes
        # 0.0333 → BH step-up rejects BOTH (aligned to input order).
        reject = bs.benjamini_hochberg([0.02, 0.03, 0.9], alpha=0.05)
        assert reject == [True, True, False]

    def test_input_order_preserved(self):
        # Same p-values, shuffled: the True flags follow their values.
        reject = bs.benjamini_hochberg([0.9, 0.02, 0.03], alpha=0.05)
        assert reject == [False, True, True]

    def test_nothing_significant(self):
        assert bs.benjamini_hochberg([0.4, 0.6, 0.8], alpha=0.05) == [False, False, False]

    def test_all_significant(self):
        assert bs.benjamini_hochberg([0.001, 0.002, 0.003], alpha=0.05) == [True, True, True]

    def test_empty(self):
        assert bs.benjamini_hochberg([]) == []


# ── standardization vs pooling (Simpson's paradox) ───────────────────────────


class TestStandardization:
    def test_simpsons_paradox_reversal(self):
        # Model A drew mostly EASY tasks, model B mostly HARD ones.
        # Per stratum, B beats A in BOTH; pooled, A looks better.
        a_cells = {"easy": (90, 0.90), "hard": (10, 0.40)}
        b_cells = {"easy": (10, 0.95), "hard": (90, 0.50)}

        # Pooled (confounded) — A's mean is dragged up by its easy mix.
        pooled_a = bs.pooled_rate(a_cells)
        pooled_b = bs.pooled_rate(b_cells)
        assert pooled_a == pytest.approx(0.85)
        assert pooled_b == pytest.approx(0.545)
        assert pooled_a > pooled_b  # pooled WRONGLY favors A

        # Standardized (common per-stratum weights) — correctly favors B.
        diff = bs.standardized_difference(a_cells, b_cells)  # rate(A) - rate(B)
        assert diff < 0  # B is higher
        assert diff == pytest.approx(0.65 - 0.725, abs=1e-6)

    def test_no_shared_stratum_returns_zero(self):
        a_cells = {"easy": (5, 0.8)}
        b_cells = {"hard": (5, 0.5)}
        assert bs.standardized_difference(a_cells, b_cells) == 0.0

    def test_standardized_rate_ignores_absent_strata(self):
        cells = {"easy": (10, 0.9)}
        weights = {"easy": 20.0, "hard": 20.0}  # model has no "hard" data
        # Only the "easy" stratum contributes → rate is just 0.9.
        assert bs.standardized_rate(cells, weights) == pytest.approx(0.9)


# ── effect sizes + confidence bucket ─────────────────────────────────────────


class TestEffectAndConfidence:
    def test_relative_delta_positive_when_cheaper(self):
        assert bs.relative_delta(0.5, 1.0) == pytest.approx(0.5)  # 50% cheaper
        assert bs.relative_delta(1.0, 1.0) == 0.0
        assert bs.relative_delta(0.5, 0.0) == 0.0  # no base → no ratio

    def test_risk_difference(self):
        assert bs.risk_difference(0.8, 0.5) == pytest.approx(0.3)

    def test_confidence_buckets(self):
        assert bs.confidence_bucket(0.9) == "high"
        assert bs.confidence_bucket(0.5) == "medium"
        assert bs.confidence_bucket(0.2) == "low"
        assert bs.confidence_bucket(0.05) == "none"
        assert bs.confidence_bucket(0.0) == "none"
