"""StackUnderflow — a knowledge base for your AI coding sessions.

Top-level API::

    import stackunderflow

    # List all Claude Code projects on this machine
    projects = stackunderflow.list_projects()

Submodule access::

    from stackunderflow.infra.discovery import locate_logs, project_metadata, ProjectInfo
    from stackunderflow.settings import Settings
"""

from stackunderflow.__version__ import __version__
from stackunderflow.infra.discovery import project_metadata as list_projects

__all__ = [
    "__version__",
    "list_projects",
]
