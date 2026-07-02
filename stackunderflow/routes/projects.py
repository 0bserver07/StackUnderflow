"""Project management routes."""

import os
from collections import defaultdict
from pathlib import Path
from typing import Annotated

from fastapi import APIRouter, HTTPException, Query
from fastapi.concurrency import run_in_threadpool
from fastapi.responses import JSONResponse

import stackunderflow.deps as deps
from stackunderflow.infra.currency import active_currency_payload
from stackunderflow.infra.discovery import locate_logs as find_claude_logs
from stackunderflow.store import db, mart_queries, queries

router = APIRouter()


# Pagination bounds for ``GET /api/projects``.
#
# ``limit`` omitted → ``PROJECTS_DEFAULT_LIMIT`` — a *large* default cap (not a
# tiny page) so existing callers that fetch "all projects" keep working for
# every realistic store (real installs top out in the low hundreds of slugs).
# The cap only bites a pathological store with more slugs than the default,
# which the paginating frontend walks page-by-page anyway.
#
# ``PROJECTS_MAX_LIMIT`` is the hard ceiling any single request can ask for, so
# a crafted ``?limit=999999`` can't force an oversized slice/serialisation.
PROJECTS_DEFAULT_LIMIT = 500
PROJECTS_MAX_LIMIT = 1000


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

    # First check if the project slug is registered in the database store
    conn = db.connect(deps.store_path)
    project_row = None
    try:
        project_row = queries.get_project(conn, slug=dir_name)
    except Exception:  # noqa: S110
        pass
    finally:
        conn.close()

    if project_row:
        # If registered in store, we bypass filesystem and glob checks.
        # It's an active/indexed project whose data is fully loaded in SQLite.
        log_path = Path(_resolve_log_dir(project_row.path, dir_name))
        project_path = project_row.path
        if not project_path:
            if dir_name.startswith("-"):
                project_path = dir_name[1:].replace("-", "/")
            else:
                project_path = dir_name
    else:
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
    include_stats: bool = False,
    sort_by: str = "last_modified",
    limit: int | None = None,
    offset: int = 0,
    provider: Annotated[list[str] | None, Query()] = None,
    include_worktrees: bool = False,
):
    """
    Get all available Claude projects with metadata.

    Args:
        include_stats: Include statistics for each project (may be slower)
        sort_by: Sort field (last_modified, first_seen, size, name)
        limit: Page size. Omitted → ``PROJECTS_DEFAULT_LIMIT`` (a large cap that
            preserves the historical "all projects" response for realistic
            stores); clamped to ``[1, PROJECTS_MAX_LIMIT]`` when provided.
        offset: Page offset (floored at 0). With ``total_count`` + ``has_more``
            in the response this is enough for the frontend to page.
        provider: Optional repeated query param (``?provider=cursor&provider=cline``)
            scoping the project list to those providers. Empty = "all".
            Case-insensitive on read, lowercased before comparison.
        include_worktrees: Campaign #8 — sessions run inside git worktrees log
            under phantom sibling slugs (``<parent>--worktrees-<x>``). By
            default those fragments are FOLDED into their parent row, which
            gains ``worktree_sessions`` / ``worktree_cost`` / ``worktree_count``.
            ``?include_worktrees=1`` returns the raw un-folded list instead,
            with each fragment row annotated ``worktree_of: <parent slug>`` so
            the frontend can badge it.

    Returns:
        JSON with projects list and metadata
    """
    # Normalise provider filter: lowercase + drop empties so callers that
    # pass `?provider=Cursor` work without round-tripping through the URL
    # canonicalisation in `services/filters.tsx`. Cheap + pure, so it stays
    # on the event loop; the blocking work is offloaded below.
    provider_filter = _normalise_provider_filter(provider)

    try:
        # The store query, mart reads and the per-directory filesystem glob
        # (`_dir_size_mb` over ~190 dirs) are blocking sync work. Run them in
        # a worker thread so the event loop keeps serving other requests
        # instead of stalling for the duration of the scan.
        payload = await run_in_threadpool(
            _compute_projects_payload,
            include_stats=include_stats,
            sort_by=sort_by,
            limit=limit,
            offset=offset,
            provider_filter=provider_filter,
            include_worktrees=include_worktrees,
        )
        return JSONResponse(payload)
    except Exception as e:
        import traceback

        traceback.print_exc()
        return JSONResponse({"error": f"Failed to get projects: {str(e)}"}, status_code=500)


