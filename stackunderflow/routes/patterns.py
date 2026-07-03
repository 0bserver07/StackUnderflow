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

``POST /api/patterns/dismiss`` is the write companion (spec 27 Phase 2): the
"What almost bit me" panel calls it to record a dismissal into the proactive
nudge governance state, so the in-session Tier-1 hooks quiet down. It writes
only the governance JSON file, never the store — see :func:`dismiss_pattern`.
"""

from __future__ import annotations

import re
from pathlib import Path
from typing import Any

from fastapi import APIRouter, HTTPException, Query
from pydantic import BaseModel

import stackunderflow.deps as deps
from stackunderflow.reports.patterns import DEFAULT_SINCE_DAYS, MAX_SINCE_DAYS, mine_patterns
from stackunderflow.store import db

router = APIRouter()

# Nudge type ids the dismiss endpoint accepts — the "What almost bit me" panel
# dismisses one of these. Mirrors ``proactive.TYPE_*`` (kept local so this route
# has no import-time dependency on the hooks package).
_DISMISS_TYPES = frozenset({"command-cluster", "file-risk", "error-signature"})

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


# ── dismiss (Tier-2 → governance write) ──────────────────────────────────────


class DismissRequest(BaseModel):
    """Body for ``POST /api/patterns/dismiss`` (the "What almost bit me" panel).

    * ``type`` — one of ``command-cluster`` / ``file-risk`` / ``error-signature``.
    * ``scope`` — ``"fingerprint"`` (default; mute *this* specific nudge) or
      ``"type"`` (mute the whole kind).
    * ``target_key`` / ``counts`` — only for fingerprint scope: the nudge's
      target (normalised command / signature / path) and its two salient counts.
      The route feeds these through the *same* ``proactive.make_signal`` Tier-1
      uses, so the dismissed fingerprint is byte-identical to the one the hook's
      governance gate reads.
    """

    type: str
    scope: str = "fingerprint"
    target_key: str | None = None
    counts: list[int] | None = None


def _coerce_int(value: Any) -> int:
    try:
        return int(value)
    except (TypeError, ValueError):
        return 0


@router.post("/api/patterns/dismiss")
async def dismiss_pattern(body: DismissRequest) -> dict:
    """Record a dashboard dismissal into the proactive governance state.

    Writes the ``feedback`` ``dismissed`` counter that
    :func:`stackunderflow.hooks.proactive.record_dismissal` bumps and
    :func:`~stackunderflow.hooks.proactive.should_surface` reads for adaptive
    quieting (spec §4.3 / §5 Tier-2), so Tier-1 quiets accordingly. Only ever
    touches the governance JSON file at ``~/.stackunderflow/proactive_state.json``
    — never ``store.db``. Advisory: an unknown type is a 400, but a governance
    write hiccup is swallowed (``record_dismissal`` never raises).
    """
    from stackunderflow.hooks import proactive

    sig_type = body.type.strip().lower() if isinstance(body.type, str) else ""
    if sig_type not in _DISMISS_TYPES:
        raise HTTPException(status_code=400, detail=f"Unknown nudge type '{body.type}'.")

    scope = (body.scope or "fingerprint").strip().lower()
    if scope == "type" or not body.target_key:
        # Mute the whole kind: the dismissal key IS the type id.
        proactive.record_dismissal(sig_type)
        return {"ok": True, "scope": "type", "dismissed": sig_type}

    counts = [_coerce_int(c) for c in (body.counts or [])][:2]
    while len(counts) < 2:
        counts.append(0)
    # Same signal Tier-1 builds → same fingerprint (session_id is not part of it).
    signal = proactive.make_signal(
        sig_type, body.target_key, None, (counts[0], counts[1]), eligible=True
    )
    proactive.record_dismissal(signal.fingerprint)
    return {"ok": True, "scope": "fingerprint", "dismissed": signal.fingerprint}
