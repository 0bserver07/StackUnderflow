"""Agent-teams routes — surfaces Claude Code parallel-agent topology.

Three read-only endpoints, all under ``/api/agent-teams``:

* ``GET /api/agent-teams``
  List of recent agent-team activity (top-level sessions that spawned
  sub-agents, with counts).

* ``GET /api/agent-teams/{session_id}``
  Full dependency graph for one session — the lead session plus every
  spawned agent, with message previews + cost.

* ``GET /api/agent-teams/{session_id}/agent/{agent_session_id}``
  Drill into one agent's full transcript (mirrors ``/api/jsonl-content``
  but scoped + validated against the lead session).

Empty-store contract: when a store has no sidechain messages, the list
route returns ``{"teams": []}`` cleanly (no 500). The graph + transcript
routes return 404 when the asked session can't be found, and 200 with
an empty ``agents`` list when the session exists but spawned no
sub-agents.

See ``docs/specs/agent-teams.md`` for the design rationale and the
"why no schema migration" choice.
"""

from __future__ import annotations

from fastapi import APIRouter, HTTPException, Query
from fastapi.responses import JSONResponse

import stackunderflow.deps as deps
from stackunderflow.services import agent_teams as agent_teams_service
from stackunderflow.store import db, schema

router = APIRouter()


@router.get("/api/agent-teams")
async def list_agent_teams(limit: int = Query(50, ge=1, le=500)) -> JSONResponse:
    """List recent sessions that spawned at least one sub-agent.

    Empty stores return ``{"teams": []}`` — never raises 500 on a fresh
    install. ``limit`` is bounded ``[1, 500]`` to keep the dashboard
    payload predictable; the default of 50 covers every realistic
    "recent activity" use case.
    """
    conn = db.connect(deps.store_path)
    try:
        # Idempotent + cheap on an already-current store; protects the
        # route from a fresh-install run where the schema migrations
        # haven't yet been applied (e.g. during the first server boot
        # before the lifespan hook runs).
        schema.apply(conn)
        teams = agent_teams_service.list_team_sessions(conn, limit=limit)
    finally:
        conn.close()

    return JSONResponse(
        {"teams": [agent_teams_service.team_summary_to_dict(t) for t in teams]}
    )


@router.get("/api/agent-teams/{session_id}")
async def get_agent_team(session_id: str) -> JSONResponse:
    """Full dependency graph for one team (rooted at ``session_id``).

    404 when no session with that id exists in the store. 200 with an
    empty ``agents`` array when the session exists but spawned no
    sub-agents — lets the dashboard distinguish "wrong url" from
    "no agents yet".
    """
    conn = db.connect(deps.store_path)
    try:
        schema.apply(conn)
        graph = agent_teams_service.build_team_graph(
            conn, lead_session_id=session_id
        )
    finally:
        conn.close()

    if graph is None:
        raise HTTPException(
            status_code=404,
            detail=f"Lead session not found in store: {session_id}",
        )
    return JSONResponse(agent_teams_service.team_graph_to_dict(graph))


@router.get("/api/agent-teams/{session_id}/agent/{agent_session_id}")
async def get_agent_team_transcript(
    session_id: str, agent_session_id: str
) -> JSONResponse:
    """Drill into one agent's full transcript.

    404 when either the lead or the agent session is missing, or when
    the two sessions live in different projects (defensive cross-
    project fence — see :func:`agent_teams_service.get_agent_transcript`).
    """
    conn = db.connect(deps.store_path)
    try:
        schema.apply(conn)
        rows = agent_teams_service.get_agent_transcript(
            conn,
            lead_session_id=session_id,
            agent_session_id=agent_session_id,
        )
    finally:
        conn.close()

    if rows is None:
        raise HTTPException(
            status_code=404,
            detail=(
                f"Agent session {agent_session_id} not found in the same "
                f"project as lead {session_id}"
            ),
        )
    return JSONResponse(
        {
            "session_id": session_id,
            "agent_session_id": agent_session_id,
            "messages": rows,
            "message_count": len(rows),
        }
    )


# Re-exported for tests and documentation.
__all__ = ["router"]
