"""Guard: no route module may call a full-table messages aggregate unscoped.

The bulk helpers in ``store/queries.py`` GROUP BY over the partitioned
``messages`` view. Called without an explicit ``project_ids`` scope from a
request path, a single mart-less project turns every ``include_stats``
request into a full scan of all 16 partitions — >180s and a wedged server
on a real 382K-message store, the exact failure fixed alongside this guard.
The helpers deliberately keep ``None``-means-all for non-route callers
(tests, ad-hoc analysis), so the contract "routes always pass a scope"
lives here as a test rather than in the signature.

AST-based: every call to a name in ``_SCOPED_HELPERS`` anywhere under
``stackunderflow/routes/`` must carry a ``project_ids`` keyword — however it
is reached (``queries.bulk_project_cost(...)`` or a bare imported name). A
helper with the same full-scan shape added later belongs in the set. Import
aliasing (``import ... as bpc``) would evade the walk; none exists, and the
existence check below keeps the guarded names honest against renames.

Runtime companions: ``test_projects_bulk_scoping.py`` spies the live route
and pins that the ids passed are exactly the mart-uncovered set.
"""

from __future__ import annotations

import ast
from pathlib import Path

import pytest

from stackunderflow.store import queries

ROUTES_DIR = Path(__file__).resolve().parents[3] / "stackunderflow" / "routes"

# Helpers whose unscoped form is a full scan of the messages view. Routes
# must never call them without an explicit project_ids scope.
_SCOPED_HELPERS = frozenset({"bulk_project_lite_stats", "bulk_project_cost"})


def test_guarded_helper_names_still_exist() -> None:
    """A rename must update ``_SCOPED_HELPERS`` — not silently disarm it."""
    for name in _SCOPED_HELPERS:
        assert callable(getattr(queries, name, None)), (
            f"queries.{name} is gone — update _SCOPED_HELPERS in this guard "
            f"to the helper's new name so route callsites stay covered"
        )


def _helper_calls(tree: ast.AST):
    for node in ast.walk(tree):
        if not isinstance(node, ast.Call):
            continue
        fn = node.func
        name = fn.attr if isinstance(fn, ast.Attribute) else getattr(fn, "id", None)
        if name in _SCOPED_HELPERS:
            yield name, node


@pytest.mark.parametrize(
    "path", sorted(ROUTES_DIR.glob("*.py")), ids=lambda p: p.name
)
def test_route_calls_to_bulk_aggregates_are_scoped(path: Path) -> None:
    tree = ast.parse(path.read_text(encoding="utf-8"), filename=str(path))
    offenders = [
        f"{path.name}:{node.lineno} — {name}() without project_ids="
        for name, node in _helper_calls(tree)
        if not any(kw.arg == "project_ids" for kw in node.keywords)
    ]
    assert not offenders, (
        "Unscoped full-table aggregate reachable from a route. Pass the "
        "explicit id set the response actually needs (see the wedge notes on "
        "the helpers in store/queries.py):\n  " + "\n  ".join(offenders)
    )
