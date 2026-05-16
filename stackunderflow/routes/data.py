"""Data/stats/dashboard routes — store-backed, no pipeline or cache imports."""

from __future__ import annotations

import threading
import time
from pathlib import Path
from typing import Annotated

from fastapi import APIRouter, HTTPException, Query
from fastapi.responses import JSONResponse

import stackunderflow.deps as deps
from stackunderflow.adapters import registered
from stackunderflow.api.messages import get_messages_summary, get_paginated_messages
from stackunderflow.infra.currency import active_currency_payload
from stackunderflow.ingest import run_ingest
from stackunderflow.routes.cost import COST_KEYS, _convert_in_place
from stackunderflow.store import db, mart_queries, queries, schema

router = APIRouter()


# ── dashboard payload memo ────────────────────────────────────────────────────

# In-process memo for /api/dashboard-data. Key = (slug, tz_offset);
# value = (signature, cached_payload). Signature is (max_last_ts, msg_count)
# pulled from the sessions table — both move whenever ingest writes new data,
# so a stale entry can never survive a refresh. /api/refresh calls
# ``invalidate_dashboard_cache()`` defensively for the project it just touched.
_DASHBOARD_CACHE: dict[tuple[str, int], tuple[tuple[str | None, int], dict]] = {}
_DASHBOARD_CACHE_LOCK = threading.Lock()


def _dashboard_signature(conn, project_id: int) -> tuple[str | None, int]:
    row = conn.execute(
        "SELECT MAX(last_ts) AS max_ts, COALESCE(SUM(message_count), 0) AS n "
        "FROM sessions WHERE project_id = ?",
        (project_id,),
    ).fetchone()
    return (row["max_ts"], int(row["n"] or 0))


def invalidate_dashboard_cache(slug: str | None = None) -> None:
    """Drop cached dashboard payloads. ``slug=None`` clears every entry."""
    with _DASHBOARD_CACHE_LOCK:
        if slug is None:
            _DASHBOARD_CACHE.clear()
            return
        for key in list(_DASHBOARD_CACHE):
            if key[0] == slug:
                del _DASHBOARD_CACHE[key]


# ── helpers ───────────────────────────────────────────────────────────────────

def _require_project() -> str:
    if not deps.current_log_path:
        raise HTTPException(status_code=400, detail="No project selected")
    return deps.current_log_path


def _get_project_id(conn, log_path: str) -> int:
    slug = Path(log_path).name
    row = queries.get_project(conn, slug=slug)
    if row is None:
        raise HTTPException(
            status_code=404,
            detail=f"Project '{slug}' not found in store — try /api/refresh first",
        )
    return row.id


def _reindex_services(log_path: str, messages: list[dict]) -> None:
    project_dir = Path(log_path).name
    for svc, name in [
        (deps.search_service, "search"),
        (deps.qa_service, "qa"),
        (deps.tag_service, "tags"),
    ]:
        if svc is None:
            continue
        try:
            if name == "tags":
                svc.index_project(messages)
            else:
                svc.index_project(project_dir, messages)
        except Exception as e:
            deps.logger.debug(f"{name} index update failed: {e}")


# ── routes ────────────────────────────────────────────────────────────────────

# ── /api/stats payload trimming ───────────────────────────────────────────────

# Heavy per-row arrays that dominate the response body on real stores.
# On the maintainer's chimera project these accounted for >90% of a 4 MB
# response: 2.65 MB for ``user_interactions.command_details`` (one entry
# per user command), 1.2 MB for ``errors.assistant_details`` (one entry
# per assistant message), plus the outlier / error_details tails. We
# strip them by default and re-include only when ``details=true`` so the
# legacy "full body" contract is still reachable.
_HEAVY_NESTED_LISTS: tuple[tuple[str, str], ...] = (
    ("errors", "assistant_details"),
    ("errors", "error_details"),
    ("user_interactions", "command_details"),
    ("user_interactions", "tool_count_distribution"),
)

