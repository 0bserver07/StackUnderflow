# Spec 26 — Comparative Benchmark Engine

*Empirical "which model wins for your work" — design spec for issue #99*
*Status: design call for the maintainer. No code. Supersedes the stale schema/dependency assumptions in the issue body (see §0).*

---

## 0. Corrections to the issue body (read first)

The issue was filed early in Wave 5 and three of its assumptions are now stale. This spec builds on the current tree, not the issue's snapshot.

| Issue says | Reality (verified 2026-07-03) | Consequence for this spec |
|---|---|---|
| Schema slot **v022** for `benchmark_runs`/`benchmark_outcomes` | `CURRENT_VERSION = 27` (`store/schema.py:29`); **v022 is taken** (`v022_project_mart_message_dims`). Next free slot is **v028**. | The MVP needs **no migration at all** (compute-on-read). The optional perf mart lands at **v028**, additively. |
| **Blocked by** Spec 21 (static analysis), Spec 23 (LLM grader), Spec 18 (mode recommender) | All **shipped**: `services/static_analysis/` + `v018`; `services/grading.py` + `v020`/`v021`; `services/mode_recommender.py` + `v016`. | Not blockers — **reuse surfaces**. The engine is mostly a *join + statistics* layer over data that already exists. |
| **Blocked by** Spec 25 "fork mode" (live replay) | No live model-re-execution harness exists. `reports/forks.py` is *observational* DAG analysis; `playback_fs._replay_session` reconstructs file state, it does not call a model. | Live replay is **not required for the credible MVP** and is demoted to an optional, gated Phase 3 (§11). |

**The central design call this spec makes:** the credible, shippable core is an **observational benchmark over the user's own history** (a natural experiment they already ran), *not* live replay. This is what makes it local-first, zero-cost, always-available, and — with the statistical controls in §4 — honest. Live replay is a real statistical upgrade (it removes selection bias) but it costs money, needs a cloud/Ollama execution harness that violates "no external services by default," and carries its own "replay ≠ reality" validity gap that the issue itself flags. It belongs behind an explicit opt-in, later.

**Schema note re: the versioning hard rule.** Every "v0NN" below is a SQLite `PRAGMA user_version` migration slot (routine additive engineering, the same kind agents add every wave). It is unrelated to the product/PyPI version, which is maintainer-only and untouched here.

---

## 1. Goal & scope

### The question it answers
> *"For the kind of work I actually do, which model gives me the best outcome per dollar — and do I have enough evidence to trust that answer?"*

The deliverable is a per-task-type verdict of the form **"for `fix`-type tasks of `large` size, `claude-sonnet-4-6` wins at $0.42 per successful outcome vs `claude-opus-4-7` at $1.10 — 31 sessions, 90% CI, medium confidence,"** *or*, far more often and just as valuable, **"insufficient comparable evidence to call a winner for `refactor` tasks yet — here's the gap."**

The last clause is not a fallback. Surfacing "you can't yet conclude this" credibly is the feature. A benchmark that always names a winner is folklore with a progress bar.

### In scope
- Compare every model the user has run, **from local history**, grouped by task-type and outcome, with confounder controls.
- Rank on a transparent composite: **cost, success, effort/latency, reasoning efficiency** — each surfaced per-axis, never collapsed into one opaque number.
- The flagship metric: **cost per successful outcome**, per model per task stratum.
- CLI, HTTP route (both with `--json`), a meta-agent tool, and a UI panel.
- A recommender: "for *this new task*, which model does your own history favor?" (the outcome-aware successor to `mode_recommender`, which today ranks on cost alone).

### Out of scope
- **Live replay / re-running sessions against models** — optional Phase 3 (§11), maintainer may defer indefinitely.
- **Cross-user / fleet aggregation** — nothing leaves the machine (local-first invariant).
- **Real-time per-session scoring** — this is an offline, cache-backed analytical view.
- **Naming or ranking third-party models as competitors** — see §12. The engine compares *whatever the user has priced history for*; user-facing copy names only the user's own model IDs.
- **Grading sessions** — the engine *consumes* `session_quality_metrics`; it does not run the grader.

---

## 2. Grounding — what already exists (the reuse surface)

Every metric the engine needs is already computed and stored per session. The engine is a stratification + statistics layer over these:

