"""Mode recommender — heuristic v1 (Spec 18 / GitHub issue #88).

Pattern-match an incoming prompt against the user's own past sessions
and recommend the cheapest model that historically solved similar
tasks. *Heuristic v1, not ML* — the full benchmark engine is Spec 26.

Why this exists
---------------
Every team is overpaying by routing the wrong tasks to the wrong model
(e.g. running Opus on a one-line typo fix). The user's own history is
the best ground truth they have: if the last 12 "fix a flake" tasks
they ran on Sonnet all came in cheap and one-shot, the next "fix a
flake" prompt should default to Sonnet, not Opus.

Public surface
--------------
- :func:`recommend(conn, prompt, current_model=None) -> dict`
  Same payload powers the CLI (``stackunderflow recommend mode``), the
  MCP tool (``recommend_mode``) and the meta-agent (``recommend_mode``).
- :func:`extract_features(prompt) -> dict`
  Pulled out as ``_extract_features`` per the spec but exported sans
  underscore for tests + introspection.

Heuristic ("similar task")
--------------------------
A past session is *similar* to the incoming prompt when **all three**
hold:

1. **Same intent.** :func:`_intent_of` maps the prompt to one of
   ``{build, fix, refactor, test, explore}`` via a small keyword list
   (built / add / implement / fix / debug / refactor / etc). Sessions
   whose first user turn maps to the same intent are candidates.
2. **Token-count band.** The incoming prompt's character count divided
   by 4 (Claude-style rough token estimate) is bucketed into one of
   ``{tiny<200, small<800, med<3000, large}``. Past sessions whose
   first user turn falls in the same band are kept.
3. **Language overlap.** :func:`_language_hints` extracts known
   language hints (``python`` / ``typescript`` / ``rust`` / etc) from
   the prompt and from each candidate. Candidates with a non-empty
   overlap pass; if the incoming prompt declares zero languages the
   filter is a no-op (any candidate passes).

Cost-delta math
---------------
Among candidates we group by ``primary_model`` (from ``session_mart``)
and compute median ``cost_usd`` per session. The cheapest model wins.
``cost_delta_usd`` is ``(median_current_model_cost - median_pick_cost)``
in a *typical session of this shape*; positive ⇒ the user would have
saved that much per task by switching. ``cost_usd`` is read off
``session_mart.cost_usd`` (which itself is rolled up from
``usage_events.cost_usd`` post-v0.8.0 cost-fix), never recomputed in
this layer.

Confidence
----------
Three terms multiplied together (see :func:`_compute_confidence`):

- **Sample size:** ``min(1, similar_count / 5)``. Below 5 similar
  sessions, confidence is < 1.0.
- **Spread:** ``1 - σ/μ`` of cost across candidates of the picked
  model, clamped to [0, 1]. Tight cluster ⇒ high confidence.
- **Cost gap:** how much cheaper the pick is vs the median across
  *all* candidate models. ``min(1, gap_ratio)``.

Empty store / zero matches → ``confidence = 0.0`` and a clean
"no historical data" message in ``rationale``.

Cache
-----
Recommendations are cached for 24h in the ``mode_recommendations``
table (v016 migration). The cache key is ``md5(json.dumps(features,
sort_keys=True))`` so a re-extracted-feature change naturally
invalidates entries.
"""

from __future__ import annotations

import hashlib
import json
import re
import sqlite3
import statistics
from dataclasses import dataclass
from datetime import UTC, datetime, timedelta
from typing import Any

__all__ = [
    "Recommendation",
    "extract_features",
    "hash_features",
    "find_similar_past_sessions",
    "recommend",
    "CACHE_TTL_HOURS",
    "TOKEN_BANDS",
]

# ── tunables ────────────────────────────────────────────────────────────────

CACHE_TTL_HOURS = 24

# Token bands for the "same shape" filter. Edges are token-count thresholds
# (chars/4 estimate) — anything below the upper bound of a band falls in it.
# Keeping bands wide on purpose: v1 is matching shape, not token-budget
# accounting.
TOKEN_BANDS: tuple[tuple[str, int], ...] = (
    ("tiny", 200),
    ("small", 800),
    ("med", 3000),
    ("large", 10**9),  # catch-all
)

