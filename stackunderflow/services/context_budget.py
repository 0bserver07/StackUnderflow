"""Per-session context-budget estimator.

Every AI coding session pays a "context tax" — the system prompt, registered
MCP server descriptions, available skills, agent definitions, and memory
files all consume tokens before the user types anything. This service walks
the visible config files and produces a structured estimate of the per-turn
overhead.

The estimator is deliberately simple. It does **not** call out to a model
API to count tokens precisely; instead it uses the well-known
``len(text) // 4`` heuristic (one token ≈ 4 characters of English prose).
The heuristic is documented prominently in CLI / API output so users know
the numbers are rough — useful for spotting bloat, not for billing.

Sources inspected:

* **System prompt** — fixed estimate (Claude Code's default sits around
  3000 tokens; the constant is exposed for callers that want to override).
* **Project ``CLAUDE.md``** at the project root.
* **Global ``~/.claude/CLAUDE.md``**.
* **MCP servers** — the ``mcpServers`` map in ``~/.claude.json`` plus
  any ``.claude/settings.json`` at the project root. For each server we
  charge a base 200 tokens for the server description plus 50 tokens per
  declared tool (or a flat 200 when tool counts aren't statically known).
* **Skills** — every ``SKILL.md`` under ``~/.claude/skills/<name>/``.
* **Subagents** — every ``*.md`` under ``.claude/agents/`` (project) and
  ``~/.claude/agents/`` (global).

Defensive: any missing file, malformed JSON, or unreadable directory
contributes a zero-token slice rather than raising, so the estimator
returns a usable budget on machines where the user has never configured
MCP / skills / etc.
"""

from __future__ import annotations

import json
import logging
from dataclasses import asdict, dataclass, field
from pathlib import Path

logger = logging.getLogger(__name__)


# ── tunables ─────────────────────────────────────────────────────────────────

# Approximate token estimate for Claude Code's built-in system prompt.
# Sourced from public scratch counts; the value is exposed as a module
# constant so tests can override it without monkeypatching internals.
DEFAULT_SYSTEM_PROMPT_TOKENS = 3000

# Per-MCP-server overhead. ``BASE`` accounts for the server description
# block ("here's what server X does"); ``PER_TOOL`` is the typical cost
# of a single tool's name + description + parameter schema. When tool
# counts aren't known statically (the common case — tools come from the
# server's runtime response), we charge a flat ``UNKNOWN_TOOLS`` so the
# slice still reflects that the server exists.
MCP_BASE_TOKENS = 200
MCP_PER_TOOL_TOKENS = 50
MCP_UNKNOWN_TOOLS_FALLBACK = 200

# Heuristic conversion: the rule of thumb is "1 token ≈ 4 characters of
# English prose". Underestimates code-heavy or non-Latin content; close
# enough for budget triage.
CHARS_PER_TOKEN = 4

# Anthropic Sonnet 4.5/4.6 input rate at the time of writing: $3 per
# million input tokens. Documented inline so tests can pin against it.
DEFAULT_INPUT_USD_PER_MILLION = 3.0

# Order-of-magnitude monthly session count. Tweakable; surfaced as a
# constant rather than a magic number so the docs stay honest.
DEFAULT_SESSIONS_PER_MONTH = 100


# ── dataclasses ─────────────────────────────────────────────────────────────


@dataclass
class ContextSlice:
    """One contributor to the per-session context budget."""

    name: str
    tokens: int
    source_path: str | None = None


@dataclass
class ContextBudget:
    """The full per-session context budget, broken down by source."""

    total_tokens: int
    slices: list[ContextSlice] = field(default_factory=list)
    cost_per_session_usd: float = 0.0
    estimated_monthly_cost_usd: float = 0.0
    heuristic: str = (
        f"len(text) // {CHARS_PER_TOKEN}; per-MCP-server "
        f"{MCP_BASE_TOKENS} + {MCP_PER_TOOL_TOKENS}/tool"
    )

    def to_dict(self) -> dict:
        return {
            "total_tokens": self.total_tokens,
            "slices": [asdict(s) for s in self.slices],
            "cost_per_session_usd": self.cost_per_session_usd,
            "estimated_monthly_cost_usd": self.estimated_monthly_cost_usd,
            "heuristic": self.heuristic,
        }


# ── token counting ──────────────────────────────────────────────────────────


def estimate_tokens(text: str) -> int:
    """Estimate the token count of ``text`` using the 4-char heuristic.

    Empty / None safely returns 0. The estimate is rounded down — a 4001
    character string returns 1000 tokens, not 1001 — which keeps the
    function deterministic and trivially testable.
    """
    if not text:
        return 0
    return len(text) // CHARS_PER_TOKEN


