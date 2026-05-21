"""StackUnderflow: offline, local-first observability toolkit for AI coding agents.

Ingests and indexes session logs from 17 coding agent providers to surface cost
analytics, interactive session playback (with step-by-step filesystem
reconstruction), and a searchable knowledge base that both developers
and agents can query to learn from past decisions and failures. All offline.

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