_HEAVY_TOP_LEVEL_LISTS: tuple[str, ...] = (
    "session_costs",
    "command_costs",
    "session_efficiency",
    "retry_signals",
)


def _strip_heavy_blocks(stats: dict) -> None:
    """In-place — clear the heaviest per-row lists from a stats dict.

    The keys stay (shape-stable contract — clients can still introspect
    ``stats["errors"]["assistant_details"]``) but the value becomes an
    empty list / dict. Top-level lists (``session_costs`` etc) are emptied
    too so the dashboard's lightweight cards still find their key without
    paying for the full per-session walk.
    """
    for parent, child in _HEAVY_NESTED_LISTS:
        section = stats.get(parent)
        if isinstance(section, dict) and child in section:
            cur = section[child]
            section[child] = [] if isinstance(cur, list) else {}
    for k in _HEAVY_TOP_LEVEL_LISTS:
        cur = stats.get(k)
        if isinstance(cur, list):
            stats[k] = []
    # Outliers: cap each list at 10 entries (was 156 + 288 on chimera).
    out = stats.get("outliers")
    if isinstance(out, dict):
        for k in ("high_tool_commands", "high_step_commands"):
            v = out.get(k)
            if isinstance(v, list) and len(v) > 10:
                out[k] = v[:10]


def _cap_daily_stats(stats: dict, days: int) -> None:
    """In-place — cap ``daily_stats`` to the last ``days`` calendar entries.

    ``daily_stats`` is a date-keyed dict (``"YYYY-MM-DD" → {...}``) from
    the aggregator. We sort the keys and keep the most recent ``days``.
    """
    ds = stats.get("daily_stats")
    if not isinstance(ds, dict) or days <= 0:
        return
    if len(ds) <= days:
        return
    keep = sorted(ds.keys())[-days:]
    stats["daily_stats"] = {k: ds[k] for k in keep}


def _filter_includes(stats: dict, include: set[str]) -> dict:
    """Return a copy of ``stats`` keeping only keys named in ``include``.

    ``currency`` always passes through (UI needs it for any cost block).
    Unknown names in ``include`` are silently ignored.
    """
    out = {k: v for k, v in stats.items() if k in include or k == "currency"}
    return out


@router.get("/api/stats")
async def get_stats(
    timezone_offset: int = 0,
    days: int | None = None,
    include: Annotated[list[str] | None, Query()] = None,
    details: bool = False,
):
    """Get statistics for the current project.

    Args:
        timezone_offset: Browser timezone offset for daily bucketing.
        days: Cap ``daily_stats`` to the most recent ``days`` calendar
            entries (default 90). Pass ``0`` to disable the cap.
        include: Repeated query param — return only the named top-level
            blocks (e.g. ``?include=overview&include=models``). When omitted
            every block is returned. ``currency`` always passes through.
        details: When false (default), strip the heaviest per-row lists
            (``user_interactions.command_details``,
            ``errors.assistant_details``, ``errors.error_details``,
            ``session_costs``, ``command_costs``, etc). On real stores
            this drops the payload from ~4 MB to ~150 KB. Set ``true``
            to opt back into the legacy "full body" response.
    """
    log_path = _require_project()
    t0 = time.time()
    conn = db.connect(deps.store_path)
    try:
        project_id = _get_project_id(conn, log_path)
        _, stats = queries.get_project_stats(conn, project_id=project_id, tz_offset=timezone_offset)
    finally:
        conn.close()
    deps.logger.debug(f"stats [store] {(time.time()-t0)*1000:.1f}ms")

    if isinstance(stats, dict):
        # Cap daily_stats — default 90 days, ``days=0`` disables.
        cap_days = 90 if days is None else max(0, days)
        if cap_days > 0:
            _cap_daily_stats(stats, cap_days)
        if not details:
            _strip_heavy_blocks(stats)

    currency = active_currency_payload()
    if currency["rate_from_usd"] != 1.0:
        _convert_in_place(stats, currency["rate_from_usd"])
    if isinstance(stats, dict):
        stats["currency"] = currency

    if include:
        wanted = {s.strip() for s in include if s and s.strip()}
        if wanted and isinstance(stats, dict):
            stats = _filter_includes(stats, wanted)

    return stats


