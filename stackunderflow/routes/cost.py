"""Cost / analytics routes — split out of ``/api/dashboard-data`` per spec §A3.

Two endpoints live here:

* ``GET /api/cost-data`` — returns only the 9 analytics keys produced by the
  collector sweep in ``aggregator.summarise()``. The base dashboard payload
  kept the high-level overview; this endpoint serves the heavy per-session /
  per-command / per-tool breakdowns the Cost tab consumes lazily.

* ``GET /api/interaction/{interaction_id}`` — returns one enriched
  ``Interaction`` (the user command + every assistant response + every
  tool_result between them) so the Messages tab can link deep to a specific
  prompt without paging through the full message list.
"""

from __future__ import annotations

import copy
import threading
from datetime import date, timedelta
from pathlib import Path
from typing import Annotated, Any

from fastapi import APIRouter, HTTPException, Query

import stackunderflow.deps as deps
from stackunderflow.infra.currency import active_currency_payload
from stackunderflow.store import db, mart_queries, queries

router = APIRouter()


# ── currency conversion helpers ──────────────────────────────────────────────

# Cost-bearing fields inside the §A3 analytics payload. Each is converted
# from USD into the active currency so the frontend never has to multiply
# by an FX rate.
_COST_FIELDS_PER_ROW: tuple[str, ...] = (
    "cost",
    "total_cost",
    "estimated_retry_cost",
    "estimated_cost",
    "estimated_wasted_cost",
    "cost_per_command",
)

# Sub-trees whose numeric leaves are ratios / period-over-period percentages,
# NOT absolute USD amounts — currency conversion must skip them wholesale.
# ``trends.delta_pct`` carries ``cost`` / ``cost_per_command`` keys that are
# percentage deltas (``(cur - prior) / prior * 100``); FX-scaling them would
# silently distort the number the UI renders as a "%". We prune the whole
# branch by parent name before any leaf reaches the cost whitelist.
_NO_CONVERT_SUBTREES: frozenset[str] = frozenset({"delta_pct"})


def _convert_amount(value: Any, rate: float) -> Any:
    if isinstance(value, (int, float)) and not isinstance(value, bool):
        return float(value) * rate
    return value


def _scale_all_numbers(node: Any, rate: float) -> Any:
    """Scale EVERY numeric leaf under ``node`` by ``rate``.

    Used once we're already inside a cost-named subtree (e.g.
    ``daily_stats[day]["cost"] = {"total", "by_model"}``) where every number
    is a USD amount but the leaf keys — ``total``, the model ids under
    ``by_model``, and each model's ``input_cost`` / ``output_cost`` /
    ``cache_*_cost`` components (a bare float on the mart path) — don't match
    the field-name whitelist ``_convert_in_place`` keys on. Mutates dicts in
    place; rebuilds lists. ``delta_pct`` is still pruned so a percentage
    subtree that ever lands under a cost key is never FX-scaled.
    """
    if isinstance(node, dict):
        for key, val in list(node.items()):
            if key in _NO_CONVERT_SUBTREES:
                continue
            node[key] = _scale_all_numbers(val, rate)
        return node
    if isinstance(node, list):
        return [_scale_all_numbers(item, rate) for item in node]
    return _convert_amount(node, rate)


def _convert_in_place(node: Any, rate: float) -> Any:
    """Recursively scale every cost-named numeric leaf by ``rate``.

    We deliberately key on field names — touching every numeric value would
    incorrectly scale token counts, durations, and retry counts that share
    a parent dict with cost figures. Sub-trees named in
    ``_NO_CONVERT_SUBTREES`` (e.g. ``trends.delta_pct``) hold percentages,
    not dollars, so we prune them rather than descend.
    """
    if isinstance(node, dict):
        for key, val in list(node.items()):
            if key in _NO_CONVERT_SUBTREES:
                continue
            if key in _COST_FIELDS_PER_ROW:
                # ``cost`` is a scalar on session/command/tool rows but a
                # nested USD subtree on ``daily_stats[day]`` —
                # ``{"total", "by_model": {model: {input_cost, ...}}}`` from
                # the aggregator, or ``{"total", "by_model": {model: <float>}}``
                # on the mart path. #21: that subtree shipped unconverted
                # because ``_convert_amount`` no-ops on a dict and the whitelist
                # skips recursion, so EUR/GBP DailyCost bars summed raw USD.
                # Inside a known-cost subtree every numeric leaf is dollars, so
                # scale them all regardless of leaf-key name.
                if isinstance(val, (dict, list)):
                    node[key] = _scale_all_numbers(val, rate)
                else:
                    node[key] = _convert_amount(val, rate)
            else:
                _convert_in_place(val, rate)
    elif isinstance(node, list):
        for item in node:
            _convert_in_place(item, rate)
    return node


# The 9 analytics keys that moved off ``/api/dashboard-data`` (spec §A3).
COST_KEYS: tuple[str, ...] = (
    "session_costs",
    "command_costs",
    "tool_costs",
    "token_composition",
    "outliers",
    "retry_signals",
    "session_efficiency",
    "error_cost",
    "trends",
)