def _normalise_provider_filter(provider: list[str] | None) -> set[str] | None:
    """Lowercase + drop empties so ``?provider=Cursor`` matches store rows."""
    if not provider:
        return None
    normed = {p.strip().lower() for p in provider if p and p.strip()}
    return normed or None


def _clamp_pagination(limit: int | None, offset: int) -> tuple[int, int]:
    """Resolve ``(limit, offset)`` to bounded, non-negative integers.

    ``limit is None`` → :data:`PROJECTS_DEFAULT_LIMIT` (preserve the historical
    "return everything" behaviour for realistic stores). An explicit ``limit``
    is clamped to ``[1, PROJECTS_MAX_LIMIT]``; ``offset`` floors at ``0``. The
    returned ``limit`` is always a positive int, so the caller can slice
    unconditionally.
    """
    if limit is None:
        resolved_limit = PROJECTS_DEFAULT_LIMIT
    else:
        resolved_limit = max(1, min(int(limit), PROJECTS_MAX_LIMIT))
    return resolved_limit, max(0, int(offset))


def _compute_projects_payload(
    *,
    include_stats: bool,
    sort_by: str,
    limit: int | None,
    offset: int,
    provider_filter: set[str] | None,
    include_worktrees: bool = False,
) -> dict:
    """Blocking body of ``GET /api/projects`` — runs in a worker thread.

    Opens its own SQLite connection (sqlite handles are single-thread, so
    connecting here keeps connect/use/close on one thread), reads project
    rows + marts, globs each project directory for its on-disk size, then
    folds worktree fragments into their parents (campaign #8), sorts /
    paginates and applies the active-currency conversion. Returns the JSON
    payload dict the route ships verbatim.

    Ordering matters: the worktree fold runs BEFORE the sort + page slice so
    ``total_count`` / ``has_more`` count folded rows, never phantom fragments.

    Caching note: this payload is computed per-request — nothing memoises it
    server-side today (the only cache in this module, ``_dir_size_cache``, is
    keyed on (path, mtime) and its values are fold-mode-independent). If a
    response cache is ever added, its key MUST include ``include_worktrees``
    or the folded and raw variants would cross-contaminate.
    """
    limit, offset = _clamp_pagination(limit, offset)
    conn = db.connect(deps.store_path)
    try:
        project_rows = queries.list_projects(conn)
        if provider_filter is not None:
            project_rows = [p for p in project_rows if (p.provider or "").lower() in provider_filter]

        # Wave 3A: prefer ``project_mart`` for the stats payload —
        # one indexed scan over the materialised totals beats the
        # bulk-aggregate pass (PR #65) which still touches every
        # message row. The bulk helpers stay as the fallback so
        # stores that haven't run the ETL pipeline keep working.
        session_counts = queries.bulk_session_counts(conn)

        mart_rows: dict[int, dict] = {}
        if include_stats:
            for row in mart_queries.list_project_mart(conn):
                mart_rows[int(row["project_id"])] = row

        # Project ids whose mart row is missing fall back to the
        # bulk SQL helpers — keeps the response shape stable while
        # an in-flight ETL backfill is still working through the
        # store.
        uncovered_ids = {p.id for p in project_rows if p.id not in mart_rows}
        if include_stats and uncovered_ids:
            lite_stats = queries.bulk_project_lite_stats(conn)
            cost_by_pid = queries.bulk_project_cost(conn)
        else:
            lite_stats = {}
            cost_by_pid = {}

        # Schema has UNIQUE(provider, slug) — same project used through
        # multiple providers (e.g. claude + codex) yields multiple rows.
        # Merge them so the user-facing list has one entry per slug.
        slug_groups: dict[str, list] = defaultdict(list)
        for p in project_rows:
            slug_groups[p.slug].append(p)

        projects = []
        for slug, group in slug_groups.items():
            primary = max(group, key=lambda p: p.last_modified)
            log_path = _resolve_log_dir(primary.path, slug)
            projects.append(
                {
                    "dir_name": slug,
                    "log_path": log_path,
                    "file_count": sum(session_counts.get(p.id, 0) for p in group),
                    "total_size_mb": _dir_size_mb(log_path),
                    "last_modified": max(p.last_modified for p in group),
                    "first_seen": min(p.first_seen for p in group),
                    "display_name": primary.display_name,
                    "in_cache": False,
                    "url_slug": slug,
                    "stats": None,
                    "provider": primary.provider,
                    "providers": sorted({p.provider for p in group}),
                    "_ids": [p.id for p in group],
                }
            )

        # Campaign #8 — worktree attribution roll-up. Sessions run inside git
        # worktrees log under phantom sibling slugs; fold them into their
        # parent row (default) or annotate them (?include_worktrees=1). This
        # runs BEFORE the sort + page slice below so total_count / has_more
        # stay truthful about the folded list.
        worktree_parent_by_slug = _worktree_parents_from_store(conn)
        if include_worktrees:
            _annotate_worktree_fragments(projects, worktree_parent_by_slug)
        else:
            projects, folded = _fold_worktree_fragments(projects, worktree_parent_by_slug)
            if folded:
                fragment_cost_usd = _fragment_costs_usd(
                    conn,
                    folded,
                    mart_rows=mart_rows,
                    cost_by_pid=cost_by_pid,
                    mart_loaded=include_stats,
                )
                parent_by_slug = {p["dir_name"]: p for p in projects}
                for parent_slug, fragments in folded.items():
                    parent = parent_by_slug[parent_slug]
                    parent["worktree_count"] = len(fragments)
                    parent["worktree_sessions"] = sum(f["file_count"] for f in fragments)
                    parent["worktree_cost"] = sum(fragment_cost_usd[f["dir_name"]] for f in fragments)

        if sort_by == "last_modified":
            projects.sort(key=lambda x: x["last_modified"], reverse=True)
        elif sort_by == "first_seen":
            projects.sort(key=lambda x: x["first_seen"])
        elif sort_by == "size":
            projects.sort(key=lambda x: x["total_size_mb"], reverse=True)
        elif sort_by == "name":
            projects.sort(key=lambda x: x["display_name"])

        # ``total_count`` is the full slug count *before* the page slice so the
        # frontend can size its pager. The per-project ``_stats_for_ids`` pass
        # below runs only over the page slice — that's what keeps the mart
        # fast-path bounded (we never resolve stats for projects off-page).
        total_count = len(projects)
        projects = projects[offset : offset + limit]

        if include_stats:
            for proj in projects:
                proj["stats"] = _stats_for_ids(
                    proj["_ids"],
                    mart_rows=mart_rows,
                    lite_stats=lite_stats,
                    cost_by_pid=cost_by_pid,
                )

        for proj in projects:
            proj.pop("_ids", None)
    finally:
        conn.close()

    currency = active_currency_payload()
    rate = currency["rate_from_usd"]
    if rate != 1.0:
        for proj in projects:
            # ``worktree_cost`` is summed in USD like every other cost field —
            # convert it with the same rate so a folded parent never mixes
            # currencies (it exists even when include_stats is off).
            if "worktree_cost" in proj:
                proj["worktree_cost"] = float(proj["worktree_cost"]) * rate
            if include_stats:
                stats = proj.get("stats")
                if isinstance(stats, dict) and "total_cost" in stats:
                    stats["total_cost"] = float(stats["total_cost"]) * rate

    return {
        "projects": projects,
        "total_count": total_count,
        # Echo the resolved (clamped) page bounds so the frontend can compute
        # the next offset without re-deriving the clamp rules client-side.
        "limit": limit,
        "offset": offset,
        "has_more": offset + limit < total_count,
        "cache_status": {
            "cached_count": 0,
            "total_projects": total_count,
        },
        "currency": currency,
    }


