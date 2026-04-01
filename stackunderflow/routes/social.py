"""Social layer routes: agents, discussions, votes, simulation."""

from typing import Any

from fastapi import APIRouter, HTTPException
from fastapi.responses import JSONResponse

import stackunderflow.deps as deps

router = APIRouter()


# ── Agents ────────────────────────────────────────────────────────────────────

@router.get("/api/agents")
async def list_agents():
    """List all agent personas."""
    if deps.social_service is None:
        return JSONResponse(
            {"error": "Social service is not available."},
            status_code=503,
        )
    try:
        agents = deps.social_service.list_agents()
        return JSONResponse({"agents": agents})
    except Exception as e:
        return JSONResponse({"error": f"Failed to list agents: {str(e)}"}, status_code=500)


@router.get("/api/agents/{agent_id}")
async def get_agent(agent_id: str):
    """Get a single agent persona."""
    if deps.social_service is None:
        return JSONResponse({"error": "Social service is not available."}, status_code=503)
    try:
        agent = deps.social_service.get_agent(agent_id)
        if not agent:
            raise HTTPException(status_code=404, detail="Agent not found")
        return JSONResponse(agent)
    except HTTPException:
        raise
    except Exception as e:
        return JSONResponse({"error": f"Failed to get agent: {str(e)}"}, status_code=500)


@router.post("/api/agents")
async def create_agent(data: dict[str, Any]):
    """Create a new agent persona."""
    if deps.social_service is None:
        return JSONResponse({"error": "Social service is not available."}, status_code=503)
    try:
        agent = deps.social_service.create_agent(data)
        return JSONResponse(agent, status_code=201)
    except Exception as e:
        return JSONResponse({"error": f"Failed to create agent: {str(e)}"}, status_code=500)


@router.put("/api/agents/{agent_id}")
async def update_agent(agent_id: str, data: dict[str, Any]):
    """Update an agent persona."""
    if deps.social_service is None:
        return JSONResponse({"error": "Social service is not available."}, status_code=503)
    try:
        agent = deps.social_service.update_agent(agent_id, data)
        if not agent:
            raise HTTPException(status_code=404, detail="Agent not found")
        return JSONResponse(agent)
    except HTTPException:
        raise
    except Exception as e:
        return JSONResponse({"error": f"Failed to update agent: {str(e)}"}, status_code=500)


# ── Discussions ───────────────────────────────────────────────────────────────

@router.get("/api/discussions/counts")
async def get_discussion_counts(qa_ids: str = ""):
    """Get discussion counts for multiple Q&A pairs."""
    if deps.social_service is None:
        return JSONResponse({"error": "Social service is not available."}, status_code=503)
    try:
        ids = [i.strip() for i in qa_ids.split(",") if i.strip()]
        if not ids:
            return JSONResponse({"counts": {}})
        stats = deps.social_service.get_qa_social_stats(ids)
        counts = {qid: s["discussion_count"] for qid, s in stats.items()}
        return JSONResponse({"counts": counts})
    except Exception as e:
        return JSONResponse({"error": f"Failed to get counts: {str(e)}"}, status_code=500)


@router.get("/api/discussions/social-stats")
async def get_social_stats(qa_ids: str = ""):
    """Get social stats (discussion count, votes, agent avatars) for Q&A pairs."""
    if deps.social_service is None:
        return JSONResponse({"error": "Social service is not available."}, status_code=503)
    try:
        ids = [i.strip() for i in qa_ids.split(",") if i.strip()]
        if not ids:
            return JSONResponse({"stats": {}})
        stats = deps.social_service.get_qa_social_stats(ids)
        return JSONResponse({"stats": stats})
    except Exception as e:
        return JSONResponse({"error": f"Failed to get social stats: {str(e)}"}, status_code=500)


@router.get("/api/discussions/{qa_id}")
async def get_discussion_tree(qa_id: str):
    """Get the threaded discussion tree for a Q&A pair."""
    if deps.social_service is None:
        return JSONResponse({"error": "Social service is not available."}, status_code=503)
    try:
        tree = deps.social_service.get_discussion_tree(qa_id)
        return JSONResponse(tree)
    except Exception as e:
        return JSONResponse({"error": f"Failed to get discussion: {str(e)}"}, status_code=500)


@router.post("/api/discussions/{qa_id}")
async def post_discussion(qa_id: str, data: dict[str, Any]):
    """Post a new discussion comment on a Q&A pair."""
    if deps.social_service is None:
        return JSONResponse({"error": "Social service is not available."}, status_code=503)
    try:
        content = data.get("content", "").strip()
        if not content:
            raise HTTPException(status_code=400, detail="Content is required")
        if data.get("author_type") == "agent":
            raise HTTPException(status_code=403, detail="Agent posts can only be created via simulation")
        author_type = data.get("author_type", "human")
        author_id = data.get("author_id", "human")
        parent_id = data.get("parent_id")
        post = deps.social_service.create_discussion(
            qa_id=qa_id,
            author_type=author_type,
            author_id=author_id,
            content=content,
            parent_id=parent_id,
        )
        return JSONResponse(post, status_code=201)
    except HTTPException:
        raise
    except Exception as e:
        return JSONResponse({"error": f"Failed to post discussion: {str(e)}"}, status_code=500)


