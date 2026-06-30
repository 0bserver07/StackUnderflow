"""Cost anomaly / outlier detection for the optimize surface.

Flags days (and sessions) whose dollar cost is a statistical outlier versus
the project's own rolling baseline, so a one-off spend spike surfaces without
the user having to scan the cost chart by eye.

Method — robust first, parametric fallback:

* **Median + MAD (median absolute deviation).** The baseline is the *median*
  daily/session cost and the spread is the MAD. A point is flagged when

      cost - median  >  k · (MAD · 1.4826)

  We only flag the *upper* tail (overspend); a cheap day is never an anomaly.
  The ``1.4826`` factor scales MAD to a normal-consistent σ estimate so the
  ``k`` threshold reads in familiar "sigma" units. MAD is used over stddev
  because a single huge spike inflates stddev (masking itself) but barely
  moves the median/MAD — the textbook reason robust statistics exist.

* **Stddev (2σ) fallback.** When every point is identical the MAD is 0 and the
  robust test can't separate anything; we fall back to ``mean + k·stddev``.
  When *that* spread is also 0 (truly flat series) nothing is flagged.

The detector is advisory and never raises: an empty/absent mart, a series too
short to have a baseline (``< MIN_POINTS``), or any arithmetic edge returns an
empty result. Reads are bounded by the caller's :class:`Scope`.
"""

from __future__ import annotations

import sqlite3
import statistics
from dataclasses import asdict, dataclass, field
from typing import Any

from stackunderflow.reports.scope import Scope
from stackunderflow.store import mart_queries

__all__ = [
    "CostAnomaly",
    "find_cost_anomalies",
    "MAD_K",
    "MIN_POINTS",
    "TOP_N",
]


# ── tunables ────────────────────────────────────────────────────────────────

# Robust threshold multiplier. ``k = 3`` (in normal-consistent σ units) is the
# conventional outlier cut — ~3 sigma, ≈ the 99.7th percentile under
# normality — strict enough that ordinary day-to-day variance doesn't trip it.
MAD_K = 3.0
# A series shorter than this has no meaningful baseline; skip rather than flag
# noise. Five points is the floor where a median/MAD is defensible.
MIN_POINTS = 5
# Scale factor that makes MAD a consistent estimator of σ for normal data, so
# ``MAD_K`` reads in sigma units.
_MAD_TO_SIGMA = 1.4826
# Hard cap on returned outliers — the panel shows the worst few, not a wall.
TOP_N = 10
# Floor on the per-bucket cost worth flagging. Below this a "3× the median"
# spike is pennies and pure noise (e.g. a $0.002 day next to a $0.0001 median).
_MIN_ABSOLUTE_USD = 0.05


# ── result dataclass ────────────────────────────────────────────────────────


@dataclass(frozen=True)
class CostAnomaly:
    """One flagged outlier bucket (a day or a session)."""

    kind: str          # "day" | "session"
    key: str           # the day (YYYY-MM-DD) or session id
    cost_usd: float
    baseline_usd: float       # the median (robust path) or mean (fallback)
    deviation_usd: float      # cost - baseline (always > 0 for a flag)
    ratio: float | None       # cost / baseline, when baseline > 0
    score: float              # how many σ-equivalent units past baseline
    method: str               # "mad" | "stddev"
    reason: str
    details: dict[str, Any] = field(default_factory=dict)

    def to_dict(self) -> dict[str, Any]:
        return asdict(self)


# ── statistics core ─────────────────────────────────────────────────────────


def _flag_outliers(
    points: list[tuple[str, float]],
    *,
    kind: str,
    k: float,
    min_points: int,
    extra: dict[str, dict[str, Any]] | None = None,
) -> list[CostAnomaly]:
    """Return upper-tail outliers from ``[(key, cost), ...]``.

    ``extra`` optionally maps a key → an extra ``details`` dict (e.g. the
    model / message count behind a session) merged into the anomaly.
    """
    usable = [(key, float(cost)) for key, cost in points if cost is not None]
    if len(usable) < min_points:
        return []

    costs = [c for _, c in usable]
    median = statistics.median(costs)
    abs_dev = [abs(c - median) for c in costs]
    mad = statistics.median(abs_dev)

    method: str
    baseline: float
    spread_sigma: float  # one σ-equivalent unit of spread

    if mad > 0:
        method = "mad"
        baseline = median
        spread_sigma = mad * _MAD_TO_SIGMA
    else:
        # Flat-by-median series — fall back to mean + k·stddev. Needs ≥ 2
        # points for a sample stdev; usable already ≥ min_points ≥ 2.
        method = "stddev"
        baseline = statistics.fmean(costs)
        stdev = statistics.pstdev(costs)
        if stdev <= 0:
            return []  # truly flat — nothing deviates
        spread_sigma = stdev

    threshold = baseline + k * spread_sigma

    out: list[CostAnomaly] = []
    for key, cost in usable:
        if cost < _MIN_ABSOLUTE_USD:
            continue
        if cost <= threshold:
            continue
        deviation = cost - baseline
        score = deviation / spread_sigma if spread_sigma > 0 else 0.0
        ratio = (cost / baseline) if baseline > 0 else None
        out.append(
            CostAnomaly(
                kind=kind,
                key=key,
                cost_usd=round(cost, 4),
                baseline_usd=round(baseline, 4),
                deviation_usd=round(deviation, 4),
                ratio=round(ratio, 2) if ratio is not None else None,
                score=round(score, 2),
                method=method,
                reason=_reason(kind, cost, baseline, ratio, score, method),
                details=(extra or {}).get(key, {}),
            )
        )

    # Worst first (largest dollar deviation), then cap.
    out.sort(key=lambda a: a.deviation_usd, reverse=True)
    return out