| Signal | Source | Shape |
|---|---|---|
| Per-session **model** attribution | `compare._primary_model_for_session` (canonical; shared with `session_mart`, `mode_recommender`) | model with most assistant msgs; lexical tiebreak |
| Per-session **cost** | `session_mart.cost_usd` (rolled from `usage_events.cost_usd`, priced black-box) | never recomputed → pricing-invariant safe |
| **Success / quality** grade | `grading.get_stored_grade(conn, sid)` → `session_quality_metrics` | `overall_score` 1–10, `grades.{goal_clarity,execution_efficiency,success}`; **only real LLM grades persist** (`v021`), `grade_source∈{llm,fallback}` |
| Objective **code deltas** | `static_analysis/runner.get_session_quality` → `static_analysis_findings` | per-metric `improved/regressed/neutral`, `avg_delta` (complexity, lint_count, coverage, type-completeness) |
| **Ground-truth outcomes** | `outcome_attribution.get_outcomes_for_session` → `commit_session_link`+`pr_outcomes`+`ci_runs` | PR `state∈{open,merged,closed}`, `reverted_at`; CI `status` |
| **Effort / behavior** proxies | `compare.ModelStats` | `one_shot_pct`, `retry_rate`, `cache_hit_rate` |
| **Reasoning efficiency** | `usage_events.reasoning_tokens` (`v026`), `aggregator.reasoning_share` | reasoning ÷ output; **0 for Anthropic/Grok (no wire count)** |
| **Abandonment** cost | `reports/forks.py::analyze_forks` | sidechain share, abandoned-branch spend |
| Task **size** proxy | `mode_recommender.TOKEN_BANDS` (`tiny<200, small<800, med<3000, large`) | reused for stratification |
| Task **intent** | `tag_service._detect_intents` (`build/fix/explore/refactor/test/ops`) | ⚠ persisted to `~/.stackunderflow/tags.json`, **not SQLite** — see §5 |
| Existing **model×cost** compare | `compare.compare_models(conn, period, project_filter, provider_filter) → [ModelStats]` | no task/outcome grouping — *this is the gap the engine fills* |
| Existing **recommender (cost-only)** | `mode_recommender.recommend(...)` → `Recommendation` | ranks cheapest similar; **no outcome dimension** |

**Two reconciliations are prerequisites** (flagged by exploration):
1. **Intent taxonomy divergence** — `mode_recommender` has 5 labels, `tag_service` has 6 (`+ops`). Move 0 below unifies them into one canonical `classify_task()`.
2. **Intent isn't in SQLite** — it's a side JSON file. The engine re-derives intent deterministically at read time (MVP) and materializes it onto the Phase-2 mart.

---

## 3. Approach — the empirical method

### 3.1 Unit of comparison
The **session** is the unit. Each session gets one `primary_model` via `compare._primary_model_for_session` (reuse verbatim — do not invent a new attribution, or the engine will disagree with `session_mart`/`compare`/`mode_recommender`).

### 3.2 Task strata — controlling the confounder
The models were **not randomly assigned**. A user reaches for the expensive model on hard problems and the cheap one on quick edits (or vice-versa). So a naive "success rate by model" is confounded by task difficulty: if a model shows a low raw win rate, it may simply have drawn the hard tasks.

**Primary defense: stratify, never pool.** Compare models only *within* a stratum of comparable tasks:

```
stratum key = (intent, size_band)          # MVP
            = (intent, size_band, language) # Phase 2, when cells stay populated
```

- `intent` ∈ {build, fix, explore, refactor, test, ops} — canonical `classify_task()`.
- `size_band` ∈ {tiny, small, med, large} — reuse `TOKEN_BANDS` thresholds, applied to *session* token volume (not just the prompt).
- `language` — dominant language touched (`static_analysis_findings.language`, else file-extension histogram), nullable.

