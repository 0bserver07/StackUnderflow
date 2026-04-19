"""Project management routes."""

import os
from pathlib import Path

from fastapi import APIRouter, HTTPException
from fastapi.responses import JSONResponse

import stackunderflow.deps as deps
from stackunderflow.infra.discovery import locate_logs as find_claude_logs
from stackunderflow.store import db, queries

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
        project_path = dir_name[1:].replace("-", "/")
    else:
        project_path = dir_name

    deps.current_project_path = project_path
    deps.current_log_path = str(log_path)

    # Index for search/QA in background (search and QA services use store data)
    try:
        if deps.search_service is not None:
            conn = db.connect(deps.store_path)
            try:
                project_row = queries.get_project(conn, slug=dir_name)
                if project_row is not None:
                    queries.list_sessions(conn, project_id=project_row.id)
            finally:
                conn.close()
    except Exception:  # noqa: S110
        pass

    return JSONResponse(
        {
            "status": "success",
            "project_path": project_path,
            "log_path": str(log_path),
            "log_dir_name": dir_name,
            "message": f"Now analyzing logs from: {dir_name}",
        }
    )


# Get recent projects from store
@router.get("/api/recent-projects")
async def get_recent_projects():
    """Get list of recent projects from session store"""
    try:
        conn = db.connect(deps.store_path)
        try:
            project_rows = queries.list_projects(conn)
        finally:
            conn.close()

        projects = [
            {
                "dir_name": p.slug,
                "log_path": p.path or "",
                "last_modified": p.last_modified,
                "file_count": 0,  # not tracked in store
            }
            for p in project_rows
        ]

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
    try:
        conn = db.connect(deps.store_path)
        try:
            project_rows = queries.list_projects(conn)
        finally:
            conn.close()

        projects = [
            {
                "dir_name": p.slug,
                "log_path": p.path or "",
                "file_count": 0,
                "total_size_mb": 0.0,
                "last_modified": p.last_modified,
                "first_seen": p.first_seen,
                "display_name": p.display_name,
                "in_cache": False,
                "url_slug": p.slug,
                "stats": None,
            }
            for p in project_rows
        ]

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
                    "cached_count": 0,
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