def _reason(
    kind: str,
    cost: float,
    baseline: float,
    ratio: float | None,
    score: float,
    method: str,
) -> str:
    """Human-readable one-liner explaining the flag."""
    noun = "day" if kind == "day" else "session"
    base_label = "median" if method == "mad" else "mean"
    if ratio is not None and ratio >= 1.5:
        mult = f"{ratio:.1f}×"
        return (
            f"This {noun} cost ${cost:,.2f} — {mult} the {base_label} "
            f"of ${baseline:,.2f} ({score:.1f}σ over baseline)."
        )
    return (
        f"This {noun} cost ${cost:,.2f} vs a {base_label} of "
        f"${baseline:,.2f} ({score:.1f}σ over baseline)."
    )


# ── data sourcing ────────────────────────────────────────────────────────────


def _daily_cost_points(
    conn: sqlite3.Connection,
    *,
    scope: Scope | None,
) -> list[tuple[str, float]]:
    """Per-day total cost from ``daily_mart``, summed across projects/models."""
    day_from = _iso_to_day(scope.since) if scope and scope.since else None
    day_to = _iso_to_day(scope.until) if scope and scope.until else None
    rows = mart_queries.daily_global(conn, day_from=day_from, day_to=day_to)
    by_day: dict[str, float] = {}
    for r in rows:
        day = r.get("day")
        if not day:
            continue
        by_day[day] = by_day.get(day, 0.0) + float(r.get("cost_usd", 0.0) or 0.0)
    return sorted(by_day.items())


def _session_cost_points(
    conn: sqlite3.Connection,
    *,
    scope: Scope | None,
) -> tuple[list[tuple[str, float]], dict[str, dict[str, Any]]]:
    """Per-session cost + a per-session details map from ``session_mart``."""
    since = scope.since if scope else None
    until = scope.until if scope else None
    rows = mart_queries.session_mart_rows_for_compare(
        conn, since_iso=since, until_iso=until,
    )
    points: list[tuple[str, float]] = []
    extra: dict[str, dict[str, Any]] = {}
    for r in rows:
        sid = r.get("session_id")
        if not sid:
            continue
        points.append((sid, float(r.get("cost_usd", 0.0) or 0.0)))
        extra[sid] = {
            "model": r.get("primary_model"),
            "provider": r.get("provider"),
            "first_ts": r.get("first_ts"),
            "message_count": int(r.get("message_count", 0) or 0),
        }
    return points, extra


def _iso_to_day(iso: str) -> str:
    """``2026-04-25T10:00:00+00:00`` → ``2026-04-25`` (defensive slice)."""
    return iso[:10]


# ── public entry point ───────────────────────────────────────────────────────


def find_cost_anomalies(
    conn: sqlite3.Connection,
    *,
    scope: Scope | None = None,
    k: float = MAD_K,
    min_points: int = MIN_POINTS,
    top_n: int = TOP_N,
    include_sessions: bool = True,
) -> dict[str, Any]:
    """Flag cost outliers over *scope* and return the worst ``top_n``.

    Returns a dict::

        {
            "method": "mad" | "stddev" | "none",
            "k": <float>,
            "anomalies": [CostAnomaly.to_dict(), ...],   # days + sessions, worst-first
            "day_count": <int>,        # days examined (baseline size)
            "session_count": <int>,    # sessions examined
        }

    The ``method`` reported at the top level is the day series' method (the
    primary signal); per-anomaly ``method`` is authoritative for each row.
    Sessions are included when ``include_sessions`` and the per-session mart
    is populated; they share the same statistical test as days.
    """
    day_points = _daily_cost_points(conn, scope=scope)
    day_anoms = _flag_outliers(
        day_points, kind="day", k=k, min_points=min_points,
    )

    session_anoms: list[CostAnomaly] = []
    session_count = 0
    if include_sessions:
        sess_points, sess_extra = _session_cost_points(conn, scope=scope)
        session_count = len(sess_points)
        session_anoms = _flag_outliers(
            sess_points, kind="session", k=k, min_points=min_points,
            extra=sess_extra,
        )

    combined = [*day_anoms, *session_anoms]
    combined.sort(key=lambda a: a.deviation_usd, reverse=True)
    combined = combined[:top_n]

    if day_anoms:
        method = day_anoms[0].method
    elif session_anoms:
        method = session_anoms[0].method
    else:
        method = "none"

    return {
        "method": method,
        "k": k,
        "anomalies": [a.to_dict() for a in combined],
        "day_count": len(day_points),
        "session_count": session_count,
    }
