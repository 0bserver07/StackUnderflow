"""``/api/worktrees`` — worktree intelligence: detect, attribute, prune preview.

Thin HTTP wrapper around :mod:`stackunderflow.services.worktrees`. Surfaces
every git worktree the store knows about — which project owns it, what its
sessions cost, whether its work landed — plus a per-worktree verdict
(``ACTIVE`` | ``MERGED_SAFE_TO_PRUNE`` | ``HAS_UNIQUE_WORK``) and the exact
prune commands as a PREVIEW. Neither endpoint ever mutates git state; the
service layer guarantees git is only read.

Contract
========

``GET /api/worktrees?log_path=<path>``

* ``log_path`` — optional project log path. When present, the scan is
  scoped to that project's root. When omitted, the active dashboard
  project (``deps.current_log_path``) is used; with neither, the scan
  spans every root the store knows about. Same resolution order as
  ``routes/forks.py``.

Response::

    {
      "scope": "<resolved path>" | "store",   # what was scanned
      "worktrees": [WorktreeInfo.to_dict()],  # see services/worktrees.py
      "summary": {total, safe_to_prune, has_unique_work, active,
                  attributed_cost_usd},
      "scanned_at": "<server time, ISO-8601 UTC>",
      "currency": {code, symbol, rate_from_usd, warning}
    }

Currency contract matches every other cost endpoint: ``cost_usd`` on each
worktree and ``summary.attributed_cost_usd`` are pre-converted into the
active currency before send, so the frontend never multiplies by an FX rate.

``POST /api/worktrees/attribute``

Runs :func:`stackunderflow.services.worktrees.attribute_fragments` and
returns ``{"updated": <rows>}``. POST because it writes the store (the
additive attribution column on ``projects`` — never git); the operation is
idempotent, so re-POSTing after everything is linked returns ``updated: 0``.

The ``stackunderflow worktrees`` CLI calls :func:`assemble_worktrees_payload`
too, so the two surfaces can never disagree.
"""

from __future__ import annotations

from datetime import UTC, datetime
from typing import Any

from fastapi import APIRouter, Query

import stackunderflow.deps as deps
from stackunderflow.infra.currency import active_currency_payload
from stackunderflow.services.worktrees import attribute_fragments, list_worktrees
from stackunderflow.store import db

router = APIRouter()

_LOG_PATH_QUERY = Query(None, description="Project log path; omit for whole-store")

# Verdict → summary counter. Kept explicit so an unknown verdict from the
# service can't silently skew the counts — it simply isn't tallied.
_VERDICT_COUNTERS: dict[str, str] = {
    "ACTIVE": "active",
    "MERGED_SAFE_TO_PRUNE": "safe_to_prune",
    "HAS_UNIQUE_WORK": "has_unique_work",
}


def assemble_worktrees_payload(conn: Any, *, project_root: str | None) -> dict[str, Any]:
    """Build the shared ``GET /api/worktrees`` / CLI payload (see module docstring).

    ``project_root=None`` means whole-store: the service scans every known
    root. Dollar figures come out already converted to the active currency.
    """
    infos = list_worktrees(conn, project_root=project_root)
    worktrees = [info.to_dict() for info in infos]

    summary: dict[str, Any] = {
        "total": len(worktrees),
        "safe_to_prune": 0,
        "has_unique_work": 0,
        "active": 0,
        "attributed_cost_usd": 0.0,
    }
    for wt in worktrees:
        counter = _VERDICT_COUNTERS.get(str(wt.get("verdict", "")))
        if counter is not None:
            summary[counter] += 1
        summary["attributed_cost_usd"] += float(wt.get("cost_usd") or 0.0)

    currency = active_currency_payload()
    rate = currency["rate_from_usd"]
    if rate != 1.0:
        for wt in worktrees:
            wt["cost_usd"] = float(wt.get("cost_usd") or 0.0) * rate
        summary["attributed_cost_usd"] = float(summary["attributed_cost_usd"]) * rate

    return {
        "scope": project_root if project_root else "store",
        "worktrees": worktrees,
        "summary": summary,
        "scanned_at": datetime.now(UTC).isoformat(),
        "currency": currency,
    }


@router.get("/api/worktrees")
async def get_worktrees(log_path: str | None = _LOG_PATH_QUERY):
    """Return ``{scope, worktrees, summary, scanned_at, currency}``.

    Read-only: git is only queried, never mutated — prune commands in each
    worktree entry are a preview for the user to run themselves.
    """
    # When invoked directly (tests, not via FastAPI's DI) the ``Query(None)``
    # default leaks through as a Query sentinel — coerce anything that isn't a
    # real string into None so the service never sees a non-path object. Same
    # pattern ``routes/forks.py`` uses.
    log_path_str = log_path if isinstance(log_path, str) else None
    path = log_path_str or deps.current_log_path

    conn = db.connect(deps.store_path)
    try:
        return assemble_worktrees_payload(conn, project_root=path)
    finally:
        conn.close()


@router.post("/api/worktrees/attribute")
async def post_attribute():
    """Attribute worktree session fragments to their parent projects.

    Returns ``{"updated": <rows changed>}``. Writes ONLY the additive
    attribution column on ``projects`` — never git state. Idempotent:
    once every fragment is linked, re-POSTing returns ``updated: 0``.
    """
    conn = db.connect(deps.store_path)
    try:
        updated = attribute_fragments(conn)
        # The store connection is autocommit; the explicit commit only
        # matters if the service opened its own transaction. Harmless
        # either way.
        conn.commit()
    finally:
        conn.close()
    return {"updated": int(updated)}