# Intent keyword lookup. Earlier patterns win on overlap (a prompt that
# says "fix the test" maps to ``fix``, not ``test``). Keep the keyword
# lists short and obvious — heuristic v1, not NLP.
_INTENT_KEYWORDS: tuple[tuple[str, tuple[str, ...]], ...] = (
    (
        "fix",
        ("fix", "bug", "broken", "regression", "debug", "patch",
         "error", "fail", "crash", "stack trace"),
    ),
    (
        "refactor",
        ("refactor", "clean up", "rename", "extract", "rewrite",
         "simplify", "deduplicate", "tidy", "untangle"),
    ),
    (
        "test",
        ("test", "tests", "unit test", "pytest", "spec", "coverage",
         "fixture", "snapshot test"),
    ),
    (
        "build",
        ("build", "add", "implement", "create", "new feature",
         "ship", "write a", "scaffold"),
    ),
    (
        "explore",
        ("explain", "what does", "how does", "summarise", "summarize",
         "describe", "trace", "show me", "find", "search"),
    ),
)

# Language hints. Lowercased substring match. The list is intentionally
# short and high-signal — extending it is cheap (no ALTER TABLE because
# the cache key auto-invalidates on feature changes).
_LANGUAGE_HINTS: tuple[tuple[str, tuple[str, ...]], ...] = (
    ("python",     ("python", ".py", "pytest", "django", "flask", "fastapi")),
    ("typescript", ("typescript", ".ts", ".tsx", "react ", "vite", "next.js")),
    ("javascript", ("javascript", ".js ", " js ", "node.js", "nodejs", "npm ")),
    ("rust",       ("rust", ".rs", "cargo ")),
    ("go",         (" go ", ".go", "golang", "go mod ")),
    ("sql",        ("sql", "sqlite", "postgres", "select ", "create table")),
    ("shell",      ("bash", "zsh", "shell", ".sh ", "#!/bin/")),
    ("html",       ("html", ".html", "<div", "<span")),
    ("css",        ("css", ".css", "tailwind")),
)

# How many past sessions (max) we pull for the similarity scan. The scan
# is in-Python on the candidate set; 200 keeps it cheap on huge stores.
_PAST_SESSION_SCAN_LIMIT = 200

# Minimum count of similar sessions before we'll surface a non-zero
# confidence. Below this, ``recommend`` falls through to "no historical
# data" with confidence 0.0 — the spec's required clean empty path.
_MIN_SIMILAR_FOR_RECOMMENDATION = 3


# ── data shape ──────────────────────────────────────────────────────────────


@dataclass(frozen=True)
class Recommendation:
    """One recommend() result. JSON-serialisable via :meth:`to_dict`.

    ``recommended_model`` is the cheapest model whose past similar
    sessions had the lowest median cost. ``current_model`` is what the
    caller passed in (or ``None``). ``cost_delta_usd`` is positive when
    switching would *save* money — typically ``median_current_cost -
    median_recommended_cost`` in a session of this shape; ``0.0`` when
    the recommended model **is** the current one or no current model
    was passed.
    """

    recommended_model: str
    current_model: str | None
    confidence: float
    cost_delta_usd: float
    similar_session_count: int
    evidence_session_ids: list[str]
    features: dict[str, Any]
    task_pattern_hash: str
    rationale: str
    cache_hit: bool

    def to_dict(self) -> dict[str, Any]:
        return {
            "recommended_model": self.recommended_model,
            "current_model": self.current_model,
            "confidence": round(float(self.confidence), 4),
            "cost_delta_usd": round(float(self.cost_delta_usd), 6),
            "similar_session_count": int(self.similar_session_count),
            "evidence_session_ids": list(self.evidence_session_ids),
            "features": dict(self.features),
            "task_pattern_hash": self.task_pattern_hash,
            "rationale": self.rationale,
            "cache_hit": bool(self.cache_hit),
        }


# ── feature extraction ──────────────────────────────────────────────────────