@router.get("/api/dashboard-data")
async def get_dashboard_data(
    timezone_offset: int = 0,
    provider: Annotated[list[str] | None, Query()] = None,
    model: Annotated[list[str] | None, Query()] = None,
):
    """Get optimized data for initial dashboard load.

    Args:
        timezone_offset: Browser timezone offset for daily-bucket bucketing.
        provider: Optional repeated query param scoping the response to those
            providers. The current project is per-provider in the store, so
            an active filter that excludes the project's provider returns an
            empty payload (signals "no data in this scope" to the UI).
        model: Optional repeated query param scoping the per-model breakdown
            inside `models`. The aggregator runs project-wide; we filter the
            top-level `models` map so the model-distribution card respects
            the user's selection.
    """
    log_path = _require_project()
    t0 = time.time()
    slug = Path(log_path).name
    cache_key = (slug, timezone_offset)

    provider_filter: set[str] | None = None
    if provider:
        normed = {p.strip().lower() for p in provider if p and p.strip()}
        if normed:
            provider_filter = normed

    model_filter: set[str] | None = None
    if model:
        normed_m = {m.strip().lower() for m in model if m and m.strip()}
        if normed_m:
            model_filter = normed_m

    conn = db.connect(deps.store_path)
    try:
        project_id = _get_project_id(conn, log_path)
        # Provider filter: if the active project's provider is excluded,
        # short-circuit to an empty stats body. The UI's empty-state path
        # already handles this gracefully.
        if provider_filter is not None:
            project_row = queries.get_project(conn, slug=slug)
            if project_row is not None and (project_row.provider or "").lower() not in provider_filter:
                currency = active_currency_payload()
                return {
                    "statistics": {},
                    "messages_page": {"messages": [], "page": 1, "per_page": 50, "total": 0},
                    "message_count": 0,
                    "is_reindexing": deps.is_reindexing,
                    "config": {
                        "messages_initial_load": deps.config.get("messages_initial_load"),
                        "max_date_range_days": deps.config.get("max_date_range_days"),
                    },
                    "currency": currency,
                    "filtered": True,
                }
        sig = _dashboard_signature(conn, project_id)

        with _DASHBOARD_CACHE_LOCK:
            cached = _DASHBOARD_CACHE.get(cache_key)
        if cached is not None and cached[0] == sig:
            payload = dict(cached[1])
            payload["is_reindexing"] = deps.is_reindexing
            payload["config"] = {
                "messages_initial_load": deps.config.get("messages_initial_load"),
                "max_date_range_days": deps.config.get("max_date_range_days"),
            }
            payload["statistics"] = _apply_currency_to_stats(payload["statistics"])
            # Apply model filter on the cached payload too so a filter change
            # doesn't require waiting for the cache to expire. We deep-copy
            # before mutating because `_apply_currency_to_stats` already
            # returned a copy at rate ≠ 1, but at rate == 1 it returns the
            # cached dict by reference — filtering would otherwise corrupt
            # the cache entry.
            if model_filter is not None:
                stats_copy = payload["statistics"]
                if isinstance(stats_copy, dict) and isinstance(stats_copy.get("models"), dict):
                    import copy as _copy
                    stats_copy = _copy.deepcopy(stats_copy)
                    stats_copy["models"] = {
                        k: v for k, v in stats_copy["models"].items()
                        if k.lower() in model_filter
                    }
                    payload["statistics"] = stats_copy
            payload["currency"] = active_currency_payload()
            deps.logger.debug(
                f"dashboard-data [hit] {(time.time()-t0)*1000:.1f}ms"
            )
            return payload

        # Wave 3A: when the project is materialised in ``project_mart``,
        # serve the dashboard payload from mart reads. Other keys
        # (tools/errors/hourly_pattern/sessions/user_interactions) are
        # not yet covered by marts — they get shape-stable empties so
        # the JSON contract holds. The heavy detail blocks already
        # live behind dedicated endpoints (/api/cost-data,
        # /api/commands, /api/tool-distribution) that load lazily.
        if mart_queries.mart_has_project_row(conn, project_id=project_id):
            stats = _stats_from_marts(
                conn,
                project_id=project_id,
                provider_filter=provider_filter,
                model_filter=None,  # model filter applied below for parity
            )
            messages = []  # dashboard-data only ever exposed first 50 — see §A3
        else:
            messages, stats = queries.get_project_stats(
                conn, project_id=project_id, tz_offset=timezone_offset
            )
    finally:
        conn.close()

    first_page = get_paginated_messages(messages, page=1, per_page=50)
    # §A3: the heavy analytics sections moved to /api/cost-data. Strip them
    # from this payload so the initial dashboard load stays under 1 MB.
    lean_stats = {k: v for k, v in stats.items() if k not in COST_KEYS}
    # §D1: user_interactions.command_details is the bulk of the remaining
    # payload (~1.8 MB on chimera). Drop the per-command array so only the
    # summary stats — counts, averages, percentages — survive. The Commands
    # tab now fetches that list paginated from /api/commands.
    # §D2: tool_count_distribution can also balloon (one bucket per distinct
    # tool count, dense on busy projects). It moved to /api/tool-distribution
    # for the same reason — keep dashboard-data lean.
    ui = lean_stats.get("user_interactions")
    if isinstance(ui, dict):
        lean_stats["user_interactions"] = {
            k: v for k, v in ui.items()
            if k not in {"command_details", "tool_count_distribution"}
        }
    # Apply model filter to the `models` map so the Overview tab's model
    # distribution card respects the active selection. We don't recompute
    # downstream aggregates (cost, tokens) — the dashboard surfaces them
    # at the project level and recomputing would be expensive without a
    # corresponding visible payoff. Frontend tabs that care about per-model
    # cost (Compare, Cost-by-provider) hit purpose-built endpoints that
    # already accept ``?model=`` independently.
    if model_filter is not None:
        models = lean_stats.get("models")
        if isinstance(models, dict):
            lean_stats["models"] = {
                k: v for k, v in models.items()
                if k.lower() in model_filter
            }
    payload = {
        "statistics": lean_stats,
        "messages_page": first_page,
        "message_count": len(messages),
        "is_reindexing": deps.is_reindexing,
        "config": {
            "messages_initial_load": deps.config.get("messages_initial_load"),
            "max_date_range_days": deps.config.get("max_date_range_days"),
        },
    }
    with _DASHBOARD_CACHE_LOCK:
        # Cache the USD-denominated payload — currency conversion happens
        # on every request so a config change doesn't require a cache flush.
        _DASHBOARD_CACHE[cache_key] = (sig, payload)
    payload = dict(payload)
    payload["statistics"] = _apply_currency_to_stats(payload["statistics"])
    payload["currency"] = active_currency_payload()
    deps.logger.debug(f"dashboard-data [miss] {(time.time()-t0)*1000:.1f}ms")
    return payload