def _read_text(path: Path) -> str:
    """Return file contents or empty string on any read error.

    Defensive by design: a missing CLAUDE.md or an unreadable skill file
    must contribute a zero-token slice, never an exception.
    """
    try:
        return path.read_text(encoding="utf-8", errors="replace")
    except (OSError, UnicodeError) as exc:
        logger.debug("context-budget: failed to read %s: %s", path, exc)
        return ""


# ── slice builders ──────────────────────────────────────────────────────────


def _system_prompt_slice() -> ContextSlice:
    return ContextSlice(
        name="system_prompt",
        tokens=DEFAULT_SYSTEM_PROMPT_TOKENS,
        source_path=None,
    )


def _memory_slice(name: str, path: Path) -> ContextSlice:
    """Token cost of a CLAUDE.md (project or global)."""
    if not path.exists():
        return ContextSlice(name=name, tokens=0, source_path=str(path))
    text = _read_text(path)
    return ContextSlice(name=name, tokens=estimate_tokens(text), source_path=str(path))


def _mcp_servers_from_claude_json(claude_json_path: Path) -> dict[str, dict]:
    """Pull ``mcpServers`` from ``~/.claude.json``.

    Returns an empty dict on any read / parse failure so a missing or
    malformed config can't take the estimator down.
    """
    if not claude_json_path.exists():
        return {}
    try:
        raw = claude_json_path.read_text(encoding="utf-8", errors="replace")
        data = json.loads(raw)
    except (OSError, json.JSONDecodeError) as exc:
        logger.debug("context-budget: failed to parse %s: %s", claude_json_path, exc)
        return {}
    servers = data.get("mcpServers")
    if not isinstance(servers, dict):
        return {}
    # Filter to dict-valued entries — older configs sometimes had stub strings.
    return {k: v for k, v in servers.items() if isinstance(v, dict)}


def _mcp_servers_from_settings(settings_path: Path) -> dict[str, dict]:
    """Pull ``mcpServers`` from a project-level ``.claude/settings.json``."""
    if not settings_path.exists():
        return {}
    try:
        raw = settings_path.read_text(encoding="utf-8", errors="replace")
        data = json.loads(raw)
    except (OSError, json.JSONDecodeError) as exc:
        logger.debug("context-budget: failed to parse %s: %s", settings_path, exc)
        return {}
    servers = data.get("mcpServers")
    if not isinstance(servers, dict):
        return {}
    return {k: v for k, v in servers.items() if isinstance(v, dict)}


def _mcp_server_slice(name: str, definition: dict, source_path: Path | None) -> ContextSlice:
    """Charge a single MCP server.

    If the definition explicitly enumerates ``tools`` (rare — usually
    runtime-discovered) we cost it precisely; otherwise we charge the
    ``UNKNOWN_TOOLS_FALLBACK`` flat fee so the slice still surfaces.
    """
    tools = definition.get("tools")
    if isinstance(tools, list):
        tool_cost = MCP_PER_TOOL_TOKENS * len(tools)
    else:
        tool_cost = MCP_UNKNOWN_TOOLS_FALLBACK
    return ContextSlice(
        name=f"mcp:{name}",
        tokens=MCP_BASE_TOKENS + tool_cost,
        source_path=str(source_path) if source_path else None,
    )


def _skill_slices(skills_dir: Path) -> list[ContextSlice]:
    """One slice per ``SKILL.md`` under ``skills_dir``.

    Iterates the immediate children of ``skills_dir``; each subdirectory
    that contains a ``SKILL.md`` becomes a slice. Missing parent directory
    yields an empty list rather than raising.
    """
    if not skills_dir.exists() or not skills_dir.is_dir():
        return []
    out: list[ContextSlice] = []
    try:
        children = sorted(skills_dir.iterdir())
    except OSError as exc:
        logger.debug("context-budget: cannot list %s: %s", skills_dir, exc)
        return []
    for child in children:
        if not child.is_dir():
            continue
        skill_md = child / "SKILL.md"
        if not skill_md.exists():
            continue
        text = _read_text(skill_md)
        out.append(
            ContextSlice(
                name=f"skill:{child.name}",
                tokens=estimate_tokens(text),
                source_path=str(skill_md),
            )
        )
    return out


