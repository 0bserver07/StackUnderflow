"""Live observability helpers — Spec 13.

Pure read-side helpers over ``usage_events`` (live cost grain) and
``message_tool_mart`` (live tool-call grain). Each function returns a
plain ``dict`` / ``list`` so the route layer can JSON-encode without
massaging.

Three primitives drive the live tab:

* :func:`recent_events` / :func:`recent_tool_calls` — incremental fetch
  by ``id`` watermark for the SSE stream.
* :func:`rolling_burn` — last-N-min cost + month-end projection
  (extrapolated from today's burn so far, same shape as
  ``services.plans.project_month_end``).
* :func:`tool_latency_percentiles` — P50/P95/P99 per tool, derived from
  ``messages.timestamp`` deltas between the assistant message that
  emitted a ``tool_use`` and the next message in the same session
  (which carries the ``tool_result``). The mart timestamp is the
  source-message timestamp; the next message is found via the same
  ``(session_fk, seq+1)`` lookup the message_tool builder uses.

All functions tolerate a fresh / empty store: a missing
``usage_events`` table makes them return zeros, and an absent
``message_tool_mart`` makes the latency helper return ``{}``. The route
layer never crashes on a cold install.
"""

from __future__ import annotations

import calendar
import sqlite3
from datetime import UTC, datetime, timedelta
from typing import Any

# ── small utilities ────────────────────────────────────────────────────


def _table_exists(conn: sqlite3.Connection, name: str) -> bool:
    row = conn.execute(
        "SELECT 1 FROM sqlite_master WHERE type='table' AND name = ?",
        (name,),
    ).fetchone()
    return row is not None


def _now_utc() -> datetime:
    """Wrapped so tests can monkey-patch the clock."""
    return datetime.now(UTC)


def _iso_to_dt(iso_ts: str | None) -> datetime | None:
    """Parse an ISO 8601 timestamp; tolerate the trailing ``Z`` form.

    Returns ``None`` on parse failure — every consumer treats that as
    "this row contributes nothing to the rolling window" rather than
    raising. Cheap defensive fallback for the live tab.
    """
    if not iso_ts:
        return None
    try:
        norm = iso_ts.replace("Z", "+00:00") if iso_ts.endswith("Z") else iso_ts
        ts = datetime.fromisoformat(norm)
        if ts.tzinfo is None:
            ts = ts.replace(tzinfo=UTC)
        return ts
    except (ValueError, TypeError):
        return None


# ── max-id watermarks (for the SSE seed) ────────────────────────────────


def max_event_id(conn: sqlite3.Connection) -> int:
    """Return ``max(usage_events.id)`` or 0 on an empty / missing table."""
    if not _table_exists(conn, "usage_events"):
        return 0
    row = conn.execute("SELECT MAX(id) FROM usage_events").fetchone()
    val = row[0] if row else None
    return int(val) if val is not None else 0


def max_tool_call_id(conn: sqlite3.Connection) -> int:
    """Return ``max(message_tool_mart.id)`` or 0 on absent/empty mart."""
    if not _table_exists(conn, "message_tool_mart"):
        return 0
    row = conn.execute("SELECT MAX(id) FROM message_tool_mart").fetchone()
    val = row[0] if row else None
    return int(val) if val is not None else 0


# ── incremental readers ────────────────────────────────────────────────


def recent_events(
    conn: sqlite3.Connection,
    *,
    since_id: int = 0,
    limit: int = 50,
) -> list[dict[str, Any]]:
    """Return ``usage_events`` rows with ``id > since_id``, oldest first.

    The SSE handler calls this every poll cycle with the highest id it
    has emitted to-date. Limit is a defence against a long-stalled
    stream resuming and trying to fan out 10K rows in one chunk —
    individual SSE messages stay small enough to flush in a single
    write.
    """
    if not _table_exists(conn, "usage_events"):
        return []
    rows = conn.execute(
        "SELECT e.id, e.ts, e.project_id, e.session_id, e.model, "
        "       e.cost_usd, e.input_tokens, e.output_tokens, "
        "       e.cache_read_tokens, e.cache_create_tokens, "
        "       e.cost_source, p.slug AS project_slug, "
        "       p.display_name AS project_name "
        "  FROM usage_events e "
        "  LEFT JOIN projects p ON p.id = e.project_id "
        " WHERE e.id > ? "
        " ORDER BY e.id "
        " LIMIT ?",
        (int(since_id), int(limit)),
    ).fetchall()
    return [dict(r) for r in rows]


