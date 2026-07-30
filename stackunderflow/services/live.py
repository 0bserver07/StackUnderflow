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
    """Return the *newest* ``limit`` ``usage_events`` rows with ``id >
    since_id``, re-sorted oldest first.

    The SSE handler calls this every poll cycle with the highest id it
    has emitted to-date. Limit is a defence against a long-stalled
    stream resuming and trying to fan out 10K rows in one chunk —
    individual SSE messages stay small enough to flush in a single
    write.

    Skip-ahead: the fetch is ``ORDER BY id DESC LIMIT ?`` and the page is
    reversed here, so a large backlog yields its *newest* rows instead of
    draining oldest-first. A 231K-row backlog at 50 rows/s took 77 minutes
    (and ~119MB) per open tab to reach "now" through a client ring buffer
    that only keeps the last 100 rows — every one of those rows was
    evicted before the user could see it. Emitting newest-first-by-page
    means one cycle catches up. The intermediate rows are **deliberately
    skipped**; the live tab is a tail, not a log reader.

    The returned page is still ascending by id so the caller's
    ``max(watermark, row_id)`` lands on the true maximum and the UI's
    merge (which assumes each batch arrives oldest-first) stays sorted.
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
        " ORDER BY e.id DESC "
        " LIMIT ?",
        (int(since_id), int(limit)),
    ).fetchall()
    return [dict(r) for r in reversed(rows)]


def recent_tool_calls(
    conn: sqlite3.Connection,
    *,
    since_id: int = 0,
    limit: int = 50,
) -> list[dict[str, Any]]:
    """Return the *newest* ``limit`` ``message_tool_mart`` rows with
    ``id > since_id``, re-sorted oldest first.

    Joined to ``projects`` so the stream payload carries a human-friendly
    project name without a follow-up round-trip.

    Same skip-ahead contract as :func:`recent_events`: ``ORDER BY id DESC
    LIMIT ?`` then reverse, so a backlog is caught up in one cycle and the
    page still arrives ascending for the caller's watermark + the UI merge.
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
        " ORDER BY t.id DESC "
        " LIMIT ?",
        (int(since_id), int(limit)),
    ).fetchall()
    return [dict(r) for r in reversed(rows)]


# ── burn rate ───────────────────────────────────────────────────────────

# Today/MTD burn is recomputed at most this often within a single stream.
# Those figures crawl upward as cost accrues over minutes, so a few seconds
# of staleness is invisible — and it spares the ``idx_events_day`` scan on
# every 5s burn tick. ``window_cost`` is never cached: it stays live.
BURN_TODAY_CACHE_TTL_SECONDS = 30.0


def _day_str(dt: datetime) -> str:
    """UTC ``YYYY-MM-DD`` key for the ``idx_events_day`` prefilter."""
    return dt.strftime("%Y-%m-%d")


def _burn_cutoffs(
    now_dt: datetime, window_minutes: int, tz_offset: int
) -> tuple[datetime, datetime, datetime, datetime]:
    """Return ``(window, today, month, local_now)`` cutoffs, tz-aware.

    ``tz_offset`` is minutes *added* to a UTC timestamp to reach local
    wall-clock time — the exact convention ``aggregator._local_day``
    uses. Computing the day/month boundaries with the same offset keeps
    the live Today/MTD/projection figures aligned with the Cost and
    Overview tabs around local midnight and the 1st of the month
    (instead of bucketing on UTC midnight, which made non-UTC users see
    Live disagree with the rest of the app).

    The returned ``today``/``month`` cutoffs are the *UTC instants* of
    the local day/month start, so they compare directly against the
    stored ISO-8601 UTC ``ts`` values.
    """
    local_now = now_dt + timedelta(minutes=tz_offset)
    local_today = local_now.replace(hour=0, minute=0, second=0, microsecond=0)
    local_month = local_today.replace(day=1)
    today_cutoff = local_today - timedelta(minutes=tz_offset)
    month_cutoff = local_month - timedelta(minutes=tz_offset)
    window_cutoff = now_dt - timedelta(minutes=window_minutes)
    return window_cutoff, today_cutoff, month_cutoff, local_now


