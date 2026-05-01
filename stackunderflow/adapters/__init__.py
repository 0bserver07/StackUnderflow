"""Source adapters for session data.

Each adapter turns a specific tool's on-disk session format (Claude Code's
JSONL, Codex's rollout JSONL, etc.) into a stream of normalised `Record`s.
The ingest layer drives adapters; route handlers and reports only ever see
store rows.

Default-on adapters (always registered):

  - Claude Code (per-project JSONL + legacy ~/.claude/history.jsonl)
  - Codex (rollout JSONL)
  - Cursor (vscdb) — promoted out of beta in v0.7.0
  - Cline (VS Code globalStorage) — promoted out of beta in v0.7.0

Beta adapters are gated by environment variables (default: off). The 12 below
remain opt-in pending broader real-world validation:

  STACKUNDERFLOW_BETA_KILOCODE=1       # opt into the KiloCode adapter (Cline parser)
  STACKUNDERFLOW_BETA_ROOCODE=1        # opt into the Roo Code adapter (Cline parser)
  STACKUNDERFLOW_BETA_OPENCODE=1       # opt into the OpenCode (SQLite) adapter
  STACKUNDERFLOW_BETA_CURSOR_AGENT=1   # opt into the Cursor Agent (transcripts + SQLite) adapter
  STACKUNDERFLOW_BETA_QWEN=1           # opt into the Qwen (jsonl) adapter
  STACKUNDERFLOW_BETA_GEMINI=1         # opt into the Gemini (jsonl/json) adapter
  STACKUNDERFLOW_BETA_COPILOT=1        # opt into the GitHub Copilot adapter
                                       #   (legacy + VS Code transcript JSONL)
  STACKUNDERFLOW_BETA_CODEIUM=1        # opt into the Codeium adapter (discovery
                                       #   stub — protobuf decoding deferred)
  STACKUNDERFLOW_BETA_CONTINUE=1       # opt into the Continue IDE adapter
                                       #   (defensive SQLite parser)
  STACKUNDERFLOW_BETA_DROID=1          # opt into the Droid (Factory) adapter
  STACKUNDERFLOW_BETA_KIRO=1           # opt into the Kiro (kiroagent) adapter
  STACKUNDERFLOW_BETA_OPENCLAW=1       # opt into the OpenClaw (multi-base) adapter
  STACKUNDERFLOW_BETA_PI=1             # opt into the Pi+OMP shared adapter
"""

import os

from .base import Record, SessionRef, SourceAdapter

__all__ = ["Record", "SessionRef", "SourceAdapter", "registered", "register"]

_registry: list[SourceAdapter] = []


def register(adapter: SourceAdapter) -> None:
    """Add an adapter to the global registry."""
    _registry.append(adapter)


def registered() -> list[SourceAdapter]:
    """Return the current registry. The ingest layer iterates this."""
    return list(_registry)


def _beta_enabled(name: str) -> bool:
    """Return True when the matching ``STACKUNDERFLOW_BETA_<NAME>`` env
    var is set to a truthy value."""
    val = os.environ.get(f"STACKUNDERFLOW_BETA_{name.upper()}", "")
    return val.strip().lower() in ("1", "true", "yes", "on")


from .claude import ClaudeAdapter as _ClaudeAdapter  # noqa: E402
from .cline import ClineAdapter as _ClineAdapter  # noqa: E402
from .codex import CodexAdapter as _CodexAdapter  # noqa: E402
from .cursor import CursorAdapter as _CursorAdapter  # noqa: E402

register(_ClaudeAdapter())
register(_CodexAdapter())

# Cursor (vscdb). macOS-only for v1; spec §3.1.
register(_CursorAdapter())

# Cline (VS Code globalStorage). macOS-only for v1; spec §3.2.
register(_ClineAdapter())

# Beta: KiloCode (VS Code globalStorage, Cline parser reuse). Off by
# default — set STACKUNDERFLOW_BETA_KILOCODE=1 to enable. macOS-only for v1.
if _beta_enabled("KILOCODE"):
    from .cline import KiloCodeAdapter as _KiloCodeAdapter  # noqa: E402

    register(_KiloCodeAdapter())

