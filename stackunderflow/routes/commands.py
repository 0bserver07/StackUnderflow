"""Paginated per-command list — ``/api/commands`` per spec §D1.

``/api/dashboard-data`` used to ship the full ``user_interactions.command_details``
array (one entry per user prompt). On large projects that array alone was
~1.8 MB of payload, which blocked the initial Overview render even when the
Commands tab was not yet open. §D1 moves the list to this endpoint and the
dashboard payload keeps only the summary (counts, averages, distribution).

Shape:

``GET /api/commands?log_path=&offset=0&limit=50&sort=cost&order=desc``

Returns ``{commands: [...], total: N, offset, limit}`` — one entry per
``Interaction`` in the project's enriched dataset, each carrying enough
fields for the Commands tab to render a row without another round-trip.
"""

from __future__ import annotations

from collections import Counter
from pathlib import Path
from typing import Any

from fastapi import APIRouter, HTTPException

import stackunderflow.deps as deps
from stackunderflow.infra.costs import compute_cost
from stackunderflow.stats.enricher import Interaction
from stackunderflow.store import db, queries

router = APIRouter()


# ── sort configuration ───────────────────────────────────────────────────────

# Each sort key maps to a function that extracts the sortable value from a
# command dict. Keeping this as a small dispatch table avoids a 5-branch
# ``if/elif`` chain inside the route handler and makes "supported sort keys"
# self-documenting at the top of the module.
_SORT_KEYS: dict[str, Any] = {
    "cost":   lambda c: c["cost"],
    "tokens": lambda c: sum(c["tokens"].values()),
    "tools":  lambda c: c["tools_used"],
    "steps":  lambda c: c["steps"],
    "time":   lambda c: c["timestamp"] or "",
}

_DEFAULT_LIMIT = 50
_MAX_LIMIT = 500


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


def _preview(text: str, limit: int = 200) -> str:
    if not text:
        return ""
    return text.replace("\n", " ").replace("\r", " ").strip()[:limit]


def _interaction_to_command(ix: Interaction) -> dict[str, Any]:
    """Flatten an Interaction into the per-command row shape the Commands tab
    consumes. Mirrors the field set of ``command_costs`` (aggregator §1.2) but
    is **not** truncated to top 50 — every interaction in the project becomes
    one entry here so offset/limit pagination makes sense."""
    tokens: Counter[str] = Counter()
    by_model: dict[str, Counter[str]] = {}
    had_error = False
    models_used: set[str] = set()
    for r in ix.responses + ix.tool_results:
        if r.is_error:
            had_error = True
        for k, v in r.tokens.items():
            tokens[k] += v
        if r.kind == "assistant" and r.model and r.model != "N/A":
            models_used.add(r.model)
            m = by_model.setdefault(r.model, Counter())
            for k, v in r.tokens.items():
                m[k] += v

    cost = sum(
        compute_cost(dict(tok_c), model)["total_cost"]
        for model, tok_c in by_model.items()
    )

    return {
        "interaction_id": ix.interaction_id,
        "session_id": ix.session_id,
        "timestamp": ix.start_time or "",
        "prompt_preview": _preview(ix.command.content or "", 200),
        "cost": cost,
        "tokens": dict(tokens),
        "tools_used": ix.tool_count,
        "steps": ix.assistant_steps,
        "models_used": sorted(models_used),
        "had_error": had_error,
    }


def _build_commands(interactions: list[Interaction]) -> list[dict]:
    """Build the full (uncapped) list of per-command rows."""
    return [_interaction_to_command(ix) for ix in interactions]


@router.get("/api/commands")
async def get_commands(
    log_path: str | None = None,
    offset: int = 0,
    limit: int = _DEFAULT_LIMIT,
    sort: str = "cost",
    order: str = "desc",
):
    """Return a paginated, sorted slice of the project's command list.

    Query params:
      * ``log_path`` — optional; defaults to ``deps.current_log_path``.
      * ``offset`` / ``limit`` — standard pagination. ``limit`` clamps to
        ``[1, 500]`` and defaults to 50.
      * ``sort`` — one of ``cost``, ``tokens``, ``tools``, ``steps``, ``time``.
        Invalid values fall back to ``cost``.
      * ``order`` — ``desc`` (default) or ``asc``.

    Response:
      ``{commands: [...], total: N, offset, limit}``
    """
    path = _resolve_log_path(log_path)

    # Clamp pagination inputs defensively so obviously-bad requests still
    # return a deterministic empty/first-page slice instead of 500ing.
    if offset < 0:
        offset = 0
    if limit < 1:
        limit = _DEFAULT_LIMIT
    if limit > _MAX_LIMIT:
        limit = _MAX_LIMIT

    sort_key = _SORT_KEYS.get(sort, _SORT_KEYS["cost"])
    reverse = order != "asc"

    conn = db.connect(deps.store_path)
    try:
        project_id = _project_id_for(conn, path)
        dataset, _ = queries.build_enriched_dataset(conn, project_id=project_id)
    finally:
        conn.close()

    if dataset is None:
        return {"commands": [], "total": 0, "offset": offset, "limit": limit}

    commands = _build_commands(dataset.interactions)
    commands.sort(key=sort_key, reverse=reverse)

    total = len(commands)
    page = commands[offset : offset + limit]
    return {
        "commands": page,
        "total": total,
        "offset": offset,
        "limit": limit,
    }
