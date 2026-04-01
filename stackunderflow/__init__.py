"""StackUnderflow — a knowledge base for your AI coding sessions.

Top-level API::

    import stackunderflow

    # List all Claude Code projects on this machine
    projects = stackunderflow.list_projects()

    # Process a project's logs → (messages, statistics)
    messages, stats = stackunderflow.process(projects[0]["log_path"])

Submodule access::

    from stackunderflow.pipeline import process, reader, dedup, classifier
    from stackunderflow.infra.discovery import locate_logs, project_metadata, ProjectInfo
    from stackunderflow.infra.cache import TieredCache
    from stackunderflow.settings import Settings
"""

from stackunderflow.__version__ import __version__
from stackunderflow.infra.discovery import project_metadata as list_projects
from stackunderflow.pipeline import process

__all__ = [
    "__version__",
    "process",
    "list_projects",
]
