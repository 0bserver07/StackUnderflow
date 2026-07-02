"""Cross-session pattern / failure mining — the "coding health" report.

Where :mod:`stackunderflow.reports.anomaly` flags statistical *outliers*
(one expensive day), this module keys on **recurrence across sessions**:
the same file failing in session after session, the same error signature
resurfacing for weeks, Bash failures clustering on one command. One bad
moment is noise; the same bad moment in five sessions is a pattern worth
fixing.

Three pattern families are mined from the enricher-derived data already
in the store:

1. **Per-file risk** — for every file the agent touched (``Edit`` /
   ``Write`` / ``Read`` family), how many sessions touched it, how many of
   those sessions saw a tool failure *on that file*, and when it last
   failed. "``auth_test.py`` fails in 40% of the sessions that touch it."
2. **Recurring error signatures** — tool errors normalised into stable
   signatures (paths/numbers stripped) and aggregated across sessions:
   occurrence count, distinct sessions, and — where derivable — what the
   sessions that *moved past* the error did next (resolution hints).
3. **Command failure clusters** — failing ``Bash`` calls grouped by their
   normalised command head ("``npm install``", "``pytest``"), so "Bash
   timeouts cluster on ``npm install``" is one row, not thirty.

Data sourcing (all reads bounded — never a full-store scan):

* **Tool touches** come from ``message_tool_mart`` (one row per tool call,
  indexed by day / file_path / project) narrowed to the window's days.
* **Failures** come from a windowed ``messages`` read pre-filtered in SQL
  to rows whose ``raw_json`` can contain an errored ``tool_result`` block
  (``is_error`` LIKE screen — the JSON parse confirms), so only actual
  error rows are ever parsed.
* Each error is attributed back to the tool call that produced it by
  matching ``tool_use_id`` against the nearest preceding assistant
  message's ``tools_json`` (indexed ``(session_fk, seq)`` walk, bounded
  hops, memoised parses).
* **Interruptions** use the classifier's marker prefixes against
  ``content_text`` in the same window.

Design contract (mirrors :mod:`stackunderflow.reports.forks`):

* **Advisory, never raises.** Missing tables, malformed JSON, absent
  marts, or arithmetic edges all degrade to an empty-but-well-formed
  report. Callers never wrap this in try/except for correctness.
* **Window-bounded.** Every SQL statement is bounded by ``since_days``
  (default 90, hard cap 365) and, when given, ``project_ids``. Row caps
  (:data:`MAX_ERROR_ROWS`, :data:`MAX_TOOL_ROWS`) keep a pathological
  store from ballooning memory.
* **Own query helpers.** All SQL lives here behind ``sqlite_master``
  guards; this module does not touch ``store/queries.py``.
* **Deterministic.** Stable sort keys everywhere; two runs over the same
  store produce byte-identical reports.

:func:`file_risk` exposes the per-file lookup programmatically so the
active-recall hook (campaign #5) can ask "what do we know about this
path?" without going through HTTP.
"""

from __future__ import annotations

import json
import re
import sqlite3
from bisect import bisect_right
from collections import Counter
from dataclasses import asdict, dataclass, field
from datetime import UTC, datetime, timedelta
from typing import Any

# Precedent for reusing the classifier's internals: ``etl/marts/project.py``
# imports ``classifier._determine_kind`` so the mart counts match the pipeline.
# Reusing ``_categorise`` here keeps error categories identical to the
# Overview's errors-by-category block.
from stackunderflow.stats.classifier import (
    INTERRUPT_API,
    INTERRUPT_PREFIX,
    _categorise,
)

__all__ = [
    "CommandCluster",
    "DEFAULT_SINCE_DAYS",
    "ErrorSignature",
    "FileRisk",
    "MAX_SINCE_DAYS",
    "MIN_RECURRENCE_SESSIONS",
    "PatternsReport",
    "file_risk",
    "mine_patterns",
]


# ── tunables ────────────────────────────────────────────────────────────────

# Default mining window. 90 days is wide enough for "recurred for weeks"
# patterns while keeping the scan bounded on multi-year stores.
DEFAULT_SINCE_DAYS = 90
# Hard ceiling on the window — there is deliberately no "all time" spec, so
# the module can never be coaxed into an unbounded full-store scan.
MAX_SINCE_DAYS = 365
# An error signature must appear in at least this many DISTINCT sessions to
# count as recurring. One session's flailing is a retry loop, not a pattern.
MIN_RECURRENCE_SESSIONS = 2
# Caps on returned list sizes — the panel shows the worst few, not a wall.
TOP_N_FILES = 20
TOP_N_SIGNATURES = 20
TOP_N_COMMANDS = 15
TOP_N_HINTS = 3
# Bounded-memory guards. The error screen is SQL-prefiltered so real stores
# sit far below these; they exist so a pathological store degrades to a
# truncated (still well-formed) report instead of an OOM.
MAX_ERROR_ROWS = 20_000
MAX_TOOL_ROWS = 500_000
# How many preceding assistant-with-tools messages to inspect when matching
# an errored tool_result back to its tool_use call. The very first hop hits
# in practice (results directly follow their call); parallel tool fan-out is
# why this is > 1.
_ATTRIBUTION_HOPS = 5

