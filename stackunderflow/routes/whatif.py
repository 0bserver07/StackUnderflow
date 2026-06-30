"""Cross-provider what-if route — reprice a project's token workload.

``GET /api/whatif`` answers "what would this exact workload have cost on a
different model?". It reads the token totals a project (or the whole store)
actually consumed out of the ``usage_events`` cost mart and hands them to
:func:`stackunderflow.services.whatif.build_whatif`, which reprices that same
token shape against a curated cross-provider candidate set via
:func:`stackunderflow.infra.costs.compute_cost` (used strictly as a black box —
no pricing internals are touched here).

Scope:

* with ``?log_path=`` (or an active ``deps.current_log_path``), the totals are
  for that one project — the Cost tab's per-project what-if;
* with no project selected, the totals span the whole store.

Response shape::

    {
        "scope": "project" | "all",
        "project_slug": str | null,
        "tokens": {input, output, cache_read, cache_create, total},
        "actual": {"cost_usd": float, "models": [str, ...]},
        "candidates": [{provider, model, label, cost_usd, delta_usd, delta_pct}, ...],
        "cheapest": <candidate row | null>,
        "currency": {code, symbol, rate_from_usd},
    }

``candidates`` is sorted cheapest-first. Every dollar field (``actual.cost_usd``
and each candidate's ``cost_usd`` / ``delta_usd``) is pre-converted to the
active currency — same contract as ``/api/cost-data`` — so one
``formatCost(amount, currency)`` callsite renders correctly. ``delta_pct`` is a
dimensionless percentage and stays as-is.
"""

from __future__ import annotations

import sqlite3
from pathlib import Path

from fastapi import APIRouter, HTTPException

import stackunderflow.deps as deps
from stackunderflow.infra.currency import active_currency_payload
from stackunderflow.services.whatif import TokenTotals, build_whatif
from stackunderflow.store import db, queries

router = APIRouter()


def _table_exists(conn: sqlite3.Connection, name: str) -> bool:
    row = conn.execute(
        "SELECT 1 FROM sqlite_master WHERE type='table' AND name = ?",
        (name,),
    ).fetchone()
    return row is not None


def _project_ids_for(conn: sqlite3.Connection, path: str) -> list[int]:
    """Resolve a log path to its store project ids (one per provider)."""
    slug = Path(path).name
    rows = queries.get_projects_by_slug(conn, slug=slug)
    if not rows:
        raise HTTPException(
            status_code=404,
            detail=f"Project '{slug}' not found in store — try /api/refresh first",
        )
    return [r.id for r in rows]


def _token_totals(
    conn: sqlite3.Connection, *, project_ids: list[int] | None
) -> tuple[TokenTotals, float, list[str]]:
    """Sum token totals + actual cost + distinct models from ``usage_events``.

    Scoped to ``project_ids`` when given, else the whole store. A fresh store
    with no ``usage_events`` returns zeros and an empty model list. Returns
    ``(TokenTotals, actual_cost_usd, models_used)``.
    """
    if not _table_exists(conn, "usage_events"):
        return TokenTotals(), 0.0, []

    where = ""
    params: list[object] = []
    if project_ids is not None:
        if not project_ids:
            return TokenTotals(), 0.0, []
        placeholders = ",".join("?" for _ in project_ids)
        where = f"WHERE project_id IN ({placeholders})"
        params = list(project_ids)

    row = conn.execute(
        "SELECT "
        "  COALESCE(SUM(input_tokens), 0)        AS it, "
        "  COALESCE(SUM(output_tokens), 0)       AS ot, "
        "  COALESCE(SUM(cache_read_tokens), 0)   AS crt, "
        "  COALESCE(SUM(cache_create_tokens), 0) AS cct, "
        "  COALESCE(SUM(cost_usd), 0.0)          AS cost "
        f"FROM usage_events {where}",
        params,
    ).fetchone()

    totals = TokenTotals(
        input=int(row[0] or 0),
        output=int(row[1] or 0),
        cache_read=int(row[2] or 0),
        cache_create=int(row[3] or 0),
    )
    actual_cost = float(row[4] or 0.0)

    model_sql = f"SELECT DISTINCT model FROM usage_events {where}"
    if where:
        model_sql += " AND model <> ''"
    else:
        model_sql += " WHERE model <> ''"
    models = [r[0] for r in conn.execute(model_sql, params).fetchall() if r[0]]

    return totals, actual_cost, models


def _convert_payload(payload: dict, rate: float) -> dict:
    """Pre-convert every USD dollar field in the what-if payload by ``rate``."""
    if rate == 1.0:
        return payload
    actual = payload.get("actual")
    if isinstance(actual, dict) and isinstance(actual.get("cost_usd"), (int, float)):
        actual["cost_usd"] = actual["cost_usd"] * rate
    for row in payload.get("candidates", []):
        if isinstance(row.get("cost_usd"), (int, float)):
            row["cost_usd"] = row["cost_usd"] * rate
        if isinstance(row.get("delta_usd"), (int, float)):
            row["delta_usd"] = row["delta_usd"] * rate
        # delta_pct is dimensionless — left untouched.
    cheapest = payload.get("cheapest")
    if isinstance(cheapest, dict):
        # ``cheapest`` is the same object identity as candidates[0] (already
        # scaled above); re-scaling would double it. Rebind to the scaled row.
        rows = payload.get("candidates", [])
        payload["cheapest"] = rows[0] if rows else None
    return payload


@router.get("/api/whatif")
async def get_whatif(log_path: str | None = None) -> dict:
    """Reprice the project's (or store's) token workload across providers.

    Query params:

    * ``log_path`` — optional project log path; defaults to
      ``deps.current_log_path``. With a project resolved the totals are scoped
      to it; with none selected the comparison spans the whole store.

    Opens one short-lived connection and issues only ``SELECT`` queries — no
    schema apply, no writes, no network.
    """
    currency = active_currency_payload()
    path = log_path or deps.current_log_path

    conn = db.connect(deps.store_path)
    try:
        if path:
            project_ids = _project_ids_for(conn, path)
            totals, actual_cost, models = _token_totals(conn, project_ids=project_ids)
            scope = "project"
            slug: str | None = Path(path).name
        else:
            totals, actual_cost, models = _token_totals(conn, project_ids=None)
            scope = "all"
            slug = None
    finally:
        conn.close()

    payload = build_whatif(
        totals,
        actual_cost_usd=actual_cost,
        actual_models=models,
    )
    rate = float(currency.get("rate_from_usd") or 1.0)
    payload = _convert_payload(payload, rate)

    payload["scope"] = scope
    payload["project_slug"] = slug
    payload["currency"] = currency
    return payload


__all__ = ["router"]
