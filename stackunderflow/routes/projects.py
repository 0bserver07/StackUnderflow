"""Project management routes."""

import asyncio
import os
from pathlib import Path

from fastapi import APIRouter, HTTPException
from fastapi.responses import JSONResponse

import stackunderflow.deps as deps
from stackunderflow.infra.discovery import locate_logs as find_claude_logs
from stackunderflow.infra.preloader import warm as _warm_projects
from stackunderflow.pipeline import process as run_pipeline

router = APIRouter()


# Set project endpoint
@router.post("/api/project")
async def set_project(data: dict[str, str]):
    """Set the project path to analyze"""
    project_path = data.get("project_path")
    if not project_path:
        raise HTTPException(status_code=400, detail="Project path is required")

    # Validate project path exists
    if not os.path.exists(project_path):
        raise HTTPException(status_code=400, detail=f"Project path does not exist: {project_path}")

    # Find Claude logs for this project
    log_path = find_claude_logs(project_path)
    if not log_path or not os.path.exists(log_path):
        raise HTTPException(
            status_code=404,
            detail=f"Claude logs not found for project: {project_path}. "
            f"Make sure you have used Claude with this project.",
        )

    deps.current_project_path = project_path
    deps.current_log_path = log_path

    return JSONResponse(
        {
            "status": "success",
            "project_path": project_path,
            "log_path": log_path,
            "message": "Project set successfully. You can now view the dashboard.",
        }
    )


# Get current project
@router.get("/api/project")
async def get_current_project():
    """Get the current project being analyzed"""
    if not deps.current_project_path:
        return JSONResponse({"status": "no_project", "message": "No project selected"})

    return JSONResponse(
        {
            "status": "active",
            "project_path": deps.current_project_path,
            "log_path": deps.current_log_path,
            "log_dir_name": Path(deps.current_log_path).name if deps.current_log_path else None,
        }
    )


# Set project by log directory name
@router.post("/api/project-by-dir")
async def set_project_by_dir(data: dict[str, str]):
    """Set the project by log directory name"""
    dir_name = data.get("dir_name")
    if not dir_name:
        raise HTTPException(status_code=400, detail="Directory name is required")

    # Build the log path
    claude_base = Path.home() / ".claude" / "projects"
    log_path = (claude_base / dir_name).resolve()
    if not str(log_path).startswith(str(claude_base.resolve()) + os.sep):
        raise HTTPException(status_code=400, detail="Invalid path")

    if not log_path.exists() or not log_path.is_dir():
        raise HTTPException(status_code=404, detail=f"Log directory not found: {dir_name}")

    # Check if it has log files
    log_files = list(log_path.glob("*.jsonl"))
    if not log_files:
        raise HTTPException(status_code=404, detail=f"No log files found in directory: {dir_name}")

    # Try to convert back to project path (best effort)
    if dir_name.startswith("-"):
        # Convert from hashed format
        project_path = dir_name[1:].replace("-", "/")
    else:
        # Use directory name as is
        project_path = dir_name

    deps.current_project_path = project_path
    deps.current_log_path = str(log_path)

    # Pre-warm the current project immediately
    if not deps.cache.fetch(deps.current_log_path):
        deps.is_reindexing = True
        deps.logger.debug(f"Pre-warming {dir_name}...")
        try:
            messages, stats = run_pipeline(deps.current_log_path)

            # Save to file cache first (creates metadata)
            deps.cache.persist_stats(deps.current_log_path, stats)
            deps.cache.persist_messages(deps.current_log_path, messages)

            # Then store in memory cache
            deps.cache.store(deps.current_log_path, messages, stats)

            # Index for search in background
            try:
                if deps.search_service is not None:
                    deps.search_service.index_project(dir_name, messages)
            except Exception as search_err:
                deps.logger.debug(f"Search indexing failed for {dir_name}: {search_err}")

            # Index Q&A pairs in background
            try:
                if deps.qa_service is not None:
                    deps.qa_service.index_project(dir_name, messages)
            except Exception as qa_err:
                deps.logger.debug(f"Q&A indexing failed for {dir_name}: {qa_err}")

            # Index tags in background
            try:
                if deps.tag_service is not None:
                    deps.tag_service.index_project(messages)
            except Exception as tag_err:
                deps.logger.debug(f"Tag indexing failed for {dir_name}: {tag_err}")

            deps.logger.debug(f"Successfully pre-warmed {dir_name}")
        except Exception as e:
            deps.logger.debug(f"Failed to pre-warm: {e}")
        finally:
            deps.is_reindexing = False

    # Warm cache for other recent projects in background
    asyncio.create_task(_warm_projects(deps.cache, deps.current_log_path, skip_current=True))

    return JSONResponse(
        {
            "status": "success",
            "project_path": project_path,
            "log_path": str(log_path),
            "log_dir_name": dir_name,
            "message": f"Now analyzing logs from: {dir_name}",
        }
    )


# Get recent projects from Claude logs directory
@router.get("/api/recent-projects")
async def get_recent_projects():
    """Get list of recent projects from Claude logs"""
    try:
        claude_base = Path.home() / ".claude" / "projects"
        if not claude_base.exists():
            return JSONResponse({"projects": []})

        # Get all project directories
        projects = []
        for project_dir in claude_base.iterdir():
            if project_dir.is_dir():
                # Check if it has log files
                log_files = list(project_dir.glob("*.jsonl"))
                if log_files:
                    # Get most recent modification time
                    latest_mod = max(f.stat().st_mtime for f in log_files)
                    projects.append(
                        {
                            "dir_name": project_dir.name,  # The actual directory name
                            "log_path": str(project_dir),
                            "last_modified": latest_mod,
                            "file_count": len(log_files),
                        }
                    )

        # Sort by last modified, most recent first
        projects.sort(key=lambda x: x["last_modified"], reverse=True)

        # Return top 20 to show more options
        return JSONResponse({"projects": projects[:20]})

    except Exception as e:
        return JSONResponse({"projects": [], "error": str(e)})