# File-grained tools — the only ones whose ``file_path`` names an actual
# file (Grep/Glob paths are directories and would pollute the risk table).
_WRITE_TOOLS = frozenset({"Edit", "Write", "MultiEdit", "NotebookEdit"})
_READ_TOOLS = frozenset({"Read"})
_TOUCH_TOOLS = _WRITE_TOOLS | _READ_TOOLS

# CLI heads whose first subcommand is part of the cluster identity
# ("git push" vs "git status" are different failure populations).
_SUBCOMMAND_HEADS = frozenset({
    "apt", "brew", "bundle", "cargo", "composer", "docker", "dotnet", "gh",
    "git", "go", "gradle", "kubectl", "make", "mvn", "npm", "pip", "pip3",
    "pnpm", "poetry", "stackunderflow", "terraform", "uv", "yarn",
})
# Heads whose "subcommand" is a script path — keep only its basename.
_SCRIPT_HEADS = frozenset({"bash", "node", "npx", "python", "python3", "ruby", "sh"})


# ── result dataclasses ──────────────────────────────────────────────────────


@dataclass(frozen=True)
class FileRisk:
    """Cross-session risk profile for one file path."""

    path: str
    touch_count: int = 0            # tool calls on this file (read+write family)
    edit_count: int = 0             # write-family calls only
    read_count: int = 0
    touch_session_count: int = 0    # distinct sessions that touched it
    failure_count: int = 0          # errored tool calls attributed to it
    failure_session_count: int = 0  # distinct sessions with such a failure
    failure_rate: float | None = None  # failure sessions / touch sessions; None when untracked
    interruption_count: int = 0     # user rejected/interrupted a tool on this file
    last_touch_ts: str | None = None
    last_failure_ts: str | None = None
    categories: dict[str, int] = field(default_factory=dict)
    reason: str = ""

    def to_dict(self) -> dict[str, Any]:
        return asdict(self)


@dataclass(frozen=True)
class ErrorSignature:
    """One normalised error signature recurring across sessions."""

    signature: str
    category: str
    count: int                      # total occurrences in window
    session_count: int              # distinct sessions it occurred in
    resolved_session_count: int     # sessions that moved past it (see module doc)
    first_ts: str | None
    last_ts: str | None
    top_tools: list[str] = field(default_factory=list)
    top_files: list[str] = field(default_factory=list)
    # What resolved sessions did right after the LAST occurrence — the
    # closest derivable thing to "the sessions that fixed it did Y first".
    resolution_hints: list[dict[str, Any]] = field(default_factory=list)
    example: str = ""               # raw (trimmed) first line of one occurrence
    reason: str = ""

    def to_dict(self) -> dict[str, Any]:
        return asdict(self)


@dataclass(frozen=True)
class CommandCluster:
    """Failing Bash calls grouped by normalised command head."""

    command: str
    failure_count: int
    session_count: int
    categories: dict[str, int] = field(default_factory=dict)
    last_failure_ts: str | None = None
    example: str = ""
    reason: str = ""

    def to_dict(self) -> dict[str, Any]:
        return asdict(self)


@dataclass(frozen=True)
class PatternsReport:
    """Full coding-health payload for one window."""

    window: dict[str, Any] = field(default_factory=dict)
    sources: dict[str, bool] = field(default_factory=dict)
    totals: dict[str, int] = field(default_factory=dict)
    file_risk: list[dict[str, Any]] = field(default_factory=list)
    error_signatures: list[dict[str, Any]] = field(default_factory=list)
    command_clusters: list[dict[str, Any]] = field(default_factory=list)

    def to_dict(self) -> dict[str, Any]:
        return asdict(self)


# ── internal row shapes ─────────────────────────────────────────────────────


@dataclass(frozen=True, slots=True)
class _ToolCall:
    session_id: str
    ts: str
    epoch: float
    tool_name: str
    file_path: str | None


@dataclass(frozen=True, slots=True)
class _ErrorEvent:
    session_id: str
    ts: str
    epoch: float
    category: str
    signature: str
    example: str
    tool_name: str | None
    file_path: str | None
    command: str | None


@dataclass(slots=True)
class _Collected:
    """Everything one bounded collection pass produced."""

    since_iso: str
    since_days: int
    mart_available: bool
    tool_calls: list[_ToolCall] = field(default_factory=list)
    errors: list[_ErrorEvent] = field(default_factory=list)
    interruption_count: int = 0
    interruption_sessions: set[str] = field(default_factory=set)


# ── small helpers ───────────────────────────────────────────────────────────