def _resolve_log_path(log_path: str | None) -> str:
    """Prefer explicit query param, fall back to ``deps.current_log_path``."""
    path = log_path or deps.current_log_path
    if not path:
        raise HTTPException(
            status_code=400,
            detail="No project selected or log_path provided",
        )
    return path


def _project_ids_for(conn, path: str) -> list[int]:
    slug = Path(path).name
    rows = queries.get_projects_by_slug(conn, slug=slug)
    if not rows:
        raise HTTPException(
            status_code=404,
            detail=f"Project '{slug}' not found in store — try /api/refresh first",
        )
    return [r.id for r in rows]


# ── project-stats memo (perf, RANK 11) ────────────────────────────────────────

# ``/api/cost-data`` and ``/api/tool-distribution`` both need the full,
# uncached ``queries.get_project_stats`` sweep (the collector pipeline —
# 1.4-4s on big projects). Without a memo the Overview tab pays that cost
# 2-3x over: ``/api/dashboard-data`` runs the pipeline, then ``/api/cost-data``
# runs it again, then ``/api/tool-distribution`` a third time. We memoize the
# raw (USD, pre-overlay) stats dict keyed on (store, slug, tz_offset) plus a
# sessions signature (max ``last_ts``, summed ``message_count``). The signature
# moves the instant ingest writes new rows, so a stale entry can't outlive a
# refresh — the same self-invalidation contract data.py's ``_DASHBOARD_CACHE``
# relies on. (Pricing-config edits don't bump the signature; those are rare and
# self-heal on the next ingest — matching the dashboard cache's blast radius.)
_STATS_CACHE: dict[tuple[str, str, int, tuple[int, ...]], tuple[tuple[str | None, int], dict]] = {}
_STATS_CACHE_LOCK = threading.Lock()


def _stats_signature(conn, project_ids: list[int]) -> tuple[str | None, int]:
    """(max last_ts, summed message_count) over the project's sessions.

    Mirrors ``routes.data._dashboard_signature`` — replicated rather than
    imported because data.py imports from this module (importing back would
    cycle).
    """
    if not project_ids:
        return (None, 0)
    placeholders = ",".join("?" for _ in project_ids)
    row = conn.execute(
        f"SELECT MAX(last_ts) AS max_ts, COALESCE(SUM(message_count), 0) AS n "
        f"FROM sessions WHERE project_id IN ({placeholders})",
        tuple(project_ids),
    ).fetchone()
    return (row["max_ts"], int(row["n"] or 0))


def _invalidate_stats_cache(slug: str | None = None) -> None:
    """Drop memoized stats. ``slug=None`` clears every entry."""
    with _STATS_CACHE_LOCK:
        if slug is None:
            _STATS_CACHE.clear()
            return
        for key in list(_STATS_CACHE):
            if key[1] == slug:
                del _STATS_CACHE[key]


def _project_stats_cached(conn, *, project_ids: list[int], slug: str, tz_offset: int) -> dict:
    """Memoized ``queries.get_project_stats`` → stats dict (USD, pre-overlay).

    Returns a deep copy so the caller may mutate freely (mart overlay,
    currency conversion) without poisoning the shared cache entry. On a
    signature mismatch (new ingest) or cold cache the full pipeline runs and
    the result is cached for the next reader.
    """
    sig = _stats_signature(conn, project_ids)
    # The id tuple is part of the key so a provider-narrowed subset (#33 —
    # /api/tool-distribution filtering a multi-provider slug) never collides
    # with the slug's all-provider entry.
    cache_key = (str(deps.store_path), slug, tz_offset, tuple(sorted(project_ids)))
    with _STATS_CACHE_LOCK:
        cached = _STATS_CACHE.get(cache_key)
    if cached is not None and cached[0] == sig:
        return copy.deepcopy(cached[1])
    _, stats = queries.get_project_stats(conn, project_id=project_ids, tz_offset=tz_offset)
    with _STATS_CACHE_LOCK:
        _STATS_CACHE[cache_key] = (sig, stats)
    return copy.deepcopy(stats)


# ``?range=`` values the route accepts. ``all`` (and absent) mean "no window";
# the two windowed values mirror the Cost tab's FilterBar presets.
_COST_DATA_RANGE_DAYS: dict[str, int | None] = {"all": None, "7d": 7, "30d": 30}