def _window_cost(conn: sqlite3.Connection, window_cutoff: datetime) -> float:
    """Sum ``cost_usd`` over the rolling window — always live, indexed.

    ``day >= ?`` lets ``idx_events_day`` skip every prior day so the
    scan touches only the current (and, straddling midnight, previous)
    UTC day rather than the whole ~200K-row table.
    """
    row = conn.execute(
        "SELECT SUM(cost_usd) FROM usage_events WHERE day >= ? AND ts >= ?",
        (_day_str(window_cutoff), window_cutoff.isoformat()),
    ).fetchone()
    return float(row[0] or 0.0)


def _today_month_cost(
    conn: sqlite3.Connection,
    today_cutoff: datetime,
    month_cutoff: datetime,
    *,
    now: datetime,
    cache: dict[str, Any] | None = None,
) -> tuple[float, float]:
    """Return ``(today_cost, month_to_date)`` — indexed + cached.

    Both sums fold into one ``idx_events_day``-bounded scan (``day >=``
    the month-start day, a safe lower bound: any counted row has
    ``ts >= month_cutoff`` hence ``day >= month_cutoff``'s day). When a
    ``cache`` dict is supplied (the SSE stream owns one) the result is
    reused across burn ticks until either the day/month boundary moves
    or ``BURN_TODAY_CACHE_TTL_SECONDS`` elapses.
    """
    key = (today_cutoff.isoformat(), month_cutoff.isoformat())
    if cache is not None:
        hit = cache.get("today_month")
        if hit is not None:
            ckey, cached_at, ctoday, cmonth = hit
            age = (now - cached_at).total_seconds()
            if ckey == key and 0 <= age < BURN_TODAY_CACHE_TTL_SECONDS:
                return ctoday, cmonth

    row = conn.execute(
        "SELECT "
        "  SUM(CASE WHEN ts >= ? THEN cost_usd ELSE 0 END) AS today_cost, "
        "  SUM(CASE WHEN ts >= ? THEN cost_usd ELSE 0 END) AS month_cost "
        "FROM usage_events WHERE day >= ?",
        (today_cutoff.isoformat(), month_cutoff.isoformat(), _day_str(month_cutoff)),
    ).fetchone()
    today_cost = float(row[0] or 0.0)
    month_cost = float(row[1] or 0.0)
    if cache is not None:
        cache["today_month"] = (key, now, today_cost, month_cost)
    return today_cost, month_cost