def _table_exists(conn: sqlite3.Connection, name: str) -> bool:
    """True when *name* is a queryable relation (table OR view — ``messages``
    is a routing view over monthly partitions post-v008)."""
    try:
        row = conn.execute(
            "SELECT 1 FROM sqlite_master "
            "WHERE type IN ('table', 'view') AND name = ? LIMIT 1",
            (name,),
        ).fetchone()
    except sqlite3.Error:
        return False
    return row is not None


def _ts_to_epoch(ts: str | None) -> float:
    """Best-effort ISO-8601 → epoch seconds; ``0.0`` on anything unparseable.

    Store timestamps mix ``...Z`` and ``...+00:00`` suffixes across
    providers, so ordering comparisons go through epoch instead of relying
    on lexicographic ISO ordering.
    """
    if not ts:
        return 0.0
    try:
        return datetime.fromisoformat(ts.replace("Z", "+00:00")).timestamp()
    except (ValueError, TypeError):
        return 0.0


def _clamp_days(since_days: int) -> int:
    try:
        days = int(since_days)
    except (TypeError, ValueError):
        return DEFAULT_SINCE_DAYS
    return max(1, min(days, MAX_SINCE_DAYS))


def _basename(path: str) -> str:
    return path.replace("\\", "/").rstrip("/").rsplit("/", 1)[-1]


_HEX_RE = re.compile(r"\b[0-9a-f]{8,}\b", re.I)
_NUM_RE = re.compile(r"\d+")
_PATH_RE = re.compile(r"(?:[A-Za-z]:)?(?:/[^\s'\":,)\]]+){2,}")
_WS_RE = re.compile(r"\s+")


def _normalise_signature(text: str) -> str:
    """Collapse an error body into a stable cross-session signature.

    First meaningful line only; absolute paths → their basename; long hex
    runs and numbers → placeholders; whitespace collapsed; truncated. Two
    occurrences of "File /a/b/foo.py:212 not found" and
    "File /x/foo.py:7 not found" normalise identically.
    """
    line = ""
    for candidate in str(text).splitlines():
        candidate = candidate.strip()
        if candidate:
            line = candidate
            break
    line = _PATH_RE.sub(lambda m: _basename(m.group(0)), line)
    line = _HEX_RE.sub("<hex>", line)
    line = _NUM_RE.sub("<n>", line)
    line = _WS_RE.sub(" ", line).strip()
    return line[:160] if line else "<empty error body>"


_ENV_ASSIGN_RE = re.compile(r"^[A-Za-z_][A-Za-z0-9_]*=\S*\s+")
_CD_PREFIX_RE = re.compile(r"^cd\s+\S+\s*&&\s*")


def _normalise_command(cmd: str) -> str:
    """Reduce a Bash command line to its cluster key ("npm install", "pytest").

    Strips leading ``cd X &&`` hops and env assignments, then keys on the
    executable basename plus — for known multi-command CLIs — the first
    non-flag subcommand token.
    """
    s = str(cmd).strip()
    for _ in range(3):  # bounded prefix stripping; malformed input can't loop
        new = _CD_PREFIX_RE.sub("", _ENV_ASSIGN_RE.sub("", s)).strip()
        if new == s:
            break
        s = new
    tokens = s.split()
    if not tokens:
        return "<empty>"
    head = _basename(tokens[0])
    sub = ""
    if head in _SUBCOMMAND_HEADS or head in _SCRIPT_HEADS:
        for tok in tokens[1:]:
            if tok.startswith("-"):
                continue
            sub = _basename(tok) if head in _SCRIPT_HEADS else tok
            break
    return f"{head} {sub}".strip()[:80]


# ── data sourcing (all bounded) ─────────────────────────────────────────────


def _project_filter(column: str, project_ids: list[int] | None) -> tuple[str, list[Any]]:
    """``(sql_fragment, params)`` for an optional project narrow.

    ``None`` means no filter; an empty list means "a filter was requested
    but matched nothing" — handled by callers before SQL is built.
    """
    if not project_ids:
        return "", []
    placeholders = ",".join("?" for _ in project_ids)
    return f"AND {column} IN ({placeholders}) ", list(project_ids)


def _load_tool_calls(
    conn: sqlite3.Connection,
    *,
    since_day: str,
    project_ids: list[int] | None,
) -> tuple[list[_ToolCall], bool]:
    """Window-bounded read of ``message_tool_mart`` → ``(calls, mart_available)``.

    ``mart_available`` is False when the mart table is missing entirely —
    callers then know touch denominators are untracked (fresh store or ETL
    never ran) and report ``failure_rate`` as ``None`` instead of a
    misleading 100%.
    """
    if not _table_exists(conn, "message_tool_mart"):
        return [], False
    sql = (
        "SELECT session_id, ts, tool_name, file_path "
        "FROM message_tool_mart "
        "WHERE day >= ? "
    )
    params: list[Any] = [since_day]
    proj_sql, proj_params = _project_filter("project_id", project_ids)
    sql += proj_sql
    params.extend(proj_params)
    # Deterministic order; (ts, message_id, call_index) is the ingest order.
    sql += "ORDER BY session_id, ts, message_id, call_index LIMIT ?"
    params.append(MAX_TOOL_ROWS)
    try:
        rows = conn.execute(sql, params).fetchall()
    except sqlite3.Error:
        return [], False
    out = [
        _ToolCall(
            session_id=r["session_id"] or "",
            ts=r["ts"] or "",
            epoch=_ts_to_epoch(r["ts"]),
            tool_name=r["tool_name"] or "",
            file_path=r["file_path"],
        )
        for r in rows
    ]
    return out, True


