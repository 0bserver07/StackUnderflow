"""Pricing health routes + assembler — the ``pricing doctor`` surface.

A single read-only assembler (:func:`assemble_pricing_health`) reports
pricing health from the store. Both the HTTP route
(``GET /api/pricing/doctor``) and the CLI command
(``stackunderflow pricing doctor``) call it, so the two surfaces never
disagree — the same lockstep pattern :mod:`stackunderflow.etl.status`
uses for ETL status.

The assembler is strictly read-only: every query is a ``SELECT`` guarded
by a ``sqlite_master`` table probe (so it is safe against a fresh-install
store with no ``usage_events``), and the rate-freshness probe reads the
on-disk overlay without a network fetch. Nothing here writes the DB or
the pricing cache.

Three health dimensions, mapping to the spec's ``cost_source`` contract
(``docs/specs/session-schema-v1.md``):

* **unpriced_models** — distinct models in ``usage_events`` with no
  resolvable rate card (:func:`infra.costs.is_rate_card_model` is False).
  These should carry ``cost_source='unknown'`` and contribute $0; a
  ``billable`` entry (some row priced ``rate_card``/``live`` against an
  unresolvable id) is a real defect.
* **rate_freshness** — age / staleness of the LiteLLM pricing overlay
  cache (rates older than ``stale_days``).
* **unknown_cost_source** — per-model rollup of ``cost_source='unknown'``
  rows, plus ``unknown_nonzero_cost_rows`` (the d2d4eb9 contract: an
  ``unknown`` row's ``cost_usd`` must be 0.0).

Each affected model carries the dollar delta a resolvable rate would add
(``estimated_delta_usd``), when estimable.
"""

from __future__ import annotations

import sqlite3
from typing import Any

from fastapi import APIRouter

import stackunderflow.deps as deps
from stackunderflow.infra.costs import estimate_cost, is_rate_card_model
from stackunderflow.services.pricing_service import PricingService
from stackunderflow.store import db

router = APIRouter()

# Default freshness threshold (days) — mirrors PricingService.STALE_THRESHOLD.
DEFAULT_STALE_DAYS = 7

# Default cap on per-list entries in the payload (most-actionable first).
DEFAULT_LIMIT = 50

_BILLABLE_SOURCES = frozenset({"rate_card", "live"})


# ── public assembler ─────────────────────────────────────────────────────────


def assemble_pricing_health(
    conn: sqlite3.Connection,
    *,
    stale_days: int = DEFAULT_STALE_DAYS,
    limit: int = DEFAULT_LIMIT,
) -> dict[str, Any]:
    """Return the pricing-health payload. Read-only — never writes.

    Parameters
    ----------
    conn:
        Open store connection. The caller owns its lifecycle.
    stale_days:
        Overlay-cache age (days) past which rates are reported stale.
    limit:
        Max entries returned in each model list (full counts stay in the
        ``summary``). Lists are sorted by estimated dollar exposure
        descending so the highest-impact rows survive truncation.

    Returns
    -------
    dict
        See module docstring for the shape. Always complete — a
        fresh-install store with no ``usage_events`` returns an empty,
        ``ok``-true payload (with the freshness block still populated).
    """
    freshness = _rate_freshness(stale_days)

    if not _table_exists(conn, "usage_events"):
        return _empty_payload(stale_days, freshness)

    unpriced = _unpriced_models(conn)
    unknown = _unknown_cost_source(conn)
    violation_rows = _unknown_nonzero_cost_rows(conn)
    total_events, total_cost = _totals(conn)

    # Sort by exposure (None deltas sink to the bottom).
    unpriced.sort(key=lambda d: -(d["estimated_delta_usd"] or 0.0))
    unknown.sort(key=lambda d: -(d["estimated_delta_usd"] or 0.0))

    billable_unpriced = [u for u in unpriced if u["billable"]]
    # ``ok`` reflects HARD defects only — the two contracts the CI
    # invariants also lock: no billable row references an unresolvable
    # model, and no ``unknown`` row carries a nonzero cost. Unpriced
    # exotic models (correctly stamped ``unknown`` ⇒ $0) and a stale
    # overlay are surfaced as warnings, not failures.
    ok = not billable_unpriced and violation_rows == 0

    exposure = round(sum(u["estimated_delta_usd"] or 0.0 for u in unpriced), 6)

    return {
        "stale_days": stale_days,
        "ok": ok,
        "summary": {
            "total_events": total_events,
            "total_cost_usd": round(total_cost, 6),
            "unpriced_model_count": len(unpriced),
            "billable_unpriced_model_count": len(billable_unpriced),
            "unknown_cost_source_model_count": len(unknown),
            "unknown_nonzero_cost_rows": violation_rows,
            "estimated_unpriced_exposure_usd": exposure,
            "rate_cache_stale": freshness["stale"],
        },
        "unpriced_models": unpriced[: max(0, limit)],
        "unknown_cost_source": unknown[: max(0, limit)],
        "rate_freshness": freshness,
    }