def _resolve_log_dir(path: str | None, slug: str) -> str:
    if path:
        return path
    return str(Path.home() / ".claude" / "projects" / slug)


# ── Campaign #8: worktree fragment detection + roll-up ───────────────────────

_WORKTREE_SLUG_MARKERS = ("--claude-worktrees-", "--worktrees-")


def _is_worktree_slug(slug: str) -> str | None:
    """Map a worktree-session slug to its parent project slug, else ``None``.

    PRIVATE fallback copy — the canonical implementation is
    ``stackunderflow.services.worktrees.is_worktree_slug`` (built on the
    parallel #8 detection branch); kept private here so the two branches
    can't import-collide before the lead reconciles them at integration.

    Claude Code derives the log slug from the session cwd (``/`` → ``-``), so
    a session inside a git worktree logs under a phantom sibling slug:

      ``<parent>--worktrees-<name>``          (checkout under ``…/.worktrees/``)
      ``<parent>--claude-worktrees-<name>``   (checkout under ``…/.claude/worktrees/``)

    The leftmost marker wins so a nested worktree still attributes to the
    root repo slug. The parent prefix and the worktree name must both be
    non-empty.
    """
    best: int | None = None
    for marker in _WORKTREE_SLUG_MARKERS:
        idx = slug.find(marker)
        if idx > 0 and slug[idx + len(marker) :]:
            best = idx if best is None else min(best, idx)
    return slug[:best] if best is not None else None