def _load_error_rows(
    conn: sqlite3.Connection,
    *,
    since_iso: str,
    project_ids: list[int] | None,
) -> list[sqlite3.Row]:
    """Windowed ``messages`` rows that can contain an errored tool_result.

    The LIKE screen matches both JSON spacings (``"is_error": true`` from
    the stdlib-json writer, ``"is_error":true`` from compact emitters).
    ``_`` is a single-char LIKE wildcard, so the screen is slightly wider
    than the literal — the JSON parse in ``_extract_error_events`` is the
    authoritative filter. Only these rows ever have ``raw_json`` parsed.
    """
    if not (_table_exists(conn, "messages") and _table_exists(conn, "sessions")):
        return []
    sql = (
        "SELECT m.session_fk AS session_fk, s.session_id AS session_id, "
        "       m.seq AS seq, m.timestamp AS timestamp, m.raw_json AS raw_json "
        "FROM messages m "
        "JOIN sessions s ON s.id = m.session_fk "
        "WHERE m.timestamp >= ? "
        "  AND (m.raw_json LIKE '%\"is_error\": true%' "
        "       OR m.raw_json LIKE '%\"is_error\":true%') "
    )
    params: list[Any] = [since_iso]
    proj_sql, proj_params = _project_filter("s.project_id", project_ids)
    sql += proj_sql
    params.extend(proj_params)
    sql += "ORDER BY s.session_id, m.seq LIMIT ?"
    params.append(MAX_ERROR_ROWS)
    try:
        return conn.execute(sql, params).fetchall()
    except sqlite3.Error:
        return []


def _load_interruptions(
    conn: sqlite3.Connection,
    *,
    since_iso: str,
    project_ids: list[int] | None,
) -> tuple[int, set[str]]:
    """Count interruption-marked messages in window → ``(total, sessions)``.

    Uses the classifier's exact marker prefixes against ``content_text``
    (the store keeps the flattened surface text the classifier's
    ``startswith`` check reads).
    """
    if not (_table_exists(conn, "messages") and _table_exists(conn, "sessions")):
        return 0, set()
    sql = (
        "SELECT s.session_id AS session_id, COUNT(*) AS n "
        "FROM messages m "
        "JOIN sessions s ON s.id = m.session_fk "
        "WHERE m.timestamp >= ? "
        "  AND (m.content_text LIKE ? OR m.content_text LIKE ?) "
    )
    params: list[Any] = [since_iso, INTERRUPT_PREFIX + "%", INTERRUPT_API + "%"]
    proj_sql, proj_params = _project_filter("s.project_id", project_ids)
    sql += proj_sql
    params.extend(proj_params)
    sql += "GROUP BY s.session_id"
    try:
        rows = conn.execute(sql, params).fetchall()
    except sqlite3.Error:
        return 0, set()
    total = sum(int(r["n"] or 0) for r in rows)
    return total, {r["session_id"] for r in rows if r["session_id"]}


# ── error extraction + attribution ──────────────────────────────────────────


def _error_bodies(payload: dict) -> list[tuple[str | None, str]]:
    """``[(tool_use_id, error_text), ...]`` for every errored tool_result block."""
    msg = payload.get("message")
    if not isinstance(msg, dict):
        return []
    content = msg.get("content")
    if not isinstance(content, list):
        return []
    out: list[tuple[str | None, str]] = []
    for block in content:
        if not isinstance(block, dict):
            continue
        if block.get("type") != "tool_result" or not block.get("is_error"):
            continue
        body = block.get("content", "")
        if isinstance(body, list):
            body = " ".join(
                b.get("text", "") for b in body if isinstance(b, dict)
            )
        tid = block.get("tool_use_id")
        out.append((tid if isinstance(tid, str) else None, str(body)))
    return out