@router.get("/api/cost-data")
async def get_cost_data(
    log_path: str | None = None,
    timezone_offset: int = 0,
    model: Annotated[list[str] | None, Query()] = None,
    range_: Annotated[str | None, Query(alias="range")] = None,
):
    """Return only the 9 cost/analytics sections split off from dashboard-data.

    Shape: ``{key: stats[key]}`` for every key in ``COST_KEYS``. Missing keys
    default to empty containers (``[]``, ``{}``) so the frontend can render
    without guarding for undefined sections.

    #57: ``model`` is a repeated query param (``?model=opus-4-8&model=…``) that
    broadens the dashboard's model filter beyond cost. When set, the
    ``token_composition.daily``/``totals`` blocks are sourced from
    ``daily_mart`` narrowed to the selected model(s) — so the Token Composition
    donut/stack on the Cost tab reflect the filter, not just the by-model cost
    chart. Empty/absent == all models (the existing contract). ``token_composition``
    is the block the daily mart can narrow by model; ``tool_mart`` has no model
    dimension, so ``tool_costs`` stays all-model and the frontend badges it.

    #24: ``range`` (``7d`` | ``30d`` | ``all``) windows the ``tool_costs``
    block on the mart fast-path — ``tool_mart`` is keyed on ``day``, so the
    per-tool rollup can honour the Cost tab's date filter server-side. The
    window anchors on the project's most recent ``daily_mart`` day (matching
    the frontend's "anchor on latest activity" cutoff) rather than today, so
    idle projects don't collapse to an empty chart. The response's
    ``tool_costs_windowed`` flag tells the frontend whether the window was
    actually applied — the aggregator fallback and the model-filtered path
    (see #57 above) can't window, and keep their honesty badge.
    """
    path = _resolve_log_path(log_path)
    slug = Path(path).name

    model_filter: set[str] | None = None
    if model:
        normed = {m.strip().lower() for m in model if m and m.strip()}
        if normed:
            model_filter = normed

    if range_ is not None and range_ not in _COST_DATA_RANGE_DAYS:
        raise HTTPException(
            status_code=400,
            detail=(f"Unknown range '{range_}'. Valid: {', '.join(sorted(_COST_DATA_RANGE_DAYS))}"),
        )
    window_days = _COST_DATA_RANGE_DAYS.get(range_ or "all")

    tool_costs_windowed = False
    conn = db.connect(deps.store_path)
    try:
        project_ids = _project_ids_for(conn, path)
        # RANK 11: memoize the heavy pipeline so repeat Overview/Cost loads
        # (and the sibling /api/tool-distribution call) skip the recompute.
        stats = _project_stats_cached(conn, project_ids=project_ids, slug=slug, tz_offset=timezone_offset)
        # Wave 3A: when the project is materialised, overlay the
        # token_composition.daily/totals blocks with daily_mart-derived
        # values. Per-session / per-command / per-tool detail blocks
        # (session_costs, command_costs, tool_costs, outliers,
        # retry_signals, session_efficiency, error_cost) stay
        # aggregator-driven — they need lower-grain marts deferred to
        # Wave 4. ``trends`` keeps its aggregator-driven shape because
        # the period split (current vs prior) needs interaction-level
        # correlations the daily mart can\'t see by itself.
        if len(project_ids) == 1 and mart_queries.mart_has_project_row(conn, project_id=project_ids[0]):
            stats = _overlay_mart_rollups(
                conn,
                project_id=project_ids[0],
                stats=stats,
                model_filter=model_filter,
                window_days=window_days,
            )
            tool_costs_windowed = bool(stats.pop("_tool_costs_windowed", False))
    finally:
        conn.close()

    payload: dict[str, Any] = {}
    for key in COST_KEYS:
        val = stats.get(key)
        if val is None:
            # dict-shaped sections get {}, list-shaped get [] — safer than null
            # for the typed React consumers downstream.
            val = {} if key in {"tool_costs", "token_composition", "outliers", "error_cost", "trends"} else []
        payload[key] = val

    currency = active_currency_payload()
    if currency["rate_from_usd"] != 1.0:
        for key in COST_KEYS:
            _convert_in_place(payload[key], currency["rate_from_usd"])
    payload["currency"] = currency
    payload["tool_costs_windowed"] = tool_costs_windowed
    return payload


