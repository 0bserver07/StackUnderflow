"""Waste-finding heuristics for the CLI ``optimize`` command.

Two layers live here:

1. ``find_waste()`` — the original Q&A-loop heuristic. Surfaces projects
   where the user had to push back on the assistant repeatedly.

2. ``find_patterns()`` — a broader waste-detection sweep that returns a
   list of :class:`Finding` objects covering: bloated CLAUDE.md, unused
   MCP servers, ghost agents, low read:edit ratio, junk reads, cache
   overhead, bash output limits.

3. ``find_context_budget_findings()`` — flags per-session context
   overhead (system prompt + MCP + skills + memory files) that exceeds
   ``CONTEXT_BUDGET_BLOAT_THRESHOLD``.

Detectors return empty lists silently on filesystem / parse errors —
patterns are advisory, never load-bearing.
"""

from __future__ import annotations

import json
import re
import sqlite3
from collections import Counter, defaultdict
from dataclasses import asdict, dataclass, field
from pathlib import Path
from typing import Any

from stackunderflow.reports.scope import Scope
from stackunderflow.services.context_budget import (
    ContextBudget,
    estimate_context_budget,
    estimate_global_budget,
)
from stackunderflow.services.qa_service import QAService
from stackunderflow.store import mart_queries, queries

__all__ = [
    "Finding",
    "find_patterns",
    "find_waste",
    "find_context_budget_findings",
    "CONTEXT_BUDGET_BLOAT_THRESHOLD",
]


# ── tunables ────────────────────────────────────────────────────────────────

CLAUDE_MD_TOKEN_THRESHOLD = 5_000      # ≈ 4 chars per token (rough)
JUNK_READ_REPEAT_THRESHOLD = 5         # same path Read >= N times
LOW_READ_EDIT_READ_FLOOR = 20          # Reads >= N to qualify
CACHE_OVERHEAD_RATIO = 0.5             # cache_create / total_input
BASH_OUTPUT_BYTES_THRESHOLD = 50_000   # 50 KB output
UNUSED_TOOL_LOOKBACK_DAYS = 30
# Per-session context budget above this threshold flags as bloat — paid
# on every turn. ~$6/mo just for the preamble at $3/M × 100 sessions/mo.
CONTEXT_BUDGET_BLOAT_THRESHOLD = 20_000


# ── Finding dataclass ───────────────────────────────────────────────────────


@dataclass(frozen=True)
class Finding:
    """A single waste-pattern hit."""

    pattern_id: str
    severity: str
    title: str
    description: str
    affected_count: int
    suggested_fix: str
    estimated_waste_tokens: int | None = None
    details: dict[str, Any] = field(default_factory=dict)

    def to_dict(self) -> dict[str, Any]:
        return asdict(self)


_SEVERITY_ORDER = {"high": 0, "medium": 1, "low": 2}


def _qa_service_factory() -> QAService:
    """Indirection point for tests to swap in a throwaway QAService."""
    return QAService()


# ── legacy: find_waste ──────────────────────────────────────────────────────


def find_waste(
    conn: sqlite3.Connection,
    *,
    scope: Scope,
    include: list[str] | None = None,
    exclude: list[str] | None = None,
) -> list[dict]:
    """Rank projects by number of looped Q&A pairs.

    Returns a list of dicts: `{project, looped_pairs, sample_questions}`.
    Projects with zero looped pairs are omitted.
    """
    slugs = [p.slug for p in queries.list_projects(conn)]

    if include is not None:
        slugs = [s for s in slugs if s in include]
    if exclude is not None:
        slugs = [s for s in slugs if s not in exclude]

    svc = _qa_service_factory()

    rows: list[dict] = []
    for slug in slugs:
        result = svc.list_qa(
            project=slug,
            resolution_status="looped",
            date_from=scope.since,
            date_to=scope.until,
            per_page=100,
        )
        if result["total"] == 0:
            continue
        samples = [r["question_text"][:120] for r in result["results"][:3]]
        rows.append({
            "project": slug,
            "looped_pairs": result["total"],
            "sample_questions": samples,
        })

    rows.sort(key=lambda r: r["looped_pairs"], reverse=True)
    return rows


# ── detectors ───────────────────────────────────────────────────────────────