class _CallResolver:
    """Match a ``tool_use_id`` back to its tool call, memoising parses.

    Walks the nearest preceding assistant-with-tools messages in the same
    session ((session_fk, seq)-indexed, one row per hop, at most
    ``_ATTRIBUTION_HOPS``) and looks the id up in their ``tools_json``.
    Parallel tool calls share one assistant message, so the memo makes a
    burst of sibling errors cost a single parse.
    """

    def __init__(self, conn: sqlite3.Connection) -> None:
        self._conn = conn
        self._parsed: dict[tuple[int, int], dict[str, dict[str, Any]]] = {}

    def resolve(
        self, session_fk: int, seq: int, tool_use_id: str | None
    ) -> dict[str, Any] | None:
        if not tool_use_id:
            return None
        for hop in range(_ATTRIBUTION_HOPS):
            try:
                row = self._conn.execute(
                    "SELECT seq, tools_json FROM messages "
                    "WHERE session_fk = ? AND seq < ? AND role = 'assistant' "
                    "  AND tools_json != '[]' "
                    "ORDER BY seq DESC LIMIT 1 OFFSET ?",
                    (session_fk, seq, hop),
                ).fetchone()
            except sqlite3.Error:
                return None
            if row is None:
                return None
            calls = self._calls_for(session_fk, int(row["seq"]), row["tools_json"])
            hit = calls.get(tool_use_id)
            if hit is not None:
                return hit
        return None

    def _calls_for(
        self, session_fk: int, seq: int, tools_json: str | None
    ) -> dict[str, dict[str, Any]]:
        key = (session_fk, seq)
        cached = self._parsed.get(key)
        if cached is not None:
            return cached
        calls: dict[str, dict[str, Any]] = {}
        try:
            entries = json.loads(tools_json) if tools_json else []
        except (json.JSONDecodeError, TypeError, ValueError):
            entries = []
        if isinstance(entries, list):
            for t in entries:
                if not isinstance(t, dict):
                    continue
                tid = t.get("id")
                if isinstance(tid, str) and tid:
                    calls[tid] = {
                        "name": t.get("name") or "Unknown",
                        "input": t.get("input") if isinstance(t.get("input"), dict) else {},
                    }
        self._parsed[key] = calls
        return calls


def _file_path_from_input(tool_input: dict[str, Any]) -> str | None:
    for key in ("file_path", "path", "notebook_path"):
        val = tool_input.get(key)
        if isinstance(val, str) and val:
            return val
    return None


def _extract_error_events(
    conn: sqlite3.Connection, rows: list[sqlite3.Row]
) -> list[_ErrorEvent]:
    """Parse pre-screened rows into attributed error events. Never raises."""
    resolver = _CallResolver(conn)
    out: list[_ErrorEvent] = []
    for row in rows:
        try:
            payload = json.loads(row["raw_json"]) if row["raw_json"] else {}
        except (json.JSONDecodeError, TypeError, ValueError):
            continue
        if not isinstance(payload, dict):
            continue
        bodies = _error_bodies(payload)
        if not bodies:
            continue  # LIKE screen false positive (literal text in a body)
        ts = row["timestamp"] or ""
        epoch = _ts_to_epoch(ts)
        for tool_use_id, text in bodies:
            call = resolver.resolve(
                int(row["session_fk"] or 0), int(row["seq"] or 0), tool_use_id
            )
            tool_name = call["name"] if call else None
            tool_input = call["input"] if call else {}
            file_path = (
                _file_path_from_input(tool_input)
                if tool_name in _TOUCH_TOOLS
                else None
            )
            command = None
            if tool_name == "Bash":
                cmd = tool_input.get("command")
                if isinstance(cmd, str) and cmd.strip():
                    command = cmd.strip()
            first_line = ""
            for candidate in text.splitlines():
                candidate = candidate.strip()
                if candidate:
                    first_line = candidate
                    break
            out.append(
                _ErrorEvent(
                    session_id=row["session_id"] or "",
                    ts=ts,
                    epoch=epoch,
                    category=_categorise(text),
                    signature=_normalise_signature(text),
                    example=first_line[:200],
                    tool_name=tool_name,
                    file_path=file_path,
                    command=command,
                )
            )
    return out


# ── collection pass ─────────────────────────────────────────────────────────


def _collect(
    conn: sqlite3.Connection,
    *,
    since_days: int,
    project_ids: list[int] | None,
    now: datetime | None,
) -> _Collected:
    """Run every bounded read for one window. Advisory: errors → empty parts."""
    days = _clamp_days(since_days)
    current = now or datetime.now(UTC)
    since_dt = current - timedelta(days=days)
    since_iso = since_dt.isoformat()
    since_day = since_iso[:10]

    collected = _Collected(since_iso=since_iso, since_days=days, mart_available=False)

    # ``project_ids == []`` means "a project filter matched nothing" — scope
    # to nothing rather than silently widening to the whole store.
    if project_ids is not None and len(project_ids) == 0:
        return collected

    try:
        collected.tool_calls, collected.mart_available = _load_tool_calls(
            conn, since_day=since_day, project_ids=project_ids
        )
    except Exception:  # noqa: BLE001 — advisory
        collected.tool_calls, collected.mart_available = [], False
    try:
        rows = _load_error_rows(conn, since_iso=since_iso, project_ids=project_ids)
        collected.errors = _extract_error_events(conn, rows)
    except Exception:  # noqa: BLE001 — advisory
        collected.errors = []
    try:
        collected.interruption_count, collected.interruption_sessions = (
            _load_interruptions(conn, since_iso=since_iso, project_ids=project_ids)
        )
    except Exception:  # noqa: BLE001 — advisory
        collected.interruption_count, collected.interruption_sessions = 0, set()
    return collected


