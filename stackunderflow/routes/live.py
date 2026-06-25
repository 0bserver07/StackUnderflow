"""Live observability routes — Spec 13.

Two endpoints share the ``/api/live`` prefix:

* ``GET /api/live/stats`` — one-shot snapshot for the initial render
  (burn block + tool-latency percentiles + the current SSE watermarks).
* ``GET /api/live/stream`` — Server-Sent Events stream that emits
  ``event``, ``tool_call``, and ``burn_tick`` messages as they land.

SSE wire format
---------------

Each message is a standard ``data: <json>\\n\\n`` block with an
explicit ``event:`` line so the browser's ``EventSource`` can
``addEventListener("tool_call", …)``-dispatch them. Payload shape::

    {"type": "event", "ts": "...", "payload": {…usage_events row…}}
    {"type": "tool_call", "ts": "...", "payload": {…message_tool_mart row…}}
    {"type": "burn_tick", "ts": "...", "payload": {…rolling_burn(…)…}}

The handler also emits a ``ready`` event on connect carrying the
seed watermarks and the current watcher state so the UI can hide the
"connecting…" banner immediately and surface "watcher not running"
without a follow-up call to ``/api/etl/status``.

Lifecycle
---------

We poll the store every ``POLL_INTERVAL_SECONDS`` (default 2s — well
inside the 5s burn cadence and fast enough that "live" feels live for
the per-tool-call streams). The handler:

1. Opens one ``sqlite3`` connection for the whole stream and reuses it
   every cycle (autocommit + WAL, so reads still see the watcher's
   latest commits without holding a transaction open between ticks).
2. Sleeps in 100ms slices between cycles so a client disconnect breaks
   out within at most 100ms — Starlette propagates the disconnect via
   ``request.is_disconnected()`` and we honour it on every slice.
3. Caps each cycle's emission at ``MAX_PER_CYCLE`` rows per stream
   (events, tool_calls) so a backfill doesn't fan out a megabyte in one
   tick — the next cycle picks up from the new watermark.
4. Emits a ``burn_tick`` every ``BURN_INTERVAL_SECONDS`` (5s per spec).
5. On any exception inside the loop, logs and exits cleanly — the
   client sees the connection drop and reconnects on its own
   ``EventSource`` timer.

The "watcher must be running" UX is not handled here — the snapshot
route surfaces ``watcher_running`` so the frontend banners off that
without a separate call. The stream still opens and emits ``burn_tick``
even when the watcher is down, so a static dashboard with a stale
``store.db`` is still useful.
"""

from __future__ import annotations

import asyncio
import json
import logging
from collections.abc import AsyncIterator
from datetime import UTC, datetime
from typing import Any

from fastapi import APIRouter, Request
from fastapi.responses import StreamingResponse

import stackunderflow.deps as deps
from stackunderflow.services import live as live_svc
from stackunderflow.store import db, schema

router = APIRouter()
_log = logging.getLogger(__name__)

# Cycle pacing. Two ticks per second is enough for "live" on the tool
# stream (typical session emits 1-3 tool calls/sec) and lets the burn
# tick land within ~2.5s of its 5s schedule worst-case.
POLL_INTERVAL_SECONDS = 2.0

# Burn-tick cadence per spec.
BURN_INTERVAL_SECONDS = 5.0

# Cap per-cycle emission so a backfill burst doesn't fan out a 50KB
# message in a single tick. The next cycle picks up the rest.
MAX_PER_CYCLE = 100

# Smaller sleep slices so a client disconnect breaks out fast.
DISCONNECT_POLL_INTERVAL_SECONDS = 0.1


def _watcher_running() -> bool | str:
    """Return ``True`` / ``False`` / ``"unknown"`` for the watcher state.

    Mirrors the ``etl/status`` introspection so the live tab and the ETL
    badge always agree about whether the watcher thread is alive.
    """
    handle = getattr(deps, "watcher_handle", None)
    if handle is None:
        return "unknown"
    try:
        thread = getattr(handle, "thread", None)
        return bool(thread and thread.is_alive())
    except Exception as exc:  # noqa: BLE001 — never raise from a status probe
        _log.debug("live: watcher introspection raised: %s", exc)
        return "unknown"


def _open_conn():
    """Open a short-lived store connection with row access by name."""
    conn = db.connect(deps.store_path)
    schema.apply(conn)
    return conn


@router.get("/api/live/stats")
async def get_live_stats(timezone_offset: int = 0) -> dict[str, Any]:
    """Snapshot of the live surface: burn + latency + watcher state.

    ``timezone_offset`` (minutes east of UTC — the same value the other
    cost routes take, i.e. ``new Date().getTimezoneOffset()``) shifts the
    Today/MTD/projection buckets to the caller's local day so the live
    tab agrees with Cost/Overview.

    Includes ``watcher.running`` so the UI can render the
    "watcher-not-running" banner without a parallel ``/api/etl/status``
    fetch — keeps the live tab's first paint to one round-trip.
    """
    conn = _open_conn()
    try:
        snap = live_svc.snapshot(conn, tz_offset=timezone_offset)
    finally:
        conn.close()
    snap["watcher"] = {"running": _watcher_running()}
    return snap


def _format_sse(event_name: str, payload: dict[str, Any]) -> str:
    """Encode one SSE message: explicit ``event:`` + JSON ``data:`` line.

    Browsers' ``EventSource`` exposes named events via
    ``addEventListener(event_name, …)``; we always set one so the UI
    code can dispatch on type without having to parse the payload's
    inner ``type`` field. The inner ``type`` is kept too so non-browser
    consumers (curl + jq) can read the stream as plain JSON-lines.
    """
    body = json.dumps(payload, default=str)
    return f"event: {event_name}\ndata: {body}\n\n"


