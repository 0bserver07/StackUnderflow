"""StackUnderflow: the local observability for your coding agents.

Search, replay, and analyse every session, all offline. Starts with
Claude Code; adapters for more coding agents are on the way.

Top-level API::

    import stackunderflow

    # Every project the local store knows about, across all providers.
    projects = stackunderflow.list_projects()
    # [{"slug": ..., "provider": ..., "display_name": ..., ...}, ...]

    # Pipeline-formatted messages + statistics for one project.
    messages, stats = stackunderflow.process(projects[0]["slug"])

The public functions read from the SQLite store at
``~/.stackunderflow/store.db``. If the store does not exist yet (fresh
install, no ingest run), ``list_projects()`` returns an empty list.

Submodule access::

    from stackunderflow.infra.discovery import locate_logs, project_metadata, ProjectInfo
    from stackunderflow.settings import Settings
    from stackunderflow.store import db, queries
"""

from __future__ import annotations

from stackunderflow.__version__ import __version__
from stackunderflow.api import list_projects, list_sessions, process

__all__ = [
    "__version__",
    "list_projects",
    "list_sessions",
    "process",
]