# ── mining ──────────────────────────────────────────────────────────────────


@dataclass(slots=True)
class _FileAgg:
    touch_count: int = 0
    edit_count: int = 0
    read_count: int = 0
    touch_sessions: set[str] = field(default_factory=set)
    failure_count: int = 0
    failure_sessions: set[str] = field(default_factory=set)
    interruption_count: int = 0
    last_touch: tuple[float, str] | None = None
    last_failure: tuple[float, str] | None = None
    categories: Counter = field(default_factory=Counter)


def _build_file_map(collected: _Collected) -> dict[str, _FileAgg]:
    files: dict[str, _FileAgg] = {}
    for call in collected.tool_calls:
        if not call.file_path or call.tool_name not in _TOUCH_TOOLS:
            continue
        agg = files.setdefault(call.file_path, _FileAgg())
        agg.touch_count += 1
        if call.tool_name in _WRITE_TOOLS:
            agg.edit_count += 1
        else:
            agg.read_count += 1
        if call.session_id:
            agg.touch_sessions.add(call.session_id)
        if agg.last_touch is None or call.epoch > agg.last_touch[0]:
            agg.last_touch = (call.epoch, call.ts)
    for err in collected.errors:
        if not err.file_path:
            continue
        agg = files.setdefault(err.file_path, _FileAgg())
        agg.failure_count += 1
        if err.session_id:
            agg.failure_sessions.add(err.session_id)
        agg.categories[err.category] += 1
        if err.category == "User Interruption":
            agg.interruption_count += 1
        if agg.last_failure is None or err.epoch > agg.last_failure[0]:
            agg.last_failure = (err.epoch, err.ts)
    return files


def _file_risk_entry(path: str, agg: _FileAgg) -> FileRisk:
    # Union denominator is defensive against mart lag: a session whose
    # failure we saw but whose touch the mart hasn't materialised yet
    # still counts as a toucher, so the rate can never exceed 1.0.
    denom_sessions = agg.touch_sessions | agg.failure_sessions
    rate: float | None = None
    if agg.touch_count > 0 and denom_sessions:
        rate = round(len(agg.failure_sessions) / len(denom_sessions), 4)
    reason = _file_reason(agg, rate, len(denom_sessions))
    return FileRisk(
        path=path,
        touch_count=agg.touch_count,
        edit_count=agg.edit_count,
        read_count=agg.read_count,
        touch_session_count=len(denom_sessions),
        failure_count=agg.failure_count,
        failure_session_count=len(agg.failure_sessions),
        failure_rate=rate,
        interruption_count=agg.interruption_count,
        last_touch_ts=agg.last_touch[1] if agg.last_touch else None,
        last_failure_ts=agg.last_failure[1] if agg.last_failure else None,
        categories=dict(sorted(agg.categories.items())),
        reason=reason,
    )


def _file_reason(agg: _FileAgg, rate: float | None, denom: int) -> str:
    name_part = (
        f"Failed in {len(agg.failure_sessions)} of {denom} sessions that touched it"
        if denom
        else f"{agg.failure_count} failures recorded"
    )
    if rate is not None:
        pct = f" ({rate * 100:.0f}%)"
    elif agg.failure_count:
        pct = " (touch history untracked — rate unknown)"
    else:
        pct = ""
    sample = " — small sample" if 0 < denom < 3 else ""
    return f"{name_part}{pct}{sample}."


@dataclass(slots=True)
class _SigAgg:
    category: str = ""
    count: int = 0
    sessions: set[str] = field(default_factory=set)
    first: tuple[float, str] | None = None
    last: tuple[float, str] | None = None
    tools: Counter = field(default_factory=Counter)
    files: Counter = field(default_factory=Counter)
    example: str = ""
    # session_id → epoch of the LAST occurrence in that session.
    last_by_session: dict[str, float] = field(default_factory=dict)


def _session_timeline(collected: _Collected) -> dict[str, list[tuple[float, str, str]]]:
    """session_id → tool calls sorted by epoch (for resolution lookups).

    Tuples are ``(epoch, tool_name, file_path_or_empty)`` — plain strings in
    every slot so natural tuple ordering (which :func:`bisect_right` relies
    on) never compares ``None``.
    """
    timeline: dict[str, list[tuple[float, str, str]]] = {}
    for call in collected.tool_calls:
        if call.session_id:
            timeline.setdefault(call.session_id, []).append(
                (call.epoch, call.tool_name, call.file_path or "")
            )
    for events in timeline.values():
        events.sort()
    return timeline


def _hint_action(tool_name: str, file_path: str) -> str:
    return f"{tool_name} {_basename(file_path)}" if file_path else tool_name


