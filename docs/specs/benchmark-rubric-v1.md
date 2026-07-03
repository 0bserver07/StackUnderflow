# Benchmark Rubric v1 — ratified defaults

*Companion to `docs/specs/benchmark-engine.md` (issue #99). This file is the
ratification that spec §4.6 requires before the engine may ship: the engine
proposes, this document decides. The values here are the loaded defaults in
`reports/benchmark.py`; they are surfaced in every payload (`weights`,
`rubric_version`, `ci_level`, `success_threshold`) and overridable per call, so
tuning them is data, not a code change.*

*"Rubric v1" is the version of **this scoring rubric**, not the product/PyPI
version. It has nothing to do with the maintainer-only product version and moves
on its own cadence.*

---

## Composite weights

The per-stratum composite is a weighted blend of three axes, each normalized
*within the stratum* so the comparison is like-for-like:

| Axis | Weight | What it measures |
|---|---:|---|
| **success** | **0.45** | Did the work land — the tiered outcome signal (§ below). |
| **cost** | **0.35** | Dollars per successful outcome (min-max inverse; cheapest wins). |
| **effort** | **0.20** | Turns / retries (min-max inverse; fewest wins). |

Weights sum to 1.0. Rationale: outcome dominates (a cheap failure is not a win),
cost is the flagship economic axis, effort is a real but noisier signal.

**Wall-clock duration is descriptive only** — it includes human away-from-keyboard
time, so it is never scored into the winner.

**Reasoning efficiency is descriptive only and is never scored.** Providers that
don't report a wire reasoning-token count read as 0, so cross-provider reasoning
is not apples-to-apples. It is surfaced (`reasoning_share`) for context, never
folded into the composite.

## Success threshold τ

```
τ = 7.0
```

A real LLM grade (`grades.success`, `session_quality_metrics`) at or above 7.0
counts as a success on the grade tier; below counts as a failure. Fallback
(non-LLM) grades are never persisted, so only real grades reach this tier.

## Success signal — tiered, highest-confidence-first

Composed per session; the first tier with a signal decides `outcome_success`,
and the deciding tier is recorded. Sessions with no signal are `NULL` — excluded
from rates, but counted in the coverage figure shown beside every verdict.

| Tier | Source | success = 1 | success = 0 |
|---|---|---|---|
| 1 ground truth | PR / CI via commit link | PR merged & not reverted, or CI passed | PR reverted, or CI failed |
| 2 code delta | `static_analysis_findings` | net-improved | net-regressed |
| 3 LLM grade | `grades.success` (real only) | ≥ τ | < τ |
| 4 behavioral | `session_mart` proxies | one-shot | high-retry (≥ 8 turns) |

## Confidence level

```
CI_LEVEL = 0.90
```

All intervals (Wilson for success rates; seeded percentile bootstrap for
cost/turns; difference effects) report at 90%. The maintainer may raise this to
0.95 per call; 0.90 is the default.

## Statistical gates (honesty contract)

Unchanged from spec §4 and pinned in `services/benchmark_stats.py`:

| Gate | Value |
|---|---|
| Min sessions per model per stratum | 5 |
| Min qualifying models to compare a stratum | 2 |
| Min balanced sessions for a cross-task headline | 20 |
| Min relative cost effect | 10% |
| Min success effect | 10 percentage points |
| Min grade effect | 0.5 points |
| Multiple-comparison control | Benjamini–Hochberg FDR at α = 1 − CI_LEVEL |
| Bootstrap iterations / seed | 2000 / pinned (reproducible) |

Below the sample floor the verdict is the literal string **"insufficient
evidence"** with a `confidence` of `none`. A cross-task winner must win the
composite in ≥ 2 strata (each a real, FDR-significant separation), never clearly
lose a stratum, and clear the balanced-sample floor.

## Canonical intent taxonomy

Ratified as the **6-label** set (adopting `ops` over the recommender's former
5), owned by `services/task_classifier.py` and shared by the tag service, the
mode recommender, and the benchmark:

```
build · fix · explore · refactor · test · ops
```

## Scope

Phase 3 live-replay is **deferred** (no `benchmark run` in this MVP). The MVP is
the observational engine (spec §8 Moves 0–3): a natural experiment over the
user's own history, stated as such in every payload.
