"""Comparative benchmark engine — "which model wins for your work?" (issue #99).

An **observational** benchmark over the user's own history: a natural
experiment they already ran, not live re-execution. Spec 26 makes the central
design call that this — not replay — is the credible, local-first, zero-cost,
always-available core. This module is the join + statistics layer over data
every other surface already computes per session; the hard part is not the
join, it is refusing to name a winner when the evidence can't support one.

Design contract (mirrors :mod:`stackunderflow.reports.forks` /
:mod:`stackunderflow.reports.anomaly`):

* **Advisory, never raises.** A schemaless store, an empty ``session_mart``, a
  single-model store, or any arithmetic edge returns an empty-but-well-formed
  verdict (``"insufficient evidence"``). Callers never wrap this for
  correctness.
* **Read-only + scope-bounded.** All SQL is guarded by ``sqlite_master`` and
  narrowed by the caller's :class:`Scope` + optional ``project_ids``.
* **Cost is a black box.** ``cost_usd`` is read **only** from ``session_mart``
  and never recomputed — keeps ``test_pricing_invariants`` green.
* **Honesty first (§4).** Sample floors, Wilson / bootstrap CIs, difference
  effects, BH-FDR across the family, direct standardization (never pooled), and
  "insufficient evidence" as a first-class verdict with a ``confidence`` label.

The rubric weights + success threshold τ are **maintainer-owned** (ratified in
``docs/specs/benchmark-rubric-v1.md``); they are surfaced in the payload and
overridable via the ``weights`` argument — never silently hard-coded.
"""

from __future__ import annotations

import json
import sqlite3
import statistics
from dataclasses import dataclass, field
from typing import Any

from stackunderflow.reports.scope import Scope
from stackunderflow.services import benchmark_stats as bs
from stackunderflow.services import task_classifier

__all__ = [
    "analyze_benchmark",
    "RUBRIC_VERSION",
    "DEFAULT_WEIGHTS",
    "SUCCESS_THRESHOLD",
    "recommend_from_history",
]


# ── rubric v1 (maintainer-owned; ratified in benchmark-rubric-v1.md) ─────────

RUBRIC_VERSION = 1
# Composite weights — surfaced in the payload and overridable. Sum to 1.0.
DEFAULT_WEIGHTS: dict[str, float] = {"success": 0.45, "cost": 0.35, "effort": 0.20}
# τ — grade-tier success threshold (a real LLM grade ≥ τ counts as success).
SUCCESS_THRESHOLD = 7.0

# Behavioural (Tier-4) proxy: a session with this many assistant turns is read
# as high-retry (a soft failure signal). One-shot sessions are a soft success.
_HIGH_RETRY_TURNS = 8

# The natural-experiment caveat, stated verbatim in every payload (§4.7).
NATURAL_EXPERIMENT_WARNING = (
    "This compares models over sessions you already ran — a natural "
    "experiment, not a controlled trial. Models were not randomly assigned to "
    "tasks, so the engine stratifies by task type and size and standardizes "
    "across strata to control for the confounder it can measure (task "
    "difficulty). It cannot control for the ones it can't (your skill drift "
    "over time, per-project difficulty, prompt-quality differences)."
)

_METHOD_NOTES: tuple[str, ...] = (
    "Observed history is a natural experiment, not a randomized trial.",
    "Models are compared only within a stratum of comparable tasks (intent × "
    "size); cross-task figures use direct standardization, never a pooled mean.",
    "Success is composed from the highest-confidence signal available per "
    "session (PR/CI → code-delta → LLM grade → behavioral); sessions with no "
    "signal are excluded from rates but counted in coverage.",
    "Tier-1 commit attribution is a coarse 24h + cwd heuristic — a signal, not "
    "gospel.",
    "Reasoning efficiency is descriptive only and is never scored into the "
    "winner (providers that report 0 reasoning tokens aren't apples-to-apples).",
    "A win must clear a practical effect floor and survive Benjamini–Hochberg "
    "FDR control; below the sample floor the verdict is 'insufficient evidence'.",
)


# ── per-session fact ─────────────────────────────────────────────────────────


@dataclass(slots=True)
class _SessionFact:
    session_id: str
    project_id: int
    primary_model: str
    intent: str
    size_band: str
    language: str | None
    cost_usd: float
    num_turns: int
    is_one_shot: bool
    output_tokens: int
    reasoning_tokens: int
    first_ts: str
    outcome_success: int | None  # 1 / 0 / None (unmeasured)
    outcome_tier: str | None     # which tier decided it


# ── table guard ──────────────────────────────────────────────────────────────


def _table_exists(conn: sqlite3.Connection, name: str) -> bool:
    """True when *name* is a queryable table or view (``messages`` is a view)."""
    try:
        row = conn.execute(
            "SELECT 1 FROM sqlite_master "
            "WHERE type IN ('table', 'view') AND name = ? LIMIT 1",
            (name,),
        ).fetchone()
    except sqlite3.Error:
        return False
    return row is not None


