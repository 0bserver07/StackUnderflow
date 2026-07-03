"""Small, pure, seeded statistics for the comparative benchmark engine.

Spec 26 §4 ("statistical honesty — the crux"). A benchmark that always names a
winner is folklore with a progress bar; the whole value is refusing to conclude
when the evidence is thin, and separating a real difference from noise when it
is not. That discipline is entirely in this module.

Everything here is:

* **Pure + deterministic.** No store, no clock, no network. The bootstrap draws
  from a ``random.Random(_SEED)`` seeded generator so two runs over the same
  numbers are byte-identical (the report's read-through cache and the tests both
  rely on this).
* **stdlib only.** ``statistics`` (incl. ``NormalDist`` for the z-score) — no
  numpy, consistent with the rest of the tree. ``reports/anomaly.py`` already
  sets this tone with a robust-MAD ``MIN_POINTS`` floor; the engine extends it.

The functions, mapped to the spec:

* :func:`wilson_interval` — §4.2 success-rate CI (correct for small n where the
  normal approximation breaks).
* :func:`percentile_bootstrap_ci` — §4.2 cost/grade/turns CI (skewed
  continuous), seeded.
* :func:`benjamini_hochberg` — §4.4 FDR control across the family of tests.
* :func:`standardized_difference` / :func:`pooled_rate` — §3.2/§4.7 direct
  standardization vs the confounded pooled mean (Simpson's-paradox defense).
* sample floors + effect thresholds + :func:`confidence_bucket` — §4.1/§4.3/§4.5
  ("insufficient evidence" as a first-class verdict).
"""

from __future__ import annotations

import random
import statistics
from typing import Any

__all__ = [
    "SEED",
    "CI_LEVEL",
    "BOOTSTRAP_ITERS",
    "MIN_SESSIONS_PER_CELL",
    "MIN_MODELS_PER_CELL",
    "MIN_BALANCED_TOTAL",
    "MIN_EFFECT_COST",
    "MIN_EFFECT_SUCCESS",
    "MIN_EFFECT_GRADE",
    "z_for_confidence",
    "wilson_interval",
    "percentile",
    "percentile_bootstrap_ci",
    "benjamini_hochberg",
    "pooled_rate",
    "standardized_rate",
    "standardized_difference",
    "relative_delta",
    "risk_difference",
    "confidence_bucket",
]


# ── pinned tunables ──────────────────────────────────────────────────────────

# Pinned so the seeded bootstrap is reproducible: two runs on the same store
# agree, and a test can assert byte-identical CIs.
SEED = 1729

# Default confidence level for every interval. The maintainer may set 0.95;
# 0.90 is the ratified default (docs/specs/benchmark-rubric-v1.md).
CI_LEVEL = 0.90

# Bootstrap resamples for a continuous-metric CI. 2000 is the spec's figure —
# enough for a stable 90% interval without making the live join expensive.
BOOTSTRAP_ITERS = 2000

# Sample-size floors — refuse before you mislead (§4.1). 5 mirrors
# ``anomaly.MIN_POINTS`` and ``mode_recommender``'s n/5 term.
MIN_SESSIONS_PER_CELL = 5   # a model needs ≥5 sessions in a stratum to be scored
MIN_MODELS_PER_CELL = 2     # need ≥2 qualifying models to compare a stratum at all
MIN_BALANCED_TOTAL = 20     # per model, across strata, for a headline verdict

# Practical-effect floors — statistical separation is necessary, not sufficient
# (§4.3). A win must also clear a floor a human would care about.
MIN_EFFECT_COST = 0.10      # ≥10% relative cost difference
MIN_EFFECT_SUCCESS = 0.10   # ≥10 percentage points
MIN_EFFECT_GRADE = 0.5      # ≥0.5 grade points


# ── z-score ──────────────────────────────────────────────────────────────────


def z_for_confidence(ci_level: float = CI_LEVEL) -> float:
    """Two-sided z critical value for ``ci_level`` (e.g. 0.90 → 1.6449).

    Uses ``statistics.NormalDist`` — stdlib, no scipy. ``ci_level`` is clamped
    to a sane open interval so a degenerate 0 or 1 can't blow up ``inv_cdf``.
    """
    ci = min(max(float(ci_level), 0.5), 0.999999)
    return statistics.NormalDist().inv_cdf(1.0 - (1.0 - ci) / 2.0)