async def _stream_loop(
    request: Request,
    *,
    poll_interval: float = POLL_INTERVAL_SECONDS,
    burn_interval: float = BURN_INTERVAL_SECONDS,
    max_iterations: int | None = None,
    tz_offset: int = 0,
) -> AsyncIterator[str]:
    """Yield SSE-encoded payloads until the client disconnects.

    ``max_iterations`` is a test affordance — the production path
    leaves it ``None`` so the loop runs forever (until disconnect).

    ``tz_offset`` (minutes east of UTC) is forwarded to ``rolling_burn``
    so the periodic ``burn_tick`` buckets Today/MTD on the client's
    local day, matching the snapshot.

    One store connection is opened for the whole stream and reused across
    every cycle — the previous per-cycle ``connect`` + ``schema.apply``
    (every ~2s, for the life of the tab) re-ran the migration check on a
    hot path for no benefit. The connection is autocommit + WAL, so each
    cycle's reads still see the watcher's latest commits without holding
    a transaction open between ticks.
    """
    # Seed: max ids at connect time so the first tick only sends *new*
    # rows, not the entire table. The frontend gets the same snapshot
    # via /api/live/stats; the ``ready`` event repeats it so a consumer
    # that only opens the stream still has the watermarks it needs.
    conn = _open_conn()
    # Per-stream burn cache: today/MTD reused across burn ticks (see
    # ``live_svc.rolling_burn``). window_cost stays live every tick.
    burn_cache: dict[str, Any] = {}
    try:
        seed_event_id = live_svc.max_event_id(conn)
        seed_tool_id = live_svc.max_tool_call_id(conn)

        yield _format_sse(
            "ready",
            {
                "type": "ready",
                "ts": datetime.now(UTC).isoformat(),
                "payload": {
                    "watermarks": {
                        "event_id": seed_event_id,
                        "tool_call_id": seed_tool_id,
                    },
                    "watcher": {"running": _watcher_running()},
                    "burn_interval_seconds": burn_interval,
                },
            },
        )

        last_event_id = seed_event_id
        last_tool_id = seed_tool_id
        last_burn_at = 0.0  # forces an immediate burn_tick on cycle 0
        loop = asyncio.get_event_loop()
        iterations = 0

        while True:
            if max_iterations is not None and iterations >= max_iterations:
                return
            iterations += 1

            # Fast disconnect check — a closed client should free the
            # generator within DISCONNECT_POLL_INTERVAL_SECONDS even mid-cycle.
            if await request.is_disconnected():
                _log.debug("live.stream: client disconnected; stopping loop")
                return

            # ── new rows since the last cycle (reused connection) ────────
            now = datetime.now(UTC)
            try:
                new_events = live_svc.recent_events(conn, since_id=last_event_id, limit=MAX_PER_CYCLE)
                new_tools = live_svc.recent_tool_calls(conn, since_id=last_tool_id, limit=MAX_PER_CYCLE)
                do_burn = (loop.time() - last_burn_at) >= burn_interval
                burn = (
                    live_svc.rolling_burn(
                        conn,
                        window_minutes=5,
                        now=now,
                        tz_offset=tz_offset,
                        cache=burn_cache,
                    )
                    if do_burn
                    else None
                )
            except Exception as exc:  # noqa: BLE001 — keep the stream alive
                _log.warning("live.stream: cycle read failed: %s", exc)
                new_events = []
                new_tools = []
                burn = None

            for row in new_events:
                yield _format_sse(
                    "event",
                    {
                        "type": "event",
                        "ts": row.get("ts") or now.isoformat(),
                        "payload": row,
                    },
                )
                last_event_id = max(last_event_id, int(row["id"]))

            for row in new_tools:
                yield _format_sse(
                    "tool_call",
                    {
                        "type": "tool_call",
                        "ts": row.get("ts") or now.isoformat(),
                        "payload": row,
                    },
                )
                last_tool_id = max(last_tool_id, int(row["id"]))

            if burn is not None:
                yield _format_sse(
                    "burn_tick",
                    {
                        "type": "burn_tick",
                        "ts": burn["ts"],
                        "payload": burn,
                    },
                )
                last_burn_at = loop.time()

            # ── disconnect-aware sleep ───────────────────────────────────
            # Slice the poll interval so a disconnect mid-sleep breaks out
            # within one slice (100ms) rather than the full poll interval.
            slept = 0.0
            while slept < poll_interval:
                if await request.is_disconnected():
                    return
                chunk = min(DISCONNECT_POLL_INTERVAL_SECONDS, poll_interval - slept)
                await asyncio.sleep(chunk)
                slept += chunk
    finally:
        conn.close()


@router.get("/api/live/stream")
async def get_live_stream(request: Request, timezone_offset: int = 0) -> StreamingResponse:
    """Open the SSE stream for the live tab.

    ``timezone_offset`` (minutes east of UTC) is forwarded to the burn
    tick so its Today/MTD buckets match the snapshot on the client's
    local day.

    Sends the standard headers a browser ``EventSource`` expects
    (``Cache-Control: no-cache`` to defeat any intermediary caching;
    ``X-Accel-Buffering: no`` to flush immediately behind nginx if
    someone fronts the dashboard with one).
    """
    return StreamingResponse(
        _stream_loop(request, tz_offset=timezone_offset),
        media_type="text/event-stream",
        headers={
            "Cache-Control": "no-cache",
            "X-Accel-Buffering": "no",
            "Connection": "keep-alive",
        },
    )


__all__ = ["router", "_stream_loop", "_format_sse"]