def _stats_from_marts(
    conn,
    *,
    project_id: int,
    provider_filter: set[str] | None = None,
    model_filter: set[str] | None = None,
) -> dict:
    """Build the dashboard ``statistics`` block from mart reads only.

    Three mart sources combine into the legacy aggregator shape:

    * ``project_mart`` → ``overview`` lifetime totals
    * ``daily_mart``   → ``daily_stats`` time-series + ``models`` map
    * cost / token rollups in both → keys consumed by the UI's Overview
      cards

    Keys that depend on raw-message columns the marts don't carry —
    ``tools``, ``errors``, ``hourly_pattern``, ``cache``, per-session
    detail, ``user_interactions`` — are returned with shape-stable
    empties. The heavy detail blocks already live behind dedicated
    endpoints (``/api/cost-data``, ``/api/commands``,
    ``/api/tool-distribution``) that the dashboard fetches lazily;
    the trade-off here is sub-50ms initial paint vs slightly less
    rich initial response.
    """
    proj_row = mart_queries.get_project_mart_row(conn, project_id=project_id)
    daily_rows = mart_queries.daily_for_project(
        conn,
        project_id=project_id,
        provider_filter=provider_filter,
        model_filter=model_filter,
    )

    overview = mart_queries.daily_mart_to_overview(
        daily_rows, project_mart_row=proj_row
    )
    daily_stats = mart_queries.daily_mart_by_day(daily_rows)
    models = mart_queries.daily_mart_by_model(daily_rows)

    return {
        "overview": overview,
        "tools": {"usage_counts": {}, "error_counts": {}, "error_rates": {}},
        "sessions": {
            "count": int(proj_row.get("total_sessions", 0)) if proj_row else 0,
        },
        "daily_stats": daily_stats,
        "hourly_pattern": [],
        "errors": {"total": 0},
        "models": models,
        "user_interactions": {},
        "cache": {"hit_rate": 0.0},
    }