def _worktree_parents_from_store(conn) -> dict[str, str]:
    """``{slug: parent_slug}`` from the v027 ``projects.worktree_of`` column.

    Feature-detected via ``PRAGMA table_info`` (the same probe
    ``schema._column_exists`` uses) so a pre-v027 store — where the column
    doesn't exist yet — returns ``{}`` and the slug-shape fallback carries
    the classification alone, with no error.
    """
    cols = {row[1] for row in conn.execute("PRAGMA table_info(projects)").fetchall()}
    if "worktree_of" not in cols:
        return {}
    rows = conn.execute(
        "SELECT slug, worktree_of FROM projects WHERE worktree_of IS NOT NULL AND worktree_of != ''"
    ).fetchall()
    return {str(row[0]): str(row[1]) for row in rows}


def _worktree_parent_of(slug: str, parent_by_slug: dict[str, str]) -> str | None:
    """Resolve a slug's worktree parent: v027 attribution first, shape second."""
    return parent_by_slug.get(slug) or _is_worktree_slug(slug)


def _fold_worktree_fragments(
    projects: list[dict],
    parent_by_slug: dict[str, str],
) -> tuple[list[dict], dict[str, list[dict]]]:
    """Partition assembled rows into ``(kept, {parent_slug: fragment_rows})``.

    A row folds only when its resolved parent (a) exists in this listing
    universe (post provider-filter — never fold into a parent that isn't
    listed; unmatched fragments stay visible) and (b) is not itself a
    fragment (otherwise the roll-up would sum into a row that then
    disappears; a chained/cyclic attribution degrades to "stays visible",
    never to lost data).
    """
    listed = {p["dir_name"] for p in projects}
    kept: list[dict] = []
    folded: dict[str, list[dict]] = {}
    for proj in projects:
        slug = proj["dir_name"]
        parent = _worktree_parent_of(slug, parent_by_slug)
        if (
            parent
            and parent != slug
            and parent in listed
            and _worktree_parent_of(parent, parent_by_slug) is None
        ):
            folded.setdefault(parent, []).append(proj)
        else:
            kept.append(proj)
    return kept, folded