def _overlay_mart_rollups(
    conn,
    *,
    project_id: int,
    stats: dict,
    model_filter: set[str] | None = None,
    window_days: int | None = None,
) -> dict:
    """Replace the rollup blocks of ``stats`` with mart-derived values.

    Touches keys the day/tool/command marts can reconstruct:

    * ``token_composition.daily`` / ``token_composition.totals`` — from
      ``daily_mart`` (Wave 3A). ``model_filter`` (#57) narrows the daily rows to
      the selected model(s) so the Token Composition charts honour the
      dashboard's model filter; ``None`` == all models.
    * ``tool_costs`` — from ``tool_mart`` (Wave 5). The mart's
      pre-attributed 1/N cost/token shares mirror the aggregator's
      ``_ToolCostCollector`` contract so the JSON shape is identical.
      ``tool_mart`` is keyed (day, project, provider, tool_name) with NO model
      dimension, so it can't be model-filtered — when a model filter is active
      we therefore SKIP the tool_costs overlay and leave the aggregator's
      all-model figures (the frontend badges them as all-model).
      ``window_days`` (#24) narrows the rollup to the last N calendar days,
      anchored on the project's most recent ``daily_mart`` day; when the
      window is applied the ``_tool_costs_windowed`` marker is set on
      ``stats`` so the route can report it (the caller pops the marker).

    ``command_costs`` and ``session_costs`` stay aggregator-driven.
    Their existing shapes are per-Interaction / per-session lists keyed
    on ``interaction_id`` / ``session_id`` and carry per-row fields
    (``prompt_preview``, ``timestamp``, ``models_used``, ``tools_used``,
    ``steps``, ``had_error``) that the (day, project_id, command_name)
    ``command_mart`` rollup cannot reconstruct — the grain throws them
    away on ingest. ``command_mart_for_project`` returns sums over
    ``command_name``, not per-Interaction rows; extending the helper
    cannot recover what the grain doesn't store. A future mart at
    (project_id, interaction_id) grain could power this overlay; until
    then the aggregator's ``_CommandCostCollector`` is the only source
    of the per-Interaction shape the frontend (`CommandCostList`,
    typed against `CommandCost[]`) consumes. The route's ``trends``
    block also stays on the aggregator path because the period split
    (current vs prior) needs interaction-level correlations the daily
    mart can't see by itself. Returns the same dict object (mutated)
    so the caller's payload assembly stays unchanged.

    The ``command_mart`` IS populated by the ETL pipeline and read by
    ``reports/optimize.py``'s pattern early-exits + the CLI
    ``stackunderflow report`` command — see
    ``store/mart_queries.command_mart_for_project``. It just isn't a
    drop-in source for THIS route's response shape.
    """
    daily_rows = mart_queries.daily_for_project(
        conn, project_id=project_id, model_filter=model_filter
    )
    if not daily_rows:
        # A model filter that excludes every row of this project leaves the
        # token_composition blocks empty (shape-stable) so the donut/stack
        # render their zero state instead of all-model totals.
        if model_filter is not None:
            tc = stats.get("token_composition")
            if not isinstance(tc, dict):
                tc = {"daily": {}, "totals": {}, "per_session": {}}
                stats["token_composition"] = tc
            tc["daily"] = {}
            tc["totals"] = {"input": 0, "output": 0, "cache_read": 0, "cache_creation": 0}
            # No rows survive the filter → no reasoning to attribute either.
            tc.pop("reasoning_share", None)
        return stats

    daily: dict[str, dict[str, int]] = {}
    totals = {"input": 0, "output": 0, "cache_read": 0, "cache_creation": 0}
    for r in daily_rows:
        day = r.get("day")
        if not day:
            continue
        bucket = daily.setdefault(
            day,
            {"input": 0, "output": 0, "cache_read": 0, "cache_creation": 0},
        )
        bucket["input"] += int(r.get("input_tokens", 0) or 0)
        bucket["output"] += int(r.get("output_tokens", 0) or 0)
        bucket["cache_read"] += int(r.get("cache_read", 0) or 0)
        bucket["cache_creation"] += int(r.get("cache_create", 0) or 0)
        totals["input"] += int(r.get("input_tokens", 0) or 0)
        totals["output"] += int(r.get("output_tokens", 0) or 0)
        totals["cache_read"] += int(r.get("cache_read", 0) or 0)
        totals["cache_creation"] += int(r.get("cache_create", 0) or 0)

    tc = stats.get("token_composition")
    if not isinstance(tc, dict):
        tc = {"daily": {}, "totals": {}, "per_session": {}}
        stats["token_composition"] = tc

    # Reasoning is an attribution-only subset of output that the marts do NOT
    # carry (materialising it on ``daily_mart`` is a deferred follow-up). The
    # aggregator DID compute an authoritative all-model reasoning total from the
    # full pipeline, so when NO model filter is active we carry that total (and
    # its derived share) onto the mart-derived ``totals`` — the reasoning count
    # is model-independent here, so pairing it with the mart output total is
    # exact. Under a model filter we drop it: ``daily_mart`` can't attribute
    # reasoning to the selected model(s), and showing an all-model reasoning
    # figure against model-scoped output would misattribute (same reason
    # ``tool_costs`` is skipped when filtered).
    prior_totals = tc.get("totals") if isinstance(tc.get("totals"), dict) else {}
    reasoning_total = int(prior_totals.get("reasoning", 0) or 0)
    if model_filter is None and reasoning_total > 0:
        totals["reasoning"] = reasoning_total
        out_tok = int(totals.get("output", 0) or 0)
        tc["reasoning_share"] = (reasoning_total / out_tok) if out_tok > 0 else 0.0
    else:
        tc.pop("reasoning_share", None)

    tc["daily"] = daily
    tc["totals"] = totals

    # Wave 5: overlay tool_costs from tool_mart when populated. Empty
    # mart → keep whatever the aggregator emitted (default fallback).
    # #57: skip when a model filter is active — ``tool_mart`` has no model
    # dimension, so the mart rollup can't be narrowed to the selected model;
    # leaving the aggregator's all-model tool_costs (badged in the UI) beats
    # silently showing all-model numbers that look model-scoped.
    if model_filter is None and mart_queries.mart_has_tool_rows(conn):
        # #24: window the per-tool rollup to the last N calendar days,
        # anchored on the most recent daily_mart day (daily_rows is the full,
        # unfiltered series here — model_filter is None on this branch). The
        # anchor day counts as day 1, so 7d == the last 7 calendar days.
        day_from: str | None = None
        if window_days is not None:
            anchor = max((r.get("day") or "") for r in daily_rows)
            try:
                anchor_date = date.fromisoformat(anchor)
                day_from = (anchor_date - timedelta(days=window_days - 1)).isoformat()
            except ValueError:
                # Un-parseable anchor day (malformed source timestamp) — fall
                # back to the unwindowed rollup rather than 500ing; the
                # frontend badge covers the "window not applied" case.
                day_from = None
        tool_rows = mart_queries.tool_mart_for_project(
            conn,
            project_id=project_id,
            day_from=day_from,
        )
        if day_from is not None:
            # Windowed request: an empty in-window rollup means "no tool
            # activity in range" and must replace the aggregator's all-time
            # block (an all-time chart under a 7d filter is the #24 bug).
            stats["tool_costs"] = _tool_mart_to_aggregator_shape(tool_rows)
            stats["_tool_costs_windowed"] = True
        elif tool_rows:
            stats["tool_costs"] = _tool_mart_to_aggregator_shape(tool_rows)

    return stats


