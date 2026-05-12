"""Opt-in hybrid capture — StackUnderflow ↔ Claude Code lifecycle hooks.

Claude Code can run a command on certain lifecycle events. When (and *only*
when) the user runs ``stackunderflow hooks install``, we register four:

============================  ===========================  ==================
Claude Code event             hook id                       captured as
============================  ===========================  ==================
``PostToolUse`` (Bash)        ``stackunderflow-post-tool-use``  ``failure`` (non-zero exit)
``UserPromptSubmit``          ``stackunderflow-user-prompt``    ``correction`` (matched the heuristic)
``Stop``                      ``stackunderflow-stop``           ``boundary`` (turn end + session totals)
``PreCompact``                ``stackunderflow-pre-compact``    ``snapshot`` (pre-compaction)
============================  ===========================  ==================

Each fire becomes a row in ``captured_events`` (migration ``v010``). The
stored payload is metadata only by default — never the raw prompt or tool
stdout/stderr — unless the user installed with ``--capture-content``.

This gives outcome-aware discovery (spec 01) a deterministic failure /
correction feed; it stays optional, and spec 01 falls back to its transcript
heuristic on hook-less installs, so there is no producer dependency between
the two.

Public surface (also the CLI ``stackunderflow hooks {install,uninstall,status,repair,run}``):

    from stackunderflow import hooks
    hooks.install(scope="project", dry_run=False, capture_content=False) -> InstallReport
    hooks.uninstall(scope="project")                                     -> UninstallReport
    hooks.status(scope=None)                                             -> dict
    hooks.repair(scope="project", dry_run=False)                         -> RepairReport
    hooks.run(hook_id, payload)                                          -> int   # called by Claude Code

Hard constraints (see the spec): opt-in only · backup before mutation ·
``repair`` from day one · never delete other tools' hooks · scope explicit ·
portable commands.
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
    "resolve_settings_path",
    "ensure_captured_events_table",
]