def rolling_burn(
    conn: sqlite3.Connection,
    *,
    window_minutes: int = 5,
    now: datetime | None = None,
    tz_offset: int = 0,
    cache: dict[str, Any] | None = None,
) -> dict[str, Any]:
    """Return rolling burn metrics over ``usage_events.cost_usd``.

    Shape::

        {
          "window_minutes": int,
          "window_cost": float,           # last N min total
          "per_minute": float,            # window_cost / window_minutes
          "per_hour": float,              # per_minute * 60
          "today_cost": float,            # sum since *local* midnight
          "month_to_date": float,         # sum since *local* month start
          "projected_month_end": float,   # MTD + (avg-daily * days_left)
          "ts": str,                      # ISO timestamp the snapshot was taken
        }

    ``tz_offset`` (minutes east of UTC, ``aggregator._local_day``'s
    convention) shifts the Today/MTD/projection day boundaries to the
    caller's local timezone so the live tab agrees with Cost/Overview.

    Cost source: ``usage_events.cost_usd`` per the post-v0.8.0
    real-store contract. The legacy ``messages`` recompute path is not
    used here — that lived in the aggregator and is a known-stale cost
    surface for live data.

    Performance: the three sums are split into a live window query and a
    cached today/MTD query, both bounded by ``idx_events_day`` so a burn
    tick never full-scans ``usage_events``. Pass a ``cache`` dict (the
    SSE loop does) to memoize today/MTD between ticks.
    """
    now_dt = now if now is not None else _now_utc()
    if now_dt.tzinfo is None:
        now_dt = now_dt.replace(tzinfo=UTC)

    window_cutoff, today_cutoff, month_cutoff, local_now = _burn_cutoffs(now_dt, window_minutes, tz_offset)

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

    window_cost = _window_cost(conn, window_cutoff)
    today_cost, month_cost = _today_month_cost(conn, today_cutoff, month_cutoff, now=now_dt, cache=cache)

    per_minute = window_cost / max(window_minutes, 1)
    per_hour = per_minute * 60.0

    # Month-end projection: average daily burn so far × days remaining.
    # Same approach as ``services.plans.project_month_end`` (used by the
    # plan tab) but seeded from real-time MTD, not aggregated daily mart.
    # Uses the *local* calendar so "days so far" / "days left" match the
    # local-midnight buckets above.
    days_in_month = calendar.monthrange(local_now.year, local_now.month)[1]
    days_so_far = max(local_now.day, 1)  # day 1 → divide-by-zero guard
    days_left = max(days_in_month - local_now.day, 0)
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


# Ceiling on how many session ids we bind into one ``IN (…)`` list.
# SQLite's ``SQLITE_MAX_VARIABLE_NUMBER`` is 999 on pre-3.32 builds (32766
# after), and this repo supports back to 3.30 — 900 leaves headroom for the
# other bound parameters. Same value, same reasoning as
# ``store.queries._MAX_IN_PARAMS``. A window with more distinct sessions than
# this falls back to the in-SQL session subquery (see ``_latency_samples``).
_MAX_BOUND_SESSIONS = 900