# ── Wilson score interval (proportions) ──────────────────────────────────────


def wilson_interval(
    successes: int, n: int, *, ci_level: float = CI_LEVEL
) -> tuple[float, float]:
    """Wilson score interval for a proportion — correct at small n.

    The normal approximation (p ± z·√(p(1-p)/n)) is badly wrong for small n or
    p near 0/1 (it can leave [0,1]); Wilson is the standard fix and the reason
    a 3-of-4 success rate reports honest uncertainty instead of a fake 0.75±.

    ``n == 0`` → ``(0.0, 1.0)`` (no information → widest possible interval);
    callers gate on the sample floor long before this, but the function stays
    total. The result is always clamped to ``[0, 1]``.
    """
    if n <= 0:
        return (0.0, 1.0)
    z = z_for_confidence(ci_level)
    p = successes / n
    z2 = z * z
    denom = 1.0 + z2 / n
    center = (p + z2 / (2.0 * n)) / denom
    margin = (z / denom) * ((p * (1.0 - p) / n + z2 / (4.0 * n * n)) ** 0.5)
    lo = max(0.0, center - margin)
    hi = min(1.0, center + margin)
    return (lo, hi)


# ── percentile + bootstrap (continuous, skewed) ──────────────────────────────


def percentile(sorted_values: list[float], q: float) -> float:
    """Linear-interpolation percentile of an already-sorted list.

    ``q`` in ``[0, 1]``. Matches numpy's default ``'linear'`` method so the
    bootstrap CI edges line up with what a reader would compute by hand.
    """
    if not sorted_values:
        return 0.0
    if len(sorted_values) == 1:
        return float(sorted_values[0])
    q = min(max(float(q), 0.0), 1.0)
    rank = q * (len(sorted_values) - 1)
    lo_idx = int(rank)
    frac = rank - lo_idx
    if lo_idx + 1 >= len(sorted_values):
        return float(sorted_values[-1])
    lo = sorted_values[lo_idx]
    hi = sorted_values[lo_idx + 1]
    return float(lo + (hi - lo) * frac)


def percentile_bootstrap_ci(
    values: list[float],
    *,
    statistic: str = "median",
    iters: int = BOOTSTRAP_ITERS,
    ci_level: float = CI_LEVEL,
    seed: int = SEED,
) -> tuple[float, float]:
    """Percentile bootstrap CI for a skewed continuous metric (cost / turns).

    Resamples ``values`` with replacement ``iters`` times, recomputes
    ``statistic`` (``"median"`` or ``"mean"``) on each resample, and returns the
    central ``ci_level`` percentile band of that bootstrap distribution.

    Deterministic: the resampling uses ``random.Random(seed)`` with ``seed``
    pinned to :data:`SEED`, so the same input yields byte-identical bounds on
    every run. Empty → ``(0.0, 0.0)``; a single value → ``(v, v)`` (a
    degenerate but honest interval — one point has no spread).
    """
    clean = [float(v) for v in values]
    if not clean:
        return (0.0, 0.0)
    if len(clean) == 1:
        return (clean[0], clean[0])

    stat_fn = statistics.mean if statistic == "mean" else statistics.median
    # Deterministic pseudo-random resampling — reproducibility, not crypto.
    rng = random.Random(seed)  # noqa: S311
    n = len(clean)
    draws: list[float] = []
    for _ in range(max(1, iters)):
        sample = [clean[rng.randrange(n)] for _ in range(n)]
        draws.append(float(stat_fn(sample)))
    draws.sort()
    alpha = (1.0 - ci_level) / 2.0
    return (percentile(draws, alpha), percentile(draws, 1.0 - alpha))


# ── Benjamini–Hochberg FDR ───────────────────────────────────────────────────