def _apply_currency_to_stats(stats: dict) -> dict:
    """Return a copy of ``stats`` with cost figures scaled to the active currency."""
    currency = active_currency_payload()
    rate = currency["rate_from_usd"]
    if rate == 1.0:
        return stats
    # Deep-copy via JSON round-trip — _convert_in_place mutates, and we don't
    # want to scale the cached USD payload.
    import copy
    scaled = copy.deepcopy(stats)
    _convert_in_place(scaled, rate)
    return scaled


@router.get("/api/messages")
async def get_messages(
    limit: int | None = None,
    timezone_offset: int = 0,
    provider: Annotated[list[str] | None, Query()] = None,
    model: Annotated[list[str] | None, Query()] = None,
):
    """Get messages for the current project.

    Args:
        limit: Maximum number of messages to return.
        timezone_offset: Browser timezone offset.
        provider: Optional repeated query param scoping to those providers.
            The current project belongs to one provider, so an active filter
            that excludes it returns an empty list.
        model: Optional repeated query param scoping by model id. Filtered
            client-side after the messages list comes back from the store
            (the messages table doesn't have a separate index on model and
            this list is already loaded fully into memory at the API).
    """
    log_path = _require_project()
    t0 = time.time()

    provider_filter: set[str] | None = None
    if provider:
        normed = {p.strip().lower() for p in provider if p and p.strip()}
        if normed:
            provider_filter = normed

    model_filter: set[str] | None = None
    if model:
        normed_m = {m.strip().lower() for m in model if m and m.strip()}
        if normed_m:
            model_filter = normed_m

    conn = db.connect(deps.store_path)
    try:
        project_id = _get_project_id(conn, log_path)
        # Provider filter — short-circuit when the project is excluded.
        if provider_filter is not None:
            slug = Path(log_path).name
            project_row = queries.get_project(conn, slug=slug)
            if project_row is not None and (project_row.provider or "").lower() not in provider_filter:
                return []
        messages = queries.get_project_messages(conn, project_id=project_id, limit=limit)
    finally:
        conn.close()

    if model_filter is not None and messages:
        messages = [m for m in messages if (m.get("model") or "").lower() in model_filter]

    deps.logger.debug(f"messages [store] {(time.time()-t0)*1000:.1f}ms")
    return messages