# ── success-signal composition (tiered) ──────────────────────────────────────


def _outcome_from_ground_truth(outcomes: dict[str, Any]) -> int | None:
    """Tier 1 — PR/CI ground truth. ``1`` merged&clean / CI pass, ``0`` reverted
    / CI fail, ``None`` when neither is present."""
    prs = outcomes.get("prs") or []
    ci = outcomes.get("ci_runs") or []
    reverted = any(p.get("reverted_at") for p in prs)
    merged_ok = any(
        (p.get("state") == "merged") and not p.get("reverted_at") for p in prs
    )
    ci_pass = any(c.get("status") == "success" for c in ci)
    ci_fail = any(c.get("status") == "failure" for c in ci)
    if reverted:
        return 0
    if merged_ok or ci_pass:
        return 1
    if ci_fail:
        return 0
    return None


def _outcome_from_static(metric_summary: dict[str, Any]) -> int | None:
    """Tier 2 — net code-delta. ``1`` net-improved, ``0`` net-regressed,
    ``None`` when the session was analyzed but shows no net direction."""
    improved = sum(int(m.get("improved", 0) or 0) for m in metric_summary.values())
    regressed = sum(int(m.get("regressed", 0) or 0) for m in metric_summary.values())
    if improved > regressed:
        return 1
    if regressed > improved:
        return 0
    return None


def _outcome_from_grade(grade_success: float | None) -> int | None:
    """Tier 3 — real LLM grade. ``1`` if ``grades.success ≥ τ`` else ``0``."""
    if grade_success is None:
        return None
    return 1 if grade_success >= SUCCESS_THRESHOLD else 0


def _outcome_from_behavior(is_one_shot: bool, num_turns: int) -> int | None:
    """Tier 4 — behavioral proxy. One-shot → ``1``; high-retry → ``0``; else
    ``None`` (no confident behavioral read)."""
    if is_one_shot:
        return 1
    if num_turns >= _HIGH_RETRY_TURNS:
        return 0
    return None


# ── data loading ─────────────────────────────────────────────────────────────


def _load_facts(
    conn: sqlite3.Connection,
    *,
    scope: Scope | None,
    project_ids: list[int] | None,
) -> list[_SessionFact]:
    """Load one :class:`_SessionFact` per scoped session, or ``[]``.

    Cost/model/tokens come from ``session_mart`` (cost is never recomputed).
    Intent + text-language are derived from the first user turn via the
    canonical ``task_classifier``; size band from the session's real token
    volume. Success is composed from the tiered signals in bulk-loaded side
    tables. Every table touch is guarded so a partial store degrades to fewer
    tiers rather than raising.
    """
    if not _table_exists(conn, "session_mart") or not _table_exists(conn, "sessions"):
        return []
    if project_ids is not None and len(project_ids) == 0:
        return []

    sql = (
        "SELECT sm.session_id AS session_id, sm.project_id AS project_id, "
        "       sm.primary_model AS primary_model, "
        "       COALESCE(sm.cost_usd, 0.0) AS cost_usd, sm.first_ts AS first_ts, "
        "       COALESCE(sm.input_tokens, 0) AS input_tokens, "
        "       COALESCE(sm.output_tokens, 0) AS output_tokens, "
        "       COALESCE(sm.assistant_message_count, 0) AS assistant_message_count, "
        "       COALESCE(sm.is_one_shot, 0) AS is_one_shot, "
        "       (SELECT m.content_text FROM messages m "
        "        WHERE m.session_fk = s.id AND m.role = 'user' "
        "        ORDER BY m.seq ASC LIMIT 1) AS first_user_text "
        "FROM session_mart sm "
        "JOIN sessions s ON s.session_id = sm.session_id "
        "WHERE sm.primary_model IS NOT NULL AND sm.primary_model != '' "
    )
    params: list[Any] = []
    if project_ids:
        placeholders = ",".join("?" for _ in project_ids)
        sql += f"AND sm.project_id IN ({placeholders}) "
        params.extend(project_ids)
    if scope is not None and scope.since is not None:
        sql += "AND sm.first_ts >= ? "
        params.append(scope.since)
    if scope is not None and scope.until is not None:
        sql += "AND sm.first_ts <= ? "
        params.append(scope.until)

    try:
        rows = conn.execute(sql, params).fetchall()
    except sqlite3.Error:
        return []
    if not rows:
        return []

    grades = _load_grades(conn)
    static_lang, static_outcome = _load_static(conn, {r["session_id"] for r in rows})
    ground_truth = _load_ground_truth(conn, {r["session_id"] for r in rows})
    reasoning = _load_reasoning(conn, scope=scope, project_ids=project_ids)

    facts: list[_SessionFact] = []
    for r in rows:
        sid = str(r["session_id"])
        text = str(r["first_user_text"] or "")
        intent = task_classifier.classify_intent(text)
        size_band = task_classifier.band_for_token_count(
            int(r["input_tokens"] or 0) + int(r["output_tokens"] or 0)
        )
        language = static_lang.get(sid) or task_classifier.dominant_language(text)
        turns = int(r["assistant_message_count"] or 0)
        one_shot = bool(r["is_one_shot"])

        success, tier = _compose_success(
            sid,
            ground_truth=ground_truth,
            static_outcome=static_outcome,
            grade_success=grades.get(sid),
            is_one_shot=one_shot,
            num_turns=turns,
        )
        rt, ot = reasoning.get(sid, (0, int(r["output_tokens"] or 0)))
        facts.append(
            _SessionFact(
                session_id=sid,
                project_id=int(r["project_id"] or 0),
                primary_model=str(r["primary_model"]),
                intent=intent,
                size_band=size_band,
                language=language,
                cost_usd=float(r["cost_usd"] or 0.0),
                num_turns=turns,
                is_one_shot=one_shot,
                output_tokens=int(ot or 0),
                reasoning_tokens=int(rt or 0),
                first_ts=str(r["first_ts"] or ""),
                outcome_success=success,
                outcome_tier=tier,
            )
        )
    return facts


