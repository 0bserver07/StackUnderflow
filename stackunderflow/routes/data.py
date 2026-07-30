"""Data/stats/dashboard routes — store-backed, no pipeline or cache imports."""

from __future__ import annotations

import json
import threading
import time
from collections import OrderedDict
from pathlib import Path
from typing import Annotated

from fastapi import APIRouter, HTTPException, Query
from fastapi.concurrency import run_in_threadpool
from fastapi.responses import JSONResponse

import stackunderflow.deps as deps
from stackunderflow.adapters import registered
from stackunderflow.api.messages import (
    build_messages_page,
    get_messages_summary,
    get_paginated_messages,
    page_bounds,
)
from stackunderflow.infra.currency import active_currency_payload
from stackunderflow.ingest import run_ingest
from stackunderflow.routes.cost import (
    COST_KEYS,
    _convert_in_place,
    _invalidate_stats_cache,
    _project_stats_cached,
)
from stackunderflow.stats.aggregator import cache_cost_saved_base_units
from stackunderflow.store import db, mart_queries, queries, schema

router = APIRouter()


# ── dashboard payload memo ────────────────────────────────────────────────────

# In-process memo for /api/dashboard-data. Key = (slug, tz_offset);
# value = (signature, cached_payload). Signature is (max_last_ts, msg_count)
# pulled from the sessions table — both move whenever ingest writes new data,
# so a stale entry can never survive a refresh. /api/refresh calls
# ``invalidate_dashboard_cache()`` defensively for the project it just touched.
#
# Bounded (COST-5b, same defect class as ``routes.cost._STATS_CACHE``): payloads
# are multi-MB and every (slug, tz_offset) pair minted a permanent entry, so an
# unbounded dict grew without limit across a long-running server. LRU-capped at
# ``_DASHBOARD_CACHE_MAX`` under the existing lock; the key shape, the signature
# contract and ``invalidate_dashboard_cache`` are unchanged.
_DASHBOARD_CACHE: OrderedDict[tuple[str, int], tuple[tuple[str | None, int], dict]] = OrderedDict()
_DASHBOARD_CACHE_LOCK = threading.Lock()
_DASHBOARD_CACHE_MAX = 8