# ── Votes ─────────────────────────────────────────────────────────────────────

@router.post("/api/votes/toggle")
async def toggle_vote(data: dict[str, Any]):
    """Toggle a vote on a Q&A pair or discussion post."""
    if deps.social_service is None:
        return JSONResponse({"error": "Social service is not available."}, status_code=503)
    try:
        target_type = data.get("target_type")
        target_id = data.get("target_id")
        if not target_type or not target_id:
            raise HTTPException(status_code=400, detail="target_type and target_id are required")
        result = deps.social_service.toggle_vote(
            target_type=target_type,
            target_id=target_id,
        )
        return JSONResponse(result)
    except HTTPException:
        raise
    except Exception as e:
        return JSONResponse({"error": f"Failed to toggle vote: {str(e)}"}, status_code=500)


@router.get("/api/votes/counts")
async def get_vote_counts(target_type: str = "", target_ids: str = ""):
    """Get vote counts for a list of targets."""
    if deps.social_service is None:
        return JSONResponse({"error": "Social service is not available."}, status_code=503)
    try:
        ids = [i.strip() for i in target_ids.split(",") if i.strip()]
        if not target_type or not ids:
            return JSONResponse({"counts": {}})
        counts = deps.social_service.get_vote_counts(target_type, ids)
        return JSONResponse({"counts": counts})
    except Exception as e:
        return JSONResponse({"error": f"Failed to get vote counts: {str(e)}"}, status_code=500)


@router.get("/api/votes/user")
async def get_user_votes(target_type: str = "", target_ids: str = ""):
    """Get which targets the user has voted on."""
    if deps.social_service is None:
        return JSONResponse({"error": "Social service is not available."}, status_code=503)
    try:
        ids = [i.strip() for i in target_ids.split(",") if i.strip()]
        if not target_type or not ids:
            return JSONResponse({"votes": {}})
        votes = deps.social_service.get_user_votes(target_type, ids)
        return JSONResponse({"votes": votes})
    except Exception as e:
        return JSONResponse({"error": f"Failed to get user votes: {str(e)}"}, status_code=500)


# ── Agent Simulation ──────────────────────────────────────────────────────────

@router.post("/api/simulate/discuss/{qa_id}")
async def trigger_agent_discussion(qa_id: str, data: dict[str, Any] | None = None):
    """Trigger an AI agent discussion on a Q&A pair."""
    if deps.social_service is None or deps.agent_sim_service is None:
        return JSONResponse(
            {"error": "Social or simulation service is not available."},
            status_code=503,
        )
    if deps.qa_service is None:
        return JSONResponse({"error": "Q&A service is not available."}, status_code=503)
    try:
        agent_ids = (data or {}).get("agent_ids")
        run = await deps.agent_sim_service.trigger_discussion(
            qa_id=qa_id,
            social_service=deps.social_service,
            qa_service=deps.qa_service,
            agent_ids=agent_ids,
        )
        return JSONResponse(run, status_code=201)
    except ValueError as e:
        return JSONResponse({"error": str(e)}, status_code=400)
    except Exception as e:
        deps.logger.error(f"Failed to trigger discussion: {e}")
        return JSONResponse({"error": f"Failed to trigger discussion: {str(e)}"}, status_code=500)


@router.get("/api/simulate/status/{run_id}")
async def get_simulation_status(run_id: str):
    """Get the status of an agent discussion simulation."""
    if deps.social_service is None:
        return JSONResponse({"error": "Social service is not available."}, status_code=503)
    try:
        run = deps.social_service.get_agent_run(run_id)
        if not run:
            raise HTTPException(status_code=404, detail="Simulation run not found")
        return JSONResponse(run)
    except HTTPException:
        raise
    except Exception as e:
        return JSONResponse({"error": f"Failed to get status: {str(e)}"}, status_code=500)


@router.post("/api/simulate/cancel/{run_id}")
async def cancel_simulation(run_id: str):
    """Cancel a running agent discussion simulation."""
    if deps.social_service is None or deps.agent_sim_service is None:
        return JSONResponse(
            {"error": "Social or simulation service is not available."},
            status_code=503,
        )
    try:
        cancelled = deps.agent_sim_service.cancel_run(run_id, deps.social_service)
        if not cancelled:
            return JSONResponse({"error": "Simulation not found or already finished"}, status_code=404)
        return JSONResponse({"status": "cancelled"})
    except Exception as e:
        return JSONResponse({"error": f"Failed to cancel: {str(e)}"}, status_code=500)
