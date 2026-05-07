"""Cross-session discovery service.

Pure read-only service that answers three "have I been here before?"
questions for the MCP and CLI surfaces:

* :func:`find_sessions_in_path` — sessions for a given project path
  (or any ancestor)
* :func:`find_sessions_touching_file` — sessions whose tool calls
  reference a particular file
* :func:`search_past_decisions` — sessions whose message content
  matches a free-text query

The full implementation is co-owned with ``cli/discovery``-shaped
commands and is delivered by the sibling ``discovery-service-cli``
branch. This module declares the **contract** the MCP server (and CLI)
codes against — the dataclass shape and three function signatures —
so the two branches merge cleanly:

>>> from stackunderflow.services.discovery import (
...     SessionMatch,
...     find_sessions_in_path,
...     find_sessions_touching_file,
...     search_past_decisions,
... )

The contract surface:

``SessionMatch`` — frozen dataclass with::

    session_id      str
    project_slug    str
    project_path    str | None
    provider        str
    first_ts        str | None      # ISO-8601
    last_ts         str | None      # ISO-8601
    message_count   int
    cost_usd        float
    snippet         str | None      # short context excerpt, may be None

``find_sessions_in_path(conn, path, *, since=None, limit=20, provider=None)``
``find_sessions_touching_file(conn, file_path, *, limit=20, mode="any")``
``search_past_decisions(conn, query, *, project=None, since=None, limit=20)``

All three return ``list[SessionMatch]`` ordered by ``last_ts`` DESC and
take a live ``sqlite3.Connection`` as the first positional argument so
callers control connection lifecycle.
"""

from __future__ import annotations

import sqlite3
from dataclasses import dataclass

__all__ = [
    "SessionMatch",
    "find_sessions_in_path",
    "find_sessions_touching_file",
    "search_past_decisions",
]


@dataclass(frozen=True, slots=True)
class SessionMatch:
    """One session match returned by the discovery service.

    Carries enough provenance for an MCP/CLI consumer to either show
    a row or pivot into ``session_query`` for the full event stream.
    """

    session_id: str
    project_slug: str
    project_path: str | None
    provider: str
    first_ts: str | None
    last_ts: str | None
    message_count: int
    cost_usd: float
    snippet: str | None


# ── contract stubs ───────────────────────────────────────────────────────────
#
# The real implementations live on the ``discovery-service-cli`` branch
# and replace these stubs on merge. Keeping the stubs here lets the MCP
# layer import the contract today and lets tests mock the three
# functions without an import-time error.


def _not_yet() -> list[SessionMatch]:
    raise NotImplementedError(
        "stackunderflow.services.discovery is provided by the "
        "discovery-service-cli branch; this stub exists only to declare "
        "the import contract for the MCP layer.",
    )


def find_sessions_in_path(
    conn: sqlite3.Connection,
    path: str,
    *,
    since: str | None = None,
    limit: int = 20,
    provider: str | None = None,
) -> list[SessionMatch]:
    """Return sessions whose project path is ``path`` or any ancestor.

    See module docstring for the contract.
    """
    del conn, path, since, limit, provider
    return _not_yet()


def find_sessions_touching_file(
    conn: sqlite3.Connection,
    file_path: str,
    *,
    limit: int = 20,
    mode: str = "any",
) -> list[SessionMatch]:
    """Return sessions whose tool calls reference ``file_path``.

    See module docstring for the contract.
    """
    del conn, file_path, limit, mode
    return _not_yet()


def search_past_decisions(
    conn: sqlite3.Connection,
    query: str,
    *,
    project: str | None = None,
    since: str | None = None,
    limit: int = 20,
) -> list[SessionMatch]:
    """Return sessions whose message content matches ``query``.

    See module docstring for the contract.
    """
    del conn, query, project, since, limit
    return _not_yet()
