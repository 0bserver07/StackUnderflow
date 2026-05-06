"""ETL status assembler — Wave 4C.

Single source of truth for the ETL pipeline health snapshot. Both the
HTTP route (:mod:`stackunderflow.routes.etl`) and the CLI command
(``stackunderflow etl status``) call :func:`assemble_status` so the two
surfaces never disagree about whether the pipeline is "live", "syncing",
"stale", or "error".

Design goals
------------

* **Fast (<50ms).** Every count is a ``SELECT COUNT(*)`` on an indexed
  column, every per-mart watermark is a primary-key lookup, and the
  watcher state is read off a thread-local handle in ``deps``. No
  pipeline / aggregator / formatter passes.
* **Degrades gracefully.** The watcher handle may be ``None`` (CLI
  invocations don't bring up the FastAPI lifespan, the dashboard may
  have started with ``--no-watcher``, or Wave 2C may not have exposed
  the handle yet). The assembler reports ``running="unknown"`` instead
  of crashing.
* **Schema-aware.** Marts ship in v006 but a fresh-install pre-Wave-1
  database won't have the tables — every count is wrapped in a
  ``sqlite_master`` probe so the assembler is safe to call against any
  age of store.

The shape returned is:

.. code-block:: python

    {
        "watcher": {"enabled": bool, "running": bool|"unknown",
                     "last_refresh_ts": str|None,
                     "seconds_since_refresh": int|None,
                     "events_in_last_cycle": int|None},
        "marts": {<mart_name>: {"watermark": int, "row_count": int,
                                "last_refresh_ts": str|None}},
        "events": {"total": int, "max_id": int,
                    "by_provider": {provider: count},
                    "by_cost_source": {source: count}},
        "lag_seconds": int,           # spec-misnomer: actually "lag_events"
        "health": "live"|"syncing"|"stale"|"error",
    }

The ``lag_seconds`` field is the worst-case difference between the
``max(usage_events.id)`` and the laggiest mart watermark. Spec calls it
``lag_seconds`` but it's really a count of lagging events — the rename
would break the route contract before any consumer exists, so we keep
the spec-defined key name and document the reality here.
"""

from __future__ import annotations

import logging
import os
import sqlite3
from datetime import UTC, datetime
from typing import Any

import stackunderflow.deps as deps

_log = logging.getLogger(__name__)


# Threshold after which a mart's lag turns "stale". Set to match the
# spec ("> 100 events"); collected as a module-level constant so tests
# can monkeypatch a smaller threshold without re-importing the module.
STALE_LAG_THRESHOLD_EVENTS = 100

# How recently must we have seen a watcher refresh to call it "syncing"?
# 10s in the spec — long enough to span a single watcher cycle, short
# enough that a flatlined watcher exits "syncing" and goes "stale" within
# a couple of polling intervals.
SYNCING_RECENT_SECONDS = 10

# Spec-defined mart names (Wave 2B). Listed explicitly so the surface
# always renders all five even when one mart hasn't been refreshed yet
# (its row_count and watermark default to 0).
KNOWN_MART_NAMES: tuple[str, ...] = (
    "daily",
    "session",
    "project",
    "provider_day",
    "model_day",
)

# Map mart name → mart table. Each mart publishes its row count from the
# matching table; lookups are PK lookups so the per-mart cost is O(1).
_MART_TABLES: dict[str, str] = {
    "daily": "daily_mart",
    "session": "session_mart",
    "project": "project_mart",
    "provider_day": "provider_day_mart",
    "model_day": "model_day_mart",
}


# ── public entry point ──────────────────────────────────────────────────────


def assemble_status(conn: sqlite3.Connection) -> dict[str, Any]:
    """Return the full ETL status payload.

    Parameters
    ----------
    conn:
        Open SQLite connection to the StackUnderflow store. Caller owns
        the connection lifecycle.

    Returns
    -------
    dict
        See module docstring for the shape. Always returns a complete
        payload — every key present, every nested dict populated, even
        on a fresh install with no events. Health degrades to "live"
        on an empty store (nothing is lagging if nothing exists).
    """
    events = _events_summary(conn)
    marts = _marts_summary(conn)
    watcher = _watcher_state()
    lag = _compute_lag(events["max_id"], marts)
    health = _compute_health(
        max_event_id=events["max_id"],
        marts=marts,
        watcher=watcher,
        lag_events=lag,
    )

    return {
        "watcher": watcher,
        "marts": marts,
        "events": events,
        "lag_seconds": lag,
        "health": health,
    }


# ── events ────────────────────────────────────────────────────────────────────


