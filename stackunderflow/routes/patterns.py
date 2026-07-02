"""``GET /api/patterns`` — the cross-session coding-health report.

Thin HTTP wrapper around :func:`stackunderflow.reports.patterns.mine_patterns`.
Recurrence-keyed intelligence across ALL sessions in a bounded window:
per-file failure rates, recurring error signatures (with resolution hints
where derivable), and Bash command failure clusters.

Contract
========

``GET /api/patterns?project=<slug>&since=<window>``

* ``project`` — optional ``projects.slug``. When present, the report is
  scoped to every project row with that slug (one per provider). When
  omitted, the active dashboard project (``deps.current_log_path``) is
  used; with neither, the report spans the whole store (still
  window-bounded). An unknown slug yields an empty report, not a 500 —
  the feature is advisory.
* ``since`` — window size as ``<days>d`` (e.g. ``7d``, ``30d``, ``90d``).
  Default ``90d``; bounded to 1..365 days (there is deliberately no
  ``all`` — the mining pass never does an unbounded full-store scan).
  Anything else → 400.

Response::

    {
      "project": "<slug or null>",         # the scope that was applied
      "since": "90d",                      # echo of the validated window
      "report": {
        "window":  {"since": "<iso>", "days": 90},
        "sources": {"message_tool_mart": true},   # touch data available?
        "totals":  {tool_call_count, error_count, attributed_error_count,
                    interruption_count, interruption_session_count,
                    session_count, sessions_with_failures, files_touched},
        "file_risk": [                     # worst files first, capped
          {path, touch_count, edit_count, read_count, touch_session_count,
           failure_count, failure_session_count, failure_rate,  # 0..1 | null
           interruption_count, last_touch_ts, last_failure_ts,
           categories: {<error category>: n}, reason}
        ],
        "error_signatures": [              # recurring (>= 2 sessions) only
          {signature, category, count, session_count, resolved_session_count,
           first_ts, last_ts, top_tools: [..], top_files: [..],
           resolution_hints: [{action, count}], example, reason}
        ],
        "command_clusters": [              # >= 2 failures per cluster
          {command, failure_count, session_count,
           categories: {<error category>: n}, last_failure_ts, example, reason}
        ]
      }
    }

No dollar figures — this endpoint carries no currency payload. Every list
is deterministically ordered and capped, so the same store always renders
the same panel.
"""

from __future__ import annotations

import re
from pathlib import Path
from typing import Any

from fastapi import APIRouter, HTTPException, Query

import stackunderflow.deps as deps
from stackunderflow.reports.patterns import DEFAULT_SINCE_DAYS, MAX_SINCE_DAYS, mine_patterns
from stackunderflow.store import db

router = APIRouter()

_SINCE_QUERY = Query("90d", description="Window as <days>d, e.g. 7d | 30d | 90d (max 365d)")
_PROJECT_QUERY = Query(None, description="Project slug; omit for the active project / whole store")

_SINCE_RE = re.compile(r"^(\d{1,3})d$")


def _parse_since(since: str | None) -> int:
    """``"90d"`` → 90. Raises ``HTTPException(400)`` on anything invalid."""
    if since is None:
        return DEFAULT_SINCE_DAYS
    m = _SINCE_RE.match(since.strip())
    if m:
        days = int(m.group(1))
        if 1 <= days <= MAX_SINCE_DAYS:
            return days
    raise HTTPException(
        status_code=400,
        detail=(
            f"Invalid since '{since}'. Use <days>d between 1d and "
            f"{MAX_SINCE_DAYS}d, e.g. 7d, 30d, 90d."
        ),
    )


def _project_ids_for_slug(conn: Any, slug: str) -> list[int]:
    """Every ``projects.id`` carrying *slug* (one row per provider).

    Own resolver (no ``store/queries.py`` dependency), guarded so a bare
    store yields an empty scope rather than a 500 — same convention as
    ``routes/forks.py``.
    """
    try:
        rows = conn.execute(
            "SELECT id FROM projects WHERE slug = ?", (slug,)
        ).fetchall()
    except Exception:  # noqa: BLE001 — advisory route, never 500 on a bad store
        return []
    return [int(r["id"]) for r in rows]


@router.get("/api/patterns")
async def get_patterns(
    project: str | None = _PROJECT_QUERY,
    since: str = _SINCE_QUERY,
):
    """Return ``{project, since, report}`` (see the module docstring)."""
    # When invoked directly (tests, not via FastAPI's DI) the ``Query``
    # defaults leak through as Query sentinels — coerce anything that isn't
    # a real string. Same pattern ``routes/forks.py`` uses.
    project_str = project if isinstance(project, str) else None
    since_str = since if isinstance(since, str) else "90d"
    days = _parse_since(since_str)

    # Explicit ?project= wins; otherwise scope to the dashboard's active
    # project (log-path basename == slug); otherwise whole store.
    slug = project_str
    if slug is None and deps.current_log_path:
        slug = Path(deps.current_log_path).name

    conn = db.connect(deps.store_path)
    try:
        project_ids = _project_ids_for_slug(conn, slug) if slug else None
        report = mine_patterns(conn, since_days=days, project_ids=project_ids)
    finally:
        conn.close()

    return {
        "project": slug,
        "since": f"{days}d",
        "report": report,
    }