# ── dimension builders ────────────────────────────────────────────────────────


def _unpriced_models(conn: sqlite3.Connection) -> list[dict[str, Any]]:
    """Distinct ``(provider, model)`` with no resolvable rate card.

    Aggregates tokens + cost per model and computes the would-be cost a
    resolvable rate would charge (``estimated_delta_usd``). A model whose
    rows include a billable ``cost_source`` (``rate_card``/``live``) is
    flagged ``billable`` — that combination is a real bug (a priced row
    against an unresolvable id), not the expected unknown-model case.
    """
    out: list[dict[str, Any]] = []
    for r in conn.execute(
        """
        SELECT provider, model,
               COUNT(*)                  AS events,
               SUM(input_tokens)         AS it,
               SUM(output_tokens)        AS ot,
               SUM(cache_read_tokens)    AS crt,
               SUM(cache_create_tokens)  AS cct,
               SUM(cost_usd)             AS cost,
               GROUP_CONCAT(DISTINCT cost_source) AS sources
        FROM usage_events
        WHERE model <> ''
        GROUP BY provider, model
        """
    ):
        model = r["model"]
        if is_rate_card_model(model):
            continue
        it, ot, crt, cct = int(r["it"]), int(r["ot"]), int(r["crt"]), int(r["cct"])
        cost = float(r["cost"] or 0.0)
        est = estimate_cost(
            {"input": it, "output": ot, "cache_creation": cct, "cache_read": crt},
            model,
        )
        sources = sorted(s for s in (r["sources"] or "").split(",") if s)
        out.append(
            {
                "provider": r["provider"],
                "model": model,
                "events": int(r["events"]),
                "input_tokens": it,
                "output_tokens": ot,
                "cache_read_tokens": crt,
                "cache_create_tokens": cct,
                "current_cost_usd": round(cost, 6),
                "estimated_cost_usd": round(est, 6) if est > 0 else None,
                "estimated_delta_usd": round(est - cost, 6) if est > 0 else None,
                "cost_sources": sources,
                "billable": bool(_BILLABLE_SOURCES.intersection(sources)),
            }
        )
    return out


def _unknown_cost_source(conn: sqlite3.Connection) -> list[dict[str, Any]]:
    """Per-model rollup of ``cost_source='unknown'`` rows.

    These are tokens-without-dollars by contract; the estimate quantifies
    what re-pricing them (e.g. after a manifest/rate-card update + ``etl
    backfill --force``) would recover.
    """
    out: list[dict[str, Any]] = []
    for r in conn.execute(
        """
        SELECT provider, model,
               COUNT(*)                  AS events,
               SUM(cost_usd)             AS cost,
               SUM(input_tokens)         AS it,
               SUM(output_tokens)        AS ot,
               SUM(cache_read_tokens)    AS crt,
               SUM(cache_create_tokens)  AS cct
        FROM usage_events
        WHERE cost_source = 'unknown'
        GROUP BY provider, model
        """
    ):
        model = r["model"]
        it, ot, crt, cct = int(r["it"]), int(r["ot"]), int(r["crt"]), int(r["cct"])
        cost = float(r["cost"] or 0.0)
        est = (
            estimate_cost(
                {"input": it, "output": ot, "cache_creation": cct, "cache_read": crt},
                model,
            )
            if model
            else 0.0
        )
        out.append(
            {
                "provider": r["provider"],
                "model": model,
                "events": int(r["events"]),
                "tokens": it + ot + crt + cct,
                "cost_usd": round(cost, 6),
                "estimated_cost_usd": round(est, 6) if est > 0 else None,
                "estimated_delta_usd": round(est - cost, 6) if est > 0 else None,
            }
        )
    return out