def _compose_success(
    session_id: str,
    *,
    ground_truth: dict[str, dict[str, Any]],
    static_outcome: dict[str, int | None],
    grade_success: float | None,
    is_one_shot: bool,
    num_turns: int,
) -> tuple[int | None, str | None]:
    """Walk the four tiers in precedence order; the first non-None wins."""
    gt = ground_truth.get(session_id)
    if gt is not None:
        val = _outcome_from_ground_truth(gt)
        if val is not None:
            return val, "ground_truth"
    st = static_outcome.get(session_id)
    if st is not None:
        return st, "code_delta"
    gr = _outcome_from_grade(grade_success)
    if gr is not None:
        return gr, "llm_grade"
    bh = _outcome_from_behavior(is_one_shot, num_turns)
    if bh is not None:
        return bh, "behavioral"
    return None, None


def _load_grades(conn: sqlite3.Connection) -> dict[str, float]:
    """``session_id → grades.success`` from ``session_quality_metrics`` (real
    grades only ever persist there)."""
    if not _table_exists(conn, "session_quality_metrics"):
        return {}
    try:
        rows = conn.execute(
            "SELECT session_id, grades_json FROM session_quality_metrics"
        ).fetchall()
    except sqlite3.Error:
        return {}
    out: dict[str, float] = {}
    for r in rows:
        try:
            grades = json.loads(r["grades_json"])
            if isinstance(grades, dict) and "success" in grades:
                out[str(r["session_id"])] = float(grades["success"])
        except (TypeError, ValueError):
            continue
    return out


def _load_static(
    conn: sqlite3.Connection, session_ids: set[str]
) -> tuple[dict[str, str], dict[str, int | None]]:
    """Return ``(dominant_language, net_outcome)`` from ``static_analysis_findings``.

    Reuses the canonical ``get_session_quality`` reader per session that has
    findings (bounded — most sessions have none), so the improved/regressed
    polarity matches the rest of the product exactly.
    """
    if not _table_exists(conn, "static_analysis_findings") or not session_ids:
        return {}, {}
    try:
        rows = conn.execute(
            "SELECT DISTINCT session_id FROM static_analysis_findings"
        ).fetchall()
    except sqlite3.Error:
        return {}, {}
    analyzed = {str(r["session_id"]) for r in rows} & session_ids
    if not analyzed:
        return {}, {}

    from stackunderflow.services import static_analysis

    langs: dict[str, str] = {}
    outcomes: dict[str, int | None] = {}
    for sid in analyzed:
        try:
            quality = static_analysis.get_session_quality(conn, sid)
        except Exception:  # noqa: BLE001,S112 — advisory: skip a bad session
            continue
        summary = quality.summary or {}
        languages = summary.get("languages") or []
        if languages:
            langs[sid] = str(languages[0])
        outcomes[sid] = _outcome_from_static(summary.get("metrics") or {})
    return langs, outcomes


def _load_ground_truth(
    conn: sqlite3.Connection, session_ids: set[str]
) -> dict[str, dict[str, Any]]:
    """Return ``session_id → outcomes`` for sessions with a commit link.

    Bounded to commit-linked sessions (most stores have few), reusing the
    canonical ``outcome_attribution.get_outcomes_for_session`` so PR-matching
    stays in one place.
    """
    if not _table_exists(conn, "commit_session_link") or not session_ids:
        return {}
    try:
        rows = conn.execute(
            "SELECT DISTINCT session_id FROM commit_session_link"
        ).fetchall()
    except sqlite3.Error:
        return {}
    linked = {str(r["session_id"]) for r in rows} & session_ids
    if not linked:
        return {}

    from stackunderflow.services import outcome_attribution

    out: dict[str, dict[str, Any]] = {}
    for sid in linked:
        try:
            out[sid] = outcome_attribution.get_outcomes_for_session(conn, sid)
        except Exception:  # noqa: BLE001,S112 — advisory: skip a bad session
            continue
    return out