def _annotate_worktree_fragments(projects: list[dict], parent_by_slug: dict[str, str]) -> None:
    """``?include_worktrees=1`` path: no folding — badge fragments in place.

    Each fragment row gains ``worktree_of: <parent slug>``. A v027-attributed
    row is annotated even when its parent isn't listed (the attribution is
    authoritative store data; the frontend badges the orphan), while a
    shape-derived match requires a listed parent — the same existence rule
    the fold applies.
    """
    listed = {p["dir_name"] for p in projects}
    for proj in projects:
        slug = proj["dir_name"]
        parent = parent_by_slug.get(slug)
        if parent is None:
            shaped = _is_worktree_slug(slug)
            parent = shaped if shaped in listed else None
        if parent and parent != slug:
            proj["worktree_of"] = parent


def _fragment_costs_usd(
    conn,
    folded: dict[str, list[dict]],
    *,
    mart_rows: dict[int, dict],
    cost_by_pid: dict[int, float],
    mart_loaded: bool,
) -> dict[str, float]:
    """USD cost per fragment row for the parent roll-up — mart-first.

    Reuses ``mart_rows`` / ``cost_by_pid`` when the include_stats pass
    already loaded them; otherwise loads lazily — ``project_mart`` first
    (one indexed scan), then the bulk messages fallback only for fragment
    ids the mart doesn't cover (pre-ETL stores). Fragments with no cost
    data anywhere roll up as 0.0.
    """
    fragments = [frag for group in folded.values() for frag in group]
    need = {pid for frag in fragments for pid in frag["_ids"]}
    if not mart_loaded and need:
        for row in mart_queries.list_project_mart(conn):
            mart_rows.setdefault(int(row["project_id"]), row)
    if any(pid not in mart_rows for pid in need) and not cost_by_pid:
        cost_by_pid = queries.bulk_project_cost(conn)
    costs: dict[str, float] = {}
    for frag in fragments:
        total = 0.0
        for pid in frag["_ids"]:
            mart_row = mart_rows.get(pid)
            if mart_row is not None:
                total += float(mart_row.get("total_cost_usd", 0.0) or 0.0)
            else:
                total += float(cost_by_pid.get(pid, 0.0))
        costs[frag["dir_name"]] = total
    return costs


# Per-(path, mtime) cache so the project list doesn't re-glob the
# filesystem for 188 directories on every list request. mtime-keyed
# entries auto-invalidate when files are added/removed.
_dir_size_cache: dict[tuple[str, float], float] = {}


def _dir_size_mb(log_dir: str) -> float:
    p = Path(log_dir)
    try:
        st = p.stat()
    except OSError:
        return 0.0
    if not Path(log_dir).is_dir():
        return 0.0
    key = (log_dir, st.st_mtime)
    if key in _dir_size_cache:
        return _dir_size_cache[key]
    try:
        total = sum(f.stat().st_size for f in p.glob("*.jsonl"))
    except OSError:
        return 0.0
    mb = round(total / (1024 * 1024), 2)
    _dir_size_cache[key] = mb
    return mb


def _stats_for_ids(
    project_ids: list[int],
    *,
    mart_rows: dict[int, dict],
    lite_stats: dict[int, dict],
    cost_by_pid: dict[int, float],
) -> dict:
    """Resolve per-project stats — mart-first, bulk-SQL fallback.

    For each project id we prefer the materialised ``project_mart`` row
    when present, otherwise fall back to the bulk SQL helpers (PR #65).
    Provider-duplicates of one slug get summed/min'd/max'd via the
    same rules ``_bulk_lite_merge`` already applied so the UI shape is
    independent of the data source.
    """
    pre_mart_ids = [pid for pid in project_ids if pid not in mart_rows]
    mart_present_ids = [pid for pid in project_ids if pid in mart_rows]

    if not mart_present_ids:
        return _bulk_lite_merge(pre_mart_ids, lite_stats, cost_by_pid)

    # mixed case: combine mart rows + lite-stats fallback rows. Both
    # produce the same UI shape so we can sum across them safely.
    parts: list[dict] = []
    for pid in mart_present_ids:
        parts.append(_mart_row_to_stats(mart_rows[pid]))
    if pre_mart_ids:
        parts.append(_bulk_lite_merge(pre_mart_ids, lite_stats, cost_by_pid))

    if len(parts) == 1:
        return parts[0]
    starts = [p["first_message_date"] for p in parts if p["first_message_date"]]
    ends = [p["last_message_date"] for p in parts if p["last_message_date"]]
    return {
        "total_input_tokens": sum(p["total_input_tokens"] for p in parts),
        "total_output_tokens": sum(p["total_output_tokens"] for p in parts),
        "total_cache_read": sum(p["total_cache_read"] for p in parts),
        "total_cache_write": sum(p["total_cache_write"] for p in parts),
        "total_commands": _opt_sum_commands(parts),
        "avg_tokens_per_command": 0,
        "avg_steps_per_command": 0,
        "compact_summary_count": 0,
        "first_message_date": min(starts) if starts else None,
        "last_message_date": max(ends) if ends else None,
        "total_cost": sum(p["total_cost"] for p in parts),
    }


