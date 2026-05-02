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

from pathlib import Path
from typing import Any

from fastapi import APIRouter, HTTPException

import stackunderflow.deps as deps
from stackunderflow.infra.currency import active_currency_payload
from stackunderflow.store import db, queries

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
)


def _convert_amount(value: Any, rate: float) -> Any:
    if isinstance(value, (int, float)) and not isinstance(value, bool):
        return float(value) * rate
    return value


def _convert_in_place(node: Any, rate: float) -> Any:
    """Recursively scale every cost-named numeric leaf by ``rate``.

    We deliberately key on field names — touching every numeric value would
    incorrectly scale token counts, durations, and retry counts that share
    a parent dict with cost figures.
    """
    if isinstance(node, dict):
        for key, val in list(node.items()):
            if key in _COST_FIELDS_PER_ROW:
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


def _project_id_for(conn, path: str) -> int:
    slug = Path(path).name
    row = queries.get_project(conn, slug=slug)
    if row is None:
        raise HTTPException(
            status_code=404,
            detail=f"Project '{slug}' not found in store — try /api/refresh first",
        )
    return row.id


@router.get("/api/cost-data")
async def get_cost_data(log_path: str | None = None, timezone_offset: int = 0):
    """Return only the 9 cost/analytics sections split off from dashboard-data.

    Shape: ``{key: stats[key]}`` for every key in ``COST_KEYS``. Missing keys
    default to empty containers (``[]``, ``{}``) so the frontend can render
    without guarding for undefined sections.
    """
    path = _resolve_log_path(log_path)
    conn = db.connect(deps.store_path)
    try:
        project_id = _project_id_for(conn, path)
        _, stats = queries.get_project_stats(
            conn, project_id=project_id, tz_offset=timezone_offset
        )
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
    return payload


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
    period: str = "month",
    provider: list[str] | None = None,
):
    """Return total cost / message count / session count grouped by provider.

    Powers the Cost tab's `CostByProviderCard` (v0.6.1 multi-provider polish).
    Mirrors the existing ``/api/cost-data`` endpoint's currency-conversion
    contract — every cost figure is pre-converted into the active currency
    so the frontend never multiplies by an FX rate.

    Args:
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
            detail=(
                f"Unknown period '{period}'. "
                f"Valid: {', '.join(sorted(_BY_PROVIDER_PERIOD_MAP))}"
            ),
        )
    scope = parse_period(spec)

    conn = db.connect(deps.store_path)
    try:
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
        if scope.since is not None:
            sql += "AND messages.timestamp >= ? "
            params.append(scope.since)
        if scope.until is not None:
            sql += "AND messages.timestamp <= ? "
            params.append(scope.until)
        rows = conn.execute(sql, params).fetchall()
    finally:
        conn.close()

    # ── per-provider rollup ──────────────────────────────────────────────
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
        # Only assistant rows carry token counts that price out — user/tool
        # messages have zero tokens and would just inflate compute_cost calls.
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

    currency = active_currency_payload()
    rate = currency["rate_from_usd"]
    out_rows: list[dict[str, Any]] = []
    for prov, bucket in per_provider.items():
        out_rows.append(
            {
                "provider": prov,
                "cost_usd": bucket["cost_usd"] * rate,
                "message_count": bucket["message_count"],
                "session_count": len(bucket["_sessions"]),
            }
        )
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


@router.get("/api/interaction/{interaction_id}")
async def get_interaction(interaction_id: str, log_path: str | None = None):
    """Return one enriched Interaction (command + responses + tool_results).

    Looks up the interaction in the ``EnrichedDataset`` for the project at
    ``log_path``. Returns 404 if no interaction matches the given id.
    """
    path = _resolve_log_path(log_path)
    conn = db.connect(deps.store_path)
    try:
        project_id = _project_id_for(conn, path)
        dataset, _ = queries.build_enriched_dataset(conn, project_id=project_id)
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