def _load_reasoning(
    conn: sqlite3.Connection,
    *,
    scope: Scope | None,
    project_ids: list[int] | None,
) -> dict[str, tuple[int, int]]:
    """``session_id → (reasoning_tokens, output_tokens)`` from ``usage_events``.

    Reasoning is descriptive-only (0 for providers with no wire count); this is
    surfaced, never scored. ``reasoning_tokens`` stays a subset of output.
    """
    if not _table_exists(conn, "usage_events"):
        return {}
    sql = (
        "SELECT session_id, "
        "       COALESCE(SUM(reasoning_tokens), 0) AS rt, "
        "       COALESCE(SUM(output_tokens), 0) AS ot "
        "FROM usage_events WHERE 1=1 "
    )
    params: list[Any] = []
    if project_ids:
        placeholders = ",".join("?" for _ in project_ids)
        sql += f"AND project_id IN ({placeholders}) "
        params.extend(project_ids)
    if scope is not None and scope.since is not None:
        sql += "AND ts >= ? "
        params.append(scope.since)
    if scope is not None and scope.until is not None:
        sql += "AND ts <= ? "
        params.append(scope.until)
    sql += "GROUP BY session_id "
    try:
        rows = conn.execute(sql, params).fetchall()
    except sqlite3.Error:
        return {}
    return {str(r["session_id"]): (int(r["rt"] or 0), int(r["ot"] or 0)) for r in rows}


# ── per-model per-cell statistics ────────────────────────────────────────────


@dataclass(slots=True)
class _ModelCell:
    model: str
    facts: list[_SessionFact] = field(default_factory=list)

    @property
    def n(self) -> int:
        return len(self.facts)

    @property
    def qualified(self) -> bool:
        return self.n >= bs.MIN_SESSIONS_PER_CELL

    def measured(self) -> list[_SessionFact]:
        return [f for f in self.facts if f.outcome_success is not None]

    def success_count(self) -> int:
        return sum(1 for f in self.measured() if f.outcome_success == 1)

    def success_rate(self) -> float | None:
        m = self.measured()
        return (self.success_count() / len(m)) if m else None

    def total_cost(self) -> float:
        return sum(f.cost_usd for f in self.facts)

    def cost_per_outcome(self) -> float | None:
        succ = self.success_count()
        return (self.total_cost() / succ) if succ > 0 else None

    def median_cost(self) -> float:
        return statistics.median([f.cost_usd for f in self.facts]) if self.facts else 0.0

    def median_turns(self) -> float:
        return statistics.median([f.num_turns for f in self.facts]) if self.facts else 0.0

    def reasoning_share(self) -> float:
        ot = sum(f.output_tokens for f in self.facts)
        rt = sum(f.reasoning_tokens for f in self.facts)
        return (rt / ot) if ot > 0 else 0.0


def _cost_per_outcome_ci(
    facts: list[_SessionFact], *, ci_level: float
) -> tuple[float, float] | None:
    """Seeded ratio bootstrap of Σcost / Σsuccess. ``None`` when < 2 successes.

    Resamples sessions with replacement; each resample yields Σcost/Σsuccess
    (resamples with no successes are skipped). Deterministic via the pinned
    seed, so the CI is reproducible.
    """
    pairs = [(f.cost_usd, 1 if f.outcome_success == 1 else 0) for f in facts]
    total_succ = sum(s for _, s in pairs)
    if total_succ < 2 or len(pairs) < 2:
        return None
    import random

    rng = random.Random(bs.SEED)  # noqa: S311 — deterministic resampling, not crypto
    n = len(pairs)
    ratios: list[float] = []
    for _ in range(bs.BOOTSTRAP_ITERS):
        cost_sum = 0.0
        succ_sum = 0
        for _ in range(n):
            c, s = pairs[rng.randrange(n)]
            cost_sum += c
            succ_sum += s
        if succ_sum > 0:
            ratios.append(cost_sum / succ_sum)
    if not ratios:
        return None
    ratios.sort()
    alpha = (1.0 - ci_level) / 2.0
    return (bs.percentile(ratios, alpha), bs.percentile(ratios, 1.0 - alpha))