def _tool_mart_to_aggregator_shape(
    tool_rows: dict[str, dict[str, Any]],
) -> dict[str, dict[str, Any]]:
    """Reshape ``tool_mart_for_project`` rows into ``tool_costs`` JSON.

    The legacy ``_ToolCostCollector`` (``stats/aggregator.py`` §1.3)
    emits ``{tool_name: {calls, input_tokens, output_tokens,
    cache_read_tokens, cache_creation_tokens, cost}}``. ``tool_mart``
    pre-attributes the 1/N split per the same contract; we just rename
    the column keys to the aggregator's field names so the JSON
    consumer doesn't notice the swap.

    ``calls`` is the distinct ``(message, tool)`` pair count (the mart's
    ``event_count`` — the 1/N-attribution unit). ``calls_total`` (added
    in v012) surfaces the non-distinct occurrence count alongside it:
    a turn that called Read 3× contributes ``calls += 1`` but
    ``calls_total += 3``. Pre-v012 ``tool_mart`` rows carry
    ``calls_total = 0`` until a ``--force`` rebuild — consumers should
    treat that as "not yet rebuilt", not "zero calls".

    Cache token columns are materialised on ``tool_mart`` as of v023
    (``cache_read``/``cache_create``, 1/N-attributed per the same contract);
    ``tool_mart_for_project`` already surfaces them under the aggregator's
    ``cache_read_tokens`` / ``cache_creation_tokens`` field names, so the
    ToolCost card shows a real per-tool cache cost (ui-perf #20). Pre-v023
    ``tool_mart`` rows read 0 here until a ``--force`` rebuild re-derives them.
    """
    return {
        name: {
            "calls": v["calls"],
            "calls_total": v.get("calls_total", 0),
            "input_tokens": v["tokens_in"],
            "output_tokens": v["tokens_out"],
            "cache_read_tokens": v.get("cache_read_tokens", 0),
            "cache_creation_tokens": v.get("cache_creation_tokens", 0),
            "cost": v["cost"],
        }
        for name, v in tool_rows.items()
    }


# ── /api/cost-data/by-provider ──────────────────────────────────────────────


# CLI/HTTP-friendly aliases consumed by ``/api/cost-data/by-provider`` —
# kept in lock-step with ``services/compare.PERIOD_MAP`` so the same
# strings work across the dashboard's two cost-flavour endpoints.
_BY_PROVIDER_PERIOD_MAP: dict[str, str] = {
    "today": "today",
    "week": "7days",
    "month": "month",
    "all": "all",
}