def _agent_slices(agents_dir: Path, *, scope: str) -> list[ContextSlice]:
    """One slice per ``*.md`` under ``agents_dir``.

    ``scope`` distinguishes project-local (``project``) from global
    (``global``) agent directories in the slice name.
    """
    if not agents_dir.exists() or not agents_dir.is_dir():
        return []
    out: list[ContextSlice] = []
    try:
        files = sorted(p for p in agents_dir.iterdir() if p.suffix == ".md" and p.is_file())
    except OSError as exc:
        logger.debug("context-budget: cannot list %s: %s", agents_dir, exc)
        return []
    for f in files:
        text = _read_text(f)
        out.append(
            ContextSlice(
                name=f"agent:{scope}:{f.stem}",
                tokens=estimate_tokens(text),
                source_path=str(f),
            )
        )
    return out


# ── cost projection ─────────────────────────────────────────────────────────


def _project_cost(
    total_tokens: int,
    *,
    input_usd_per_million: float = DEFAULT_INPUT_USD_PER_MILLION,
    sessions_per_month: int = DEFAULT_SESSIONS_PER_MONTH,
) -> tuple[float, float]:
    """Convert a token count into per-session and per-month USD figures."""
    per_session = (total_tokens / 1_000_000.0) * input_usd_per_million
    per_month = per_session * sessions_per_month
    return per_session, per_month


# ── public API ──────────────────────────────────────────────────────────────


def estimate_context_budget(
    project_dir: Path,
    *,
    home_dir: Path | None = None,
) -> ContextBudget:
    """Estimate the per-session context budget for ``project_dir``.

    ``home_dir`` overrides ``Path.home()`` for tests; production callers
    should leave it ``None``.
    """
    home = home_dir or Path.home()
    project_dir = Path(project_dir)

    slices: list[ContextSlice] = []

    # 1. System prompt (fixed estimate)
    slices.append(_system_prompt_slice())

    # 2. Memory files (project + global)
    slices.append(_memory_slice("memory:project_CLAUDE.md", project_dir / "CLAUDE.md"))
    slices.append(_memory_slice("memory:global_CLAUDE.md", home / ".claude" / "CLAUDE.md"))

    # 3. MCP servers from both global and project-local configs
    global_servers = _mcp_servers_from_claude_json(home / ".claude.json")
    for name, defn in sorted(global_servers.items()):
        slices.append(_mcp_server_slice(name, defn, home / ".claude.json"))
    project_settings = project_dir / ".claude" / "settings.json"
    project_servers = _mcp_servers_from_settings(project_settings)
    for name, defn in sorted(project_servers.items()):
        # Skip if a server with this name already came from the global config
        # — same name, same description budget; we don't double-charge.
        if name in global_servers:
            continue
        slices.append(_mcp_server_slice(name, defn, project_settings))

    # 4. Skills (global only — skills don't currently have a project-local
    # location in the Claude Code harness, but we structure the call so a
    # future location is easy to add)
    slices.extend(_skill_slices(home / ".claude" / "skills"))

    # 5. Subagents (project + global)
    slices.extend(_agent_slices(project_dir / ".claude" / "agents", scope="project"))
    slices.extend(_agent_slices(home / ".claude" / "agents", scope="global"))

    total = sum(s.tokens for s in slices)
    per_session, per_month = _project_cost(total)
    return ContextBudget(
        total_tokens=total,
        slices=slices,
        cost_per_session_usd=per_session,
        estimated_monthly_cost_usd=per_month,
    )


def estimate_global_budget(*, home_dir: Path | None = None) -> ContextBudget:
    """Estimate the per-session context budget for global Claude Code config.

    "Global" means anything that loads regardless of which project the
    user is in: the system prompt, ``~/.claude/CLAUDE.md``, registered
    MCP servers, every skill under ``~/.claude/skills/``, and every
    global agent under ``~/.claude/agents/``. Project-only artefacts
    (project ``CLAUDE.md``, project subagents, project ``.claude/
    settings.json``) are intentionally excluded.
    """
    home = home_dir or Path.home()
    slices: list[ContextSlice] = []

    slices.append(_system_prompt_slice())
    slices.append(_memory_slice("memory:global_CLAUDE.md", home / ".claude" / "CLAUDE.md"))

    global_servers = _mcp_servers_from_claude_json(home / ".claude.json")
    for name, defn in sorted(global_servers.items()):
        slices.append(_mcp_server_slice(name, defn, home / ".claude.json"))

    slices.extend(_skill_slices(home / ".claude" / "skills"))
    slices.extend(_agent_slices(home / ".claude" / "agents", scope="global"))

    total = sum(s.tokens for s in slices)
    per_session, per_month = _project_cost(total)
    return ContextBudget(
        total_tokens=total,
        slices=slices,
        cost_per_session_usd=per_session,
        estimated_monthly_cost_usd=per_month,
    )