def _build_signatures(
    collected: _Collected,
    timeline: dict[str, list[tuple[float, str, str]]],
) -> list[ErrorSignature]:
    sigs: dict[tuple[str, str], _SigAgg] = {}
    for err in collected.errors:
        key = (err.category, err.signature)
        agg = sigs.setdefault(key, _SigAgg(category=err.category))
        agg.count += 1
        if err.session_id:
            agg.sessions.add(err.session_id)
            prev = agg.last_by_session.get(err.session_id, 0.0)
            if err.epoch >= prev:
                agg.last_by_session[err.session_id] = err.epoch
        if agg.first is None or err.epoch < agg.first[0]:
            agg.first = (err.epoch, err.ts)
        if agg.last is None or err.epoch > agg.last[0]:
            agg.last = (err.epoch, err.ts)
        if err.tool_name:
            agg.tools[err.tool_name] += 1
        if err.file_path:
            agg.files[err.file_path] += 1
        if not agg.example:
            agg.example = err.example

    out: list[ErrorSignature] = []
    for (category, signature), agg in sigs.items():
        if len(agg.sessions) < MIN_RECURRENCE_SESSIONS:
            continue
        resolved = 0
        hints: Counter = Counter()
        for session_id, last_epoch in agg.last_by_session.items():
            events = timeline.get(session_id)
            if not events:
                continue
            # First tool call strictly after the signature's last occurrence
            # in this session: the session demonstrably moved on.
            idx = bisect_right(events, (last_epoch, "￿", "￿"))
            if idx < len(events):
                resolved += 1
                _, tool_name, file_path = events[idx]
                hints[_hint_action(tool_name, file_path)] += 1
        top_hints = [
            {"action": action, "count": n}
            for action, n in sorted(hints.items(), key=lambda kv: (-kv[1], kv[0]))[:TOP_N_HINTS]
        ]
        out.append(
            ErrorSignature(
                signature=signature,
                category=category,
                count=agg.count,
                session_count=len(agg.sessions),
                resolved_session_count=resolved,
                first_ts=agg.first[1] if agg.first else None,
                last_ts=agg.last[1] if agg.last else None,
                top_tools=[t for t, _ in sorted(agg.tools.items(), key=lambda kv: (-kv[1], kv[0]))[:3]],
                top_files=[f for f, _ in sorted(agg.files.items(), key=lambda kv: (-kv[1], kv[0]))[:3]],
                resolution_hints=top_hints,
                example=agg.example,
                reason=_signature_reason(agg, resolved, top_hints),
            )
        )
    out.sort(key=lambda s: (-s.session_count, -s.count, s.signature))
    return out


def _signature_reason(agg: _SigAgg, resolved: int, hints: list[dict[str, Any]]) -> str:
    base = (
        f"Recurred in {len(agg.sessions)} sessions ({agg.count} occurrences)."
    )
    if resolved and hints:
        return (
            f"{base} {resolved} moved past it — most often the next step was "
            f"{hints[0]['action']}."
        )
    if resolved:
        return f"{base} {resolved} moved past it."
    return f"{base} No session in window is known to have moved past it."


@dataclass(slots=True)
class _CmdAgg:
    failure_count: int = 0
    sessions: set[str] = field(default_factory=set)
    categories: Counter = field(default_factory=Counter)
    last: tuple[float, str] | None = None
    example: str = ""


def _build_command_clusters(collected: _Collected) -> list[CommandCluster]:
    cmds: dict[str, _CmdAgg] = {}
    for err in collected.errors:
        if err.tool_name != "Bash" or not err.command:
            continue
        key = _normalise_command(err.command)
        agg = cmds.setdefault(key, _CmdAgg())
        agg.failure_count += 1
        if err.session_id:
            agg.sessions.add(err.session_id)
        agg.categories[err.category] += 1
        if agg.last is None or err.epoch > agg.last[0]:
            agg.last = (err.epoch, err.ts)
        if not agg.example:
            agg.example = err.command[:120]

    out: list[CommandCluster] = []
    for command, agg in cmds.items():
        if agg.failure_count < 2:
            continue  # a single failure isn't a cluster
        top_cat = min(
            agg.categories.items(), key=lambda kv: (-kv[1], kv[0])
        )[0] if agg.categories else "Other"
        out.append(
            CommandCluster(
                command=command,
                failure_count=agg.failure_count,
                session_count=len(agg.sessions),
                categories=dict(sorted(agg.categories.items())),
                last_failure_ts=agg.last[1] if agg.last else None,
                example=agg.example,
                reason=(
                    f"{agg.failure_count} failures across {len(agg.sessions)} "
                    f"session{'s' if len(agg.sessions) != 1 else ''}; mostly {top_cat}."
                ),
            )
        )
    out.sort(key=lambda c: (-c.failure_count, -c.session_count, c.command))
    return out


# ── public entry points ─────────────────────────────────────────────────────