@router.get("/api/cost-data/by-provider")
async def get_cost_by_provider(
    log_path: str | None = None,
    period: str = "month",
    provider: Annotated[list[str] | None, Query()] = None,
):
    """Return total cost / message count / session count grouped by provider.

    Powers the Cost tab's `CostByProviderCard` (v0.6.1 multi-provider polish).
    Mirrors the existing ``/api/cost-data`` endpoint's currency-conversion
    contract — every cost figure is pre-converted into the active currency
    so the frontend never multiplies by an FX rate.

    Args:
        log_path: Optional project log path; defaults to
            ``deps.current_log_path``. When a project is active the rollup is
            scoped to THAT project (RANK 19 fix) — the card lives on a
            project's Cost tab and must not leak cross-project spend. With no
            project selected the rollup spans the whole store.
        period: One of ``today | week | month | all``. Defaults to ``month``
            so the card lines up with the Compare tab's default view.

    Returns:
        ``{"period": ..., "rows": [{provider, cost_usd, message_count,
        session_count}, ...], "currency": {...}}``. Rows sort by cost
        descending. Empty rows when the store has no data in window.
    """
    from stackunderflow.infra.costs import compute_cost
    from stackunderflow.reports.scope import parse_period

    spec = _BY_PROVIDER_PERIOD_MAP.get(period)
    if spec is None:
        raise HTTPException(
            status_code=400,
            detail=(f"Unknown period '{period}'. Valid: {', '.join(sorted(_BY_PROVIDER_PERIOD_MAP))}"),
        )
    scope = parse_period(spec)

    # Compute the day window for the mart fast-path. ``parse_period``
    # returns ISO timestamps; the mart's ``day`` column stores
    # ``YYYY-MM-DD``, so we slice to 10 chars.
    day_from = scope.since[:10] if scope.since else None
    day_to = scope.until[:10] if scope.until else None

    # RANK 19: when a project is active this card must show THAT project's
    # per-provider spend, not the whole store's. ``provider_day_mart`` is
    # keyed (day, provider) with no project_id, so it can only answer the
    # all-projects question — a project-scoped request therefore bypasses the
    # mart and rolls up the project-filtered ``messages`` table instead. With
    # no project selected we keep the fast global mart path (the cross-project
    # dashboard view + existing callers).
    path = log_path or deps.current_log_path

    conn = db.connect(deps.store_path)
    try:
        if path:
            project_ids = _project_ids_for(conn, path)
            out_rows = _build_by_provider_rows_from_messages(
                conn,
                scope=scope,
                compute_cost=compute_cost,
                project_ids=project_ids,
            )
        else:
            # Wave 3A: when ``provider_day_mart`` has rows in window, the
            # rollup is one indexed scan over a tiny pre-aggregated table.
            # We still fall back to the messages-table sweep when the mart
            # is empty so a half-finished backfill keeps working.
            mart_rows_pd = mart_queries.provider_day_rollup(conn, day_from=day_from, day_to=day_to)
            if mart_rows_pd:
                out_rows = _build_by_provider_rows_from_mart(mart_rows_pd)
            else:
                out_rows = _build_by_provider_rows_from_messages(conn, scope=scope, compute_cost=compute_cost)
    finally:
        conn.close()

    currency = active_currency_payload()
    rate = currency["rate_from_usd"]
    if rate != 1.0:
        for r in out_rows:
            r["cost_usd"] = r["cost_usd"] * rate
    out_rows.sort(key=lambda r: r["cost_usd"], reverse=True)

    # Provider filter: empty = all (preserve existing API contract). When
    # callers pass ``?provider=cursor&provider=cline`` we narrow the rows
    # so the card renders only the requested providers — the dashboard's
    # FilterBar passes the active set through verbatim.
    if provider:
        wanted = {p.strip().lower() for p in provider if p and p.strip()}
        if wanted:
            out_rows = [r for r in out_rows if r["provider"].lower() in wanted]

    return {
        "period": period,
        "rows": out_rows,
        "currency": currency,
    }


def _build_by_provider_rows_from_mart(
    mart_rows: list[dict[str, Any]],
) -> list[dict[str, Any]]:
    """Project ``provider_day_mart`` rollup rows into the response shape.

    The mart helper already sums by provider in SQL — this function
    just renames keys to match the JSON the frontend expects.
    """
    return [
        {
            "provider": (r.get("provider") or "unknown").lower(),
            "cost_usd": float(r.get("cost_usd", 0.0) or 0.0),
            "message_count": int(r.get("message_count", 0) or 0),
            "session_count": int(r.get("session_count", 0) or 0),
        }
        for r in mart_rows
    ]