def _approx_tokens(text: str) -> int:
    """Approximate token count assuming ~4 chars per token.

    Good enough for the bloated-CLAUDE.md heuristic. The exact tokenizer
    isn't worth the dependency; we round generously.
    """
    return max(0, len(text) // 4)


def _candidate_claude_md_paths(project_filter: list[str] | None = None) -> list[Path]:
    """Return CLAUDE.md candidates worth scanning.

    Scans ``~/.claude/projects/<slug>/`` for projects we know about. Each
    project_dir is a slugified absolute path; we don't try to round-trip
    the slug back to a real cwd (that's lossy on macOS), but if a
    matching directory contains a CLAUDE.md (e.g. a worktree under
    ``~/dev/...`` happens to have it), we surface that.

    We also consider ``~/.claude/CLAUDE.md`` as the user-global file.
    """
    out: list[Path] = []
    home_md = Path.home() / ".claude" / "CLAUDE.md"
    if home_md.is_file():
        out.append(home_md)

    # Per-project CLAUDE.md: ``~/.claude/projects/<slug>/CLAUDE.md`` is the
    # convention for some configurations; scan it defensively.
    projects_dir = Path.home() / ".claude" / "projects"
    if projects_dir.is_dir():
        for child in projects_dir.iterdir():
            if not child.is_dir():
                continue
            if project_filter is not None and child.name not in project_filter:
                continue
            md = child / "CLAUDE.md"
            if md.is_file():
                out.append(md)
    return out


def _detect_bloated_claude_md(
    conn: sqlite3.Connection,
    *,
    project_filter: list[str] | None = None,
) -> list[Finding]:
    """Pattern 1 — CLAUDE.md exceeds the token threshold.

    Inflates context every session. Severity scales with how far over
    the threshold the file is.
    """
    paths = _candidate_claude_md_paths(project_filter)
    bloated: list[tuple[Path, int]] = []
    for p in paths:
        try:
            text = p.read_text(encoding="utf-8", errors="replace")
        except OSError:
            continue
        approx = _approx_tokens(text)
        if approx > CLAUDE_MD_TOKEN_THRESHOLD:
            bloated.append((p, approx))

    if not bloated:
        return []

    bloated.sort(key=lambda t: t[1], reverse=True)
    biggest = bloated[0][1]

    if biggest >= 3 * CLAUDE_MD_TOKEN_THRESHOLD:
        sev = "high"
    elif biggest >= 2 * CLAUDE_MD_TOKEN_THRESHOLD:
        sev = "medium"
    else:
        sev = "low"

    waste = sum(approx - CLAUDE_MD_TOKEN_THRESHOLD for _, approx in bloated)

    return [
        Finding(
            pattern_id="bloated_claude_md",
            severity=sev,
            title=f"{len(bloated)} bloated CLAUDE.md file(s)",
            description=(
                f"{len(bloated)} CLAUDE.md file(s) exceed "
                f"{CLAUDE_MD_TOKEN_THRESHOLD:,} tokens and are loaded "
                "into every session's context."
            ),
            affected_count=len(bloated),
            estimated_waste_tokens=waste,
            suggested_fix=(
                "Trim CLAUDE.md to the bare essentials — move long-form notes "
                "to project-local docs and reference them on demand."
            ),
            details={
                "files": [
                    {"path": str(p), "approx_tokens": tk}
                    for p, tk in bloated
                ],
                "threshold_tokens": CLAUDE_MD_TOKEN_THRESHOLD,
            },
        )
    ]


def _registered_mcp_servers() -> list[str]:
    """Return MCP server names from common config locations.

    Looks at:
      * ``~/.claude.json`` (top-level ``mcpServers`` map)
      * ``~/.config/claude-code/settings.json`` (``mcpServers`` map)
      * ``~/.claude/settings.json`` (``mcpServers`` map)

    Missing files / parse failures are swallowed; this is best-effort.
    """
    candidates = [
        Path.home() / ".claude.json",
        Path.home() / ".config" / "claude-code" / "settings.json",
        Path.home() / ".claude" / "settings.json",
    ]
    names: set[str] = set()
    for cfg in candidates:
        try:
            data = json.loads(cfg.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError):
            continue
        if not isinstance(data, dict):
            continue
        servers = data.get("mcpServers")
        if isinstance(servers, dict):
            names.update(str(k) for k in servers)
    return sorted(names)


def _recent_tool_names(
    conn: sqlite3.Connection,
    *,
    since_iso: str | None,
) -> Counter[str]:
    """Roll up tool_name → call count from messages within [since_iso, now)."""
    sql = (
        "SELECT tools_json FROM messages "
        "WHERE tools_json != '[]' AND tools_json IS NOT NULL"
    )
    params: list[Any] = []
    if since_iso:
        sql += " AND timestamp >= ?"
        params.append(since_iso)
    counter: Counter[str] = Counter()
    for row in conn.execute(sql, params):
        try:
            tools = json.loads(row["tools_json"])
        except (json.JSONDecodeError, TypeError):
            continue
        if isinstance(tools, list):
            for name in tools:
                if isinstance(name, str) and name:
                    counter[name] += 1
    return counter


def _detect_unused_mcp_servers(
    conn: sqlite3.Connection,
    *,
    scope: Scope | None = None,
) -> list[Finding]:
    """Pattern 2 — MCP server registered but no tool called in 30 days.

    MCP tool calls show up in ``tools_json`` with the ``mcp__<server>__*``
    naming convention. We strip the server prefix and compare against the
    registry; unused servers waste a context-load slot for no benefit.
    """
    registered = _registered_mcp_servers()
    if not registered:
        return []

    since_iso: str | None
    if scope is not None and scope.since is not None:
        since_iso = scope.since
    else:
        from datetime import UTC, datetime, timedelta
        since_iso = (datetime.now(UTC) - timedelta(days=UNUSED_TOOL_LOOKBACK_DAYS)).isoformat()

    counts = _recent_tool_names(conn, since_iso=since_iso)
    used_servers: set[str] = set()
    for tool_name in counts:
        # MCP tool names follow ``mcp__<server>__<tool>`` (Claude Code's
        # convention). Be lenient about variations (single-underscore,
        # case) — anything that looks like a prefix match counts.
        m = re.match(r"^mcp__([^_]+(?:_[^_]+)*?)__", tool_name)
        if m:
            used_servers.add(m.group(1))

    unused = [s for s in registered if s not in used_servers]
    if not unused:
        return []

    if len(unused) >= 5:
        sev = "high"
    elif len(unused) >= 2:
        sev = "medium"
    else:
        sev = "low"

    return [
        Finding(
            pattern_id="unused_mcp_servers",
            severity=sev,
            title=f"{len(unused)} unused MCP server(s)",
            description=(
                f"{len(unused)} MCP server(s) registered but no tool calls "
                f"observed in the last {UNUSED_TOOL_LOOKBACK_DAYS} days."
            ),
            affected_count=len(unused),
            estimated_waste_tokens=None,
            suggested_fix=(
                "Remove unused MCP server entries from ~/.claude.json — "
                "every server adds tool definitions to each request's context."
            ),
            details={
                "unused_servers": unused,
                "registered_total": len(registered),
                "lookback_days": UNUSED_TOOL_LOOKBACK_DAYS,
            },
        )
    ]


def _registered_agents() -> list[tuple[str, Path]]:
    """Return (agent_name, file_path) pairs from local + global agent dirs."""
    out: list[tuple[str, Path]] = []
    for root in (Path.home() / ".claude" / "agents", Path.cwd() / ".claude" / "agents"):
        if not root.is_dir():
            continue
        for child in root.iterdir():
            if child.is_file() and child.suffix in {".md", ".yml", ".yaml", ".json"}:
                out.append((child.stem, child))
    # Dedupe by name (a project agent shadows a user agent — count once).
    seen: dict[str, Path] = {}
    for name, p in out:
        seen.setdefault(name, p)
    return sorted(seen.items())


def _detect_ghost_agents(
    conn: sqlite3.Connection,
    *,
    scope: Scope | None = None,
) -> list[Finding]:
    """Pattern 3 — agents defined but never spawned in 30 days.

    Subagents show up in tool calls as ``Task`` (or as MCP-style
    ``subagent_type=…`` payloads inside the raw record). Without a
    structured subagent-history field on every provider, we count any
    appearance of the agent's stem in ``content_text`` over the lookback
    window — best effort, but a defensible "is this name ever mentioned
    as a tool target?" check.
    """
    agents = _registered_agents()
    if not agents:
        return []

    since_iso: str | None
    if scope is not None and scope.since is not None:
        since_iso = scope.since
    else:
        from datetime import UTC, datetime, timedelta
        since_iso = (datetime.now(UTC) - timedelta(days=UNUSED_TOOL_LOOKBACK_DAYS)).isoformat()

    # Wave 5: short-circuit on populated tool_mart with zero Task calls.
    # The detector identifies "ghost" agents — registered but never
    # spawned via Task. If Task itself wasn't called in the lookback
    # window, every registered agent is a ghost and the detector finds
    # them all without scanning raw_json. Empty-mart fallback: full
    # aggregator pass.
    if mart_queries.mart_has_tool_rows(conn):
        task_calls = mart_queries.tool_call_count_in_window(
            conn, tool_names=("Task",),
            since_iso=since_iso, until_iso=None,
        )
        if task_calls == 0:
            return _ghost_agents_finding(agents, agents)

    sql = "SELECT raw_json FROM messages WHERE 1=1"
    params: list[Any] = []
    if since_iso:
        sql += " AND timestamp >= ?"
        params.append(since_iso)
    sql += " AND tools_json LIKE '%Task%'"

    invoked: set[str] = set()
    for row in conn.execute(sql, params):
        raw = row["raw_json"] or ""
        for name, _ in agents:
            # subagent_type is the canonical key in Claude Code's Task
            # tool schema; check for a few variations defensively.
            if (
                f'"subagent_type":"{name}"' in raw
                or f'"subagent_type": "{name}"' in raw
                or f'"agent":"{name}"' in raw
            ):
                invoked.add(name)

    ghost = [(name, p) for name, p in agents if name not in invoked]
    return _ghost_agents_finding(agents, ghost)


def _ghost_agents_finding(
    agents: list[tuple[str, Path]],
    ghost: list[tuple[str, Path]],
) -> list[Finding]:
    """Render the ghost-agents ``Finding`` from a candidate list.

    Shared between the mart short-circuit (Wave 5: when ``Task`` was
    never called, every registered agent is a ghost) and the
    aggregator path that builds ``ghost`` by inspecting raw_json. Keeps
    the severity ladder + suggested fix + details payload identical
    across both paths so the JSON contract is stable.
    """
    del agents  # currently unused — kept for forward compat / readability
    if not ghost:
        return []
    if len(ghost) >= 5:
        sev = "medium"
    else:
        sev = "low"
    return [
        Finding(
            pattern_id="ghost_agents",
            severity=sev,
            title=f"{len(ghost)} ghost agent(s)",
            description=(
                f"{len(ghost)} agent(s) defined under .claude/agents/ but never "
                f"spawned in the last {UNUSED_TOOL_LOOKBACK_DAYS} days."
            ),
            affected_count=len(ghost),
            estimated_waste_tokens=None,
            suggested_fix=(
                "Delete unused agent definitions — every agent adds to the "
                "tool schema each session loads."
            ),
            details={
                "agents": [{"name": n, "path": str(p)} for n, p in ghost],
                "lookback_days": UNUSED_TOOL_LOOKBACK_DAYS,
            },
        )
    ]


def _iter_session_messages(
    conn: sqlite3.Connection,
    *,
    scope: Scope | None,
) -> dict[int, list[sqlite3.Row]]:
    """Return ``{session_fk: [row, ...]}`` ordered by seq, scope-filtered."""
    sql = (
        "SELECT id, session_fk, seq, timestamp, role, "
        "       input_tokens, cache_create_tokens, "
        "       tools_json, raw_json, content_text "
        "FROM messages WHERE 1=1"
    )
    params: list[Any] = []
    if scope is not None:
        if scope.since:
            sql += " AND timestamp >= ?"
            params.append(scope.since)
        if scope.until:
            sql += " AND timestamp <= ?"
            params.append(scope.until)
    sql += " ORDER BY session_fk, seq"
    grouped: dict[int, list[sqlite3.Row]] = defaultdict(list)
    for row in conn.execute(sql, params):
        grouped[row["session_fk"]].append(row)
    return grouped


def _tool_calls_with_input(raw_json: str) -> list[tuple[str, dict]]:
    """Pull (tool_name, input_dict) pairs from an assistant message's raw_json.

    Defensive against the various payload shapes (Claude / Codex / Cursor
    all keep something like ``message.content[].type == 'tool_use'``).
    Falls back to an empty list on parse error or unknown shape.
    """
    try:
        obj = json.loads(raw_json)
    except (json.JSONDecodeError, TypeError):
        return []
    msg = obj.get("message") if isinstance(obj, dict) else None
    if not isinstance(msg, dict):
        return []
    body = msg.get("content")
    if not isinstance(body, list):
        return []
    out: list[tuple[str, dict]] = []
    for blk in body:
        if not isinstance(blk, dict):
            continue
        if blk.get("type") == "tool_use":
            name = blk.get("name", "")
            inp = blk.get("input", {})
            if isinstance(name, str) and isinstance(inp, dict):
                out.append((name, inp))
    return out


def _detect_low_read_edit_ratio(
    conn: sqlite3.Connection,
    *,
    scope: Scope | None = None,
    project_filter: list[str] | None = None,
) -> list[Finding]:
    """Pattern 4 — sessions with many Reads and zero Edit/Write.

    Wave 5: short-circuit when ``tool_mart`` is populated AND no Read
    tool calls exist in the window — the per-session Read+Edit signal
    requires a session-keyed parse of ``tools_json``, which no mart
    materialises (``tool_mart`` is keyed at the day/project grain), so
    we still walk ``messages`` for the actual ratio test. The mart
    just lets us skip the scan when there can't possibly be a finding.
    Empty mart → fall through to the full aggregator pass so freshly
    installed stores still emit findings.
    """
    if mart_queries.mart_has_tool_rows(conn):
        since = scope.since if scope is not None else None
        until = scope.until if scope is not None else None
        reads = mart_queries.tool_call_count_in_window(
            conn, tool_names=("Read",),
            since_iso=since, until_iso=until,
            project_filter=project_filter,
        )
        if reads < LOW_READ_EDIT_READ_FLOOR:
            return []

    grouped = _iter_session_messages(conn, scope=scope)
    bad_sessions: list[dict] = []
    for session_fk, rows in grouped.items():
        reads = 0
        edits = 0
        for r in rows:
            try:
                names = json.loads(r["tools_json"])
            except (json.JSONDecodeError, TypeError):
                continue
            if not isinstance(names, list):
                continue
            for n in names:
                if n == "Read":
                    reads += 1
                elif n in ("Edit", "Write", "MultiEdit", "NotebookEdit"):
                    edits += 1
        if reads >= LOW_READ_EDIT_READ_FLOOR and edits == 0:
            bad_sessions.append({"session_fk": session_fk, "reads": reads})

    if not bad_sessions:
        return []

    if len(bad_sessions) >= 5:
        sev = "high"
    elif len(bad_sessions) >= 2:
        sev = "medium"
    else:
        sev = "low"

    # Rough waste estimate: 2K tokens per Read on average (tools + content).
    est_waste = sum(s["reads"] for s in bad_sessions) * 2_000

    return [
        Finding(
            pattern_id="low_read_edit_ratio",
            severity=sev,
            title=f"{len(bad_sessions)} exploration-only session(s)",
            description=(
                f"{len(bad_sessions)} session(s) Read "
                f"{LOW_READ_EDIT_READ_FLOOR}+ files but never wrote or edited "
                "code."
            ),
            affected_count=len(bad_sessions),
            estimated_waste_tokens=est_waste,
            suggested_fix=(
                "Use targeted search (Grep / Glob) before bulk Read; "
                "or commit a partial edit so the exploration produces output."
            ),
            details={
                "sessions": bad_sessions[:10],
                "read_threshold": LOW_READ_EDIT_READ_FLOOR,
            },
        )
    ]


def _detect_junk_reads(
    conn: sqlite3.Connection,
    *,
    scope: Scope | None = None,
    project_filter: list[str] | None = None,
) -> list[Finding]:
    """Pattern 5 — same file Read 5+ times in one session.

    Indicates the assistant forgot what it already saw and re-fetched.

    Wave 5: short-circuit when ``tool_mart`` confirms zero Read calls
    in window. The full detector still parses ``raw_json`` for the
    file-path-level signal (per-file repeat counts can't be derived
    from any mart), but skipping the parse when no Read happened is a
    free win for project-scoped windows that didn't read anything.
    Empty-mart fallback: full aggregator pass.

    v012: the pre-flight counts ``calls_total`` (non-distinct Read
    occurrences) rather than ``event_count`` — this detector cares about
    *how many times* a file was Read, which is the legacy aggregator's
    ``calls`` semantics. On a pre-v012 ``tool_mart`` (``calls_total``
    not yet rebuilt) the count reads 0 and we fall through to the full
    scan, which still produces the correct answer.
    """
    if mart_queries.mart_has_tool_rows(conn):
        since = scope.since if scope is not None else None
        until = scope.until if scope is not None else None
        reads = mart_queries.tool_call_count_in_window(
            conn, tool_names=("Read",),
            since_iso=since, until_iso=until,
            project_filter=project_filter,
            count_column="calls_total",
        )
        if reads == 0:
            return []

    grouped = _iter_session_messages(conn, scope=scope)
    hits: list[dict] = []
    for session_fk, rows in grouped.items():
        per_path: Counter[str] = Counter()
        for r in rows:
            for name, inp in _tool_calls_with_input(r["raw_json"] or ""):
                if name != "Read":
                    continue
                fp = inp.get("file_path") or inp.get("path") or ""
                if isinstance(fp, str) and fp:
                    per_path[fp] += 1
        repeats = {p: n for p, n in per_path.items() if n >= JUNK_READ_REPEAT_THRESHOLD}
        if repeats:
            hits.append({
                "session_fk": session_fk,
                "files": [
                    {"path": p, "reads": n}
                    for p, n in sorted(repeats.items(), key=lambda kv: kv[1], reverse=True)
                ],
            })

    if not hits:
        return []

    affected_files = sum(len(h["files"]) for h in hits)
    if affected_files >= 10:
        sev = "high"
    elif affected_files >= 3:
        sev = "medium"
    else:
        sev = "low"

    # Waste estimate: each redundant read after the first costs ~2K tokens.
    redundant_reads = sum(
        max(0, f["reads"] - 1) for h in hits for f in h["files"]
    )
    est_waste = redundant_reads * 2_000

    return [
        Finding(
            pattern_id="junk_reads",
            severity=sev,
            title=f"{affected_files} file(s) re-read excessively",
            description=(
                f"{affected_files} file(s) Read "
                f"{JUNK_READ_REPEAT_THRESHOLD}+ times in a single session — "
                "assistant likely forgot prior reads."
            ),
            affected_count=affected_files,
            estimated_waste_tokens=est_waste,
            suggested_fix=(
                "Cache file contents in working memory or use Grep to "
                "search within an already-loaded file."
            ),
            details={
                "sessions": hits[:10],
                "repeat_threshold": JUNK_READ_REPEAT_THRESHOLD,
            },
        )
    ]


def _detect_cache_overhead(
    conn: sqlite3.Connection,
    *,
    scope: Scope | None = None,
) -> list[Finding]:
    """Pattern 6 — cache writes dominate the input budget.

    When ``cache_create_tokens / total_input_tokens > 50%`` the cache
    is being thrashed instead of amortising. Common cause: short
    sessions that pay the cache write cost without ever reading it
    back. We check at the session level.

    Wave 4A — reads pre-aggregated per-session totals from
    ``session_mart`` when available. The mart's ``input_tokens`` and
    ``cache_create`` columns are the same SUM-by-session_fk this
    detector used to compute on every call, so the ratio test is
    identical and the empty-mart fallback path keeps the GROUP BY
    over ``messages``. The other detectors in this module remain on
    the aggregator path because their signals (tool-call shape, raw
    JSON inspection, per-message payload sizes) aren't materialised
    into any mart yet.
    """
    if mart_queries.mart_has_session_rows(conn):
        return _detect_cache_overhead_from_mart(conn, scope=scope)
    return _detect_cache_overhead_from_messages(conn, scope=scope)


def _detect_cache_overhead_from_mart(
    conn: sqlite3.Connection,
    *,
    scope: Scope | None,
) -> list[Finding]:
    """Mart-fed cache-overhead detector — reads ``session_mart`` totals."""
    since_iso = scope.since if scope is not None else None
    until_iso = scope.until if scope is not None else None
    bad = mart_queries.session_mart_cache_overhead(
        conn,
        since_iso=since_iso,
        until_iso=until_iso,
        ratio_threshold=CACHE_OVERHEAD_RATIO,
    )
    # Re-key on ``session_fk`` for parity with the aggregator path —
    # the finding's ``details.sessions`` consumers (tests, CLI table)
    # expect that field name. ``session_id`` from the mart maps onto
    # the same logical concept; we surface it as ``session_fk`` so the
    # JSON contract stays stable across data sources.
    bad = [
        {
            "session_fk": row["session_id"],
            "cache_create_tokens": row["cache_create_tokens"],
            "input_tokens": row["input_tokens"],
            "ratio": row["ratio"],
        }
        for row in bad
    ]
    return _cache_overhead_finding(bad)


def _detect_cache_overhead_from_messages(
    conn: sqlite3.Connection,
    *,
    scope: Scope | None,
) -> list[Finding]:
    """Aggregator-path cache-overhead detector — empty-mart fallback."""
    sql = (
        "SELECT session_fk, "
        "       SUM(input_tokens) AS inp, "
        "       SUM(cache_create_tokens) AS cache_create "
        "FROM messages WHERE 1=1"
    )
    params: list[Any] = []
    if scope is not None:
        if scope.since:
            sql += " AND timestamp >= ?"
            params.append(scope.since)
        if scope.until:
            sql += " AND timestamp <= ?"
            params.append(scope.until)
    sql += " GROUP BY session_fk"

    bad: list[dict] = []
    for row in conn.execute(sql, params):
        inp = int(row["inp"] or 0)
        cache = int(row["cache_create"] or 0)
        if inp == 0 or cache == 0:
            continue
        total_input = inp + cache
        if total_input == 0:
            continue
        ratio = cache / total_input
        if ratio > CACHE_OVERHEAD_RATIO:
            bad.append({
                "session_fk": row["session_fk"],
                "cache_create_tokens": cache,
                "input_tokens": inp,
                "ratio": round(ratio, 3),
            })
    return _cache_overhead_finding(bad)


def _cache_overhead_finding(bad: list[dict]) -> list[Finding]:
    """Render the cache-overhead ``Finding`` from the candidate list.

    Shared between the mart path and the aggregator fallback so the
    finding's severity ladder + waste estimation stay in lockstep.
    """

    if not bad:
        return []

    if len(bad) >= 10:
        sev = "high"
    elif len(bad) >= 3:
        sev = "medium"
    else:
        sev = "low"

    est_waste = sum(b["cache_create_tokens"] for b in bad) // 2

    return [
        Finding(
            pattern_id="cache_overhead",
            severity=sev,
            title=f"{len(bad)} session(s) with cache thrash",
            description=(
                f"{len(bad)} session(s) where cache_create_tokens exceed "
                f"{int(CACHE_OVERHEAD_RATIO * 100)}% of total input — "
                "cache is being written but not amortised."
            ),
            affected_count=len(bad),
            estimated_waste_tokens=est_waste,
            suggested_fix=(
                "Bundle related questions into one session so cache writes "
                "amortise; avoid spawning fresh sessions for one-shot tasks."
            ),
            details={
                "sessions": bad[:10],
                "ratio_threshold": CACHE_OVERHEAD_RATIO,
            },
        )
    ]


def _detect_bash_output_limits(
    conn: sqlite3.Connection,
    *,
    scope: Scope | None = None,
    project_filter: list[str] | None = None,
) -> list[Finding]:
    """Pattern 7 — bash tool calls returning ≥ 50 KB output.

    Indicates the assistant should be using ``head`` / ``tail`` / ``grep``
    or ``--limit`` flags. We measure output by inspecting tool_result
    blocks in user messages whose preceding assistant call was a Bash
    tool. Falls back to checking ``content_text`` length on user
    tool_result rows.

    Wave 5: short-circuit when ``tool_mart`` confirms zero Bash calls
    in window. The detector still needs the raw-message scan to size
    each output, but skipping the scan when no Bash ran is the cheap
    early-exit. Empty-mart fallback: full aggregator pass.
    """
    if mart_queries.mart_has_tool_rows(conn):
        since = scope.since if scope is not None else None
        until = scope.until if scope is not None else None
        bash_calls = mart_queries.tool_call_count_in_window(
            conn, tool_names=("Bash",),
            since_iso=since, until_iso=until,
            project_filter=project_filter,
        )
        if bash_calls == 0:
            return []

    sql = (
        "SELECT id, session_fk, seq, role, raw_json, content_text "
        "FROM messages WHERE 1=1"
    )
    params: list[Any] = []
    if scope is not None:
        if scope.since:
            sql += " AND timestamp >= ?"
            params.append(scope.since)
        if scope.until:
            sql += " AND timestamp <= ?"
            params.append(scope.until)
    sql += " ORDER BY session_fk, seq"

    # Pass 1: collect (session_fk, seq) of every assistant Bash call.
    rows = list(conn.execute(sql, params))
    bash_call_seqs: dict[tuple[int, int], dict] = {}
    for r in rows:
        if r["role"] != "assistant":
            continue
        for name, inp in _tool_calls_with_input(r["raw_json"] or ""):
            if name == "Bash":
                bash_call_seqs[(r["session_fk"], r["seq"])] = {
                    "command": (inp.get("command") or "")[:140],
                }

    # Pass 2: any user tool_result that follows an assistant Bash call,
    # measured in bytes of decoded content_text.
    big: list[dict] = []
    for r in rows:
        if r["role"] != "user":
            continue
        # The user tool_result row sits at session_fk + (seq strictly past
        # the bash call). We don't know the exact seq link, so heuristic:
        # treat every user message with a tool_result whose content >= the
        # threshold as a bash-output candidate IF any preceding assistant
        # Bash call exists in the same session.
        size = len((r["content_text"] or "").encode("utf-8"))
        if size < BASH_OUTPUT_BYTES_THRESHOLD:
            continue
        # Confirm the session has at least one Bash call before this seq.
        has_prior_bash = any(
            sfk == r["session_fk"] and bseq < r["seq"]
            for (sfk, bseq) in bash_call_seqs
        )
        if not has_prior_bash:
            continue
        big.append({
            "session_fk": r["session_fk"],
            "seq": r["seq"],
            "bytes": size,
        })

    if not big:
        return []

    if len(big) >= 10:
        sev = "high"
    elif len(big) >= 3:
        sev = "medium"
    else:
        sev = "low"

    est_waste = sum(b["bytes"] for b in big) // 4  # ~4 chars/token

    return [
        Finding(
            pattern_id="bash_output_limits",
            severity=sev,
            title=f"{len(big)} oversized bash output(s)",
            description=(
                f"{len(big)} Bash tool result(s) exceeded "
                f"{BASH_OUTPUT_BYTES_THRESHOLD // 1000} KB of output — "
                "wasted tokens that head/tail/grep would have avoided."
            ),
            affected_count=len(big),
            estimated_waste_tokens=est_waste,
            suggested_fix=(
                "Pipe bash output through head/tail/grep/awk; cap with "
                "--limit/--max flags or write to a file and read selectively."
            ),
            details={
                "samples": big[:10],
                "threshold_bytes": BASH_OUTPUT_BYTES_THRESHOLD,
            },
        )
    ]


# ── orchestrator ────────────────────────────────────────────────────────────


def find_patterns(
    conn: sqlite3.Connection,
    *,
    scope: Scope | None = None,
    project_filter: list[str] | None = None,
) -> list[Finding]:
    """Run every detector and return findings sorted by severity desc.

    ``scope`` constrains time-windowed detectors; filesystem-based
    detectors (CLAUDE.md, MCP servers, agents) are scope-independent.
    ``project_filter`` narrows the CLAUDE.md scan to specific project
    slugs.
    """
    findings: list[Finding] = []

    # Filesystem-based detectors
    findings.extend(_detect_bloated_claude_md(conn, project_filter=project_filter))
    findings.extend(_detect_unused_mcp_servers(conn, scope=scope))
    findings.extend(_detect_ghost_agents(conn, scope=scope))

    # Message-based detectors
    findings.extend(_detect_low_read_edit_ratio(
        conn, scope=scope, project_filter=project_filter,
    ))
    findings.extend(_detect_junk_reads(
        conn, scope=scope, project_filter=project_filter,
    ))
    findings.extend(_detect_cache_overhead(conn, scope=scope))
    findings.extend(_detect_bash_output_limits(
        conn, scope=scope, project_filter=project_filter,
    ))

    findings.sort(
        key=lambda f: (
            _SEVERITY_ORDER.get(f.severity, 99),
            -(f.estimated_waste_tokens or 0),
        )
    )
    return findings


def find_context_budget_findings(
    conn: sqlite3.Connection,
    *,
    include: list[str] | None = None,
    exclude: list[str] | None = None,
    threshold: int = CONTEXT_BUDGET_BLOAT_THRESHOLD,
) -> list[dict]:
    """Flag projects whose per-session context budget is bloated.

    A finding is emitted (severity ``medium``) for every project whose
    estimated total budget exceeds ``threshold`` tokens. The global
    budget — i.e. the part that's the same regardless of which project
    you're in — is reported once with ``project=None`` so the CLI can
    cleanly distinguish "trim your skills" from "trim this project's
    CLAUDE.md".

    Defensive: a missing project directory contributes a zero-cost
    project slice (the global slices still count); we never raise.
    """
    findings: list[dict] = []

    # Global slices — emit one finding regardless of any project filter
    # because trimming MCP servers / skills helps every project at once.
    try:
        global_budget = estimate_global_budget()
    except Exception:  # noqa: BLE001 - estimator must never break optimize
        global_budget = None
    if global_budget is not None and global_budget.total_tokens > threshold:
        findings.append(_finding_from_budget(slug=None, budget=global_budget, threshold=threshold))

    # Per-project budgets
    projects = queries.list_projects(conn)
    slugs = [p.slug for p in projects]
    if include is not None:
        slugs = [s for s in slugs if s in include]
    if exclude is not None:
        slugs = [s for s in slugs if s not in exclude]

    by_slug = {p.slug: p for p in projects}
    for slug in slugs:
        row = by_slug[slug]
        if not row.path:
            continue
        project_dir = Path(row.path)
        if not project_dir.exists():
            continue
        try:
            budget = estimate_context_budget(project_dir)
        except Exception:  # noqa: BLE001, S112 - estimator must never break optimize
            continue
        if budget.total_tokens > threshold:
            findings.append(_finding_from_budget(slug=slug, budget=budget, threshold=threshold))

    return findings


def _finding_from_budget(*, slug: str | None, budget: ContextBudget, threshold: int) -> dict:
    """Render one ``context_budget_bloat`` finding from a ``ContextBudget``."""
    top_slices = sorted(
        (s for s in budget.slices if s.tokens > 0),
        key=lambda s: s.tokens,
        reverse=True,
    )[:5]
    return {
        "kind": "context_budget_bloat",
        "severity": "medium",
        "project": slug,  # None == global
        "total_tokens": budget.total_tokens,
        "threshold": threshold,
        "cost_per_session_usd": budget.cost_per_session_usd,
        "estimated_monthly_cost_usd": budget.estimated_monthly_cost_usd,
        "top_slices": [
            {"name": s.name, "tokens": s.tokens, "source_path": s.source_path}
            for s in top_slices
        ],
    }