def _intent_of(prompt: str) -> str:
    """Map the prompt to a coarse intent label.

    Scans the keyword lists in declaration order and returns the **first
    label whose keyword set has the most hits**. Ties broken by
    declaration order (so ``fix`` beats ``test`` on a "fix the test"
    prompt). Default ``"explore"`` when nothing matches — read-only
    questions are the most common no-keyword case.
    """
    if not prompt:
        return "explore"
    lowered = prompt.lower()
    best: tuple[str, int] = ("explore", 0)
    for label, keywords in _INTENT_KEYWORDS:
        hits = sum(1 for kw in keywords if kw in lowered)
        if hits > best[1]:
            best = (label, hits)
    return best[0]


def _token_band(prompt: str) -> str:
    """Return the band label for the rough token count of ``prompt``.

    Uses the well-known ``len(text) // 4`` Anthropic-style estimate
    rather than spinning up a tokenizer (the recommender runs on every
    prompt so the budget for feature extraction is microseconds).
    """
    n = max(0, len(prompt or "")) // 4
    for label, upper in TOKEN_BANDS:
        if n < upper:
            return label
    return TOKEN_BANDS[-1][0]


def _language_hints(prompt: str) -> list[str]:
    """Return the alphabetised list of language labels mentioned in ``prompt``."""
    if not prompt:
        return []
    lowered = prompt.lower()
    out: set[str] = set()
    for label, hints in _LANGUAGE_HINTS:
        for hint in hints:
            if hint in lowered:
                out.add(label)
                break
    return sorted(out)


_FILE_MENTION_RE = re.compile(
    r"(?:[\w./-]+/)?[\w.-]+\.(py|ts|tsx|js|jsx|rs|go|sql|sh|html|css|md|json|yaml|yml|toml)\b",
    re.IGNORECASE,
)
_CODE_BLOCK_RE = re.compile(r"```")


def extract_features(prompt: str) -> dict[str, Any]:
    """Pull the heuristic v1 feature dict from ``prompt``.

    Keys are stable + sorted so :func:`hash_features` is deterministic.

    * ``intent`` — one of build / fix / refactor / test / explore.
    * ``token_band`` — one of tiny / small / med / large.
    * ``languages`` — sorted list of detected language labels.
    * ``file_mentions`` — count of file-path-shaped tokens.
    * ``code_blocks`` — count of ```` ``` ```` fence pairs (hint at
      pasted code).
    """
    text = prompt or ""
    file_mentions = len(_FILE_MENTION_RE.findall(text))
    code_fences = len(_CODE_BLOCK_RE.findall(text))
    return {
        "intent":        _intent_of(text),
        "token_band":    _token_band(text),
        "languages":     _language_hints(text),
        "file_mentions": int(file_mentions),
        "code_blocks":   code_fences // 2,  # opens+closes → pairs
    }


def hash_features(features: dict[str, Any]) -> str:
    """Stable md5 hex digest of the feature dict.

    JSON serialisation with ``sort_keys=True`` makes the hash
    feature-order-insensitive; lists are stable because
    :func:`extract_features` already sorts them.
    """
    encoded = json.dumps(features, sort_keys=True, separators=(",", ":"))
    return hashlib.md5(encoded.encode("utf-8")).hexdigest()


# ── similarity scan ─────────────────────────────────────────────────────────


@dataclass(frozen=True)
class _PastSession:
    """One past-session candidate row pulled from the store."""
    session_id: str
    primary_model: str
    cost_usd: float
    first_user_text: str


