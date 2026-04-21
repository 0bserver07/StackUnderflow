"""StackUnderflow: the local observability for your coding agents.

Search, replay, and analyse every session, all offline. Starts with
Claude Code; adapters for more coding agents are on the way.

Top-level API::

    import stackunderflow

    # List all coding-agent projects discovered on this machine
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
