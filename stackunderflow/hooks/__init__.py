"""Opt-in hybrid capture + context injection — StackUnderflow ↔ Claude Code hooks.

Claude Code can run a command on certain lifecycle events. When (and *only*
when) the user runs ``stackunderflow hooks install``, we register hooks of two
families.

**Capture** (always installed by ``install``) — these RECORD events:

============================  ===============================  ==================
Claude Code event             hook id                          captured as
============================  ===============================  ==================
``PostToolUse`` (Bash)        ``stackunderflow-post-tool-use``  ``failure`` (non-zero exit)
``UserPromptSubmit``          ``stackunderflow-user-prompt``    ``correction`` (matched the heuristic)
``Stop``                      ``stackunderflow-stop``           ``boundary`` (turn end + session totals)
``PreCompact``                ``stackunderflow-pre-compact``    ``snapshot`` (pre-compaction)
============================  ===============================  ==================

Each fire becomes a row in ``captured_events`` (migration ``v010``). The stored
payload is metadata only by default — never the raw prompt or tool
stdout/stderr — unless the user installed with ``--capture-content``.

**Injection** (installed only with ``install --inject``) — these READ the store
and feed a small, token-bounded digest back into the live agent:

================================  ====================================  ===================
Claude Code event                 hook id                               injects
================================  ====================================  ===================
``SessionStart``                  ``stackunderflow-inject-session-start``  a project digest
``UserPromptSubmit``              ``stackunderflow-inject-user-prompt``    a matching past decision
``PreToolUse`` (Edit/Write/…)     ``stackunderflow-inject-pre-tool-use``   the file's failure modes
================================  ====================================  ===================

Capture gives outcome-aware discovery (spec 01) a deterministic failure /
correction feed; injection closes the loop the other way. Both stay optional,
and both are independently opt-in.

Public surface (also the CLI ``stackunderflow hooks {install,uninstall,status,repair,run}``):

    from stackunderflow import hooks
    hooks.install(scope="project", dry_run=False, capture_content=False, inject=False) -> InstallReport
    hooks.uninstall(scope="project")                                                  -> UninstallReport
    hooks.status(scope=None)                                                          -> dict
    hooks.repair(scope="project", dry_run=False)                                      -> RepairReport
    hooks.run(hook_id, payload)                                                       -> int   # called by Claude Code
    hooks.build_injection(hook_id, payload)                                           -> str   # injection envelope

Hard constraints (see the spec): opt-in only · backup before mutation ·
``repair`` from day one · never delete other tools' hooks · scope explicit ·
portable commands · an injection hook never disrupts the agent.
"""

from __future__ import annotations

from stackunderflow.hooks._install import (
    InstallReport,
    UninstallReport,
    install,
    resolve_settings_path,
    status,
    uninstall,
)
from stackunderflow.hooks._repair import RepairReport, repair
from stackunderflow.hooks.handlers import HOOK_IDS, ensure_captured_events_table, run
from stackunderflow.hooks.inject import build_injection

__all__ = [
    "HOOK_IDS",
    "InstallReport",
    "UninstallReport",
    "RepairReport",
    "install",
    "uninstall",
    "status",
    "repair",
    "run",
    "build_injection",
    "resolve_settings_path",
    "ensure_captured_events_table",
]