# Comprehensive projects endpoint for global stats
@router.get("/api/projects")
async def get_projects(
    include_stats: bool = False, sort_by: str = "last_modified", limit: int | None = None, offset: int = 0
):
    """
    Get all available Claude projects with metadata.

    Args:
        include_stats: Include statistics for each project (may be slower)
        sort_by: Sort field (last_modified, first_seen, size, name)
        limit: Maximum number of projects to return
        offset: Offset for pagination

    Returns:
        JSON with projects list and metadata
    """
    from stackunderflow.infra.discovery import project_metadata as get_all_projects_with_metadata

    try:
        # Get all projects with metadata
        projects = get_all_projects_with_metadata()

        # Add cache status and URL slug for each project
        for project in projects:
            project["in_cache"] = deps.cache.fetch(project["log_path"]) is not None
            project["url_slug"] = project["dir_name"]  # Use dir name for URLs

        if include_stats:
            # Add statistics from cache for cached projects
            for project in projects:
                if project["in_cache"]:
                    cache_result = deps.cache.fetch(project["log_path"])
                    if cache_result:
                        _, stats = cache_result
                        # Extract stats from nested structure
                        overview = stats.get("overview", {})
                        total_tokens = overview.get("total_tokens", {})
                        user_interactions = stats.get("user_interactions", {})

                        project["stats"] = {
                            "total_input_tokens": total_tokens.get("input", 0),
                            "total_output_tokens": total_tokens.get("output", 0),
                            "total_cache_read": total_tokens.get("cache_read", 0),
                            "total_cache_write": total_tokens.get("cache_creation", 0),
                            "total_commands": user_interactions.get("user_commands_analyzed", 0),
                            "avg_tokens_per_command": user_interactions.get("avg_tokens_per_command", 0),
                            "avg_steps_per_command": user_interactions.get("avg_steps_per_command", 0),
                            "compact_summary_count": overview.get("message_types", {}).get("compact_summary", 0),
                            "first_message_date": overview.get("date_range", {}).get("start"),
                            "last_message_date": overview.get("date_range", {}).get("end"),
                            "total_cost": overview.get("total_cost", 0),
                        }
                        deps.logger.debug(f"Added stats for cached project {project['dir_name']}: {project['stats']}")
                    else:
                        # Cache was evicted between status check and retrieval
                        project["in_cache"] = False
                else:
                    # Try file cache
                    cached_stats = deps.cache.load_stats(project["log_path"])
                    if cached_stats:
                        # Extract stats from nested structure (same as memory cache)
                        overview = cached_stats.get("overview", {})
                        total_tokens = overview.get("total_tokens", {})
                        user_interactions = cached_stats.get("user_interactions", {})

                        project["stats"] = {
                            "total_input_tokens": total_tokens.get("input", 0),
                            "total_output_tokens": total_tokens.get("output", 0),
                            "total_cache_read": total_tokens.get("cache_read", 0),
                            "total_cache_write": total_tokens.get("cache_creation", 0),
                            "total_commands": user_interactions.get("user_commands_analyzed", 0),
                            "avg_tokens_per_command": user_interactions.get("avg_tokens_per_command", 0),
                            "avg_steps_per_command": user_interactions.get("avg_steps_per_command", 0),
                            "compact_summary_count": overview.get("message_types", {}).get("compact_summary", 0),
                            "first_message_date": overview.get("date_range", {}).get("start"),
                            "last_message_date": overview.get("date_range", {}).get("end"),
                            "total_cost": overview.get("total_cost", 0),
                        }
                    else:
                        project["stats"] = None  # Will need to load in background

        # Sort projects
        if sort_by == "last_modified":
            projects.sort(key=lambda x: x["last_modified"], reverse=True)
        elif sort_by == "first_seen":
            projects.sort(key=lambda x: x["first_seen"])
        elif sort_by == "size":
            projects.sort(key=lambda x: x["total_size_mb"], reverse=True)
        elif sort_by == "name":
            projects.sort(key=lambda x: x["display_name"])

        # Apply pagination
        total_count = len(projects)
        if limit:
            projects = projects[offset : offset + limit]

        return JSONResponse(
            {
                "projects": projects,
                "total_count": total_count,
                "has_more": offset + limit < total_count if limit else False,
                "cache_status": {
                    "cached_count": sum(1 for p in projects if p["in_cache"]),
                    "total_projects": total_count,
                },
            }
        )

    except Exception as e:
        import traceback

        traceback.print_exc()
        return JSONResponse({"error": f"Failed to get projects: {str(e)}"}, status_code=500)


# Global statistics endpoint
@router.get("/api/global-stats")
async def get_global_stats():
    """
    Get aggregated statistics across all projects.

    Returns:
        JSON with global statistics including charts data
    """
    from stackunderflow.infra.discovery import project_metadata as get_all_projects_with_metadata
    from stackunderflow.pipeline.cross_project import aggregate as _aggregate

    try:
        # Get all projects
        projects = get_all_projects_with_metadata()

        # Add cache status
        for project in projects:
            project["in_cache"] = deps.cache.fetch(project["log_path"]) is not None

        # Aggregate global stats
        global_stats = await _aggregate(projects, deps.cache, deps.cache)

        # Add configuration
        global_stats["config"] = {"max_date_range_days": deps.config.get("max_date_range_days")}

        return JSONResponse(global_stats)

    except Exception as e:
        import traceback

        traceback.print_exc()
        return JSONResponse({"error": f"Failed to get global stats: {str(e)}"}, status_code=500)
