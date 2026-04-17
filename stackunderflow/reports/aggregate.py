"""Cross-project aggregation driven by Scope + include/exclude filters.

Walks the project list, runs the pipeline per project, filters `daily_stats`
entries by the Scope, sums the filtered days, and returns a dict ready to
be rendered. The pipeline function is injected via a module-level indirection
so tests can patch it without importing the real pipeline.
"""

from __future__ import annotations

import logging

from stackunderflow.pipeline import process as _run_pipeline  # re-exported for test patching
from stackunderflow.reports.scope import Scope

__all__ = ["build_report"]

_log = logging.getLogger(__name__)


def build_report(
    projects: list[dict],
    *,
    scope: Scope,
    include: list[str] | None,
    exclude: list[str] | None,
) -> dict:
    """Aggregate stats across the given projects.

    Args:
        projects: list of project metadata dicts (from `list_projects()`).
        scope: date-range window; unbounded scope includes every daily stat.
        include: if set, only these dir_names are processed.
        exclude: if set, these dir_names are skipped.

    Returns:
        Dict with total_cost, total_messages, total_sessions, by_project (sorted desc).
    """
    if include is not None:
        projects = [p for p in projects if p["dir_name"] in include]
    if exclude is not None:
        projects = [p for p in projects if p["dir_name"] not in exclude]

    per_project: list[dict] = []
    total_cost = 0.0
    total_messages = 0
    total_sessions = 0

    for p in projects:
        try:
            _messages, stats = _run_pipeline(p["log_path"])
        except Exception as exc:  # noqa: BLE001
            # Skip projects that fail to process; the pipeline itself logs the
            # underlying warning. We don't crash the whole report over one bad
            # project, but we do surface which project was skipped.
            _log.warning("skipping project %s (%s)", p.get("dir_name", "?"), exc)
            continue

        daily = stats.get("daily_stats", {})
        filtered = {day: d for day, d in daily.items() if _day_in_scope(day, scope)}

        project_cost = 0.0
        project_messages = 0
        project_sessions = 0
        for d in filtered.values():
            project_cost += d.get("cost", {}).get("total", 0.0)
            project_messages += d.get("messages", 0)
            project_sessions += d.get("sessions", 0)

        per_project.append({
            "name": p["dir_name"],
            "cost": project_cost,
            "messages": project_messages,
            "sessions": project_sessions,
        })
        total_cost += project_cost
        total_messages += project_messages
        total_sessions += project_sessions

    per_project.sort(key=lambda row: row["cost"], reverse=True)

    return {
        "scope_label": scope.label,
        "total_cost": total_cost,
        "total_messages": total_messages,
        "total_sessions": total_sessions,
        "by_project": per_project,
    }


def _day_in_scope(day_key: str, scope: Scope) -> bool:
    """`day_key` is a YYYY-MM-DD string from daily_stats.

    When the scope is unbounded on a side, that side always matches. Otherwise
    we compare against the date portion of the scope bound.
    """
    if scope.since is None and scope.until is None:
        return True
    if scope.since is not None and day_key < scope.since[:10]:
        return False
    if scope.until is not None and day_key > scope.until[:10]:
        return False
    return True
