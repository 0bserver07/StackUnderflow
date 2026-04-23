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
from stackunderflow.store import db, queries

router = APIRouter()


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
    return payload


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