def _unknown_nonzero_cost_rows(conn: sqlite3.Connection) -> int:
    """Count rows violating the contract: ``cost_source='unknown'`` ⇒ cost 0.0.

    Locked at ingest by ``etl/normalize/base._compute_cost_usd`` (fixed in
    d2d4eb9); a nonzero count here means a writer regressed.
    """
    row = conn.execute(
        "SELECT COUNT(*) AS n FROM usage_events "
        "WHERE cost_source = 'unknown' AND cost_usd <> 0"
    ).fetchone()
    return int(row["n"] if hasattr(row, "keys") else row[0])


def _totals(conn: sqlite3.Connection) -> tuple[int, float]:
    row = conn.execute(
        "SELECT COUNT(*) AS n, COALESCE(SUM(cost_usd), 0.0) AS c FROM usage_events"
    ).fetchone()
    n = int(row["n"] if hasattr(row, "keys") else row[0])
    c = float(row["c"] if hasattr(row, "keys") else row[1])
    return n, c


def _rate_freshness(stale_days: int) -> dict[str, Any]:
    """Overlay-cache freshness, with the configured threshold applied.

    Reads the on-disk overlay without a network fetch (see
    :meth:`PricingService.read_cache_status`). ``stale`` is True when the
    cache is missing, unparseable, or older than ``stale_days``.
    """
    status = PricingService.read_cache_status()
    age = status.get("age_days")
    stale = age is None or age > stale_days
    return {**status, "stale_days_threshold": stale_days, "stale": bool(stale)}


# ── helpers ───────────────────────────────────────────────────────────────────


def _empty_payload(
    stale_days: int, freshness: dict[str, Any]
) -> dict[str, Any]:
    return {
        "stale_days": stale_days,
        "ok": True,
        "summary": {
            "total_events": 0,
            "total_cost_usd": 0.0,
            "unpriced_model_count": 0,
            "billable_unpriced_model_count": 0,
            "unknown_cost_source_model_count": 0,
            "unknown_nonzero_cost_rows": 0,
            "estimated_unpriced_exposure_usd": 0.0,
            "rate_cache_stale": freshness["stale"],
        },
        "unpriced_models": [],
        "unknown_cost_source": [],
        "rate_freshness": freshness,
    }


def _table_exists(conn: sqlite3.Connection, name: str) -> bool:
    row = conn.execute(
        "SELECT 1 FROM sqlite_master WHERE type='table' AND name = ?",
        (name,),
    ).fetchone()
    return row is not None


# ── route ─────────────────────────────────────────────────────────────────────


@router.get("/api/pricing/doctor")
async def get_pricing_doctor(
    stale_days: int = DEFAULT_STALE_DAYS,
    limit: int = DEFAULT_LIMIT,
) -> dict[str, Any]:
    """Read-only pricing-health report from the store.

    Query params (both optional):

    * ``stale_days`` — overlay-cache age past which rates are stale (default 7).
    * ``limit``      — max entries per model list (default 50).

    Response shape: see :func:`assemble_pricing_health`. Opens its own
    short-lived connection and issues only ``SELECT`` queries — no schema
    apply, no writes, no network.
    """
    conn = db.connect(deps.store_path)
    try:
        return assemble_pricing_health(conn, stale_days=stale_days, limit=limit)
    finally:
        conn.close()


__all__ = [
    "DEFAULT_LIMIT",
    "DEFAULT_STALE_DAYS",
    "assemble_pricing_health",
    "router",
]
