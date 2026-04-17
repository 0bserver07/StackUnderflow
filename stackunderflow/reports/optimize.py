"""Waste-finding heuristic for the CLI `optimize` command.

Surface projects where users had to repeatedly push back on the assistant —
these sessions are the cheapest to cite as "stop using X for Y" or "try a
different model for this workload."

We lean on Plan B's `resolution_status='looped'` Q&A flag. A project with
many looped pairs is a project where the assistant often failed first try.
"""

from __future__ import annotations

from stackunderflow.reports.scope import Scope
from stackunderflow.services.qa_service import QAService

__all__ = ["find_waste"]


def _qa_service_factory() -> QAService:
    """Indirection point for tests to swap in a throwaway QAService."""
    return QAService()


def find_waste(
    projects: list[dict],
    *,
    scope: Scope,
    include: list[str] | None = None,
    exclude: list[str] | None = None,
) -> list[dict]:
    """Rank projects by number of looped Q&A pairs.

    Returns a list of dicts: `{project, looped_pairs, sample_questions}`.
    Projects with zero looped pairs are omitted.
    """
    if include is not None:
        projects = [p for p in projects if p["dir_name"] in include]
    if exclude is not None:
        projects = [p for p in projects if p["dir_name"] not in exclude]

    svc = _qa_service_factory()

    rows: list[dict] = []
    for p in projects:
        result = svc.list_qa(
            project=p["dir_name"],
            resolution_status="looped",
            date_from=scope.since,
            date_to=scope.until,
            per_page=100,
        )
        if result["total"] == 0:
            continue
        samples = [r["question_text"][:120] for r in result["results"][:3]]
        rows.append({
            "project": p["dir_name"],
            "looped_pairs": result["total"],
            "sample_questions": samples,
        })

    rows.sort(key=lambda r: r["looped_pairs"], reverse=True)
    return rows