def _mart_row_to_stats(row: dict) -> dict:
    """Project ``project_mart`` row → ProjectStats UI shape.

    ``total_commands`` is the materialised ``user_commands_analyzed`` count
    (v022): ``ProjectMartBuilder`` derives it at build time from the
    project's ``messages`` using the same classifier logic the full
    aggregator runs, so the list view now surfaces a real Commands count
    without the ~750ms ``role='user'`` scan the mart fast-path exists to
    avoid. (Pre-v022 stores whose mart hasn't been rebuilt carry ``0`` via
    the column DEFAULT until the next refresh.)

    Other aggregator-only fields (avg_tokens_per_command, etc.) default to
    zero — same as ``bulk_project_lite_stats``.
    """
    return {
        "total_input_tokens": int(row.get("total_input_tokens", 0) or 0),
        "total_output_tokens": int(row.get("total_output_tokens", 0) or 0),
        "total_cache_read": int(row.get("total_cache_read", 0) or 0),
        "total_cache_write": int(row.get("total_cache_create", 0) or 0),
        "total_commands": int(row.get("total_commands", 0) or 0),
        "avg_tokens_per_command": 0,
        "avg_steps_per_command": 0,
        "compact_summary_count": 0,
        "first_message_date": row.get("first_ts"),
        "last_message_date": row.get("last_ts"),
        "total_cost": float(row.get("total_cost_usd", 0.0) or 0.0),
    }


def _opt_sum_commands(parts: list[dict]) -> int | None:
    """Sum ``total_commands`` across merged parts, tolerating ``None``.

    Mart-backed parts carry ``None`` ("unknown", see :func:`_mart_row_to_stats`)
    while lite-backed parts carry an integer proxy. When every part is
    unknown the merged slug is unknown too (``None`` → UI renders ``-``);
    when at least one part has a real count we sum the known ones so a
    mixed provider-duplicate slug still surfaces what it can.
    """
    known = [p["total_commands"] for p in parts if p.get("total_commands") is not None]
    return sum(known) if known else None


def _bulk_lite_merge(
    project_ids: list[int],
    lite_stats: dict[int, dict],
    cost_by_pid: dict[int, float],
) -> dict:
    """Merge bulk-lite per-pid totals across provider-duplicates of one slug.

    Fallback path for stores where ``project_mart`` hasn't been
    populated yet — mirrors PR #65's contract verbatim so the UI shape
    is stable regardless of which path produces the row.
    """
    parts = [lite_stats[pid] for pid in project_ids if pid in lite_stats]
    if not parts:
        return {
            "total_input_tokens": 0,
            "total_output_tokens": 0,
            "total_cache_read": 0,
            "total_cache_write": 0,
            "total_commands": 0,
            "avg_tokens_per_command": 0,
            "avg_steps_per_command": 0,
            "compact_summary_count": 0,
            "first_message_date": None,
            "last_message_date": None,
            "total_cost": 0.0,
        }
    starts = [p["first_message_date"] for p in parts if p["first_message_date"]]
    ends = [p["last_message_date"] for p in parts if p["last_message_date"]]
    return {
        "total_input_tokens": sum(p["total_input_tokens"] for p in parts),
        "total_output_tokens": sum(p["total_output_tokens"] for p in parts),
        "total_cache_read": sum(p["total_cache_read"] for p in parts),
        "total_cache_write": sum(p["total_cache_write"] for p in parts),
        "total_commands": sum(p["total_commands"] for p in parts),
        "avg_tokens_per_command": 0,
        "avg_steps_per_command": 0,
        "compact_summary_count": 0,
        "first_message_date": min(starts) if starts else None,
        "last_message_date": max(ends) if ends else None,
        "total_cost": sum(cost_by_pid.get(pid, 0.0) for pid in project_ids),
    }