def _build_by_provider_rows_from_messages(
    conn,
    *,
    scope,
    compute_cost,
    project_ids: list[int] | None = None,
) -> list[dict[str, Any]]:
    """Aggregator-path rollup over the raw ``messages`` table.

    Used as the fallback when ``provider_day_mart`` is empty AND as the
    project-scoped path (RANK 19) — passing ``project_ids`` narrows the sweep
    to one project's sessions so the by-provider card stops leaking the whole
    store's spend. The global (no ``project_ids``) shape mirrors the v0.6.1
    implementation byte-for-byte so the JSON contract is stable regardless of
    which path produced the row.
    """
    sql = (
        "SELECT projects.provider AS provider, "
        "       sessions.id AS session_id, "
        "       COALESCE(messages.model, '') AS model, "
        "       COALESCE(messages.input_tokens, 0) AS input_tokens, "
        "       COALESCE(messages.output_tokens, 0) AS output_tokens, "
        "       COALESCE(messages.cache_create_tokens, 0) AS cache_create_tokens, "
        "       COALESCE(messages.cache_read_tokens, 0) AS cache_read_tokens, "
        "       COALESCE(messages.speed, 'standard') AS speed, "
        "       messages.role AS role "
        "FROM messages "
        "JOIN sessions ON sessions.id = messages.session_fk "
        "JOIN projects ON projects.id = sessions.project_id "
        "WHERE 1=1 "
    )
    params: list[Any] = []
    if project_ids:
        placeholders = ",".join("?" for _ in project_ids)
        sql += f"AND sessions.project_id IN ({placeholders}) "
        params.extend(project_ids)
    if scope.since is not None:
        sql += "AND messages.timestamp >= ? "
        params.append(scope.since)
    if scope.until is not None:
        sql += "AND messages.timestamp <= ? "
        params.append(scope.until)
    rows = conn.execute(sql, params).fetchall()

    per_provider: dict[str, dict[str, Any]] = {}
    for r in rows:
        prov = r["provider"] or "unknown"
        bucket = per_provider.setdefault(
            prov,
            {
                "provider": prov,
                "cost_usd": 0.0,
                "message_count": 0,
                "_sessions": set(),
            },
        )
        bucket["message_count"] += 1
        bucket["_sessions"].add(r["session_id"])
        if r["role"] == "assistant" and r["model"]:
            cost = compute_cost(
                {
                    "input": r["input_tokens"],
                    "output": r["output_tokens"],
                    "cache_creation": r["cache_create_tokens"],
                    "cache_read": r["cache_read_tokens"],
                },
                r["model"],
                provider=prov or "anthropic",
                speed=r["speed"] or "standard",
            )["total_cost"]
            bucket["cost_usd"] += cost

    return [
        {
            "provider": prov,
            "cost_usd": bucket["cost_usd"],
            "message_count": bucket["message_count"],
            "session_count": len(bucket["_sessions"]),
        }
        for prov, bucket in per_provider.items()
    ]


def _build_by_model_rows_from_messages(
    conn,
    *,
    scope,
    compute_cost,
    project_ids: list[int] | None = None,
) -> list[dict[str, Any]]:
    """Per-(day, model) rollup over the project-filtered ``messages`` table.

    The project-scoped sibling of ``mart_queries.model_day_series`` (which is
    global-grain — keyed (day, model, speed) with no project_id — and so can't
    be project-filtered). Returns the same row shape the by-model route
    consumes: ``{day, model, cost_usd, message_count}``, one row per
    (day, model), summed across speed, ordered by day. Only assistant rows
    carrying a real model contribute — mirroring the mart, where user rows
    have no model. ``cost_usd`` is USD; the route applies the FX rate.
    """
    sql = (
        "SELECT projects.provider AS provider, "
        "       substr(messages.timestamp, 1, 10) AS day, "
        "       COALESCE(messages.model, '') AS model, "
        "       COALESCE(messages.input_tokens, 0) AS input_tokens, "
        "       COALESCE(messages.output_tokens, 0) AS output_tokens, "
        "       COALESCE(messages.cache_create_tokens, 0) AS cache_create_tokens, "
        "       COALESCE(messages.cache_read_tokens, 0) AS cache_read_tokens, "
        "       COALESCE(messages.speed, 'standard') AS speed, "
        "       messages.role AS role "
        "FROM messages "
        "JOIN sessions ON sessions.id = messages.session_fk "
        "JOIN projects ON projects.id = sessions.project_id "
        "WHERE 1=1 "
    )
    params: list[Any] = []
    if project_ids:
        placeholders = ",".join("?" for _ in project_ids)
        sql += f"AND sessions.project_id IN ({placeholders}) "
        params.extend(project_ids)
    if scope.since is not None:
        sql += "AND messages.timestamp >= ? "
        params.append(scope.since)
    if scope.until is not None:
        sql += "AND messages.timestamp <= ? "
        params.append(scope.until)
    rows = conn.execute(sql, params).fetchall()

    per_key: dict[tuple[str, str], dict[str, Any]] = {}
    for r in rows:
        model = r["model"]
        if r["role"] != "assistant" or not model or model == "N/A":
            continue
        day = r["day"] or ""
        bucket = per_key.setdefault(
            (day, model),
            {"day": day, "model": model, "cost_usd": 0.0, "message_count": 0},
        )
        bucket["message_count"] += 1
        bucket["cost_usd"] += compute_cost(
            {
                "input": r["input_tokens"],
                "output": r["output_tokens"],
                "cache_creation": r["cache_create_tokens"],
                "cache_read": r["cache_read_tokens"],
            },
            model,
            provider=r["provider"] or "anthropic",
            speed=r["speed"] or "standard",
        )["total_cost"]

    return sorted(per_key.values(), key=lambda b: b["day"])


@router.get("/api/interaction/{interaction_id}")
async def get_interaction(interaction_id: str, log_path: str | None = None):
    """Return one enriched Interaction (command + responses + tool_results).

    Looks up the interaction in the ``EnrichedDataset`` for the project at
    ``log_path``. Returns 404 if no interaction matches the given id.
    """
    path = _resolve_log_path(log_path)
    conn = db.connect(deps.store_path)
    try:
        project_ids = _project_ids_for(conn, path)
        dataset, _ = queries.build_enriched_dataset(conn, project_id=project_ids)
    finally:
        conn.close()

    if dataset is None:
        raise HTTPException(status_code=404, detail="Project has no data")

    for ix in dataset.interactions:
        if ix.interaction_id == interaction_id:
            return _serialise_interaction(ix)

    raise HTTPException(status_code=404, detail=f"Interaction '{interaction_id}' not found")