Within a stratum, tasks are like-for-like. Cross-stratum aggregates use **direct standardization** (stratum-weighted averages over strata where *both* models have data) — never a raw pooled mean, which would re-import the selection bias (Simpson's-paradox territory).

**The imbalance is disclosed, not hidden.** Every result carries the per-cell assignment counts ("in `fix × large`: model A ran 18, model B ran 2"). A model with zero sessions in a stratum is **"untested here,"** never imputed.

### 3.3 What "wins" means — the four axes
All per model, per stratum, each surfaced independently:

1. **Cost** — median `session_mart.cost_usd`. Lower wins. Headline: **cost per successful outcome** = Σcost ÷ Σsuccesses.
2. **Success** — the tiered signal (§3.4). Higher wins.
3. **Effort / latency** — `num_turns` and `retry_rate` (clean), plus session `duration_seconds` (surfaced *descriptively only* — wall-clock includes human away-from-keyboard time, so it is a noisy latency proxy, never scored into the winner).
4. **Reasoning efficiency** — `reasoning_share` and reasoning-tokens-per-outcome. **Descriptive by default, not scored** — Anthropic/Grok report 0 reasoning tokens (no wire count), so cross-provider reasoning is not apples-to-apples. Only entered into the composite when *both* compared models expose real counts, and even then behind a config flag (more thinking ≠ worse).

### 3.4 The success signal — tiered, honest about coverage
LLM grading is optional (needs local Ollama) so most stores have **partial** grade coverage. The engine composes a binary `outcome_success ∈ {1, 0, NULL}` from the highest-confidence signal available per session:

| Tier | Signal | success=1 | success=0 |
|---|---|---|---|
| 1 (ground truth) | PR / CI via `commit_session_link` | PR merged & not reverted; CI passed | PR reverted; CI failed |
| 2 (code delta) | `static_analysis_findings` | net-improved | net-regressed |
| 3 (LLM grade) | `grades.success` (real only) | ≥ τ (default 7.0) | < τ |
| 4 (behavioral) | `compare` proxies | one-shot & no abandonment | high retry / abandoned branch |

Sessions with **no** signal are `NULL` — excluded from success-rate math, but **counted in a coverage figure** shown alongside every verdict ("success measured on 22/40 sessions"). The tier used is recorded per session (`evidence_json`) so a verdict is auditable. The Tier-1 heuristic is coarse (24h + cwd commit match) — disclosed as a known caveat.

### 3.5 The composite verdict
Per stratum, per model: a composite score in [0,1] = weighted blend of normalized cost (inverse), success rate, and effort. **Weights are maintainer-owned** (issue hard rule) — proposed v1 in §4.6, surfaced in the payload and configurable, never hard-coded silently. The composite is only *computed*; whether it clears the bar to be *called a winner* is decided entirely by §4.

---

## 4. Statistical honesty — the crux

This is what separates a credible benchmark from a dashboard that picks favorites from noise. The house already sets the tone: `reports/anomaly.py` uses robust MAD statistics with a `MIN_POINTS=5` floor and refuses to flag when it can't separate signal. The engine extends that discipline. **No bootstrap/Wilson/CI code exists yet** — this section specifies a small, pure, seeded `services/benchmark_stats.py` (stdlib `statistics` only, no numpy — consistent with the tree).

### 4.1 Sample-size floors (refuse before you mislead)
```
MIN_SESSIONS_PER_CELL   = 5   # a model must have ≥5 sessions in a stratum to be scored there
MIN_MODELS_PER_CELL     = 2   # need ≥2 qualifying models to compare a stratum at all
MIN_BALANCED_TOTAL      = 20  # per model, across strata, for a headline cross-task verdict
```
(5 mirrors `anomaly.MIN_POINTS` and `mode_recommender`'s `min(1, n/5)` / `_MIN_SIMILAR=3`.) Below a floor, the output is the literal string **"insufficient evidence"** plus the shortfall — never a rank.

### 4.2 Confidence intervals, not point estimates
- **Success rate** (a proportion): **Wilson score interval** — correct for small n where the normal approximation breaks. Report `[lo, hi]` at `CI_LEVEL = 0.90` (default; maintainer may set 0.95).
- **Cost / grade / turns** (continuous, skewed): **percentile bootstrap**, `BOOTSTRAP_ITERS = 2000`, deterministic `random.Random(_SEED)` (`_SEED` pinned so tests are reproducible and two runs on the same store agree).
- **The comparison uses a difference CI, not two overlapping CIs.** Overlapping-error-bar eyeballing is statistically wrong (too conservative). "Model X beats Y" requires the **CI of the paired/stratified difference to exclude 0**.

### 4.3 Effect size + practical threshold
Statistical separation is necessary, not sufficient. A win must also clear a *practical* floor:
```
MIN_EFFECT_COST     = 0.10   # ≥10% relative cost difference
MIN_EFFECT_SUCCESS  = 0.10   # ≥10 percentage points
MIN_EFFECT_GRADE    = 0.5    # ≥0.5 grade points
```
Report the effect size itself (risk difference for success; relative delta for cost) next to the CI. "3% cheaper with a tight CI" is a tie, and the UI says so.

### 4.4 Multiple comparisons
Testing M models × S strata × K metrics inflates false positives fast. Two guards:
- **Headline claims** get **Benjamini–Hochberg FDR** control across the family of (stratum × metric × pair) tests.
- A cross-task **winner** must hold on the composite in **≥ K strata (default 2)** with **no stratum where it clearly loses**, and total balanced n ≥ `MIN_BALANCED_TOTAL`. Single-cell findings are labeled **"weak / exploratory,"** never headlined.

### 4.5 Confidence label (what the user sees)
Every verdict carries `confidence ∈ {none, low, medium, high}`, derived (as `mode_recommender._compute_confidence` does — product of sub-terms) from: min qualifying n, assignment balance (how lopsided the per-cell counts are), CI width, and cross-stratum agreement. `none` maps to the "insufficient evidence" verdict. This is the single most-surfaced field.

### 4.6 Proposed rubric v1 (maintainer finalizes — issue hard rule)
Composite weights, tunable, surfaced in payload:
```
success  0.45   # did the work land
cost     0.35   # dollars per outcome
effort   0.20   # turns / retries (duration descriptive only)
# reasoning efficiency: descriptive; enters composite only when both models expose real counts (opt-in)
success threshold τ = 7.0 (grade tier)
```
The issue's hard rule stands: **this stays in `needs-design` until the maintainer commits `docs/specs/benchmark-rubric-v1.md`.** This spec proposes; it does not ratify.

### 4.7 The honesty contract, stated plainly
1. Refuse (say "insufficient evidence") more readily than conclude.
2. Never pool across strata; always standardize.
3. Always show n, coverage, assignment balance, and CI alongside any number.
4. A tie is a first-class result and is labeled a tie.
5. Observed history is a **natural experiment, not a randomized trial** — the payload states this caveat verbatim; the engine controls for the confounder it can measure (task difficulty) and *discloses* the ones it can't (user skill drift over time, per-project difficulty, prompt-quality differences).

---

## 5. Data model

### MVP — no migration
Mirror `reports/forks.py` / `reports/anomaly.py` exactly: a pure, read-only, **advisory-never-raises** module `reports/benchmark.py` that joins existing tables at query time and returns a well-formed dict (empty-but-valid on a schemaless store). Reuses `session_mart`, `model_day_mart`, `session_quality_metrics`, `static_analysis_findings`, `pr_outcomes`/`ci_runs`/`commit_session_link`, `usage_events.reasoning_tokens`. Intent is re-derived per session via canonical `classify_task()`. **Zero schema change.**

Route budgeted at **200ms** (the `/api/yield`, `/api/optimize` tier — not the 100ms mart tier, because it is an analytical composite), and wrapped in the same read-through cache as forks (`_analyze_forks_cached` pattern: keyed on `(store_path, scope, project_ids)` + a sessions signature so ingest self-invalidates).

### Phase 2 — additive perf mart `benchmark_mart` (v028)
When profiling on a large store (`session_mart` fixture is 50K rows) shows the live join exceeds budget, materialize one denormalized row per session. Standard additive recipe (migration + `MartBuilder` subclass + `_REGISTRY.register` + `mart_queries` readers + `_ADD_COLUMN_GUARDS[28]` entry):

```sql
-- v028_benchmark_mart.sql  (additive; IF NOT EXISTS; guard-backed)
BEGIN;
CREATE TABLE IF NOT EXISTS benchmark_mart (
    session_id        TEXT    PRIMARY KEY,
    project_id        INTEGER NOT NULL,
    provider          TEXT    NOT NULL,
    primary_model     TEXT    NOT NULL,   -- _primary_model_for_session
    intent            TEXT    NOT NULL,   -- canonical classify_task()
    size_band         TEXT    NOT NULL,   -- TOKEN_BANDS over session tokens
    primary_language  TEXT,               -- nullable
    first_ts          TEXT    NOT NULL,
    duration_seconds  REAL,
    num_turns         INTEGER NOT NULL DEFAULT 0,
    cost_usd          REAL    NOT NULL DEFAULT 0,   -- from session_mart; NEVER recomputed
    output_tokens     INTEGER NOT NULL DEFAULT 0,
    reasoning_tokens  INTEGER NOT NULL DEFAULT 0,
    success_grade     REAL,               -- real LLM grade only, else NULL
    overall_grade     REAL,
    static_delta      REAL,               -- net improved−regressed, else NULL
    pr_merged         INTEGER,            -- 0/1/NULL
    pr_reverted       INTEGER,
    ci_passed         INTEGER,            -- 0/1/NULL
    outcome_success   INTEGER,            -- 0/1/NULL composed tier
    outcome_tier      TEXT                -- which tier decided outcome_success
);
CREATE INDEX IF NOT EXISTS idx_benchmark_mart_cell ON benchmark_mart(intent, size_band, primary_model);
CREATE INDEX IF NOT EXISTS idx_benchmark_mart_project ON benchmark_mart(project_id);
PRAGMA user_version = 28;
COMMIT;
```

**Builder caveat that must be respected:** the standard `MartBuilder` refreshes from `usage_events.id > watermark`, but outcome facts (grades, PRs, CI) **arrive late and out-of-band** — a session graded next week must update its existing row. So `BenchmarkMartBuilder` refreshes on a *session* watermark (max `first_ts`/`last_ts` seen) **and** re-scans rows whose `outcome_success IS NULL` or whose grade/outcome tables changed since last refresh. `rebuild_from_scratch` is a full recompute. This is the one place the mart pattern is extended, and it is called out so the implementer doesn't ship a mart that silently misses late grades.

**Read-path fallback** (mirror `command_day_mart`'s "no rows → fall back" contract): the `reports/benchmark.py` reader prefers `benchmark_mart` when populated and falls back to the live join when empty, so a store that hasn't refreshed still works (just slower). The read API is identical either way.

**`cost_usd` is copied from `session_mart`, never recomputed** — keeps `test_pricing_invariants.py` (`sum(marts)==sum(events)`) green. `reasoning_tokens` stays a subset of output, never summed into cost (`v026` contract).

### Verdict cache — reuse `mode_recommendations` (no new table)
The recommender's cached verdicts fit the existing `mode_recommendations` schema (`task_pattern_hash` md5, `recommended_model`, `confidence`, `evidence_session_ids`, timestamps). Add an `outcome_aware` discriminator inside the hashed feature tuple rather than a new table — the md5-of-features design (`v016` header) exists precisely so new features need no ALTER.

---

## 6. API surface

### 6.1 HTTP route — `routes/benchmark.py` (mirrors `routes/forks.py`)
```python
import stackunderflow.deps as deps
from stackunderflow.store import db
router = APIRouter()

_PERIOD_QUERY = Query("all", description="today | week | month | all")
_LOG_PATH_QUERY = Query(None, description="Project log path; omit for whole-store")
_INTENT_QUERY = Query(None, description="Filter to one intent stratum")

@router.get("/api/benchmark")
async def get_benchmark(period=_PERIOD_QUERY, log_path=_LOG_PATH_QUERY, intent=_INTENT_QUERY):
    scope = parse_period(_PERIOD_ALIASES[period])       # 400 on bad period, like forks
    conn = db.connect(deps.store_path)
    try:
        project_ids = _project_ids_for(conn, log_path or deps.current_log_path) if ... else None
        report = _analyze_benchmark_cached(conn, scope=scope, project_ids=project_ids, intent=intent)
    finally:
        conn.close()
    # currency conversion on cost fields via active_currency_payload(), as forks does
    return {"period": period, "scope": scope.label, "report": report,
            "currency": currency, "warning": _NATURAL_EXPERIMENT_WARNING}
```
`GET /api/benchmark/recommend?intent=fix&size=large` → the outcome-aware recommendation. Register in `server.py` (import tuple + `app.include_router(benchmark.router)`).

Response `report` shape:
```jsonc
{
  "verdict": {"headline": "insufficient evidence" | "model X wins for <intent>",
              "winning_model": "claude-sonnet-4-6" | null,
              "confidence": "none|low|medium|high",
              "cost_per_outcome_usd": 0.42, "runner_up": "...", "caveats": [...]},
  "strata": [{"intent": "fix", "size_band": "large",
              "models": [{"model": "...", "n": 18, "coverage": 0.61,
                          "cost_per_outcome": {"point": 0.42, "ci": [0.31, 0.58]},
                          "success_rate": {"point": 0.78, "ci_wilson": [0.55, 0.91]},
                          "median_turns": 6, "reasoning_share": 0.0, "composite": 0.71}],
              "assignment_balance": {"claude-sonnet-4-6": 18, "claude-opus-4-7": 2},
              "cell_verdict": "weak", "effect": {...}}],
  "coverage": {"sessions_total": 240, "sessions_scored": 156, "grade_coverage": 0.34},
  "rubric_version": 1, "weights": {"success": 0.45, "cost": 0.35, "effort": 0.20},
  "method_notes": ["natural experiment, not RCT", "Tier-1 commit match is 24h+cwd heuristic", ...]
}
```

### 6.2 CLI — `@cli.group("benchmark")` (mirrors `memory`/`backup` groups)
```
stackunderflow benchmark show   [--period month] [--project PATH] [--intent fix] [--json]
stackunderflow benchmark recommend --intent fix --size large [--json]
```
- `show` → the leaderboard / stratum table (text) or the enveloped JSON.
- `recommend` → outcome-aware model pick for a described task.
- Connection via `_open_store()`; wrap in `_run_*_query` try/finally.
- **`--json` uses the shared envelope** (`cli_helpers/agent_output.build_envelope(command=..., query=..., results=[...], budget=..., truncated=...)`, `render()`), schema `stackunderflow.memory/1`, token-bounded. This makes benchmark verdicts consumable by agents exactly like `memory` queries — an agent can ask the store "which model should I use for this refactor?" and get a stable, bounded, evidence-carrying answer.

Deliberately **no `run` subcommand in the MVP** — there is nothing to "run"; the benchmark is computed from existing history. (`benchmark run` returns only with Phase 3 replay, §11.)

### 6.3 Meta-agent tool — `recommend_model_for_task` (mirrors `search_past_decisions`)
Three synced edits in `services/meta_agent.py`: append a dict to `TOOL_CATALOG` (params: `intent` required, `size?`, `language?`), add `_exec_recommend_model_for_task(conn, args) -> dict` (returns JSON-safe dict, `{"error": ...}` on bad input, never raises, ≤ `_RESULT_CHAR_BUDGET`), and register in `_EXECUTORS`. Lets the local meta-agent answer "what model for this task?" with the user's own empirical evidence.

---

## 7. UI (source-only; no build)

**Recommended placement: a panel under the existing `Compare` tab** (`dashboardTabs.ts` already ships `{ id: 'compare', label: 'Compare', icon: IconScale }` — the scale icon is literally the benchmark metaphor, and StackUnderflow already carries 20+ tabs, so avoid another). The Compare tab today renders `compare.build_compare_payload` (model × cost); add a **"Which model wins"** section beneath it fed by `/api/benchmark`:

- **Leaderboard** — models ranked by composite, each row showing cost-per-outcome, success rate with its Wilson CI as an error bar, n, and a `confidence` chip. Rows below the sample floor render greyed as **"insufficient evidence."**
- **Per-task heatmap** — intent (rows) × size_band (cols), cell colored by winning model, **hollow/hatched when the cell is under-powered** (the honesty rule made visual). Hover → per-cell counts + CIs.
- **Cost-vs-quality scatter** — one point per model, x=cost-per-outcome, y=success rate, error bars on both, dot size = n. (Recharts, already the house chart lib.)
- A persistent **method banner**: "Based on N sessions you already ran — a natural experiment, not a controlled trial," plus grade-coverage %.

*Alternative (if the maintainer prefers a dedicated surface):* a new `{ id: 'benchmark', label: 'Benchmark', icon: IconTrophy, isBeta: true }` appended to `TABS` and gated in `ProjectDashboard.tsx`, following the `forks`/`yield` beta-tab precedent. Either is a small, source-only change; this spec recommends the Compare panel and flags the tab as the explicit alternative for the maintainer.

---

## 8. Phased implementation plan (MVP first)

**Move 0 — Reconcile task classification (prerequisite).** Extract one canonical `classify_task(session_text) -> {intent, size_band, language}` used by `tag_service`, `mode_recommender`, and the benchmark. Resolve the 5-vs-6 intent divergence (adopt the 6-label `+ops` set). Pure, deterministic, unit-tested. *No behavior change to existing callers beyond the unified `ops` label.*

**Move 1 — MVP observational engine (no migration).** `reports/benchmark.py` (`analyze_benchmark(conn, *, scope, project_ids, intent=None) -> dict`, advisory-never-raises) + `services/benchmark_stats.py` (Wilson, seeded bootstrap, BH-FDR, stratified standardization; stdlib only). Success-tier composition. Verdict assembly with all §4 gates. **This is the shippable slice.**

**Move 2 — Route + CLI + `--json` + meta-agent tool.** Wire `reports/benchmark.py` to `routes/benchmark.py`, the `benchmark` CLI group, and `recommend_model_for_task`. Read-through cache. Currency conversion on cost fields.

**Move 3 — UI panel.** Compare-tab "Which model wins" section (source-only).

**Move 4 — Phase-2 perf mart `benchmark_mart` (v028), *only if* profiling demands it.** Additive migration + builder (with the late-arriving-outcome refresh semantics of §5) + mart readers + fallback. The read API is unchanged.

**Phase 3 (separate issue, maintainer opt-in) — active replay (§11).**

MVP = Moves 0–3. It ships a credible, local-first, zero-cost benchmark with no schema change. Later moves are optimizations and extensions.

---

## 9. Test strategy (deterministic fixtures)

House pattern: synthetic in-`tmp_path` stores, never `~/.stackunderflow/store.db`; robust-stats and pricing pinned by fixtures.

- **Statistics unit tests** (`services/benchmark_stats.py`) — Wilson interval vs hand-computed textbook values; bootstrap CI reproducible under the pinned seed (byte-identical across runs); BH-FDR against a known p-value vector; stratified standardization vs a worked Simpson's-paradox fixture (pooled says A wins, standardized correctly says B). **These are the credibility tests — they gate merge.**
- **Confounder guard** — fixture where model A is assigned only easy `tiny` tasks and B only hard `large` tasks; assert the engine reports **per-stratum**, refuses a pooled winner, and surfaces the imbalance.
- **Sample-floor guard** — a stratum with n=3 for a model → assert `"insufficient evidence"`, never a rank.
- **Coverage honesty** — sessions with no success signal → `NULL`, excluded from rate, counted in coverage %.
- **Success-tier precedence** — a session with both a CI pass and a low LLM grade → assert Tier-1 wins and `outcome_tier` records it.
- **Pricing invariant** — Phase-2 mart: assert `sum(benchmark_mart.cost_usd)` reconciles with `session_mart`/`usage_events` (extend `test_pricing_invariants.py`); assert `reasoning_tokens ≤ output_tokens`.
- **Perf** — add `/api/benchmark` to `test_route_perf_regression.py` `_ROUTES` at the **200ms** budget against the 50K-session fixture; if it regresses, that's the Move-4 trigger.
- **Empty/degenerate store** — schemaless and single-model stores return well-formed empty verdicts, never raise (mirror the forks/anomaly tests).
- **`--json` envelope** — conforms to `contracts/staxtrace-memory-v1/schema.json` (the existing golden-fixture check).
- **Late-arriving outcome** (Phase 2) — grade a session *after* its mart row exists; assert the next refresh updates `outcome_success` (guards the non-standard builder).

---

## 10. Risks & failure modes

| Risk | Mitigation |
|---|---|
| **Selection bias** (the core threat) — models not randomly assigned | Stratify + standardize (§3.2, §4); disclose per-cell imbalance; never pool. |
| **Tiny/biased samples → false "X is better"** | Sample floors, difference-CIs, effect thresholds, BH-FDR, "insufficient evidence" as default (§4). |
| **Partial grade coverage** skews success | Tiered success with objective Tier-1/2 signals; coverage % always shown; NULLs excluded not imputed. |
| **Coarse outcome attribution** (24h+cwd commit match) | Disclosed in `method_notes`; Tier-1 is a signal, not gospel; grade/static tiers cross-check. |
| **Reasoning not comparable cross-provider** (Anthropic=0) | Descriptive-only by default; enters composite only when both models expose real counts. |
| **Time confound** — the user got better; later model looks better | Surface first_ts spread per cell; (Phase 2) optional recency-matched sub-analysis; documented as an uncontrolled confounder. |
| **Perf on large stores** | 200ms budget + read-through cache (MVP); `benchmark_mart` fallback (Phase 2). |
| **Pricing drift** breaks cost axis | Cost only ever read from `session_mart` (black-box priced); invariant test. |
| **Overconfident UI** | Confidence chips, greyed under-powered cells, hatched heatmap, method banner — honesty is visual, not buried. |
| **Rubric bikeshedding blocks ship** | Weights are data (surfaced, configurable); MVP ships with proposed v1; maintainer ratifies `benchmark-rubric-v1.md` before dispatch per the hard rule. |

---

## 11. Optional Phase 3 — active replay (gated, maintainer may defer indefinitely)

Turning the natural experiment into a *designed* one (same task → N models) removes selection bias — a genuine statistical upgrade. But it costs real money, needs an execution harness that doesn't exist, and introduces the **"replay ≠ reality"** gap the issue itself flags (a replay has less context, different tools, different repo state than the original live session; you replay the *prompt*, not the world). So it is strictly opt-in and its results are a **separately-labeled evidence class** — never merged into observed evidence without an `evidence_source: "replay"` tag.

If built (separate issue): tables `benchmark_replay_runs` + `benchmark_replay_outcomes` at the *then-current* free slot (**not** the issue's v022); `stackunderflow benchmark run --models ... --sample-size N [--allow-cloud] [--budget-cap-usd C]`; **local Ollama only by default**, cloud forks require explicit `--allow-cloud` + a hard budget cap (forks past the cap don't run; status `cancelled-over-budget`); stratified sampling of source sessions; scored by the same §3.4 rubric. The observed-vs-replay distinction is shown prominently. This spec neither requires nor blocks it.

---

## 12. Invariants respected

- **Local-first, no external services** — MVP reads only the local store; nothing leaves the machine; no network. (Grading's Ollama is a *consumed input*, not a new dependency; the engine degrades gracefully when grades are absent.)
- **`<100ms` mart routes** — honored: the composite route is budgeted at the 200ms analytical tier (`/api/yield`/`/api/optimize` precedent), not the 100ms mart tier, and is cache-backed.
- **Pricing invariants** — cost is never recomputed; copied from `session_mart`; `sum(marts)==sum(events)` stays green; reasoning stays a non-costed output subset.
- **No competitor model named** — user-facing copy and examples use only the user's own model IDs (`claude-opus-4-7`, `claude-sonnet-4-6`, …); other providers appear generically as "candidate models StackUnderflow already prices." No third-party model is named, linked, or ranked as a competitor.
- **Additive schema** — MVP adds no migration; Phase 2's `v028` is `CREATE TABLE IF NOT EXISTS` + guard, touches no existing table.
- **Versioning hard rule** — no product/PyPI version, tag, or CHANGELOG heading touched; `v028` is a SQLite `user_version` slot only.
- **Advisory-never-raises** — `reports/benchmark.py` follows the forks/anomaly contract: empty-but-valid on any degenerate store.

---

## 13. Open questions for the maintainer

1. **Rubric v1 weights + success threshold τ** — must land in `docs/specs/benchmark-rubric-v1.md` before dispatch (issue hard rule). §4.6 proposes; you ratify.
2. **CI level** — 90% (proposed) or 95%?
3. **UI placement** — Compare-tab panel (recommended) vs a new beta `Benchmark` tab?
4. **`ops` intent** — confirm adopting `tag_service`'s 6-label set (adds `ops`) as canonical over `mode_recommender`'s 5.
5. **Reasoning axis** — keep descriptive-only, or allow it into the composite when both models expose real counts?
6. **Phase 3 replay** — greenlight as a follow-up issue, or leave out entirely?

---

## See also
- `reports/forks.py`, `reports/anomaly.py` — the advisory-report contract this mirrors.
- `services/compare.py` — the model×cost compare this extends with task/outcome grouping.
- `services/mode_recommender.py` — the cost-only recommender this supersedes with an outcome dimension.
- `services/grading.py`, `services/static_analysis/`, `services/outcome_attribution.py` — the success signals consumed.
- `store/schema.py`, `etl/marts/base.py`, `store/mart_queries.py` — the additive-mart machinery (Phase 2).
- `cli_helpers/agent_output.py` — the `--json` envelope contract.