def _fetch_past_sessions(
    conn: sqlite3.Connection,
    *,
    limit: int = _PAST_SESSION_SCAN_LIMIT,
) -> list[_PastSession]:
    """Pull the most recent N completed sessions with a primary_model.

    Joins ``session_mart`` (for ``primary_model`` + ``cost_usd``) to the
    first user message of each session in ``messages`` (for the prompt
    text we'll feature-extract from). Sessions without a primary_model
    or without any user turn are filtered out — they can't help the
    recommender either way.

    The ``COALESCE(sm.cost_usd, 0.0)`` mirrors the discovery service's
    contract for stores that haven't been backfilled yet — those rows
    contribute 0.0 to median calculations, which the cost-gap term in
    the confidence score down-weights naturally.
    """
    if not _table_exists(conn, "session_mart"):
        return []
    rows = conn.execute(
        "SELECT s.session_id AS session_id, "
        "       sm.primary_model AS primary_model, "
        "       COALESCE(sm.cost_usd, 0.0) AS cost_usd, "
        "       (SELECT m.content_text FROM messages m "
        "        WHERE m.session_fk = s.id AND m.role = 'user' "
        "        ORDER BY m.seq ASC LIMIT 1) AS first_user_text "
        "FROM sessions s "
        "JOIN session_mart sm ON sm.session_id = s.session_id "
        "WHERE sm.primary_model IS NOT NULL "
        "  AND sm.primary_model != '' "
        "ORDER BY COALESCE(s.last_ts, '') DESC "
        "LIMIT ?",
        (int(limit),),
    ).fetchall()
    out: list[_PastSession] = []
    for r in rows:
        text = r["first_user_text"]
        if not text:
            continue
        out.append(_PastSession(
            session_id=str(r["session_id"]),
            primary_model=str(r["primary_model"]),
            cost_usd=float(r["cost_usd"] or 0.0),
            first_user_text=str(text),
        ))
    return out


def _table_exists(conn: sqlite3.Connection, name: str) -> bool:
    """True iff ``name`` is a table in the connected DB.

    Guards every store touch — fresh installs without the ETL backfill
    still have ``sessions`` + ``messages`` but ``session_mart`` is
    empty until the first refresh.
    """
    row = conn.execute(
        "SELECT 1 FROM sqlite_master WHERE type='table' AND name=?",
        (name,),
    ).fetchone()
    return row is not None


def find_similar_past_sessions(
    conn: sqlite3.Connection,
    features: dict[str, Any],
    *,
    limit: int = 20,
) -> list[_PastSession]:
    """Return past sessions whose first user turn matches ``features``.

    The match is the AND of three filters: same intent, same
    ``token_band``, non-empty language overlap (when the prompt declared
    any languages — otherwise the language filter is a no-op).
    """
    candidates = _fetch_past_sessions(conn)
    target_languages = set(features.get("languages") or [])
    target_intent = features.get("intent")
    target_band = features.get("token_band")

    matched: list[_PastSession] = []
    for c in candidates:
        cand_features = extract_features(c.first_user_text)
        if cand_features["intent"] != target_intent:
            continue
        if cand_features["token_band"] != target_band:
            continue
        if target_languages:
            cand_langs = set(cand_features["languages"])
            if not target_languages & cand_langs:
                continue
        matched.append(c)
        if len(matched) >= limit:
            break
    return matched


# ── confidence + ranking ────────────────────────────────────────────────────


def _compute_confidence(
    similar: list[_PastSession],
    pick_costs: list[float],
    other_costs: list[float],
) -> float:
    """Multiply three terms (sample size, spread, cost gap) into [0, 1].

    See module docstring for the rationale of each term. Returns 0.0
    when the sample is empty (caller should already short-circuit).
    """
    if not similar or not pick_costs:
        return 0.0
    sample_term = min(1.0, len(similar) / 5.0)

    if len(pick_costs) >= 2:
        mu = statistics.mean(pick_costs)
        sigma = statistics.pstdev(pick_costs)
        spread_term = max(0.0, min(1.0, 1.0 - (sigma / mu) if mu > 0 else 0.0))
    else:
        spread_term = 0.5  # single sample — neither high nor low confidence

    if other_costs:
        median_pick = statistics.median(pick_costs)
        median_other = statistics.median(other_costs)
        if median_other > 0:
            gap_ratio = max(0.0, (median_other - median_pick) / median_other)
            cost_gap_term = min(1.0, gap_ratio)
        else:
            cost_gap_term = 0.0
    else:
        cost_gap_term = 0.5  # only one model in the pool — meaningless gap

    return round(sample_term * spread_term * cost_gap_term, 4)