def _two_proportion_pvalue(s1: int, n1: int, s2: int, n2: int) -> float:
    """Two-sided two-proportion z-test p-value (normal approx, stdlib).

    Used only to feed Benjamini–Hochberg (§4.4). Degenerate inputs (zero
    variance) return ``1.0`` — no evidence of a difference.
    """
    if n1 == 0 or n2 == 0:
        return 1.0
    p1, p2 = s1 / n1, s2 / n2
    p_pool = (s1 + s2) / (n1 + n2)
    var = p_pool * (1.0 - p_pool) * (1.0 / n1 + 1.0 / n2)
    if var <= 0:
        return 1.0
    z = (p1 - p2) / (var ** 0.5)
    return 2.0 * (1.0 - statistics.NormalDist().cdf(abs(z)))


# ── verdict assembly ─────────────────────────────────────────────────────────


def analyze_benchmark(
    conn: sqlite3.Connection,
    *,
    scope: Scope | None = None,
    project_ids: list[int] | None = None,
    intent: str | None = None,
    weights: dict[str, float] | None = None,
    ci_level: float = bs.CI_LEVEL,
) -> dict[str, Any]:
    """Compute the comparative benchmark verdict over *scope*.

    Args:
        conn: Open store connection; reads only, guarded so a schemaless DB
            returns an empty-but-valid verdict.
        scope: Optional timestamp window (``None`` = all time).
        project_ids: Optional ``projects.id`` filter (``None`` = whole store).
        intent: Optional single-intent filter (only that stratum family).
        weights: Composite weights; defaults to the ratified rubric v1.
        ci_level: Confidence level for every interval (default 0.90).

    Returns:
        The full report dict (see the module docstring / spec §6.1). Always
        well-formed; ``verdict.headline`` is ``"insufficient evidence"`` on any
        degenerate store.
    """
    used_weights = _resolve_weights(weights)
    try:
        facts = _load_facts(conn, scope=scope, project_ids=project_ids)
    except Exception:  # noqa: BLE001 — advisory: never raise from the report
        facts = []

    if intent:
        facts = [f for f in facts if f.intent == intent]

    return _assemble(facts, weights=used_weights, ci_level=ci_level)


def _resolve_weights(weights: dict[str, float] | None) -> dict[str, float]:
    """Return normalized composite weights, defaulting to rubric v1."""
    if not weights:
        return dict(DEFAULT_WEIGHTS)
    picked = {k: float(weights.get(k, DEFAULT_WEIGHTS[k])) for k in DEFAULT_WEIGHTS}
    total = sum(picked.values())
    if total <= 0:
        return dict(DEFAULT_WEIGHTS)
    return {k: v / total for k, v in picked.items()}


def _empty_report(
    weights: dict[str, float], ci_level: float, *, sessions_total: int = 0
) -> dict[str, Any]:
    return {
        "verdict": {
            "headline": "insufficient evidence",
            "winning_model": None,
            "confidence": "none",
            "cost_per_outcome_usd": None,
            "runner_up": None,
            "caveats": [
                "Not enough comparable evidence to name a winner yet.",
            ],
        },
        "strata": [],
        "coverage": {
            "sessions_total": sessions_total,
            "sessions_scored": 0,
            "grade_coverage": 0.0,
        },
        "rubric_version": RUBRIC_VERSION,
        "weights": weights,
        "ci_level": ci_level,
        "success_threshold": SUCCESS_THRESHOLD,
        "warning": NATURAL_EXPERIMENT_WARNING,
        "method_notes": list(_METHOD_NOTES),
    }