def _merge_stats_for_ui(conn, project_ids: list[int]) -> dict:
    """Sum / max / min ProjectStats across provider-duplicates of one slug.

    Kept for callers that still need full per-project aggregator output
    (none currently — the list endpoint moved to ``_bulk_lite_merge``).
    """
    parts = [_project_stats_for_ui(conn, pid) for pid in project_ids]
    if len(parts) == 1:
        return parts[0]
    total_commands = sum(p["total_commands"] for p in parts)
    starts = [p["first_message_date"] for p in parts if p["first_message_date"]]
    ends = [p["last_message_date"] for p in parts if p["last_message_date"]]
    return {
        "total_input_tokens": sum(p["total_input_tokens"] for p in parts),
        "total_output_tokens": sum(p["total_output_tokens"] for p in parts),
        "total_cache_read": sum(p["total_cache_read"] for p in parts),
        "total_cache_write": sum(p["total_cache_write"] for p in parts),
        "total_commands": total_commands,
        "avg_tokens_per_command": (
            sum(p["total_commands"] * p["avg_tokens_per_command"] for p in parts) / total_commands
            if total_commands
            else 0
        ),
        "avg_steps_per_command": (
            sum(p["total_commands"] * p["avg_steps_per_command"] for p in parts) / total_commands
            if total_commands
            else 0
        ),
        "compact_summary_count": sum(p["compact_summary_count"] for p in parts),
        "first_message_date": min(starts) if starts else None,
        "last_message_date": max(ends) if ends else None,
        "total_cost": sum(p["total_cost"] for p in parts),
    }


def _project_stats_for_ui(conn, project_id: int) -> dict:
    """Flatten aggregator output into the ProjectStats shape the UI expects."""
    _, stats = queries.get_project_stats(conn, project_id=project_id)
    if not stats:
        return {
            "total_input_tokens": 0,
            "total_output_tokens": 0,
            "total_cache_read": 0,
            "total_cache_write": 0,
            "total_commands": 0,
            "avg_tokens_per_command": 0,
            "avg_steps_per_command": 0,
            "compact_summary_count": 0,
            "first_message_date": None,
            "last_message_date": None,
            "total_cost": 0.0,
        }
    overview = stats.get("overview") or {}
    tokens = overview.get("total_tokens") or {}
    ui = stats.get("user_interactions") or {}
    kinds = overview.get("message_types") or {}
    date_range = overview.get("date_range") or {}
    return {
        "total_input_tokens": int(tokens.get("input", 0)),
        "total_output_tokens": int(tokens.get("output", 0)),
        "total_cache_read": int(tokens.get("cache_read", 0)),
        "total_cache_write": int(tokens.get("cache_creation", 0)),
        "total_commands": int(ui.get("user_commands_analyzed", 0)),
        "avg_tokens_per_command": ui.get("avg_tokens_per_command", 0),
        "avg_steps_per_command": ui.get("avg_steps_per_command", 0),
        "compact_summary_count": int(kinds.get("compact_summary", 0)) + int(kinds.get("summary", 0)),
        "first_message_date": date_range.get("start"),
        "last_message_date": date_range.get("end"),
        "total_cost": float(overview.get("total_cost", 0.0)),
    }