def _pick_cheapest_model(
    similar: list[_PastSession],
) -> tuple[str | None, list[float], list[float], list[str]]:
    """Group the similar set by ``primary_model`` and pick the cheapest.

    Returns ``(model_name, costs_for_pick, costs_for_other_models,
    evidence_session_ids)``. ``model_name`` is ``None`` when the input
    is empty.
    """
    if not similar:
        return (None, [], [], [])

    by_model: dict[str, list[_PastSession]] = {}
    for s in similar:
        by_model.setdefault(s.primary_model, []).append(s)

    # Median cost per model — robust to outliers (a runaway session
    # shouldn't sink an otherwise-cheap model).
    medians: dict[str, float] = {
        m: statistics.median([s.cost_usd for s in rows])
        for m, rows in by_model.items()
    }
    # Sort: cheapest median first; tie-break on more samples (more
    # evidence wins); final tie-break on model name for determinism.
    cheapest = min(
        medians.keys(),
        key=lambda m: (medians[m], -len(by_model[m]), m),
    )
    pick_rows = by_model[cheapest]
    pick_costs = [s.cost_usd for s in pick_rows]
    other_costs = [
        s.cost_usd for m, rows in by_model.items() if m != cheapest
        for s in rows
    ]
    # Cap evidence ids at 5 — enough for a user to drill into without
    # blowing the LLM context when the meta-agent surfaces this.
    evidence_ids = [s.session_id for s in pick_rows[:5]]
    return (cheapest, pick_costs, other_costs, evidence_ids)


def _cost_delta(
    similar: list[_PastSession],
    pick_model: str,
    current_model: str | None,
) -> float:
    """Median cost of ``current_model`` minus median of ``pick_model``.

    Returns 0.0 when no current model was passed, when the pick **is**
    the current model, or when the current model has no presence in the
    similar set (we can't quote a real saving for a model the user
    hasn't run on this shape before).
    """
    if not current_model or current_model == pick_model:
        return 0.0
    current_costs = [s.cost_usd for s in similar if s.primary_model == current_model]
    pick_costs = [s.cost_usd for s in similar if s.primary_model == pick_model]
    if not current_costs or not pick_costs:
        return 0.0
    return statistics.median(current_costs) - statistics.median(pick_costs)


# ── cache ───────────────────────────────────────────────────────────────────


def _now_iso() -> str:
    return datetime.now(UTC).isoformat()


def _cache_lookup(
    conn: sqlite3.Connection, task_pattern_hash: str
) -> dict[str, Any] | None:
    """Return the cached row when fresh (< CACHE_TTL_HOURS), else None.

    A cache hit bumps ``last_used_ts`` so frequently-asked patterns can
    later be analysed for skew (Spec 26 input).
    """
    if not _table_exists(conn, "mode_recommendations"):
        return None
    row = conn.execute(
        "SELECT recommended_model, confidence, evidence_session_ids, "
        "       created_ts, last_used_ts "
        "FROM mode_recommendations "
        "WHERE task_pattern_hash = ? "
        "ORDER BY id DESC LIMIT 1",
        (task_pattern_hash,),
    ).fetchone()
    if row is None:
        return None
    try:
        created = datetime.fromisoformat(row["created_ts"])
    except (TypeError, ValueError):
        return None
    if created.tzinfo is None:
        created = created.replace(tzinfo=UTC)
    if datetime.now(UTC) - created > timedelta(hours=CACHE_TTL_HOURS):
        return None
    # Bump last_used_ts; best-effort, never fatal.
    try:
        conn.execute(
            "UPDATE mode_recommendations SET last_used_ts = ? "
            "WHERE task_pattern_hash = ?",
            (_now_iso(), task_pattern_hash),
        )
    except sqlite3.Error:
        pass
    try:
        evidence = json.loads(row["evidence_session_ids"])
        if not isinstance(evidence, list):
            evidence = []
    except (TypeError, ValueError):
        evidence = []
    return {
        "recommended_model": row["recommended_model"],
        "confidence": float(row["confidence"] or 0.0),
        "evidence_session_ids": evidence,
    }


def _cache_store(
    conn: sqlite3.Connection,
    *,
    task_pattern_hash: str,
    recommended_model: str,
    confidence: float,
    evidence_session_ids: list[str],
) -> None:
    """Insert (or replace) one cache row. Best-effort; never raises."""
    if not _table_exists(conn, "mode_recommendations"):
        return
    now = _now_iso()
    try:
        conn.execute(
            "DELETE FROM mode_recommendations WHERE task_pattern_hash = ?",
            (task_pattern_hash,),
        )
        conn.execute(
            "INSERT INTO mode_recommendations "
            "(task_pattern_hash, recommended_model, confidence, "
            " evidence_session_ids, created_ts, last_used_ts) "
            "VALUES (?, ?, ?, ?, ?, ?)",
            (
                task_pattern_hash,
                recommended_model,
                float(confidence),
                json.dumps(list(evidence_session_ids)),
                now,
                now,
            ),
        )
    except sqlite3.Error:
        # A failed cache write must never break the recommendation; we
        # already have the answer to return.
        pass