def _assemble(
    facts: list[_SessionFact],
    *,
    weights: dict[str, float],
    ci_level: float,
) -> dict[str, Any]:
    """Turn per-session facts into the stratified verdict payload."""
    if not facts:
        return _empty_report(weights, ci_level)

    # ── coverage ──────────────────────────────────────────────────────────
    sessions_total = len(facts)
    sessions_scored = sum(1 for f in facts if f.outcome_success is not None)
    grade_scored = sum(1 for f in facts if f.outcome_tier == "llm_grade")
    coverage = {
        "sessions_total": sessions_total,
        "sessions_scored": sessions_scored,
        "grade_coverage": round(grade_scored / sessions_total, 4) if sessions_total else 0.0,
    }

    # ── stratify: (intent, size_band) → model → cell ──────────────────────
    strata: dict[tuple[str, str], dict[str, _ModelCell]] = {}
    for f in facts:
        key = (f.intent, f.size_band)
        cell = strata.setdefault(key, {}).setdefault(
            f.primary_model, _ModelCell(model=f.primary_model)
        )
        cell.facts.append(f)

    # ── p-value family for BH-FDR (success difference per cell-pair) ──────
    pvalues: list[float] = []
    pval_index: dict[tuple[tuple[str, str], str, str], int] = {}
    for key, models in strata.items():
        qualified = sorted(m for m, c in models.items() if c.qualified)
        for i in range(len(qualified)):
            for j in range(i + 1, len(qualified)):
                a, b = models[qualified[i]], models[qualified[j]]
                ma, mb = a.measured(), b.measured()
                pval_index[(key, a.model, b.model)] = len(pvalues)
                pvalues.append(
                    _two_proportion_pvalue(
                        a.success_count(), len(ma), b.success_count(), len(mb)
                    )
                )
    reject = bs.benjamini_hochberg(pvalues, alpha=1.0 - ci_level) if pvalues else []

    def _pair_significant(key: tuple[str, str], m1: str, m2: str) -> bool:
        idx = pval_index.get((key, m1, m2))
        if idx is None:
            idx = pval_index.get((key, m2, m1))
        return bool(idx is not None and idx < len(reject) and reject[idx])

    # ── per-stratum payload + cell winners ────────────────────────────────
    strata_payload: list[dict[str, Any]] = []
    # winner-tracking for the cross-task headline
    clear_wins: dict[str, int] = {}
    clear_losses: dict[str, int] = {}
    balanced_n: dict[str, int] = {}
    cell_win_widths: dict[str, list[float]] = {}
    cost_accum: dict[str, tuple[float, int]] = {}  # model → (Σcost, Σsucc) over clear-win cells

    for key in sorted(strata.keys()):
        intent_lbl, size_lbl = key
        models = strata[key]
        qualified = [c for c in models.values() if c.qualified]
        for c in qualified:
            balanced_n[c.model] = balanced_n.get(c.model, 0) + c.n

        model_rows = [_model_row(c, ci_level=ci_level) for c in models.values()]
        _fill_composites(model_rows, weights)
        model_rows.sort(key=lambda r: (r["qualified"], r["composite"]), reverse=True)

        cell_verdict = "insufficient evidence"
        winner: str | None = None
        effect: dict[str, Any] = {}
        qrows = [r for r in model_rows if r["qualified"]]
        if len(qrows) >= bs.MIN_MODELS_PER_CELL:
            top, second = qrows[0], qrows[1]
            winner = top["model"]
            sr_diff = bs.risk_difference(
                top["success_rate"]["point"] or 0.0,
                second["success_rate"]["point"] or 0.0,
            )
            cost_rel = _cost_effect(top, second)
            practical = (
                abs(sr_diff) >= bs.MIN_EFFECT_SUCCESS or cost_rel >= bs.MIN_EFFECT_COST
            )
            statistical = _pair_significant(key, top["model"], second["model"])
            effect = {
                "success_risk_difference": round(sr_diff, 4),
                "cost_relative_delta": round(cost_rel, 4),
                "statistically_separated": statistical,
                "practically_separated": practical,
            }
            if practical and statistical:
                cell_verdict = "clear"
                clear_wins[winner] = clear_wins.get(winner, 0) + 1
                clear_losses[second["model"]] = clear_losses.get(second["model"], 0) + 1
                wc = models[winner]
                cs, ss = cost_accum.get(winner, (0.0, 0))
                cost_accum[winner] = (cs + wc.total_cost(), ss + wc.success_count())
                w_ci = top["success_rate"].get("ci_wilson") or [0.0, 1.0]
                cell_win_widths.setdefault(winner, []).append(w_ci[1] - w_ci[0])
            else:
                cell_verdict = "weak"

        strata_payload.append(
            {
                "intent": intent_lbl,
                "size_band": size_lbl,
                "models": model_rows,
                "assignment_balance": {c.model: c.n for c in models.values()},
                "cell_verdict": cell_verdict,
                "winner": winner,
                "effect": effect,
            }
        )

    verdict = _headline(
        intent_filter=None,
        clear_wins=clear_wins,
        clear_losses=clear_losses,
        balanced_n=balanced_n,
        cell_win_widths=cell_win_widths,
        cost_accum=cost_accum,
    )

    return {
        "verdict": verdict,
        "strata": strata_payload,
        "coverage": coverage,
        "rubric_version": RUBRIC_VERSION,
        "weights": weights,
        "ci_level": ci_level,
        "success_threshold": SUCCESS_THRESHOLD,
        "warning": NATURAL_EXPERIMENT_WARNING,
        "method_notes": list(_METHOD_NOTES),
    }