@router.get("/api/messages/summary")
async def get_messages_summary_endpoint():
    """Get summary statistics about messages without loading all data.

    Wave 4A — when ``project_mart`` carries a row for the active
    project, the top-level ``total`` (and bonus ``total_sessions``)
    come from a single mart read. The detail blocks (``by_type``,
    ``by_model``, ``total_tokens``) still need the full message list
    because those columns aren't materialised into any mart yet, so
    we fall back to the legacy ``get_project_messages`` pass for the
    breakdown — and unconditionally when the mart is empty.
    """
    log_path = _require_project()
    conn = db.connect(deps.store_path)
    try:
        project_id = _get_project_id(conn, log_path)
        mart_totals = mart_queries.project_mart_messages_summary_totals(
            conn, project_id=project_id
        )
        messages = queries.get_project_messages(conn, project_id=project_id)
    finally:
        conn.close()
    summary = get_messages_summary(messages)
    if mart_totals is not None:
        # Mart row wins on the top-level total — it's the project's
        # lifetime message count from ``project_mart``, identical in
        # value but cheaper than counting the materialised messages
        # list. The breakdown blocks (``by_type`` / ``by_model``) still
        # come from the messages pass because those dimensions aren't
        # in any mart today.
        summary["total"] = mart_totals["total"]
        summary["total_sessions"] = mart_totals["total_sessions"]
    return summary


@router.post("/api/refresh")
async def refresh_data(request: dict):
    """Refresh project data — runs an incremental ingest pass then returns status."""
    if not deps.current_log_path:
        return await refresh_all_projects(request)

    log_path = deps.current_log_path
    t0 = time.time()
    conn = db.connect(deps.store_path)
    try:
        schema.apply(conn)
        counts = run_ingest(conn, registered())
    finally:
        conn.close()

    slug = Path(log_path).name
    new_msgs = counts.get(slug, 0)

    if new_msgs:
        invalidate_dashboard_cache(slug)
        # Optimize cache is keyed on store mtime so it'd self-invalidate
        # on the next read, but a fresh ingest is a good time to drop it
        # eagerly — keeps the next /api/optimize from racing the mtime
        # bump on a filesystem that hasn't flushed yet.
        try:
            from stackunderflow.routes.optimize import invalidate_optimize_cache
            invalidate_optimize_cache()
        except ImportError:
            pass  # optimize route not registered (test environments)
        conn2 = db.connect(deps.store_path)
        try:
            row = queries.get_project(conn2, slug=slug)
            if row is not None:
                messages = queries.get_project_messages(conn2, project_id=row.id)
                deps.is_reindexing = True
                try:
                    _reindex_services(log_path, messages)
                finally:
                    deps.is_reindexing = False
        finally:
            conn2.close()

    ms = int((time.time() - t0) * 1000)
    return JSONResponse({
        "status": "success",
        "message": (
            "Files changed - data refreshed successfully"
            if new_msgs else "No changes detected - using cached data"
        ),
        "files_changed": new_msgs > 0,
        "message_count": new_msgs,
        "refresh_time_ms": ms,
    })


async def refresh_all_projects(request: dict):
    """Refresh all projects — runs an incremental ingest pass via the session store."""
    t0 = time.time()
    conn = db.connect(deps.store_path)
    try:
        schema.apply(conn)
        counts = run_ingest(conn, registered())
    finally:
        conn.close()

    total_new = sum(counts.values())
    if total_new:
        invalidate_dashboard_cache()
        try:
            from stackunderflow.routes.optimize import invalidate_optimize_cache
            invalidate_optimize_cache()
        except ImportError:
            pass  # optimize route not registered (test environments)
    ms = int((time.time() - t0) * 1000)
    return JSONResponse({
        "status": "success",
        "message": (
            f"Ingested {total_new} new records"
            if total_new else "No changes detected"
        ),
        "files_changed": total_new > 0,
        "refresh_time_ms": ms,
        "projects_refreshed": total_new,
        "total_projects": total_new,
    })