def benjamini_hochberg(pvalues: list[float], *, alpha: float = 0.10) -> list[bool]:
    """Benjamini–Hochberg step-up FDR control.

    Testing M models × S strata × K metrics inflates false positives fast
    (§4.4). BH bounds the expected false-discovery rate at ``alpha``: sort the
    p-values ascending, find the largest rank ``k`` with ``p_(k) ≤ (k/m)·α``,
    and reject **every** hypothesis up to that rank (the step-up — a hypothesis
    can be rejected even if it fails its own threshold, because a smaller p
    later in the order carries it).

    Returns a list of booleans aligned to the **input** order (``True`` =
    reject / a real finding). Empty input → ``[]``.
    """
    m = len(pvalues)
    if m == 0:
        return []
    order = sorted(range(m), key=lambda i: pvalues[i])
    max_k = 0
    for rank, idx in enumerate(order, start=1):
        if pvalues[idx] <= (rank / m) * alpha:
            max_k = rank
    reject = [False] * m
    for rank, idx in enumerate(order, start=1):
        if rank <= max_k:
            reject[idx] = True
    return reject


# ── standardization vs pooling (Simpson's-paradox defense) ───────────────────


def pooled_rate(cells: dict[Any, tuple[int, float]]) -> float:
    """Naive pooled mean: ``Σ(n·rate) / Σn`` over a model's own strata.

    This is the *confounded* number — it re-imports the selection bias because
    a model's own assignment mix (mostly-easy vs mostly-hard) weights it. Kept
    for disclosure/contrast only; the engine never ranks on it.
    """
    num = 0.0
    den = 0
    for n, rate in cells.values():
        num += n * rate
        den += n
    return (num / den) if den else 0.0


def standardized_rate(
    cells: dict[Any, tuple[int, float]], weights: dict[Any, float]
) -> float:
    """Direct-standardized rate: a model's per-stratum rates under a *common*
    stratum weighting, over strata present in ``weights``."""
    num = 0.0
    den = 0.0
    for stratum, w in weights.items():
        if stratum in cells and w > 0:
            num += w * cells[stratum][1]
            den += w
    return (num / den) if den else 0.0


def standardized_difference(
    a_cells: dict[Any, tuple[int, float]],
    b_cells: dict[Any, tuple[int, float]],
) -> float:
    """Standardized ``rate(A) - rate(B)`` over strata where **both** have data.

    Direct standardization with a common weight per shared stratum (the
    combined sample there). This is the honest comparison the spec mandates:
    pooling A's and B's own means separately would let a lopsided assignment
    (A drew the easy tasks, B the hard ones) manufacture or reverse a winner
    — the textbook Simpson's paradox (§3.2, §4.7). ``0.0`` when no stratum is
    shared (→ "untested here", never imputed).
    """
    shared = set(a_cells) & set(b_cells)
    weights: dict[Any, float] = {
        s: float(a_cells[s][0] + b_cells[s][0]) for s in shared
    }
    if not weights:
        return 0.0
    return standardized_rate(a_cells, weights) - standardized_rate(b_cells, weights)


# ── effect sizes ─────────────────────────────────────────────────────────────


def relative_delta(new: float, base: float) -> float:
    """Signed relative change ``(base - new) / base``; positive ⇒ ``new`` lower.

    Used for the cost axis: a positive value means the candidate is cheaper by
    that fraction. ``0.0`` when ``base`` is 0 (no meaningful ratio)."""
    if base == 0:
        return 0.0
    return (base - new) / base


def risk_difference(p_a: float, p_b: float) -> float:
    """Risk difference ``p_a - p_b`` (percentage-point gap between two rates)."""
    return p_a - p_b


# ── confidence label ─────────────────────────────────────────────────────────


def confidence_bucket(score: float) -> str:
    """Map a ``[0, 1]`` confidence score to ``{none, low, medium, high}``.

    ``none`` is the "insufficient evidence" verdict — the single most-surfaced
    field (§4.5). Thresholds are deliberately conservative: the engine should
    reach ``high`` only when the sample, balance, CI width, and cross-stratum
    agreement all line up.
    """
    if score >= 0.66:
        return "high"
    if score >= 0.40:
        return "medium"
    if score >= 0.15:
        return "low"
    return "none"