def mine_patterns(
    conn: sqlite3.Connection,
    *,
    since_days: int = DEFAULT_SINCE_DAYS,
    project_ids: list[int] | None = None,
    now: datetime | None = None,
    top_files: int = TOP_N_FILES,
    top_signatures: int = TOP_N_SIGNATURES,
    top_commands: int = TOP_N_COMMANDS,
) -> dict[str, Any]:
    """Mine cross-session patterns over the window and return a dict report.

    Args:
        conn: Open store connection. Read-only use; every table access is
            ``sqlite_master``-guarded so a bare/empty DB yields an empty report.
        since_days: Window size in days (clamped to 1..:data:`MAX_SINCE_DAYS`).
        project_ids: Optional ``projects.id`` narrow. ``None`` = whole store;
            ``[]`` = a requested filter that matched nothing (empty report).
        now: Injectable clock for deterministic tests.
        top_files / top_signatures / top_commands: List caps.

    Returns:
        :meth:`PatternsReport.to_dict` — always well-formed, never raises.
    """
    try:
        collected = _collect(
            conn, since_days=since_days, project_ids=project_ids, now=now
        )
    except Exception:  # noqa: BLE001 — advisory: never raise from the report
        days = _clamp_days(since_days)
        current = now or datetime.now(UTC)
        collected = _Collected(
            since_iso=(current - timedelta(days=days)).isoformat(),
            since_days=days,
            mart_available=False,
        )

    try:
        return _assemble(
            collected,
            top_files=top_files,
            top_signatures=top_signatures,
            top_commands=top_commands,
        )
    except Exception:  # noqa: BLE001 — advisory: degrade to an empty report
        return PatternsReport(
            window={"since": collected.since_iso, "days": collected.since_days},
            sources={"message_tool_mart": collected.mart_available},
            totals=_empty_totals(),
        ).to_dict()


def _empty_totals() -> dict[str, int]:
    return {
        "tool_call_count": 0,
        "error_count": 0,
        "attributed_error_count": 0,
        "interruption_count": 0,
        "interruption_session_count": 0,
        "session_count": 0,
        "sessions_with_failures": 0,
        "files_touched": 0,
    }


def _assemble(
    collected: _Collected,
    *,
    top_files: int,
    top_signatures: int,
    top_commands: int,
) -> dict[str, Any]:
    files = _build_file_map(collected)
    timeline = _session_timeline(collected)
    signatures = _build_signatures(collected, timeline)
    clusters = _build_command_clusters(collected)

    risk_entries = [
        _file_risk_entry(path, agg)
        for path, agg in files.items()
        if agg.failure_count > 0
    ]
    risk_entries.sort(
        key=lambda f: (
            -f.failure_session_count,
            -(f.failure_rate or 0.0),
            -f.failure_count,
            f.path,
        )
    )

    sessions_seen = {
        *(c.session_id for c in collected.tool_calls if c.session_id),
        *(e.session_id for e in collected.errors if e.session_id),
        *collected.interruption_sessions,
    }
    totals = {
        "tool_call_count": len(collected.tool_calls),
        "error_count": len(collected.errors),
        "attributed_error_count": sum(
            1 for e in collected.errors if e.tool_name is not None
        ),
        "interruption_count": collected.interruption_count,
        "interruption_session_count": len(collected.interruption_sessions),
        "session_count": len(sessions_seen),
        "sessions_with_failures": len(
            {e.session_id for e in collected.errors if e.session_id}
        ),
        "files_touched": sum(1 for agg in files.values() if agg.touch_count > 0),
    }

    report = PatternsReport(
        window={"since": collected.since_iso, "days": collected.since_days},
        sources={"message_tool_mart": collected.mart_available},
        totals=totals,
        file_risk=[f.to_dict() for f in risk_entries[: max(0, top_files)]],
        error_signatures=[s.to_dict() for s in signatures[: max(0, top_signatures)]],
        command_clusters=[c.to_dict() for c in clusters[: max(0, top_commands)]],
    )
    return report.to_dict()


def file_risk(
    conn: sqlite3.Connection,
    path: str,
    *,
    since_days: int = DEFAULT_SINCE_DAYS,
    project_ids: list[int] | None = None,
    now: datetime | None = None,
) -> dict[str, Any]:
    """Per-file risk lookup for programmatic consumers (campaign #5's hook).

    Same bounded collection as :func:`mine_patterns`, returned as ONE
    :class:`FileRisk` dict for *path*. Resolution order: exact path match,
    then a unique suffix match (an absolute store path ending in ``/path``
    — hooks often see repo-relative paths). No match → a well-formed
    zero entry (``failure_rate`` ``None``). Advisory: never raises.
    """
    zero = FileRisk(path=path, reason="No activity recorded in window.")
    try:
        collected = _collect(
            conn, since_days=since_days, project_ids=project_ids, now=now
        )
        files = _build_file_map(collected)
        agg = files.get(path)
        if agg is not None:
            return _file_risk_entry(path, agg).to_dict()
        if path:
            suffix = "/" + path.lstrip("/")
            candidates = sorted(p for p in files if p.endswith(suffix))
            if len(candidates) == 1:
                return _file_risk_entry(candidates[0], files[candidates[0]]).to_dict()
    except Exception:  # noqa: BLE001 — advisory: never raise from the lookup
        return zero.to_dict()
    return zero.to_dict()