# Beta: Roo Code (VS Code globalStorage, Cline parser reuse). Off by
# default — set STACKUNDERFLOW_BETA_ROOCODE=1 to enable. macOS-only for v1.
if _beta_enabled("ROOCODE"):
    from .cline import RooCodeAdapter as _RooCodeAdapter  # noqa: E402

    register(_RooCodeAdapter())

# Beta: OpenCode (SQLite). Off by default — set
# STACKUNDERFLOW_BETA_OPENCODE=1 to enable. OS-portable via XDG_DATA_HOME.
if _beta_enabled("OPENCODE"):
    from .opencode import OpenCodeAdapter as _OpenCodeAdapter  # noqa: E402

    register(_OpenCodeAdapter())

# Beta: Cursor Agent (text/JSONL transcripts + SQLite metadata). Off by
# default — set STACKUNDERFLOW_BETA_CURSOR_AGENT=1 to enable. macOS-only for v1.
if _beta_enabled("CURSOR_AGENT"):
    from .cursor_agent import CursorAgentAdapter as _CursorAgentAdapter  # noqa: E402

    register(_CursorAgentAdapter())

# Beta: Qwen (JSONL). Off by default — set STACKUNDERFLOW_BETA_QWEN=1
# to enable. macOS-only for v1.
if _beta_enabled("QWEN"):
    from .qwen import QwenAdapter as _QwenAdapter  # noqa: E402

    register(_QwenAdapter())

# Beta: Gemini (JSONL or single JSON). Off by default — set
# STACKUNDERFLOW_BETA_GEMINI=1 to enable. macOS-only for v1.
if _beta_enabled("GEMINI"):
    from .gemini import GeminiAdapter as _GeminiAdapter  # noqa: E402

    register(_GeminiAdapter())

# Beta: Copilot (legacy ~/.copilot + VS Code transcripts). Off by default
# — set STACKUNDERFLOW_BETA_COPILOT=1 to enable. macOS-only for v1.
if _beta_enabled("COPILOT"):
    from .copilot import CopilotAdapter as _CopilotAdapter  # noqa: E402

    register(_CopilotAdapter())

# Beta: Codeium (discovery stub — see module docstring). Off by default
# — set STACKUNDERFLOW_BETA_CODEIUM=1 to enable. Yields nothing today.
if _beta_enabled("CODEIUM"):
    from .codeium import CodeiumAdapter as _CodeiumAdapter  # noqa: E402

    register(_CodeiumAdapter())

# Beta: Continue (defensive SQLite parser). Off by default — set
# STACKUNDERFLOW_BETA_CONTINUE=1 to enable. Yields nothing on empty
# state (most installs); local-inventory.md §13.
if _beta_enabled("CONTINUE"):
    from .continue_adapter import ContinueAdapter as _ContinueAdapter  # noqa: E402

    register(_ContinueAdapter())

# Beta: Droid (Factory). Off by default — set STACKUNDERFLOW_BETA_DROID=1
# to enable. Honors $FACTORY_DIR. Session-level token totals are
# distributed evenly across detected assistant messages.
if _beta_enabled("DROID"):
    from .droid import DroidAdapter as _DroidAdapter  # noqa: E402

    register(_DroidAdapter())

# Beta: Kiro (kiroagent). Off by default — set STACKUNDERFLOW_BETA_KIRO=1
# to enable. macOS-only for v1; tokens are estimated from content
# length and Records carry ``raw["cost_source"] = "estimated"``.
if _beta_enabled("KIRO"):
    from .kiro import KiroAdapter as _KiroAdapter  # noqa: E402

    register(_KiroAdapter())

# Beta: OpenClaw (and rebrand cousins). Off by default — set
# STACKUNDERFLOW_BETA_OPENCLAW=1 to enable. Walks four candidate base
# directories: ~/.openclaw, ~/.clawdbot, ~/.moltbot, ~/.moldbot.
if _beta_enabled("OPENCLAW"):
    from .openclaw import OpenClawAdapter as _OpenClawAdapter  # noqa: E402

    register(_OpenClawAdapter())

# Beta: Pi + OMP (shared format, two roots). Off by default — set
# STACKUNDERFLOW_BETA_PI=1 to enable both. One adapter scans both
# ~/.pi/agent/sessions and ~/.omp/agent/sessions.
if _beta_enabled("PI"):
    from .pi import PiAdapter as _PiAdapter  # noqa: E402

    register(_PiAdapter())