# The LEAD() body, shared by both scoping variants. ``{scope}`` is spliced
# with an *uncorrelated* session predicate — either a bound placeholder list
# or a subquery over the mart — so it is evaluated once, not per partition arm.
_LATENCY_LEAD_SQL = (
    "SELECT id, "
    "       timestamp AS msg_ts, "
    "       LEAD(timestamp) OVER ("
    "           PARTITION BY session_fk ORDER BY seq"
    "       ) AS next_ts "
    "  FROM messages "
    " WHERE session_fk IN ({scope}) "
    "   AND id >= ?"
)


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

    # ── two statements, not one ──────────────────────────────────────────
    #
    # ``messages`` is a UNION-ALL view over monthly partitions, so the LEAD()
    # window has to be bounded or it materializes every partition. The
    # previous single-statement shape bounded it with
    # ``id >= (SELECT MIN(message_id) FROM win)`` — a *scalar subquery inside
    # the view*, which SQLite re-evaluates once per UNION-ALL arm: one mart
    # scan per monthly partition (17 on the store measured below), and that
    # re-evaluation was the bulk of the poll. Worse, the comment that
    # justified it was wrong twice over:
    #
    #   * It claimed the floor let SQLite "skip partitions". It does not —
    #     ``EXPLAIN QUERY PLAN`` shows ``SCAN messages_<ym>`` on every arm, not
    #     ``SEARCH``. An ``id >=`` predicate on a UNION-ALL view is a row
    #     filter applied after the scan; there is no partition-pruning.
    #   * It leaned on "a message's next-in-session row always has a higher
    #     id". That invariant does *not* hold in ``messages`` generally —
    #     ids come from ``_messages_id_seq`` in *ingest* order, and a session
    #     resumed after a later session was ingested interleaves them. It
    #     happens to hold for the mart rows we join back to only because the
    #     builder walks ``usage_events`` in id order.
    #
    # What the new shape guarantees instead:
    #
    #   * Identity by *partition-locality*: LEAD is computed per
    #     ``session_fk`` partition, so restricting ``messages`` to exactly the
    #     sessions that own an in-window mart row cannot change any surviving
    #     row's neighbour — the dropped rows were in other partitions.
    #   * The ``id >= min_id`` floor is now a *bound literal* computed in
    #     Python from the same ``win`` rows, so it is evaluated once instead
    #     of per arm, and it keeps the identical row set the old floor did.
    #   * The session predicate is the ``session_fk IN (SELECT id FROM
    #     sessions …)`` list-subquery idiom ``store.queries`` documents: it
    #     pushes a ``session_fk`` constraint *into* each partition arm, which
    #     then seeks its ``(session_fk, seq)`` index instead of scanning.
    #
    # Measured on a 252K-message / 61.5K-mart-row / 16-partition store, 24h
    # window, median of 5: 115ms → 9.1ms with the v030
    # ``idx_message_tool_mart_ts`` index, 132ms → 13ms without it. Same store,
    # ``EXPLAIN QUERY PLAN``: the old shape emitted 17 ``SCAN messages_<ym>``
    # nodes and 17 scalar-subquery nodes (one set per UNION-ALL arm); the new
    # one emits 17 ``SEARCH`` and no subquery node. Row sets verified
    # byte-identical on 24h / 30d / 400d windows.
    win = conn.execute(
        "SELECT message_id, tool_name, session_id FROM message_tool_mart WHERE ts >= ?",
        (cutoff.isoformat(),),
    ).fetchall()
    if not win:
        # Cold-window fast path — no in-window tool calls, so no floor and
        # nothing to scope by. Skip the LEAD statement entirely.
        return {}

    min_id = min(int(r[0]) for r in win)
    session_ids = sorted({r[2] for r in win if r[2]})
    if not session_ids:
        return {}

    if len(session_ids) <= _MAX_BOUND_SESSIONS:
        placeholders = ",".join("?" for _ in session_ids)
        scope = f"SELECT id FROM sessions WHERE session_id IN ({placeholders})"  # noqa: S608 — '?' list, values bound
        params: tuple[Any, ...] = (*session_ids, min_id)
    else:
        # Pathological window (more distinct sessions than we can bind).
        # Keep the session filter in SQL as an uncorrelated subquery over the
        # mart — still evaluated once, still a hoisted floor.
        scope = (
            "SELECT id FROM sessions WHERE session_id IN "
            "(SELECT session_id FROM message_tool_mart WHERE ts >= ?)"
        )
        params = (cutoff.isoformat(), min_id)

    next_ts = {
        int(r[0]): (r[1], r[2])
        for r in conn.execute(_LATENCY_LEAD_SQL.format(scope=scope), params).fetchall()  # noqa: S608 — bound placeholders
    }

    out: dict[str, list[float]] = {}
    for message_id, tool_name, _session_id in win:
        # Inner-join semantics: a mart row whose source message is gone
        # contributes nothing, exactly as the old SQL JOIN did.
        pair = next_ts.get(int(message_id))
        if pair is None:
            continue
        name = tool_name or ""
        ts1 = _iso_to_dt(pair[0])
        ts2 = _iso_to_dt(pair[1])
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
    tz_offset: int = 0,
) -> dict[str, Any]:
    """One-shot snapshot used by ``GET /api/live/stats``.

    Returns the burn block, the latency table, and the current
    ``max_event_id`` / ``max_tool_call_id`` watermarks so the SSE
    consumer can resume from the snapshot without missing rows.

    ``tz_offset`` (minutes east of UTC) is forwarded to
    :func:`rolling_burn` so Today/MTD bucket on the caller's local day.
    """
    return {
        "burn": rolling_burn(conn, window_minutes=burn_window_minutes, tz_offset=tz_offset),
        "tool_latency": tool_latency_percentiles(conn, window_hours=latency_window_hours, top_n=top_tools),
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