@router.get("/api/providers")
async def get_providers():
    """List every provider currently active in the store.

    Powers the dashboard's `FilterBar` chip row. Returns one entry per
    distinct ``projects.provider`` value with project + session counts so
    the UI can render counts inline (the user wants to know "how much
    Cursor data am I about to scope to?" before they click).

    Cheap query — single GROUP BY over the projects table plus a join
    onto sessions for the count column. Empty stores return an empty
    array, never a 500.
    """
    try:
        conn = db.connect(deps.store_path)
        try:
            rows = conn.execute(
                "SELECT projects.provider AS provider, "
                "       COUNT(DISTINCT projects.id) AS project_count, "
                "       COUNT(DISTINCT sessions.id) AS session_count "
                "FROM projects "
                "LEFT JOIN sessions ON sessions.project_id = projects.id "
                "GROUP BY projects.provider "
                "ORDER BY project_count DESC"
            ).fetchall()
        finally:
            conn.close()
        providers = [
            {
                "provider": (r["provider"] or "unknown").lower(),
                "project_count": int(r["project_count"] or 0),
                "session_count": int(r["session_count"] or 0),
            }
            for r in rows
        ]
        return JSONResponse({"providers": providers})
    except Exception as e:
        return JSONResponse(
            {"providers": [], "error": f"Failed to list providers: {str(e)}"},
            status_code=500,
        )


@router.get("/api/global-stats")
async def get_global_stats():
    """Aggregated statistics across all projects, backed by the session store.

    The (mart-backed, ~10ms) store query + currency read are blocking sync
    work, so they run in a worker thread (``run_in_threadpool``) — the event
    loop keeps serving other requests instead of stalling on the scan.
    """
    try:
        payload = await run_in_threadpool(_compute_global_stats)
        return JSONResponse(payload)
    except Exception as e:
        return JSONResponse(
            {"error": f"Failed to get global stats: {str(e)}"},
            status_code=500,
        )


def _compute_global_stats() -> dict:
    """Blocking body of ``GET /api/global-stats`` — runs in a worker thread.

    Reads the cross-project stats from the store, converts every USD cost
    figure into the active display currency, and stamps on the ``currency``
    + ``config`` blocks the Overview expects.
    """
    conn = db.connect(deps.store_path)
    try:
        stats = queries.get_global_stats(conn)
    finally:
        conn.close()

    # The store records cost in USD; convert every cost figure into the
    # active display currency (and ship the currency block) so the Overview
    # never renders USD magnitudes under a € / £ symbol — parity with the
    # project-list + cost-data routes.
    currency = active_currency_payload()
    rate = currency["rate_from_usd"]
    if rate != 1.0:
        _convert_global_stats_costs(stats, rate)
    stats["currency"] = currency

    stats["config"] = {"max_date_range_days": deps.config.get("max_date_range_days")}
    return stats


def _convert_global_stats_costs(stats: dict, rate: float) -> None:
    """Scale every USD cost figure in the global-stats payload by ``rate``.

    Touches the three cost-bearing shapes the Overview reads —
    ``models[*].cost``, ``daily_costs[*].cost`` and the nested
    ``daily_costs[*].by_model[*]`` — and leaves token counts, message
    counts and dates untouched. (``cost.py``'s ``_convert_in_place`` keys on
    a fixed set of field *names*, so it would miss the ``by_model`` leaves
    whose keys are model ids, not ``"cost"`` — hence this purpose-built
    walker.) Mutates ``stats`` in place.
    """
    for m in stats.get("models", {}).values():
        if isinstance(m, dict) and "cost" in m:
            m["cost"] = float(m["cost"]) * rate
    for bucket in stats.get("daily_costs", []):
        if not isinstance(bucket, dict):
            continue
        if "cost" in bucket:
            bucket["cost"] = float(bucket["cost"]) * rate
        by_model = bucket.get("by_model")
        if isinstance(by_model, dict):
            for key in list(by_model):
                by_model[key] = float(by_model[key]) * rate
