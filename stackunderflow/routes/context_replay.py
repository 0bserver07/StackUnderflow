"""``GET /api/context-replay/{session_id}`` — context-window replay (issue #96).

Thin HTTP wrapper around :mod:`stackunderflow.services.context_replay`. Returns
the ordered message sequence that had accumulated in a session up to a ``seq``
cutoff (``?at=<seq>``), each turn with a content preview, an estimated token
footprint, its tool calls, and a running token total — so the dashboard can
scrub ``at`` and watch the context grow.

Read-only and advisory: an unknown session returns an empty-but-valid body
(200), never a 500. See the service docstring for the MVP context semantics
("the session's message sequence up to ``at``"; harness-side eviction is a
future refinement).

Two behaviours worth calling out:

* **Same-project fencing** (like the agent-teams routes). When a project scope
  is active — an explicit ``?project=<slug>``, a ``?log_path=``, or the server's
  current project (:data:`deps.current_log_path`) — a session that resolves to a
  *different* project is fenced: the response is the empty-but-valid shape with
  a warning, never that other project's content. No scope active ⇒ whole store.
* **Read-through cache** (like ``routes/forks``). The full per-session timeline
  is the heavy unit; it is memoized keyed on ``(store, session_fk)`` with a
  ``(max ts, message_count)`` signature that self-invalidates the instant ingest
  writes a new message. Scrubbing ``at`` re-slices the cached build in-process,
  so repeat requests stay well inside the 200 ms budget.
"""

from __future__ import annotations

import copy
import threading
from pathlib import Path
from typing import Any

from fastapi import APIRouter, Query
from fastapi.responses import JSONResponse

import stackunderflow.deps as deps
from stackunderflow.services import context_replay as context_replay_service
from stackunderflow.store import db, schema

router = APIRouter()

# ``build_context_timeline`` walks a whole session's messages and parses each
# ``raw_json`` — cheap for one session, but a scrubber fires a request per drag
# tick, all re-slicing the SAME build. Memoize the full timeline (mirrors
# ``routes/forks``'s report cache): keyed on (store, session_fk) plus a
# per-session signature (max ts, message_count) so a fresh ingest bumps the key
# and a stale entry can't outlive a refresh. The slice stays OUTSIDE the cache
# (applied to a deep copy) so every ``at`` is served without recompute.
_CONTEXT_CACHE: dict[tuple[str, int], tuple[tuple[str | None, int], dict]] = {}
_CONTEXT_CACHE_LOCK = threading.Lock()

_AT_QUERY = Query(None, description="seq cutoff (inclusive); omit for the full session")
_PROJECT_QUERY = Query(None, description="Project slug to fence to")
_LOG_PATH_QUERY = Query(None, description="Project log path; omit for the active/whole store")


def _session_signature(conn: Any, session_fk: int) -> tuple[str | None, int]:
    """(max timestamp, message_count) for one session — the cache invalidator.

    Any ingest that appends a message bumps the count / newest ts, changing the
    signature and forcing a rebuild. Advisory: a bad read returns a sentinel
    that simply misses the cache rather than raising.
    """
    try:
        row = conn.execute(
            "SELECT MAX(timestamp) AS mts, COUNT(*) AS n "
            "FROM messages WHERE session_fk = ?",
            (session_fk,),
        ).fetchone()
    except Exception:  # noqa: BLE001 — advisory: a bad store just misses cache
        return (None, -1)
    if row is None:
        return (None, 0)
    return (row["mts"], int(row["n"] or 0))


def _build_timeline_cached(conn: Any, *, session_id: str, session_fk: int) -> dict:
    """Read-through cache around :func:`build_context_timeline`.

    Deep-copies on read so a caller can slice/mutate without poisoning the
    shared entry. Miss or signature drift → one rebuild, cached for the next
    reader (the next scrub tick).
    """
    sig = _session_signature(conn, session_fk)
    key = (str(deps.store_path), int(session_fk))
    with _CONTEXT_CACHE_LOCK:
        cached = _CONTEXT_CACHE.get(key)
    if cached is not None and cached[0] == sig:
        return copy.deepcopy(cached[1])
    full = context_replay_service.build_context_timeline(conn, session_id=session_id)
    with _CONTEXT_CACHE_LOCK:
        _CONTEXT_CACHE[key] = (sig, full)
    return copy.deepcopy(full)