def _model_row(cell: _ModelCell, *, ci_level: float) -> dict[str, Any]:
    """Per-model row inside a stratum (matches spec §6.1)."""
    sr = cell.success_rate()
    measured = cell.measured()
    wilson = (
        list(bs.wilson_interval(cell.success_count(), len(measured), ci_level=ci_level))
        if measured
        else None
    )
    cost_ci = bs.percentile_bootstrap_ci(
        [f.cost_usd for f in cell.facts], statistic="median", ci_level=ci_level
    )
    cpo = cell.cost_per_outcome()
    cpo_ci = _cost_per_outcome_ci(cell.facts, ci_level=ci_level)
    return {
        "model": cell.model,
        "n": cell.n,
        "qualified": cell.qualified,
        "coverage": round(len(measured) / cell.n, 4) if cell.n else 0.0,
        "success_measured_n": len(measured),
        "success_rate": {
            "point": round(sr, 4) if sr is not None else None,
            "ci_wilson": [round(x, 4) for x in wilson] if wilson else None,
        },
        "cost_per_outcome": {
            "point": round(cpo, 6) if cpo is not None else None,
            "ci": [round(x, 6) for x in cpo_ci] if cpo_ci else None,
        },
        "median_cost": {
            "point": round(cell.median_cost(), 6),
            "ci": [round(x, 6) for x in cost_ci],
        },
        "median_turns": round(cell.median_turns(), 2),
        "reasoning_share": round(cell.reasoning_share(), 4),
        "composite": 0.0,  # filled by _fill_composites (normalized across the cell)
    }


def _fill_composites(rows: list[dict[str, Any]], weights: dict[str, float]) -> None:
    """Fill each row's normalized composite in ``[0, 1]`` (mutates ``rows``).

    The composite blends three axes, each normalized **within the stratum** so
    the comparison is like-for-like: success rate (already 0..1, higher wins),
    cost (min-max inverse — cheapest gets 1.0), and effort/turns (min-max
    inverse — fewest gets 1.0). Reasoning efficiency is deliberately absent —
    descriptive only, never scored (§3.3). A single-model cell scores every
    axis at its best (nothing to compare against).
    """
    if not rows:
        return

    def _cost_of(r: dict[str, Any]) -> float:
        cpo = r["cost_per_outcome"]["point"]
        return cpo if cpo is not None else r["median_cost"]["point"]

    costs = [_cost_of(r) for r in rows]
    turns = [r["median_turns"] for r in rows]
    for r in rows:
        success = r["success_rate"]["point"] or 0.0
        cost_norm = _inverse_minmax(costs, _cost_of(r))
        effort_norm = _inverse_minmax(turns, r["median_turns"])
        composite = (
            weights["success"] * success
            + weights["cost"] * cost_norm
            + weights["effort"] * effort_norm
        )
        r["composite"] = round(min(1.0, max(0.0, composite)), 4)


def _inverse_minmax(values: list[float], v: float) -> float:
    """Min-max *inverse* normalization: the smallest value → 1.0, largest → 0.0.

    Used for cost + effort where lower is better. A flat set (all equal, or a
    single model) → 1.0, so a lone model isn't penalized for having no rival.
    """
    lo, hi = min(values), max(values)
    if hi <= lo:
        return 1.0
    return 1.0 - (v - lo) / (hi - lo)


def _cost_effect(top: dict[str, Any], second: dict[str, Any]) -> float:
    """Relative cost advantage of the composite winner over the runner-up.

    Uses cost-per-outcome when both have it, else median cost. Positive ⇒ the
    winner is cheaper by that fraction.
    """
    tw = top["cost_per_outcome"]["point"]
    sw = second["cost_per_outcome"]["point"]
    if tw is not None and sw is not None:
        return bs.relative_delta(tw, sw)
    return bs.relative_delta(
        top["median_cost"]["point"], second["median_cost"]["point"]
    )


def _headline(
    *,
    intent_filter: str | None,
    clear_wins: dict[str, int],
    clear_losses: dict[str, int],
    balanced_n: dict[str, int],
    cell_win_widths: dict[str, list[float]],
    cost_accum: dict[str, tuple[float, int]],
) -> dict[str, Any]:
    """Decide the cross-task winner (§4.4) — or refuse.

    A headline winner must win the composite in ≥2 strata (with a real, BH-
    significant separation), never clearly lose a stratum, and clear the
    balanced-sample floor. Otherwise the verdict is "insufficient evidence".
    """
    candidates = [
        m
        for m in clear_wins
        if clear_wins[m] >= 2
        and clear_losses.get(m, 0) == 0
        and balanced_n.get(m, 0) >= bs.MIN_BALANCED_TOTAL
    ]
    if len(candidates) != 1:
        return {
            "headline": "insufficient evidence",
            "winning_model": None,
            "confidence": "none",
            "cost_per_outcome_usd": None,
            "runner_up": None,
            "caveats": _headline_caveats(candidates, clear_wins, balanced_n),
        }

    winner = candidates[0]
    # runner-up = next model by clear wins (0 if none)
    others = sorted(
        (m for m in clear_wins if m != winner),
        key=lambda m: clear_wins[m],
        reverse=True,
    )
    runner_up = others[0] if others else None

    cost_sum, succ_sum = cost_accum.get(winner, (0.0, 0))
    cost_per_outcome = (cost_sum / succ_sum) if succ_sum > 0 else None

    confidence = _confidence(
        winner=winner,
        clear_wins=clear_wins,
        clear_losses=clear_losses,
        balanced_n=balanced_n,
        cell_win_widths=cell_win_widths,
    )
    label = f"{winner} wins" + (f" for {intent_filter}" if intent_filter else "")
    return {
        "headline": label,
        "winning_model": winner,
        "confidence": confidence,
        "cost_per_outcome_usd": round(cost_per_outcome, 6) if cost_per_outcome else None,
        "runner_up": runner_up,
        "caveats": [
            "Winner holds across "
            f"{clear_wins[winner]} strata with no stratum where it clearly loses.",
            NATURAL_EXPERIMENT_WARNING,
        ],
    }