def _serialise_interaction(ix) -> dict[str, Any]:
    """Turn an ``Interaction`` dataclass into a JSON-safe dict.

    ``Record.raw_data`` can hold non-JSON-native values coming out of the raw
    JSONL payload — we drop it from the output to keep the response small and
    avoid accidental serialisation failures on odd payloads.
    """
    return {
        "interaction_id": ix.interaction_id,
        "session_id": ix.session_id,
        "start_time": ix.start_time,
        "end_time": ix.end_time,
        "model": ix.model,
        "tool_count": ix.tool_count,
        "assistant_steps": ix.assistant_steps,
        "is_continuation": ix.is_continuation,
        "tools_used": list(ix.tools_used),
        "has_task_tool": ix.has_task_tool,
        "command": _serialise_record(ix.command),
        "responses": [_serialise_record(r) for r in ix.responses],
        "tool_results": [_serialise_record(r) for r in ix.tool_results],
    }


def _serialise_record(rec) -> dict[str, Any]:
    """Flatten an enricher ``Record`` dataclass to a JSON-safe dict.

    We list fields explicitly rather than ``dataclasses.asdict(rec)`` — the
    latter would recursively copy ``raw_data``, which frequently contains
    non-JSON-native payload fragments (e.g. datetime strings masked as ints
    from non-Claude adapters).
    """
    return {
        "session_id": rec.session_id,
        "kind": rec.kind,
        "timestamp": rec.timestamp,
        "model": rec.model,
        "content": rec.content,
        "tokens": dict(rec.tokens),
        "tools": list(rec.tools),
        "is_error": rec.is_error,
        "error_category": rec.error_category,
        "is_interruption": rec.is_interruption,
        "has_tool_result": rec.has_tool_result,
        "uuid": rec.uuid,
        "parent_uuid": rec.parent_uuid,
        "is_sidechain": rec.is_sidechain,
        "message_id": rec.message_id,
        "cwd": rec.cwd,
    }


@router.get("/api/cost-data/by-model")
async def get_cost_by_model(log_path: str | None = None, period: str = "month"):
    """Per-model spend over time — powers the Cost tab's by-model chart.

    Reads the pre-aggregated ``model_day_mart`` (one indexed scan over a tiny
    table) and returns, per model, a daily cost/message series plus a total,
    sorted by total cost descending. Cost figures are pre-converted into the
    active currency, matching ``/api/cost-data/by-provider``.

    Args:
        log_path: Optional project log path; defaults to
            ``deps.current_log_path``. When a project is active the series is
            scoped to THAT project (RANK 19 fix) — ``model_day_mart`` is
            global-grain (no project_id), so a project-scoped request rolls up
            the project-filtered messages table instead. With no project
            selected the series spans the whole store via the mart.
        period: One of ``today | week | month | all`` (default ``month``).

    Returns:
        ``{"period": ..., "models": [{model, total_cost, daily: [{date,
        cost_usd, message_count}, ...]}, ...], "currency": {...}}``. Empty
        ``models`` when the store has no data in window.
    """
    from stackunderflow.infra.costs import compute_cost
    from stackunderflow.reports.scope import parse_period

    spec = _BY_PROVIDER_PERIOD_MAP.get(period)
    if spec is None:
        raise HTTPException(
            status_code=400,
            detail=(f"Unknown period '{period}'. Valid: {', '.join(sorted(_BY_PROVIDER_PERIOD_MAP))}"),
        )
    scope = parse_period(spec)

    path = log_path or deps.current_log_path

    conn = db.connect(deps.store_path)
    try:
        if path:
            project_ids = _project_ids_for(conn, path)
            rows = _build_by_model_rows_from_messages(
                conn,
                scope=scope,
                compute_cost=compute_cost,
                project_ids=project_ids,
            )
        else:
            rows = mart_queries.model_day_series(conn, since_iso=scope.since, until_iso=scope.until)
    finally:
        conn.close()

    currency = active_currency_payload()
    rate = currency["rate_from_usd"]

    models: dict[str, dict[str, Any]] = {}
    for r in rows:
        bucket = models.setdefault(r["model"], {"model": r["model"], "total_cost": 0.0, "daily": []})
        cost = r["cost_usd"] * rate
        bucket["total_cost"] += cost
        bucket["daily"].append(
            {
                "date": r["day"],
                "cost_usd": cost,
                "message_count": r["message_count"],
            }
        )

    out_models = sorted(models.values(), key=lambda m: m["total_cost"], reverse=True)
    return {"period": period, "models": out_models, "currency": currency}