def _resolve_session_row(conn: Any, session_id: str) -> tuple[int, str, int] | None:
    """``session_id`` → ``(session_fk, session_id, project_id)`` (most recent)."""
    try:
        row = conn.execute(
            "SELECT id, session_id, project_id FROM sessions WHERE session_id = ? "
            "ORDER BY last_ts DESC NULLS LAST, id DESC LIMIT 1",
            (session_id,),
        ).fetchone()
    except Exception:  # noqa: BLE001 — advisory route, never 500 on a bad store
        return None
    if row is None:
        return None
    return int(row["id"]), str(row["session_id"]), int(row["project_id"])


def _scope_project_ids(conn: Any, project: str | None, log_path: str | None) -> list[int] | None:
    """Resolve the active project scope to ``projects.id`` list, or ``None``.

    ``None`` = no scope = whole store (no fence). An explicit ``project`` slug
    wins; else a ``log_path``'s basename; else the server's current project.
    A slug that resolves to no project yields ``[]`` — an empty scope that
    fences every session (same contract as ``routes/forks``).
    """
    slug: str | None = None
    if isinstance(project, str) and project.strip():
        slug = project.strip()
    else:
        path = log_path if isinstance(log_path, str) and log_path.strip() else deps.current_log_path
        if path:
            slug = Path(path).name
    if not slug:
        return None
    try:
        rows = conn.execute("SELECT id FROM projects WHERE slug = ?", (slug,)).fetchall()
    except Exception:  # noqa: BLE001 — advisory route, never 500 on a bad store
        return []
    return [int(r["id"]) for r in rows]


@router.get("/api/context-replay/{session_id}")
async def get_context_replay(
    session_id: str,
    at: int | None = _AT_QUERY,
    project: str | None = _PROJECT_QUERY,
    log_path: str | None = _LOG_PATH_QUERY,
) -> JSONResponse:
    """Reconstruct the context for ``session_id`` up to ``?at=<seq>``.

    Response (always 200; the shape is stable even when empty)::

        {"session_id", "at_seq", "message_count", "total_tokens",
         "events": [{"seq", "role", "content_preview", "tokens",
                     "cumulative_tokens", "tool_calls"}...],
         "warnings": [...]}

    * unknown session → empty-but-valid body + a warning (advisory, not 404);
    * a session outside the active project scope → empty-but-valid + a fence
      warning (never another project's content);
    * otherwise the reconstruction, sliced to ``seq <= at`` (``at`` omitted =
      the whole session).
    """
    # ``at`` arrives as a real int (FastAPI coerces ``?at=42``); coerce anything
    # else (the Query sentinel when called directly in tests) to None.
    at_seq = at if isinstance(at, int) and not isinstance(at, bool) else None
    project_str = project if isinstance(project, str) else None
    log_path_str = log_path if isinstance(log_path, str) else None

    conn = db.connect(deps.store_path)
    try:
        # Cheap on a current store; guards a fresh-install request that lands
        # before the lifespan migration has run.
        schema.apply(conn)

        resolved = _resolve_session_row(conn, session_id)
        if resolved is None:
            # Unknown session — advisory empty-but-valid (not a 404).
            full = context_replay_service.build_context_timeline(conn, session_id=session_id)
            return JSONResponse(
                context_replay_service.slice_context_timeline(full, at_seq=at_seq)
            )
        session_fk, sid, project_id = resolved

        scope_ids = _scope_project_ids(conn, project_str, log_path_str)
        if scope_ids is not None and project_id not in scope_ids:
            # Cross-project fence — never serve another project's transcript.
            return JSONResponse(
                context_replay_service.empty_context(
                    sid,
                    at_seq=at_seq,
                    warnings=[
                        f"session {sid} is outside the active project scope"
                    ],
                )
            )

        full = _build_timeline_cached(conn, session_id=sid, session_fk=session_fk)
        body = context_replay_service.slice_context_timeline(full, at_seq=at_seq)
    finally:
        conn.close()

    return JSONResponse(body)


# Re-exported for tests and documentation.
__all__ = ["router"]