def _headline_caveats(
    candidates: list[str], clear_wins: dict[str, int], balanced_n: dict[str, int]
) -> list[str]:
    if len(candidates) > 1:
        return [
            "More than one model qualifies as a cross-task winner — no single "
            "winner can be named. Compare the per-stratum table instead.",
        ]
    if clear_wins:
        best = max(clear_wins, key=lambda m: clear_wins[m])
        return [
            f"The strongest model ({best}) wins {clear_wins[best]} stratum/strata "
            f"(need ≥2) with {balanced_n.get(best, 0)} balanced sessions "
            f"(need ≥{bs.MIN_BALANCED_TOTAL}) — not enough to headline a winner.",
        ]
    return ["Not enough comparable evidence to name a winner yet."]


def _confidence(
    *,
    winner: str,
    clear_wins: dict[str, int],
    clear_losses: dict[str, int],
    balanced_n: dict[str, int],
    cell_win_widths: dict[str, list[float]],
) -> str:
    """Product-of-terms confidence (mirrors ``mode_recommender``) → bucket."""
    n = balanced_n.get(winner, 0)
    sample_term = min(1.0, n / (2.0 * bs.MIN_BALANCED_TOTAL))
    wins = clear_wins.get(winner, 0)
    agreement_term = min(1.0, wins / 3.0)  # 3+ agreeing strata → full marks
    widths = cell_win_widths.get(winner) or [1.0]
    ci_term = max(0.0, 1.0 - (sum(widths) / len(widths)))
    score = sample_term * agreement_term * ci_term
    return bs.confidence_bucket(score)


# ── recommendation (outcome-aware; §1 / §6.2) ────────────────────────────────


def recommend_from_history(
    conn: sqlite3.Connection,
    *,
    intent: str,
    size: str | None = None,
    language: str | None = None,
    scope: Scope | None = None,
    project_ids: list[int] | None = None,
    weights: dict[str, float] | None = None,
    ci_level: float = bs.CI_LEVEL,
) -> dict[str, Any]:
    """Outcome-aware model pick for a *described* task (the successor to the
    cost-only ``mode_recommender``).

    Restricts the benchmark to the matching stratum family and returns the
    winning model with its evidence — or an honest "insufficient evidence".
    Always returns a well-formed dict; never raises.
    """
    report = analyze_benchmark(
        conn,
        scope=scope,
        project_ids=project_ids,
        intent=intent,
        weights=weights,
        ci_level=ci_level,
    )
    # Filter strata to the requested size (and language, when both known).
    strata = report.get("strata") or []
    if size:
        strata = [s for s in strata if s.get("size_band") == size]

    # Prefer a matching clear cell; fall back to the headline verdict.
    best_cell = None
    for s in strata:
        if s.get("cell_verdict") == "clear" and s.get("winner"):
            best_cell = s
            break

    verdict = report.get("verdict") or {}
    if best_cell is not None:
        winner = best_cell["winner"]
        row = next(
            (m for m in best_cell["models"] if m["model"] == winner), None
        )
        return {
            "intent": intent,
            "size": size,
            "language": language,
            "recommended_model": winner,
            "confidence": "medium" if verdict.get("winning_model") != winner else verdict.get("confidence", "medium"),
            "basis": "stratum",
            "stratum": {"intent": best_cell["intent"], "size_band": best_cell["size_band"]},
            "evidence": row,
            "rationale": (
                f"In {best_cell['intent']} × {best_cell['size_band']} tasks, "
                f"{winner} wins on the composite with a real, significant "
                f"separation from the runner-up."
            ),
            "rubric_version": RUBRIC_VERSION,
            "weights": report.get("weights"),
        }

    return {
        "intent": intent,
        "size": size,
        "language": language,
        "recommended_model": verdict.get("winning_model"),
        "confidence": verdict.get("confidence", "none"),
        "basis": "headline" if verdict.get("winning_model") else "insufficient_evidence",
        "stratum": None,
        "evidence": None,
        "rationale": (
            verdict.get("caveats", ["Not enough comparable evidence yet."]) or [""]
        )[0],
        "rubric_version": RUBRIC_VERSION,
        "weights": report.get("weights"),
    }