# ── public entry point ──────────────────────────────────────────────────────


def recommend(
    conn: sqlite3.Connection,
    prompt: str,
    *,
    current_model: str | None = None,
    use_cache: bool = True,
) -> dict[str, Any]:
    """Return a recommendation dict for ``prompt``.

    Always returns a fully-populated dict (never raises on empty data).
    When the store has no usable history, the result has
    ``confidence == 0.0`` and a ``rationale`` of ``"no historical
    data"``; ``recommended_model`` falls back to ``current_model`` when
    one was supplied, else to an empty string. Callers should treat
    ``confidence == 0.0`` as "no opinion".

    ``use_cache`` is exposed for test isolation; the CLI / MCP / meta-
    agent paths leave it on (the default).
    """
    features = extract_features(prompt)
    pattern_hash = hash_features(features)

    if use_cache:
        hit = _cache_lookup(conn, pattern_hash)
        if hit is not None:
            return Recommendation(
                recommended_model=hit["recommended_model"],
                current_model=current_model,
                confidence=hit["confidence"],
                cost_delta_usd=0.0,  # not recomputed on cache hit (see note)
                similar_session_count=len(hit["evidence_session_ids"]),
                evidence_session_ids=hit["evidence_session_ids"],
                features=features,
                task_pattern_hash=pattern_hash,
                rationale=(
                    f"cached recommendation for {features['intent']!r}-band-"
                    f"{features['token_band']!r} task"
                ),
                cache_hit=True,
            ).to_dict()

    similar = find_similar_past_sessions(conn, features)

    if len(similar) < _MIN_SIMILAR_FOR_RECOMMENDATION:
        return Recommendation(
            recommended_model=current_model or "",
            current_model=current_model,
            confidence=0.0,
            cost_delta_usd=0.0,
            similar_session_count=len(similar),
            evidence_session_ids=[s.session_id for s in similar],
            features=features,
            task_pattern_hash=pattern_hash,
            rationale=(
                "no historical data: need at least "
                f"{_MIN_SIMILAR_FOR_RECOMMENDATION} similar past sessions, "
                f"found {len(similar)}"
            ),
            cache_hit=False,
        ).to_dict()

    pick_model, pick_costs, other_costs, evidence_ids = _pick_cheapest_model(similar)
    if not pick_model:
        return Recommendation(
            recommended_model=current_model or "",
            current_model=current_model,
            confidence=0.0,
            cost_delta_usd=0.0,
            similar_session_count=0,
            evidence_session_ids=[],
            features=features,
            task_pattern_hash=pattern_hash,
            rationale="no historical data",
            cache_hit=False,
        ).to_dict()

    confidence = _compute_confidence(similar, pick_costs, other_costs)
    cost_delta = _cost_delta(similar, pick_model, current_model)

    if use_cache:
        _cache_store(
            conn,
            task_pattern_hash=pattern_hash,
            recommended_model=pick_model,
            confidence=confidence,
            evidence_session_ids=evidence_ids,
        )

    rationale_parts = [
        f"{len(similar)} past sessions matched intent={features['intent']!r}, "
        f"band={features['token_band']!r}",
    ]
    if features.get("languages"):
        rationale_parts.append(f"languages={features['languages']}")
    rationale_parts.append(
        f"cheapest model ({pick_model}) had median "
        f"${statistics.median(pick_costs):.4f}/session"
    )
    rationale = "; ".join(rationale_parts)

    return Recommendation(
        recommended_model=pick_model,
        current_model=current_model,
        confidence=confidence,
        cost_delta_usd=cost_delta,
        similar_session_count=len(similar),
        evidence_session_ids=evidence_ids,
        features=features,
        task_pattern_hash=pattern_hash,
        rationale=rationale,
        cache_hit=False,
    ).to_dict()
