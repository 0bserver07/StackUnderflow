"""ETL pipeline status route — Wave 4C.

Single endpoint surfaces watcher health, mart watermarks vs the max
event id, per-provider event counts, and a coarse ``health`` enum so
the dashboard can show a status badge and the CLI a one-line health
check.

The actual snapshot is built by
:func:`stackunderflow.etl.status.assemble_status`; this module is a
thin FastAPI shell that opens a connection, calls the assembler, and
returns the JSON. Holding the SQL in the assembler keeps the CLI
``stackunderflow etl status`` command and this route in lockstep.

Performance contract: <50ms end-to-end against a 200K-event store on
the maintainer's machine. Every count is a ``SELECT COUNT(*)`` on an
indexed column, every per-mart watermark a primary-key lookup.
"""

from __future__ import annotations

from typing import Any

from fastapi import APIRouter

import stackunderflow.deps as deps
from stackunderflow.etl.status import assemble_status
from stackunderflow.store import db, schema

router = APIRouter()


@router.get("/api/etl/status")
async def get_etl_status() -> dict[str, Any]:
    """Return a live snapshot of the ETL pipeline.

    Response shape (see :func:`stackunderflow.etl.status.assemble_status`)::

        {
          "watcher": {"enabled": bool, "running": bool|"unknown", ...},
          "marts": {"daily": {"watermark": int, "row_count": int, ...}, ...},
          "events": {"total": int, "max_id": int,
                       "by_provider": {...}, "by_cost_source": {...}},
          "lag_seconds": int,
          "health": "live"|"syncing"|"stale"|"error"
        }
    """
    conn = db.connect(deps.store_path)
    try:
        # ``schema.apply`` is idempotent and cheap on an already-current
        # store (single ``PRAGMA user_version`` read); it guarantees
        # the etl tables exist on a fresh-install machine where the
        # server hasn't yet booted to install them.
        schema.apply(conn)
        return assemble_status(conn)
    finally:
        conn.close()