def _dashboard_signature(conn, project_ids: list[int]) -> tuple[str | None, int]:
    if not project_ids:
        return (None, 0)
    placeholders = ",".join("?" for _ in project_ids)
    row = conn.execute(
        f"SELECT MAX(last_ts) AS max_ts, COALESCE(SUM(message_count), 0) AS n "
        f"FROM sessions WHERE project_id IN ({placeholders})",
        tuple(project_ids),
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


def _get_project_rows(conn, log_path: str) -> list:
    slug = Path(log_path).name
    rows = queries.get_projects_by_slug(conn, slug=slug)
    if not rows:
        raise HTTPException(
            status_code=404,
            detail=f"Project '{slug}' not found in store — try /api/refresh first",
        )
    return rows


def _get_project_ids(conn, log_path: str) -> list[int]:
    return [r.id for r in _get_project_rows(conn, log_path)]


def _filtered_project_ids(conn, log_path: str, provider_filter: set[str] | None) -> list[int]:
    """Project ids for the slug, narrowed to ``provider_filter`` when set.

    A slug maps to one project per provider (``UNIQUE(provider, slug)``), so a
    provider filter must check EVERY project row — not just the first. The old
    code did ``get_project(slug)`` (a single ``fetchone``) and tested that one
    arbitrary row, so a multi-provider project returned empty whenever an
    earlier-listed provider was excluded. Returns ``[]`` (not 404) when the
    filter excludes every provider, so callers serve a shape-stable empty body.
    """
    rows = _get_project_rows(conn, log_path)
    if provider_filter is None:
        return [r.id for r in rows]
    return [r.id for r in rows if (r.provider or "").lower() in provider_filter]


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
        project_ids = _get_project_ids(conn, log_path)
        # RANK 11, closing the last gap: /api/cost-data and
        # /api/tool-distribution already share the memoized sweep, but this
        # endpoint still recomputed the full collector pipeline (~4s on big
        # projects) on EVERY call — the dashboard's slowest request had no
        # warm path at all. The memo's ingest signature (max last_ts +
        # summed message_count over the project's sessions) moves the moment
        # ingest writes, so a warm hit can never serve pre-ingest numbers.
        # The deep copy the memo returns keeps the in-place trims below
        # (daily cap, heavy-block strip, currency, include filter) from
        # poisoning the shared entry.
        # COST-2: ``keys=None`` (full deep copy) is deliberate and load-bearing
        # here — this endpoint returns every top-level block and mutates most of
        # them in place below (daily cap, heavy-block strip, currency). Narrowing
        # the copy the way /api/cost-data does would drop blocks from the body.
        stats = _project_stats_cached(
            conn,
            project_ids=project_ids,
            slug=Path(log_path).name,
            tz_offset=timezone_offset,
            keys=None,
        )
    finally:
        conn.close()
    deps.logger.debug(f"stats [store] {(time.time() - t0) * 1000:.1f}ms")

    if isinstance(stats, dict):
        # Cap daily_stats — default 90 days, ``days=0`` disables.
        cap_days = 90 if days is None else max(0, days)
        if cap_days > 0:
            _cap_daily_stats(stats, cap_days)
        if not details:
            _strip_heavy_blocks(stats)

    # COST-7: off the event loop — resolution can touch config.json, the FX
    # cache file, and (on a lapsed 24h cache) a blocking urlopen.
    currency = await run_in_threadpool(active_currency_payload)
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
        # Provider filter: a slug maps to one project per provider, so narrow
        # project_ids to the matching providers (checking every row, not just
        # the first). Empty → no provider in scope → shape-stable empty body.
        project_ids = _filtered_project_ids(conn, log_path, provider_filter)
        if provider_filter is not None and not project_ids:
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
        sig = _dashboard_signature(conn, project_ids)

        with _DASHBOARD_CACHE_LOCK:
            cached = _DASHBOARD_CACHE.get(cache_key)
            if cached is not None and cached[0] == sig:
                _DASHBOARD_CACHE.move_to_end(cache_key)  # LRU recency bump
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
                    stats_copy["models"] = {k: v for k, v in stats_copy["models"].items() if k.lower() in model_filter}
                    payload["statistics"] = stats_copy
            payload["currency"] = active_currency_payload()
            deps.logger.debug(f"dashboard-data [hit] {(time.time() - t0) * 1000:.1f}ms")
            return payload

        # Wave 3A: when EVERY project row for this slug is materialised in
        # ``project_mart``, serve the dashboard payload from mart reads. A
        # slug maps to one project per provider (``UNIQUE(provider, slug)``),
        # so a multi-provider project (claude + codex + antigravity + …) has
        # several ids — we loop over all of them and merge the mart rows
        # (summing additive totals) instead of bailing to the ~3.1s pipeline
        # whenever ``len(project_ids) != 1`` (the old guard, which made every
        # multi-provider slug pay the full scan on each cache miss).
        #
        # Gate on ALL ids being materialised: if any provider's id is missing
        # a ``project_mart`` row (ETL hasn't caught up for that source yet) we
        # fall through to the full pipeline rather than serve an undercounted
        # merge. Blocks the marts genuinely don't carry (errors.by_category,
        # interaction-grain ``user_interactions``, hour-of-day ``hourly_pattern``)
        # stay shape-stable so the JSON contract holds; tools + cache + overview
        # + daily + models are now sourced from marts (see _stats_from_marts).
        if project_ids and all(mart_queries.mart_has_project_row(conn, project_id=pid) for pid in project_ids):
            stats = _stats_from_marts(
                conn,
                project_ids=project_ids,
                provider_filter=provider_filter,
                model_filter=None,  # model filter applied below for parity
            )
            messages = []  # dashboard-data only ever exposed first 50 — see §A3
        else:
            messages, stats = queries.get_project_stats(conn, project_id=project_ids, tz_offset=timezone_offset)
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
            k: v for k, v in ui.items() if k not in {"command_details", "tool_count_distribution"}
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
            lean_stats["models"] = {k: v for k, v in models.items() if k.lower() in model_filter}
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
        _DASHBOARD_CACHE.move_to_end(cache_key)
        while len(_DASHBOARD_CACHE) > _DASHBOARD_CACHE_MAX:
            _DASHBOARD_CACHE.popitem(last=False)  # evict least-recently-used
    payload = dict(payload)
    payload["statistics"] = _apply_currency_to_stats(payload["statistics"])
    payload["currency"] = active_currency_payload()
    deps.logger.debug(f"dashboard-data [miss] {(time.time() - t0) * 1000:.1f}ms")
    return payload


# Additive ``project_mart`` columns — safe to SUM when merging the
# per-provider rows of a single slug into one lifetime total.
_PROJECT_MART_ADDITIVE: tuple[str, ...] = (
    "total_messages",
    "total_sessions",
    "total_input_tokens",
    "total_output_tokens",
    "total_cache_read",
    "total_cache_create",
    "total_cost_usd",
    # Message-type + command dims (v022) — per-message counts, disjoint
    # across a slug's providers, so they sum like the token totals.
    "total_user_messages",
    "total_assistant_messages",
    "total_tool_use_messages",
    "total_tool_result_messages",
    "total_commands",
    # Overview rate numerators (v023) — also per-message / per-command counts,
    # so summing the per-provider rows gives the right rate denominators.
    # ``errors_by_category`` is NOT here: it's a JSON map, merged separately
    # from the unmerged per-provider rows (see ``_errors_block_from_marts``).
    "total_records",
    "total_errors",
    "total_cache_read_messages",
    "total_commands_followed_by_interruption",
    "total_command_tools",
    "total_command_steps",
)


def _merge_project_mart_rows(rows: list[dict | None]) -> dict | None:
    """Merge per-provider ``project_mart`` rows into one lifetime total.

    Sums the additive token/cost/message/session columns; takes the
    earliest ``first_ts`` and latest ``last_ts`` across rows (ISO-8601
    strings sort chronologically). A slug's providers never share a
    session id (sessions are per-provider), so summing ``total_sessions``
    needs no cross-provider dedup. Returns ``None`` for an empty list so
    callers fall back to the daily-aggregate overview path.
    """
    present: list[dict] = [r for r in rows if r]
    if not present:
        return None
    if len(present) == 1:
        return present[0]
    merged: dict = {k: 0 for k in _PROJECT_MART_ADDITIVE}
    first_seen: list[str] = []
    last_seen: list[str] = []
    for r in present:
        for k in _PROJECT_MART_ADDITIVE:
            merged[k] += r.get(k) or 0
        if r.get("first_ts"):
            first_seen.append(r["first_ts"])
        if r.get("last_ts"):
            last_seen.append(r["last_ts"])
    merged["first_ts"] = min(first_seen) if first_seen else None
    merged["last_ts"] = max(last_seen) if last_seen else None
    # Carry identity columns from the first row so the merged dict keeps a
    # shape-stable contract (consumers only read the additive + ts fields).
    for k in ("provider", "slug", "display_name"):
        merged[k] = present[0].get(k)
    return merged


def _cache_block_from_mart(merged_row: dict | None, cost_saved_units: float = 0.0) -> dict:
    """Derive the ``cache`` block from ``project_mart`` cache-token totals.

    ``total_created`` / ``total_read`` / ``tokens_saved`` / ``break_even`` come
    from the merged ``project_mart`` row (lifetime totals). ``cost_saved`` is
    priced separately at real per-model rates (``cost_saved_units``, from
    ``_cache_cost_saved_units_from_marts``) rather than the old flat
    ``read·0.9 − created·0.25`` magic constants (#40) — the same
    ``compute_cost`` basis the aggregator's ``_CacheCollector`` now uses, so
    the dollar figure tracks ``tokens_saved`` instead of disagreeing in sign.

    ``hit_rate`` (v023) is ``total_cache_read_messages /
    total_assistant_messages * 100`` — the same ratio (and 1-decimal
    rounding) ``_CacheCollector.result`` emits, now that both numerator
    (assistant rows carrying cache-read tokens) and denominator (assistant
    rows) are materialised on ``project_mart``. Both columns are additive, so
    the merged multi-provider row gives the combined-pipeline hit rate.
    """
    if not merged_row:
        return {"hit_rate": 0.0}
    created = int(merged_row.get("total_cache_create", 0) or 0)
    read = int(merged_row.get("total_cache_read", 0) or 0)
    asst = int(merged_row.get("total_assistant_messages", 0) or 0)
    w_read = int(merged_row.get("total_cache_read_messages", 0) or 0)
    hit_rate = round(w_read / asst * 100, 1) if asst else 0.0
    return {
        "total_created": created,
        "total_read": read,
        "tokens_saved": read - created,
        "cost_saved_base_units": round(cost_saved_units, 2),
        "break_even_achieved": read > created,
        "hit_rate": hit_rate,
    }


def _cache_cost_saved_units_from_marts(conn, project_ids: list[int]) -> float:
    """Lifetime cache cost-saved (base units) priced per model from ``daily_mart``.

    ``project_mart`` carries only the cache-token *totals* (no model
    breakdown), so the dollar saving is priced from ``daily_mart`` rows —
    which DO carry ``(provider, model, speed, cache_read, cache_create)`` —
    through the shared ``cache_cost_saved_base_units`` helper. That uses the
    same real-rate ``compute_cost`` basis as the aggregator's
    ``_CacheCollector`` (replacing the old flat 0.9/0.25 constants, #40).
    Unfiltered (lifetime) to stay consistent with the merged ``project_mart``
    totals the rest of the cache block reports.
    """
    entries: list[tuple[str, str, str, int, int]] = []
    for pid in project_ids:
        for r in mart_queries.daily_for_project(conn, project_id=pid):
            model = r.get("model") or ""
            read = int(r.get("cache_read", 0) or 0)
            created = int(r.get("cache_create", 0) or 0)
            if not model or (not read and not created):
                continue
            entries.append(
                (r.get("provider") or "anthropic", model, r.get("speed") or "standard", read, created)
            )
    return cache_cost_saved_base_units(entries)


def _tools_usage_from_marts(conn, project_ids: list[int]) -> dict[str, int]:
    """Merge per-tool call counts across every project id into ``usage_counts``.

    ``tool_mart`` carries the 1/N-attribution-unit call count per
    ``(tool_name, project)``; we sum it across providers so the Overview
    Tool-Use charts render real usage. ``error_counts`` / ``error_rates``
    have no mart source (no per-tool error flag is materialised) so the
    caller leaves them empty.
    """
    usage: dict[str, int] = {}
    for pid in project_ids:
        for name, t in mart_queries.tool_mart_for_project(conn, project_id=pid).items():
            usage[name] = usage.get(name, 0) + int(t.get("calls", 0) or 0)
    return usage


def _parse_category_map(val) -> dict[str, int]:
    """Normalise a ``project_mart.errors_by_category`` value to a count map.

    The column is a JSON object string; a single-provider merged row hands it
    back verbatim while a future merge may have already parsed it. Tolerates
    ``None`` / malformed / non-dict so a poison row never breaks the payload.
    """
    if isinstance(val, dict):
        return {str(k): int(v or 0) for k, v in val.items()}
    if isinstance(val, str) and val:
        try:
            parsed = json.loads(val)
        except (json.JSONDecodeError, TypeError, ValueError):
            return {}
        if isinstance(parsed, dict):
            return {str(k): int(v or 0) for k, v in parsed.items()}
    return {}


def _errors_block_from_marts(proj_rows: list[dict | None]) -> dict:
    """Build the ``errors`` block from the per-provider ``project_mart`` rows.

    Mirrors ``_ErrorsCollector.result`` on the dims v023 materialises:
    ``total`` (summed ``total_errors``), ``rate`` (``total_errors`` over
    ``total_records`` — the all-kinds record count the aggregator divides by,
    NOT the billable ``total_messages``), and ``by_category`` (the summed
    per-provider JSON maps). Reads the unmerged rows so the non-additive
    category map can be merged key-wise without a special case in
    ``_merge_project_mart_rows``.
    """
    total_errors = 0
    total_records = 0
    by_category: dict[str, int] = {}
    for r in proj_rows:
        if not r:
            continue
        total_errors += int(r.get("total_errors", 0) or 0)
        total_records += int(r.get("total_records", 0) or 0)
        for cat, n in _parse_category_map(r.get("errors_by_category")).items():
            by_category[cat] = by_category.get(cat, 0) + n
    return {
        "total": total_errors,
        "rate": (total_errors / total_records) if total_records else 0.0,
        "by_category": by_category,
    }


def _user_interactions_from_mart(
    merged_row: dict | None, *, windowed_commands: int | None = None
) -> dict:
    """Build the ``user_interactions`` block from ``project_mart`` count dims.

    ``user_commands_analyzed`` (v022) plus the v023 rate numerators
    materialise the Overview's Commands / Steps-per-Cmd / Tools-per-Cmd /
    interruption-rate KPIs. The rates use the same denominator
    (``total_commands``) and rounding as ``_command_analysis`` so a
    mart-backed Overview matches the full pipeline; the raw counts are
    surfaced alongside under the aggregator's own key names.

    #25: when a date window is active, ``windowed_commands`` (summed from
    ``command_day_mart`` for the window) overrides the lifetime
    ``user_commands_analyzed`` so the Commands KPI respects the window like the
    other Overview headline figures. The rate/avg denominators stay on the
    lifetime ``total_commands`` because their numerators (interruption / tools /
    steps) are only materialised lifetime (v023) — windowing only the numerator
    would skew the ratio, so they're left as lifetime values. ``None`` (no
    window) preserves the lifetime command count unchanged.
    """
    if not merged_row:
        return {"user_commands_analyzed": 0 if windowed_commands is None else windowed_commands}
    commands = int(merged_row.get("total_commands", 0) or 0)
    int_followed = int(merged_row.get("total_commands_followed_by_interruption", 0) or 0)
    cmd_tools = int(merged_row.get("total_command_tools", 0) or 0)
    cmd_steps = int(merged_row.get("total_command_steps", 0) or 0)
    return {
        "user_commands_analyzed": commands if windowed_commands is None else windowed_commands,
        "commands_followed_by_interruption": int_followed,
        "total_tools_used": cmd_tools,
        "total_assistant_steps": cmd_steps,
        "interruption_rate": round(int_followed / commands * 100, 1) if commands else 0.0,
        "avg_tools_per_command": round(cmd_tools / commands, 2) if commands else 0.0,
        "avg_steps_per_command": round(cmd_steps / commands, 2) if commands else 0.0,
    }


def _stats_from_marts(
    conn,
    *,
    project_ids: list[int],
    provider_filter: set[str] | None = None,
    model_filter: set[str] | None = None,
    day_from: str | None = None,
    day_to: str | None = None,
) -> dict:
    """Build the dashboard ``statistics`` block from mart reads only.

    Loops over EVERY project id for the slug (one per provider) and merges
    the mart rows so a multi-provider project gets correct lifetime totals
    without falling through to the ~3.1s aggregator pipeline:

    * ``project_mart`` (summed) → ``overview`` lifetime totals + ``cache``
      (incl. ``hit_rate``, v023), ``user_interactions`` (commands +
      interruption rate + tools/steps-per-command, v022/v023), and ``errors``
      (total + rate + ``by_category``, v023)
    * ``daily_mart``   (concatenated) → ``daily_stats`` + ``models`` map
    * ``tool_mart``    (summed)  → ``tools.usage_counts``

    The remaining keys that depend on columns no mart carries — hour-of-day
    ``hourly_pattern`` and the per-tool error flags
    (``tools.error_counts`` / ``error_rates``) — are returned with
    shape-stable values so the JSON contract holds. ``hourly_pattern`` is the
    ``{messages, tokens}`` dict the HourlyPatternChart expects (NOT a bare
    ``[]``, which is truthy and would dodge the frontend's
    ``?? {messages, tokens}`` fallback, rendering a blank chart). The heavy
    detail blocks live behind dedicated lazy endpoints (``/api/cost-data``,
    ``/api/commands``, ``/api/tool-distribution``).
    """
    proj_rows = [mart_queries.get_project_mart_row(conn, project_id=pid) for pid in project_ids]
    merged_proj = _merge_project_mart_rows(proj_rows)

    # Concatenate daily rows across every provider id; daily_mart_by_day /
    # _by_model fold the combined list, so multi-provider days merge cleanly.
    daily_rows: list[dict] = []
    for pid in project_ids:
        daily_rows.extend(
            mart_queries.daily_for_project(
                conn,
                project_id=pid,
                provider_filter=provider_filter,
                model_filter=model_filter,
            )
        )

    overview = mart_queries.daily_mart_to_overview(daily_rows, project_mart_row=merged_proj)
    daily_stats = mart_queries.daily_mart_by_day(daily_rows)
    models = mart_queries.daily_mart_by_model(daily_rows)
    tools_usage = _tools_usage_from_marts(conn, project_ids)

    # #25: when a date window is active, the Commands KPI sums the per-day
    # user-command counts (``command_day_mart``, v025) inside the window so it
    # tracks the window like the tokens/cost headline. No window → leave the
    # lifetime ``total_commands`` (``windowed_commands=None``). The mart-empty
    # case (pre-v025 backfill) also returns ``None`` so the KPI keeps the
    # lifetime value rather than dropping to 0.
    windowed_commands: int | None = None
    if (day_from or day_to) and mart_queries.mart_has_command_day_rows(conn):
        windowed_commands = mart_queries.command_count_in_window(
            conn, project_ids=project_ids, day_from=day_from, day_to=day_to
        )

    return {
        "overview": overview,
        "tools": {
            "usage_counts": tools_usage,
            "error_counts": {},
            "error_rates": {},
        },
        "sessions": {
            "count": int(merged_proj.get("total_sessions", 0)) if merged_proj else 0,
        },
        "daily_stats": daily_stats,
        "hourly_pattern": {"messages": {}, "tokens": {}},
        # total / rate / by_category materialised on ``project_mart`` (v023);
        # ``tool_count_distribution`` and the per-tool error flags stay behind
        # the lazy detail endpoints.
        "errors": _errors_block_from_marts(proj_rows),
        "models": models,
        # Commands KPI (v022) + interruption rate / avg tools-&-steps-per-command
        # (v023) now read real materialised values off the mart. The remaining
        # interaction-grain fields (tool_count_distribution, command_details)
        # stay absent; the UI reads them with ``?? 0`` and the per-project
        # detail view runs the full aggregator on demand.
        "user_interactions": _user_interactions_from_mart(
            merged_proj, windowed_commands=windowed_commands
        ),
        "cache": _cache_block_from_mart(
            merged_proj, _cache_cost_saved_units_from_marts(conn, project_ids)
        ),
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


# `/api/messages` pagination knobs. Default + max chosen so that a 26K-message
# project's default page lands under ~150 KB (was 37 MB unbounded — see
# v0.7.x payload-cap bug). Callers that need everything (export, CSV) must
# walk pages explicitly.
MESSAGES_DEFAULT_PER_PAGE = 100
MESSAGES_MAX_PER_PAGE = 500


def _empty_messages_page(*, page: int, per_page: int) -> dict:
    """Shape-stable empty envelope returned when a filter excludes the project."""
    return {
        "messages": [],
        "total": 0,
        "page": page,
        "per_page": per_page,
        "total_pages": 0,
        "start_index": 0,
        "end_index": 0,
    }


@router.get("/api/messages")
async def get_messages(
    page: int = 1,
    per_page: int = MESSAGES_DEFAULT_PER_PAGE,
    limit: int | None = None,
    timezone_offset: int = 0,
    provider: Annotated[list[str] | None, Query()] = None,
    model: Annotated[list[str] | None, Query()] = None,
):
    """Get a page of messages for the current project.

    Returns a paginated envelope:

        {messages, total, page, per_page, total_pages, start_index, end_index}

    Default ``per_page`` is 100; the maximum is 500. Earlier releases
    returned the full message list unbounded — on a 26K-message project
    that ballooned the response to ~37 MB and OOMed the Messages tab.
    Forcing pagination here caps the worst-case payload at ~750 KB.

    Args:
        page: 1-indexed page number. Out-of-range values are clamped.
        per_page: Items per page. Clamped to ``[1, 500]``.
        limit: Legacy alias preserved for one release — if set and the
            caller didn't specify a custom ``per_page``, it caps
            ``per_page`` (also clamped to ``[1, 500]``). New callers
            should use ``page``/``per_page``.
        timezone_offset: Browser timezone offset.
        provider: Optional repeated query param scoping to those providers.
            The current project belongs to one provider, so an active filter
            that excludes it returns a shape-stable empty envelope.
        model: Optional repeated query param scoping by model id. Filtered
            after the store read since the messages table has no model index.
    """
    log_path = _require_project()
    t0 = time.time()

    # Clamp pagination knobs early so a malicious / accidental huge per_page
    # doesn't slip past the default 500 cap. ``page`` is clamped after we know
    # the total below — clamping it to 1 here is just the lower-bound guard.
    if page < 1:
        page = 1
    # Backwards-compat: callers that still pass ``?limit=N`` get N as the
    # per_page when they didn't specify their own ``per_page``. This keeps
    # any in-flight clients working through one release.
    if limit is not None and per_page == MESSAGES_DEFAULT_PER_PAGE:
        per_page = limit
    if per_page < 1:
        per_page = 1
    if per_page > MESSAGES_MAX_PER_PAGE:
        per_page = MESSAGES_MAX_PER_PAGE

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
        # Provider filter: narrow to matching providers across ALL project rows
        # for the slug (not just the first). Empty → excluded → empty page.
        project_ids = _filtered_project_ids(conn, log_path, provider_filter)
        if provider_filter is not None and not project_ids:
            return _empty_messages_page(page=page, per_page=per_page)
        # Pagination is pushed into SQL. The old path called
        # ``get_project_messages`` — which materialised, enriched AND
        # aggregated every message in the project — then sliced the result in
        # Python, i.e. O(total) work per request even for one page. On a
        # 44K-message project that was the dominant Messages-tab cost. Now we
        # do one indexed COUNT for the envelope total and fetch + reconstruct
        # ONLY the requested page (model filter pushed down too, so the page
        # indices align with the filtered total).
        total = queries.count_project_messages(conn, project_id=project_ids, model_filter=model_filter)
        # Clamp the page against the real total so the SQL OFFSET matches the
        # envelope ``build_messages_page`` reports (same math as the in-memory
        # ``get_paginated_messages`` path used by /api/dashboard-data).
        _page, _pages, start_index, _end = page_bounds(total, page, per_page)
        page_messages = queries.get_project_messages_page(
            conn,
            project_id=project_ids,
            offset=start_index,
            limit=per_page,
            model_filter=model_filter,
        )
    finally:
        conn.close()

    page_payload = build_messages_page(page_messages, total=total, page=page, per_page=per_page)
    deps.logger.debug(f"messages [store] {(time.time() - t0) * 1000:.1f}ms")
    return page_payload


def _summary_by_model_and_tokens(conn, project_ids: list[int]) -> tuple[dict, int]:
    """``(by_model, total_tokens)`` from ONE scoped GROUP BY over ``messages``.

    Replaces the ``get_project_messages`` pass for these two blocks on the
    mart-backed path. Same scoping idiom as ``count_project_messages``: drive
    off ``session_fk IN (SELECT id FROM sessions WHERE project_id IN (…))``
    rather than joining ``sessions``, because against the partitioned
    ``messages`` VIEW a join makes the planner materialise the whole view.

    Parity with ``api.messages.get_messages_summary``: a row with no model
    is keyed ``"N/A"``, because that is the ``Record.model`` default the
    stats enricher stamps and therefore the key the legacy pass produced;
    ``total_tokens`` is ``input + output``, cache tokens excluded there too.

    One known, bounded divergence: Claude Code's ``"<synthetic>"`` model
    sentinel is stripped to NULL by the ingest adapter, so those rows land
    in ``"N/A"`` here while the legacy pass — which re-parsed ``raw_json``
    — gave them their own bucket. Recovering it would mean re-parsing
    ``raw_json``, i.e. the exact cost this path exists to avoid, and every
    other consumer in the tree already treats ``<synthetic>`` as "no model".
    Measured at 16 of 31,893 rows (0.05%) on the largest local project.

    These stay on ``messages`` rather than ``daily_mart`` on purpose:
    ``daily_mart`` counts only billable events, so sourcing them from it
    would silently change what the two blocks mean.
    """
    if not project_ids:
        return {}, 0
    placeholders = ",".join("?" for _ in project_ids)
    rows = conn.execute(
        f"SELECT COALESCE(NULLIF(m.model, ''), 'N/A') AS model, COUNT(*) AS n, "
        f"       SUM(COALESCE(m.input_tokens, 0) + COALESCE(m.output_tokens, 0)) AS tok "
        f"FROM messages m "
        f"WHERE m.session_fk IN "
        f"(SELECT id FROM sessions WHERE project_id IN ({placeholders})) "
        f"GROUP BY 1",
        tuple(project_ids),
    ).fetchall()
    by_model: dict = {}
    total_tokens = 0
    for r in rows:
        by_model[r["model"]] = int(r["n"] or 0)
        total_tokens += int(r["tok"] or 0)
    return by_model, total_tokens


def _messages_summary_from_marts(conn, project_ids: list[int]) -> dict:
    """Build the ``/api/messages/summary`` body without the pipeline.

    ``total`` is the summed ``project_mart.total_records`` — the count of
    every stored record, which is what ``len(messages)`` meant on the legacy
    path. It is NOT ``total_messages``: that column counts BILLABLE EVENTS,
    so the old code returned a total that contradicted its own ``by_type``.

    ``by_type`` is the ``{user, assistant}`` pair summed from
    ``total_user_messages`` / ``total_assistant_messages``. Those two
    partition the record set, so ``sum(by_type) == total`` holds. The
    ``total_tool_use_messages`` / ``total_tool_result_messages`` columns are
    deliberately NOT surfaced here — in the legacy classifier they are
    overlapping flags rather than a partition, so adding them would break
    that invariant.

    Every column read is additive, so a multi-provider slug (one project row
    per provider) merges by summing across its ids.
    """
    total = users = assistants = sessions = 0
    for pid in project_ids:
        row = mart_queries.get_project_mart_row(conn, project_id=pid)
        if not row:
            continue
        total += int(row.get("total_records") or 0)
        users += int(row.get("total_user_messages") or 0)
        assistants += int(row.get("total_assistant_messages") or 0)
        sessions += int(row.get("total_sessions") or 0)

    by_type: dict[str, int] = {}
    if users:
        by_type["user"] = users
    if assistants:
        by_type["assistant"] = assistants

    by_model, total_tokens = _summary_by_model_and_tokens(conn, project_ids)
    return {
        "total": total,
        "by_type": by_type,
        "by_model": by_model,
        "total_tokens": total_tokens,
        "total_sessions": sessions,
    }


@router.get("/api/messages/summary")
def get_messages_summary_endpoint():
    """Get summary statistics about messages without loading all data.

    When EVERY project row for the slug is materialised in ``project_mart``
    the whole body is served from the store directly — summed mart columns
    for ``total`` / ``by_type`` / ``total_sessions`` plus one scoped
    ``GROUP BY model`` for ``by_model`` / ``total_tokens``. The legacy
    ``get_project_messages`` pass (which runs the full pipeline — including
    an ``aggregator.summarise`` whose result this route then discards, ~5s
    and several hundred MB on a 50K-message project) runs only when the gate
    fails, i.e. when some provider's project row hasn't been materialised yet.

    Multi-provider slugs take the fast path too: every column involved is
    additive across the slug's per-provider rows.

    The gate is the same contract ``/api/dashboard-data`` uses
    (``mart_has_project_row`` for all ids), and the response keeps its
    shape: ``{total, by_type, by_model, total_tokens}`` (empty →
    ``{"total": 0, "by_type": {}, "by_model": {}, "total_tokens": 0}``),
    plus ``total_sessions`` on the mart path.
    """
    log_path = _require_project()
    conn = db.connect(deps.store_path)
    try:
        project_ids = _get_project_ids(conn, log_path)
        if project_ids and all(
            mart_queries.mart_has_project_row(conn, project_id=pid) for pid in project_ids
        ):
            return _messages_summary_from_marts(conn, project_ids)
        messages = queries.get_project_messages(conn, project_id=project_ids)
    finally:
        conn.close()
    return get_messages_summary(messages)


def _refresh_current_project_impl(log_path: str) -> JSONResponse:
    """Blocking body of ``/api/refresh`` for the selected project.

    Split out of the ``async`` handler so it can be dispatched with
    ``run_in_threadpool``: ``run_ingest`` walks every adapter's files and
    writes to sqlite, which would otherwise pin the event loop for the whole
    pass.

    ``run_ingest`` returns PROVIDER-keyed counts, so the old
    ``counts.get(slug, 0)`` was structurally always 0 — ``files_changed`` and
    ``message_count`` reported "no changes" no matter what was ingested. We
    sum the values, the same reading ``refresh_all_projects`` uses.

    Reindexing is NOT redone here: ``run_ingest`` already refreshes the
    search / tag / Q&A indexes for every touched slug via
    ``auto_reindex_touched``, which also honours the
    ``auto_reindex_on_ingest`` setting. The block that used to live here
    re-ran ``SearchService.index_project`` on the same slug key, deleting and
    rewriting the index ingest had just written — and ignoring that setting.
    """
    t0 = time.time()
    conn = db.connect(deps.store_path)
    try:
        schema.apply(conn)
        counts = run_ingest(conn, registered())
    finally:
        conn.close()

    slug = Path(log_path).name
    new_msgs = sum(counts.values())

    if new_msgs:
        invalidate_dashboard_cache(slug)
        # The project-stats memo self-invalidates on its sessions signature the
        # same way, but drop it here too — same defensive posture, same scope
        # (this slug only; other projects' entries are untouched by this ingest).
        _invalidate_stats_cache(slug)
        # Optimize cache is keyed on store mtime so it'd self-invalidate
        # on the next read, but a fresh ingest is a good time to drop it
        # eagerly — keeps the next /api/optimize from racing the mtime
        # bump on a filesystem that hasn't flushed yet.
        try:
            from stackunderflow.routes.optimize import invalidate_optimize_cache

            invalidate_optimize_cache()
        except ImportError:
            pass  # optimize route not registered (test environments)

    ms = int((time.time() - t0) * 1000)
    return JSONResponse(
        {
            "status": "success",
            "message": (
                "Files changed - data refreshed successfully" if new_msgs else "No changes detected - using cached data"
            ),
            "files_changed": new_msgs > 0,
            "message_count": new_msgs,
            "refresh_time_ms": ms,
        }
    )


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
        _invalidate_stats_cache()  # every slug may have moved — full clear
        try:
            from stackunderflow.routes.optimize import invalidate_optimize_cache

            invalidate_optimize_cache()
        except ImportError:
            pass  # optimize route not registered (test environments)
    ms = int((time.time() - t0) * 1000)
    return JSONResponse(
        {
            "status": "success",
            "message": (f"Ingested {total_new} new records" if total_new else "No changes detected"),
            "files_changed": total_new > 0,
            "refresh_time_ms": ms,
            "projects_refreshed": total_new,
            "total_projects": total_new,
        }
    )