def recent_tool_calls(
    conn: sqlite3.Connection,
    *,
    since_id: int = 0,
    limit: int = 50,
) -> list[dict[str, Any]]:
    """Return ``message_tool_mart`` rows with ``id > since_id``, oldest first.

    Joined to ``projects`` so the stream payload carries a human-friendly
    project name without a follow-up round-trip.
    """
    if not _table_exists(conn, "message_tool_mart"):
        return []
    rows = conn.execute(
        "SELECT t.id, t.ts, t.project_id, t.session_id, t.tool_name, "
        "       t.file_path, t.byte_count, t.call_index, "
        "       p.slug AS project_slug, p.display_name AS project_name "
        "  FROM message_tool_mart t "
        "  LEFT JOIN projects p ON p.id = t.project_id "
        " WHERE t.id > ? "
        " ORDER BY t.id "
        " LIMIT ?",
        (int(since_id), int(limit)),
    ).fetchall()
    return [dict(r) for r in rows]


# ── burn rate ───────────────────────────────────────────────────────────


def rolling_burn(
    conn: sqlite3.Connection,
    *,
    window_minutes: int = 5,
    now: datetime | None = None,
) -> dict[str, Any]:
    """Return rolling burn metrics over ``usage_events.cost_usd``.

    Shape::

        {
          "window_minutes": int,
          "window_cost": float,           # last N min total
          "per_minute": float,            # window_cost / window_minutes
          "per_hour": float,              # per_minute * 60
          "today_cost": float,            # sum since UTC midnight
          "month_to_date": float,         # sum since UTC month start
          "projected_month_end": float,   # MTD + (avg-daily * days_left)
          "ts": str,                      # ISO timestamp the snapshot was taken
        }

    Cost source: ``usage_events.cost_usd`` per the post-v0.8.0
    real-store contract. The legacy ``messages`` recompute path is not
    used here — that lived in the aggregator and is a known-stale cost
    surface for live data.
    """
    now_dt = now if now is not None else _now_utc()
    if now_dt.tzinfo is None:
        now_dt = now_dt.replace(tzinfo=UTC)

    if not _table_exists(conn, "usage_events"):
        return {
            "window_minutes": window_minutes,
            "window_cost": 0.0,
            "per_minute": 0.0,
            "per_hour": 0.0,
            "today_cost": 0.0,
            "month_to_date": 0.0,
            "projected_month_end": 0.0,
            "ts": now_dt.isoformat(),
        }

    window_cutoff = now_dt - timedelta(minutes=window_minutes)
    today_cutoff = now_dt.replace(hour=0, minute=0, second=0, microsecond=0)
    month_cutoff = today_cutoff.replace(day=1)

    # Three SUMs in one trip — sqlite is happy to fold them into the
    # same scan via conditional aggregation. ``ts`` is ISO 8601 UTC so
    # string comparison is timezone-safe (lexicographic == temporal).
    row = conn.execute(
        "SELECT "
        "  SUM(CASE WHEN ts >= ? THEN cost_usd ELSE 0 END) AS window_cost, "
        "  SUM(CASE WHEN ts >= ? THEN cost_usd ELSE 0 END) AS today_cost, "
        "  SUM(CASE WHEN ts >= ? THEN cost_usd ELSE 0 END) AS month_cost "
        "FROM usage_events",
        (
            window_cutoff.isoformat(),
            today_cutoff.isoformat(),
            month_cutoff.isoformat(),
        ),
    ).fetchone()

    window_cost = float(row[0] or 0.0)
    today_cost = float(row[1] or 0.0)
    month_cost = float(row[2] or 0.0)

    per_minute = window_cost / max(window_minutes, 1)
    per_hour = per_minute * 60.0

    # Month-end projection: average daily burn so far × days remaining.
    # Same approach as ``services.plans.project_month_end`` (used by the
    # plan tab) but seeded from real-time MTD, not aggregated daily mart.
    days_in_month = calendar.monthrange(now_dt.year, now_dt.month)[1]
    days_so_far = max(now_dt.day, 1)  # day 1 → divide-by-zero guard
    days_left = max(days_in_month - now_dt.day, 0)
    avg_daily = month_cost / days_so_far
    projected = month_cost + avg_daily * days_left

    return {
        "window_minutes": window_minutes,
        "window_cost": window_cost,
        "per_minute": per_minute,
        "per_hour": per_hour,
        "today_cost": today_cost,
        "month_to_date": month_cost,
        "projected_month_end": projected,
        "ts": now_dt.isoformat(),
    }


# ── tool latency percentiles ────────────────────────────────────────────