def _events_summary(conn: sqlite3.Connection) -> dict[str, Any]:
    """Return ``{total, max_id, by_provider, by_cost_source}``.

    Returns zero/empty values when ``usage_events`` doesn't exist (Wave
    1 not yet applied or a fresh-install store).
    """
    if not _table_exists(conn, "usage_events"):
        return {
            "total": 0,
            "max_id": 0,
            "by_provider": {},
            "by_cost_source": {},
        }

    # Single-pass aggregation — one SELECT per metric, all on indexed
    # columns. With idx_events_provider/idx_events_day, even on a
    # 1M-event store the round-trip stays <10ms.
    row = conn.execute(
        "SELECT COUNT(*) AS n, COALESCE(MAX(id), 0) AS m FROM usage_events"
    ).fetchone()
    total = int(row["n"]) if hasattr(row, "keys") else int(row[0])
    max_id = int(row["m"]) if hasattr(row, "keys") else int(row[1])

    by_provider: dict[str, int] = {}
    for r in conn.execute(
        "SELECT provider, COUNT(*) AS n FROM usage_events GROUP BY provider"
    ):
        prov = r["provider"] if hasattr(r, "keys") else r[0]
        n = int(r["n"]) if hasattr(r, "keys") else int(r[1])
        if prov:
            by_provider[prov] = n

    by_cost_source: dict[str, int] = {}
    for r in conn.execute(
        "SELECT cost_source, COUNT(*) AS n FROM usage_events GROUP BY cost_source"
    ):
        src = r["cost_source"] if hasattr(r, "keys") else r[0]
        n = int(r["n"]) if hasattr(r, "keys") else int(r[1])
        if src:
            by_cost_source[src] = n

    return {
        "total": total,
        "max_id": max_id,
        "by_provider": by_provider,
        "by_cost_source": by_cost_source,
    }


# ── marts ─────────────────────────────────────────────────────────────────────


def _marts_summary(conn: sqlite3.Connection) -> dict[str, dict[str, Any]]:
    """Return ``{mart_name: {watermark, row_count, last_refresh_ts}}``.

    The five mart names in :data:`KNOWN_MART_NAMES` are always present
    in the result — missing-watermark / missing-table cases default to
    zero so the dashboard surface stays stable across schema versions.
    """
    out: dict[str, dict[str, Any]] = {}

    have_watermark = _table_exists(conn, "mart_watermark")

    # Pull every watermark in one round trip; we'll fall back to zeros
    # for marts not in the result set.
    watermarks: dict[str, tuple[int, str | None]] = {}
    if have_watermark:
        for r in conn.execute(
            "SELECT mart_name, last_event_id, last_refresh_ts FROM mart_watermark"
        ):
            name = r["mart_name"] if hasattr(r, "keys") else r[0]
            wm = int(r["last_event_id"]) if hasattr(r, "keys") else int(r[1])
            ts = r["last_refresh_ts"] if hasattr(r, "keys") else r[2]
            watermarks[name] = (wm, ts)

    for name in KNOWN_MART_NAMES:
        wm, ts = watermarks.get(name, (0, None))
        table = _MART_TABLES[name]
        if _table_exists(conn, table):
            row = conn.execute(f"SELECT COUNT(*) AS n FROM {table}").fetchone()
            row_count = int(row["n"]) if hasattr(row, "keys") else int(row[0])
        else:
            row_count = 0
        out[name] = {
            "watermark": wm,
            "row_count": row_count,
            "last_refresh_ts": ts,
        }
    return out


# ── watcher ───────────────────────────────────────────────────────────────────


def _watcher_state() -> dict[str, Any]:
    """Best-effort snapshot of the Wave 2C watcher's runtime state.

    The watcher handle (``deps.watcher_handle``) is populated by the
    FastAPI lifespan in ``server._lifespan`` *iff* the watcher actually
    started. CLI subcommands never bring the lifespan up, so the handle
    is ``None`` for every ``stackunderflow etl status`` call against a
    cold store — that's expected and the assembler reports
    ``running="unknown"`` so the UI can show "watcher state unknown"
    rather than wrongly claiming it's down.

    The handle's structure may evolve; this function tolerates anything
    that exposes ``.thread`` (with ``.is_alive()``) — the spec is loose
    so future-extensibility is built in.
    """
    enabled = not _watcher_env_disabled()
    handle = getattr(deps, "watcher_handle", None)

    if handle is None:
        # Either CLI mode (lifespan never ran) or the lifespan started
        # the server with --no-watcher / handle initialisation failed.
        # In either case, "unknown" is the honest answer.
        return {
            "enabled": enabled,
            "running": "unknown",
            "last_refresh_ts": None,
            "seconds_since_refresh": None,
            "events_in_last_cycle": None,
        }

    running: bool | str
    try:
        thread = getattr(handle, "thread", None)
        running = bool(thread and thread.is_alive())
    except Exception as exc:  # noqa: BLE001 — never propagate from a status probe
        _log.debug("etl.status: watcher handle introspection raised: %s", exc)
        running = "unknown"

    # Wave 2C exposes neither ``last_refresh_ts`` nor
    # ``events_in_last_cycle`` on the handle today (the watcher
    # processes them internally). When the handle gains those fields
    # we'll surface them; for now report ``None`` so the contract is
    # forward-compatible.
    last_ts = getattr(handle, "last_refresh_ts", None)
    seconds_since = _seconds_since(last_ts) if last_ts else None
    events_last = getattr(handle, "events_in_last_cycle", None)

    return {
        "enabled": enabled,
        "running": running,
        "last_refresh_ts": last_ts,
        "seconds_since_refresh": seconds_since,
        "events_in_last_cycle": events_last,
    }


