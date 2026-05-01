"""Source adapters for session data.

Each adapter turns a specific tool's on-disk session format (Claude Code's
JSONL, Codex's rollout JSONL, etc.) into a stream of normalised `Record`s.
The ingest layer drives adapters; route handlers and reports only ever see
store rows.

Beta adapters are gated by environment variables (default: off):

  STACKUNDERFLOW_BETA_CURSOR=1         # opt into the Cursor (vscdb) adapter
  STACKUNDERFLOW_BETA_CLINE=1          # opt into the Cline (vscode globalStorage) adapter
  STACKUNDERFLOW_BETA_KILOCODE=1       # opt into the KiloCode adapter (Cline parser)
  STACKUNDERFLOW_BETA_ROOCODE=1        # opt into the Roo Code adapter (Cline parser)
  STACKUNDERFLOW_BETA_OPENCODE=1       # opt into the OpenCode (SQLite) adapter
  STACKUNDERFLOW_BETA_CURSOR_AGENT=1   # opt into the Cursor Agent (transcripts + SQLite) adapter
  STACKUNDERFLOW_BETA_QWEN=1           # opt into the Qwen (jsonl) adapter
  STACKUNDERFLOW_BETA_GEMINI=1         # opt into the Gemini (jsonl/json) adapter
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
from .codex import CodexAdapter as _CodexAdapter  # noqa: E402

register(_ClaudeAdapter())
register(_CodexAdapter())

# Beta: Cursor (vscdb). Off by default — set STACKUNDERFLOW_BETA_CURSOR=1
# to enable. macOS-only for v1; spec §3.1.
if _beta_enabled("CURSOR"):
    from .cursor import CursorAdapter as _CursorAdapter  # noqa: E402

    register(_CursorAdapter())

# Beta: Cline (VS Code globalStorage). Off by default — set
# STACKUNDERFLOW_BETA_CLINE=1 to enable. macOS-only for v1; spec §3.2.
if _beta_enabled("CLINE"):
    from .cline import ClineAdapter as _ClineAdapter  # noqa: E402

    register(_ClineAdapter())

# Beta: KiloCode (VS Code globalStorage, Cline parser reuse). Off by
# default — set STACKUNDERFLOW_BETA_KILOCODE=1 to enable. macOS-only for
# v1; codeburn-catalog §8.
if _beta_enabled("KILOCODE"):
    from .cline import KiloCodeAdapter as _KiloCodeAdapter  # noqa: E402

    register(_KiloCodeAdapter())

# Beta: Roo Code (VS Code globalStorage, Cline parser reuse). Off by
# default — set STACKUNDERFLOW_BETA_ROOCODE=1 to enable. macOS-only for
# v1; codeburn-catalog §14.
if _beta_enabled("ROOCODE"):
    from .cline import RooCodeAdapter as _RooCodeAdapter  # noqa: E402

    register(_RooCodeAdapter())

# Beta: OpenCode (SQLite). Off by default — set
# STACKUNDERFLOW_BETA_OPENCODE=1 to enable. OS-portable via XDG_DATA_HOME.
# codeburn-catalog §11.
if _beta_enabled("OPENCODE"):
    from .opencode import OpenCodeAdapter as _OpenCodeAdapter  # noqa: E402

    register(_OpenCodeAdapter())

# Beta: Cursor Agent (text/JSONL transcripts + SQLite metadata). Off by
# default — set STACKUNDERFLOW_BETA_CURSOR_AGENT=1 to enable. macOS-only
# for v1; codeburn-catalog §5.
if _beta_enabled("CURSOR_AGENT"):
    from .cursor_agent import CursorAgentAdapter as _CursorAgentAdapter  # noqa: E402

    register(_CursorAgentAdapter())

# Beta: Qwen (JSONL). Off by default — set STACKUNDERFLOW_BETA_QWEN=1
# to enable. macOS-only for v1; codeburn-catalog §13.
if _beta_enabled("QWEN"):
    from .qwen import QwenAdapter as _QwenAdapter  # noqa: E402

    register(_QwenAdapter())

# Beta: Gemini (JSONL or single JSON). Off by default — set
# STACKUNDERFLOW_BETA_GEMINI=1 to enable. macOS-only for v1;
# codeburn-catalog §7.
if _beta_enabled("GEMINI"):
    from .gemini import GeminiAdapter as _GeminiAdapter  # noqa: E402

    register(_GeminiAdapter())