def _percentile(sorted_values: list[float], p: float) -> float:
    """Nearest-rank percentile on a pre-sorted list. ``p`` ∈ [0, 100].

    Returns 0.0 on an empty list — the live UI renders that as a dash
    rather than a misleading number.
    """
    if not sorted_values:
        return 0.0
    if len(sorted_values) == 1:
        return float(sorted_values[0])
    # Nearest-rank: index = ceil(p/100 * N) - 1, clamped to [0, N-1].
    rank = max(0, min(len(sorted_values) - 1, int((p / 100.0) * len(sorted_values))))
    return float(sorted_values[rank])


def _latency_samples(
    conn: sqlite3.Connection,
    *,
    window_hours: int,
) -> dict[str, list[float]]:
    """Return ``{tool_name: [latency_seconds, ...]}`` over the window.

    Latency proxy: ``next_msg.timestamp - source_msg.timestamp``
    (seconds) where ``source_msg`` is the assistant message that emitted
    the ``tool_use`` and ``next_msg`` is the immediately following row
    in the same session by ``seq``. Coarse — only as fine as the source
    file's write granularity — but representative across enough samples.
    Documented in the spec under "Hard parts".
    """
    if not _table_exists(conn, "message_tool_mart"):
        return {}
    cutoff = _now_utc() - timedelta(hours=window_hours)
    rows = conn.execute(
        "SELECT t.tool_name, m.timestamp AS ts1, "
        "       (SELECT m2.timestamp FROM messages m2 "
        "          WHERE m2.session_fk = m.session_fk AND m2.seq > m.seq "
        "          ORDER BY m2.seq LIMIT 1) AS ts2 "
        "  FROM message_tool_mart t "
        "  JOIN messages m ON m.id = t.message_id "
        " WHERE t.ts >= ?",
        (cutoff.isoformat(),),
    ).fetchall()

    out: dict[str, list[float]] = {}
    for r in rows:
        name = r[0] or ""
        ts1 = _iso_to_dt(r[1])
        ts2 = _iso_to_dt(r[2])
        if not name or ts1 is None or ts2 is None:
            continue
        delta = (ts2 - ts1).total_seconds()
        # Negative deltas mean the next message timestamp is behind
        # the tool_use — happens with clock skew on imported logs.
        # Drop those rather than letting them poison the percentile.
        if delta < 0:
            continue
        out.setdefault(name, []).append(delta)
    return out


def tool_latency_percentiles(
    conn: sqlite3.Connection,
    *,
    window_hours: int = 24,
    top_n: int = 6,
) -> list[dict[str, Any]]:
    """Return per-tool latency percentiles over the last ``window_hours``.

    Sorted by sample count desc; capped at ``top_n`` so the live tab's
    sparkline grid stays bounded. Each entry::

        {
          "tool_name": str,
          "samples": int,
          "p50": float,   # seconds
          "p95": float,
          "p99": float,
        }
    """
    samples = _latency_samples(conn, window_hours=window_hours)
    out: list[dict[str, Any]] = []
    for tool, vals in samples.items():
        vals_sorted = sorted(vals)
        out.append(
            {
                "tool_name": tool,
                "samples": len(vals_sorted),
                "p50": _percentile(vals_sorted, 50),
                "p95": _percentile(vals_sorted, 95),
                "p99": _percentile(vals_sorted, 99),
            }
        )
    out.sort(key=lambda x: -x["samples"])
    return out[: max(0, int(top_n))]


# ── snapshot for /api/live/stats ────────────────────────────────────────


def snapshot(
    conn: sqlite3.Connection,
    *,
    burn_window_minutes: int = 5,
    latency_window_hours: int = 24,
    top_tools: int = 6,
) -> dict[str, Any]:
    """One-shot snapshot used by ``GET /api/live/stats``.

    Returns the burn block, the latency table, and the current
    ``max_event_id`` / ``max_tool_call_id`` watermarks so the SSE
    consumer can resume from the snapshot without missing rows.
    """
    return {
        "burn": rolling_burn(conn, window_minutes=burn_window_minutes),
        "tool_latency": tool_latency_percentiles(
            conn, window_hours=latency_window_hours, top_n=top_tools
        ),
        "watermarks": {
            "event_id": max_event_id(conn),
            "tool_call_id": max_tool_call_id(conn),
        },
    }


__all__ = [
    "max_event_id",
    "max_tool_call_id",
    "recent_events",
    "recent_tool_calls",
    "rolling_burn",
    "tool_latency_percentiles",
    "snapshot",
]