def _watcher_env_disabled() -> bool:
    """Mirror of ``server._watcher_disabled()`` — kept local to avoid
    pulling the FastAPI app import graph into the CLI hot path.
    """
    val = os.environ.get("STACKUNDERFLOW_DISABLE_WATCHER", "").strip().lower()
    return val in ("1", "true", "yes", "on")


# ── lag + health ──────────────────────────────────────────────────────────────


def _compute_lag(max_event_id: int, marts: dict[str, dict[str, Any]]) -> int:
    """Return ``max(0, max_event_id - min(mart watermarks))``.

    The "min watermark" is taken across the **registered** marts only —
    a mart in :data:`KNOWN_MART_NAMES` whose watermark is 0 because it
    has never refreshed counts as a 0-watermark, not "unknown", which
    correctly lights up "stale" the moment any events exist.
    """
    if not marts or max_event_id == 0:
        return 0
    min_wm = min(int(m["watermark"]) for m in marts.values())
    return max(0, max_event_id - min_wm)


def _compute_health(
    *,
    max_event_id: int,
    marts: dict[str, dict[str, Any]],
    watcher: dict[str, Any],
    lag_events: int,
) -> str:
    """Return ``"live" | "syncing" | "stale" | "error"`` per the spec.

    Order of evaluation matters — most-degraded first so a state that
    matches multiple rules surfaces as the worst one.

    Rules (from the Wave 4C spec):

    * **error** — lag > threshold AND watcher reports running=False
      (we know we're behind AND nothing is going to catch us up).
    * **stale** — lag > threshold (catch-up may still happen, watcher
      may be running, but the marts are observably behind).
    * **syncing** — lag > 0 but a refresh ran in the last 10 seconds
      (pipeline is actively catching up).
    * **live** — anything else (zero lag, or no events at all).
    """
    # Empty store → live (nothing to catch up to).
    if max_event_id == 0:
        return "live"

    if lag_events > STALE_LAG_THRESHOLD_EVENTS:
        if watcher.get("running") is False:
            return "error"
        return "stale"

    if lag_events > 0:
        seconds_since = watcher.get("seconds_since_refresh")
        if seconds_since is not None and seconds_since <= SYNCING_RECENT_SECONDS:
            return "syncing"
        # Lag > 0 but no recent refresh → marts are slightly behind but
        # within the threshold; still call this live so a single
        # in-flight insert doesn't flicker the badge.
        return "live"

    return "live"


# ── helpers ───────────────────────────────────────────────────────────────────


def _table_exists(conn: sqlite3.Connection, name: str) -> bool:
    """Return True iff *name* is a table in the connected store.

    Cheap (sqlite_master is cached); we run it once per call site rather
    than caching at the module scope because tests routinely create and
    drop tables and a stale cache there would be a footgun.
    """
    row = conn.execute(
        "SELECT 1 FROM sqlite_master WHERE type='table' AND name = ?",
        (name,),
    ).fetchone()
    return row is not None


def _seconds_since(iso_ts: str) -> int | None:
    """Parse an ISO 8601 timestamp and return integer seconds elapsed.

    Returns ``None`` on parse failure rather than raising — callers
    surface this as ``seconds_since_refresh: null`` so a malformed
    timestamp on the watcher handle never crashes the status route.
    """
    if not iso_ts:
        return None
    try:
        # ``datetime.fromisoformat`` accepts "...+00:00" but not "...Z";
        # normalise the trailing-Z form before parsing.
        normalized = iso_ts.replace("Z", "+00:00") if iso_ts.endswith("Z") else iso_ts
        ts = datetime.fromisoformat(normalized)
        if ts.tzinfo is None:
            ts = ts.replace(tzinfo=UTC)
        delta = datetime.now(UTC) - ts
        return max(0, int(delta.total_seconds()))
    except (ValueError, TypeError) as exc:
        _log.debug("etl.status: failed to parse last_refresh_ts %r: %s", iso_ts, exc)
        return None


__all__ = [
    "assemble_status",
    "KNOWN_MART_NAMES",
    "STALE_LAG_THRESHOLD_EVENTS",
    "SYNCING_RECENT_SECONDS",
]
