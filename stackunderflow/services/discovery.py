"""Discovery service — make StackUnderflow self-referential for coding agents.

Three pure functions take the main store connection plus arguments and
return ``list[SessionMatch]``. Used by:

* The CLI commands ``find-sessions-in-path``, ``find-sessions-touching-file``,
  and ``search-past-decisions``.
* The MCP server's discovery tools.
* Skill files shipped with Claude Code.

Design notes
------------
* No FTS dependency. The auxiliary ``search_index.db`` (populated on
  demand by ``SearchService``) is *not* connected here — the contract
  is that callers pass the main store and we work with whatever's in
  it. ``messages.content_text`` is queried via plain ``LIKE``;
  ``snippet`` excerpts are computed in Python.
* No write paths. Every query is read-only.
* Uses ``session_mart`` for cost when populated (post-Wave 4B
  backfill), falls back to ``0.0`` otherwise.
* Project filesystem path: ``projects.path`` is preferred; when null
  (the writer leaves it null today) we decode the slug back to an
  absolute path. The decode is best-effort because the slug format
  is lossy (``_`` and ``-`` both collapse to ``-``).
"""

from __future__ import annotations

import json
import re
import sqlite3
from dataclasses import asdict, dataclass
from datetime import UTC, datetime, timedelta
from pathlib import Path
from typing import Any

from stackunderflow.services import discovery_telemetry as _telemetry

__all__ = [
    "SessionMatch",
    "find_sessions_in_path",
    "find_sessions_touching_file",
    "search_past_decisions",
    "parse_since",
    "decode_slug_to_path",
]


# ── data shape ──────────────────────────────────────────────────────────────


@dataclass(frozen=True)
class SessionMatch:
    """A session that matched a discovery query.

    ``snippet`` is only populated by ``search_past_decisions`` (the only
    query whose contract includes a content excerpt). The other two
    discovery functions leave it ``None``.
    """

    session_id: str
    project_slug: str
    project_path: str
    provider: str
    first_ts: str
    last_ts: str
    message_count: int
    cost_usd: float
    snippet: str | None = None

    def to_dict(self) -> dict[str, Any]:
        return asdict(self)


# ── shared helpers ──────────────────────────────────────────────────────────


def decode_slug_to_path(slug: str) -> str:
    """Best-effort reconstruct an absolute filesystem path from a project slug.

    The Claude/Codex/Cursor slug convention encodes
    ``/Users/foo/dev/proj`` as ``-Users-foo-dev-proj`` — leading slash
    becomes leading ``-``, then every separator is a ``-``. Underscores
    in the original path collapse to ``-`` too, so the decode is lossy:
    ``-Users-foo-my-proj`` could be either ``/Users/foo/my-proj`` or
    ``/Users/foo/my_proj``. We return the ``-``-form which is what the
    matching loop will compare against the resolved caller path.
    """
    if not slug:
        return ""
    if not slug.startswith("-"):
        # Provider-specific slug shapes (e.g. cursor's workspace ids)
        # don't decode to a filesystem path. Returning empty signals "no
        # path mapping available" to the matcher.
        return ""
    return "/" + slug.lstrip("-").replace("-", "/")


_SINCE_RELATIVE_RE = re.compile(r"^\s*(\d+)\s*([dwmh])\s*$", re.IGNORECASE)


def parse_since(since: str | None) -> str | None:
    """Convert a relative or ISO ``since`` string to an ISO timestamp.

    Accepts ``"7d"``, ``"1w"``, ``"1m"``, ``"24h"`` (relative to now,
    UTC) or any ISO-8601 datetime/date string. Returns ``None`` for
    ``None`` so callers can pass it straight through.

    Raises ``ValueError`` on an unrecognised string.
    """
    if since is None:
        return None
    s = since.strip()
    if not s:
        return None

    m = _SINCE_RELATIVE_RE.match(s)
    if m:
        n = int(m.group(1))
        unit = m.group(2).lower()
        # weeks/months are convenience aliases — month == 30 days, not
        # calendar months. Documented in the CLI help.
        delta = {
            "h": timedelta(hours=n),
            "d": timedelta(days=n),
            "w": timedelta(weeks=n),
            "m": timedelta(days=30 * n),
        }[unit]
        return (datetime.now(UTC) - delta).isoformat()

    # Fall through: try ISO. ``fromisoformat`` accepts both ``YYYY-MM-DD``
    # and full datetime variants on Python 3.11+. Date-only strings
    # become midnight UTC so the comparison column (``messages.timestamp``
    # / ``sessions.last_ts``) sorts correctly.
    try:
        parsed = datetime.fromisoformat(s)
    except ValueError as exc:
        raise ValueError(
            f"Invalid since value {s!r}: expected '7d'/'1w'/'1m'/'24h' "
            f"or an ISO date/datetime."
        ) from exc
    if parsed.tzinfo is None:
        parsed = parsed.replace(tzinfo=UTC)
    return parsed.isoformat()


def _project_fs_path(stored_path: str | None, slug: str) -> str:
    """Return the canonical filesystem path for a project row.

    Prefers ``projects.path`` when populated; otherwise reconstructs from
    the slug. ``stored_path`` is what the writer persisted (currently
    always ``NULL`` in production but the column is there for adapters
    that have a real cwd).
    """
    if stored_path:
        return stored_path
    return decode_slug_to_path(slug)


def _is_ancestor(ancestor: str, descendant: str) -> bool:
    """True if ``ancestor`` is ``descendant`` itself or a directory
    ancestor of it.

    Pure path arithmetic — no filesystem access. Compares on resolved
    POSIX strings; a trailing ``/`` boundary check keeps
    ``/foo/bar`` from matching ``/foo/barbecue``.
    """
    if not ancestor or not descendant:
        return False
    a = ancestor.rstrip("/")
    d = descendant.rstrip("/")
    if a == d:
        return True
    return d.startswith(a + "/")


def _resolve_input_path(path: str | Path) -> str:
    """Resolve the caller's path to an absolute string.

    ``Path.resolve(strict=False)`` works whether or not the path
    exists on disk — we want this because tests / agents may query
    paths that have been deleted or never existed locally.
    """
    return str(Path(path).expanduser().resolve(strict=False))


# Columns we read from the joined sessions ⨯ projects ⨯ session_mart
# triple. Kept in one place so the row → SessionMatch mapper at the
# bottom doesn't drift from the SQL.
_SESSION_SELECT = (
    "  s.session_id           AS session_id,"
    "  p.slug                 AS project_slug,"
    "  p.path                 AS stored_path,"
    "  p.provider             AS provider,"
    "  s.first_ts             AS first_ts,"
    "  s.last_ts              AS last_ts,"
    "  s.message_count        AS message_count,"
    "  COALESCE(sm.cost_usd, 0.0) AS cost_usd"
)
_SESSION_FROM = (
    "FROM sessions s "
    "JOIN projects p ON p.id = s.project_id "
    "LEFT JOIN session_mart sm ON sm.session_id = s.session_id"
)


def _row_to_match(row: sqlite3.Row, snippet: str | None = None) -> SessionMatch:
    return SessionMatch(
        session_id=row["session_id"],
        project_slug=row["project_slug"],
        project_path=_project_fs_path(row["stored_path"], row["project_slug"]),
        provider=row["provider"],
        first_ts=row["first_ts"] or "",
        last_ts=row["last_ts"] or "",
        message_count=int(row["message_count"] or 0),
        cost_usd=float(row["cost_usd"] or 0.0),
        snippet=snippet,
    )


def _ensure_row_factory(conn: sqlite3.Connection) -> None:
    """Discovery code accesses columns by name; force a Row factory.

    Idempotent: only sets the factory if it's still the default. Tests
    that pre-set a factory (e.g. an in-memory connection in CLI tests)
    are honoured.
    """
    if conn.row_factory is None:
        conn.row_factory = sqlite3.Row


# ── public API ──────────────────────────────────────────────────────────────


def _record_loaded(
    conn: sqlite3.Connection,
    command: str,
    matches: list[SessionMatch],
) -> None:
    """Citation-feedback telemetry hook — bump ``loaded_count`` for the
    sessions this discovery call surfaced.

    Gated behind ``STACKUNDERFLOW_DISCOVERY_TELEMETRY`` (default on) and
    best-effort inside ``discovery_telemetry.record_loaded`` — a write
    failure never propagates out of the discovery query. Lifted to a
    one-liner so the three ``find_*`` bodies stay readable.
    """
    if not matches:
        return
    _telemetry.record_loaded(conn, command, [m.session_id for m in matches])


def find_sessions_in_path(
    conn: sqlite3.Connection,
    path: str | Path,
    *,
    since: str | None = None,
    limit: int = 20,
    provider: str | None = None,
) -> list[SessionMatch]:
    """Sessions whose project path is ``path`` or any ancestor of ``path``.

    The caller's ``path`` is resolved to an absolute string. We then
    scan all projects and keep those whose ``project_path`` is a prefix
    of the resolved path (project as ancestor of caller). So calling
    with ``/Users/x/dev/proj/src/foo`` returns the project rooted at
    ``/Users/x/dev/proj``.

    Parameters
    ----------
    conn:
        Main store connection (``~/.stackunderflow/store.db``).
    path:
        Filesystem path the agent is working in.
    since:
        Optional cutoff. ``"7d"`` / ``"1w"`` / ``"1m"`` / ``"24h"`` or
        an ISO date/datetime. Filters by ``sessions.last_ts``.
    limit:
        Max rows returned. Negative or zero means no limit.
    provider:
        Optional provider slug filter (e.g. ``"claude"``).

    Returns
    -------
    Sessions sorted by ``last_ts DESC``; ``snippet`` is always ``None``.
    """
    _ensure_row_factory(conn)
    resolved = _resolve_input_path(path)

    project_rows = conn.execute(
        "SELECT id, provider, slug, path FROM projects"
    ).fetchall()

    # Filter project rows in Python — slug decoding is too irregular to
    # express as a single ``WHERE LIKE``. Path string is small (~150
    # projects on the maintainer's real store), so this is O(N) but N
    # is tiny.
    matched_ids: list[int] = []
    for prow in project_rows:
        if provider and prow["provider"] != provider:
            continue
        fs_path = _project_fs_path(prow["path"], prow["slug"])
        if not fs_path:
            continue
        if _is_ancestor(fs_path, resolved):
            matched_ids.append(int(prow["id"]))

    if not matched_ids:
        return []

    since_iso = parse_since(since)

    placeholders = ",".join("?" for _ in matched_ids)
    where_extra = ""
    params: list[Any] = list(matched_ids)
    if since_iso:
        where_extra = " AND s.last_ts >= ?"
        params.append(since_iso)

    sql = (
        "SELECT "
        + _SESSION_SELECT
        + " "
        + _SESSION_FROM
        + f" WHERE s.project_id IN ({placeholders})"
        + where_extra
        + " ORDER BY s.last_ts DESC"
    )
    if limit and limit > 0:
        sql += " LIMIT ?"
        params.append(int(limit))

    rows = conn.execute(sql, params).fetchall()
    result = [_row_to_match(r) for r in rows]
    _record_loaded(conn, "find_sessions_in_path", result)
    return result


# ── tools-json filtering ────────────────────────────────────────────────────
#
# Read tool / Edit tool / Write tool calls are persisted in
# ``messages.tools_json`` as a JSON array. Each element is provider-
# shaped; our access patterns only need the tool name + the file_path
# argument (when present). We do substring matching on the JSON text
# because (a) it works for every provider without a per-provider
# parser and (b) SQLite's ``LIKE`` over a small JSON blob is fast
# enough for the expected dataset sizes (tens of millions of messages
# in the worst case, indexed by session_fk anyway).

_READ_TOOL_NAMES = ("Read",)
_WRITE_TOOL_NAMES = ("Edit", "Write", "MultiEdit", "NotebookEdit")
_ANY_TOOL_NAMES = _READ_TOOL_NAMES + _WRITE_TOOL_NAMES


def _tools_json_mentions_file(
    tools_json: str | None,
    *,
    file_path: str,
    mode: str,
) -> bool:
    """Inspect a row's ``tools_json`` blob for a file mention.

    ``mode='read'`` only counts Read tool args; ``mode='write'`` only
    Edit/Write/MultiEdit args; ``mode='any'`` counts any of those tools
    or a free-form mention in the arg dict.
    """
    if not tools_json or tools_json == "[]":
        return False
    try:
        tools = json.loads(tools_json)
    except (json.JSONDecodeError, ValueError):
        return False
    if not isinstance(tools, list):
        return False

    if mode == "read":
        wanted = _READ_TOOL_NAMES
    elif mode == "write":
        wanted = _WRITE_TOOL_NAMES
    else:
        wanted = _ANY_TOOL_NAMES

    for entry in tools:
        if not isinstance(entry, dict):
            continue
        name = entry.get("name") or entry.get("tool") or ""
        if name not in wanted:
            continue
        # Common arg shapes: {"input": {...}} or top-level args.
        candidate = entry.get("input") or entry.get("arguments") or entry
        if isinstance(candidate, dict):
            for key in ("file_path", "path", "filename", "notebook_path"):
                v = candidate.get(key)
                if isinstance(v, str) and file_path in v:
                    return True
        # Last-ditch substring match against the serialised entry.
        try:
            if file_path in json.dumps(entry):
                return True
        except (TypeError, ValueError):
            continue
    return False


def find_sessions_touching_file(
    conn: sqlite3.Connection,
    file_path: str | Path,
    *,
    limit: int = 20,
    mode: str = "any",
) -> list[SessionMatch]:
    """Sessions where ``file_path`` shows up in tools or message content.

    ``mode``
        * ``"read"`` — only sessions where ``file_path`` appears as an
          argument to a Read tool call.
        * ``"write"`` — Edit / Write / MultiEdit / NotebookEdit args.
        * ``"any"`` (default) — any of the above OR a free-form mention
          in ``messages.content_text``.

    The match is substring-based on the resolved absolute path. Sessions
    are returned sorted by ``last_ts DESC``.
    """
    if mode not in {"read", "write", "any"}:
        raise ValueError(
            f"mode must be 'read', 'write', or 'any'; got {mode!r}"
        )
    _ensure_row_factory(conn)
    resolved = _resolve_input_path(file_path)

    # Stage 1: cheap substring filter at SQL level so we don't pull
    # every message into Python. ``content_text`` LIKE plus
    # ``tools_json`` LIKE catches every potential hit; the Python
    # second pass refines the tools-mode filtering.
    pattern = f"%{resolved}%"
    if mode == "any":
        sql_filter = "(m.tools_json LIKE ? OR m.content_text LIKE ?)"
        sql_params: list[Any] = [pattern, pattern]
    else:
        sql_filter = "m.tools_json LIKE ?"
        sql_params = [pattern]

    rows = conn.execute(
        "SELECT s.id AS sfk, m.tools_json, m.content_text "  # noqa: S608 — sql_filter is a fixed literal selected by mode
        "FROM messages m "
        "JOIN sessions s ON s.id = m.session_fk "
        f"WHERE {sql_filter}",
        sql_params,
    ).fetchall()

    # Group hits per-session, applying the mode-specific tool-match
    # check. Sessions that only had a free-form content_text mention
    # are kept only when ``mode == 'any'``.
    matched_session_fks: set[int] = set()
    for row in rows:
        sfk = int(row["sfk"])
        if sfk in matched_session_fks:
            continue
        tools_json = row["tools_json"]
        if mode in {"read", "write"}:
            if _tools_json_mentions_file(
                tools_json, file_path=resolved, mode=mode
            ):
                matched_session_fks.add(sfk)
            continue
        # mode == "any"
        if _tools_json_mentions_file(
            tools_json, file_path=resolved, mode="any"
        ):
            matched_session_fks.add(sfk)
        else:
            content = row["content_text"] or ""
            if resolved in content:
                matched_session_fks.add(sfk)

    if not matched_session_fks:
        return []

    placeholders = ",".join("?" for _ in matched_session_fks)
    sql = (
        "SELECT "
        + _SESSION_SELECT
        + " "
        + _SESSION_FROM
        + f" WHERE s.id IN ({placeholders}) ORDER BY s.last_ts DESC"
    )
    params: list[Any] = list(matched_session_fks)
    if limit and limit > 0:
        sql += " LIMIT ?"
        params.append(int(limit))
    rows2 = conn.execute(sql, params).fetchall()
    result = [_row_to_match(r) for r in rows2]
    _record_loaded(conn, "find_sessions_touching_file", result)
    return result


# ── search past decisions ───────────────────────────────────────────────────


_SNIPPET_RADIUS = 100  # characters either side of the match


def _build_snippet(content: str, query: str) -> str | None:
    """Return a ~200-char excerpt around the first case-insensitive hit.

    Falls back to a leading slice when the query happens to span a
    boundary the substring search misses (rare; defensive). Newlines
    are collapsed so the result fits one display line.
    """
    if not content:
        return None
    haystack = content
    needle = query
    idx = haystack.lower().find(needle.lower())
    if idx < 0:
        excerpt = haystack[:_SNIPPET_RADIUS * 2]
    else:
        start = max(0, idx - _SNIPPET_RADIUS)
        end = min(len(haystack), idx + len(needle) + _SNIPPET_RADIUS)
        excerpt = haystack[start:end]
        if start > 0:
            excerpt = "…" + excerpt
        if end < len(haystack):
            excerpt = excerpt + "…"
    return " ".join(excerpt.split())


def search_past_decisions(
    conn: sqlite3.Connection,
    query: str,
    *,
    project: str | None = None,
    since: str | None = None,
    limit: int = 20,
) -> list[SessionMatch]:
    """Substring search over ``messages.content_text``.

    The store does not currently host an FTS5 virtual table for
    ``content_text`` (the auxiliary ``search_index.db`` does, but it's
    a separate connection). We therefore use ``LIKE`` here and
    generate snippets in Python — fast enough for the expected query
    cardinality (one query per agent invocation) and free of
    cross-database wiring.

    Parameters
    ----------
    conn:
        Main store connection.
    query:
        Free-form text. Empty/whitespace strings return no matches.
    project:
        Optional ``projects.slug`` filter.
    since:
        Same accepted forms as ``find_sessions_in_path``.
    limit:
        Max rows returned. Sorted by ``last_ts DESC`` so the most
        recent matching session is first.
    """
    _ensure_row_factory(conn)
    if not query or not query.strip():
        return []

    needle = query.strip()
    since_iso = parse_since(since)

    where_extra = ""
    params: list[Any] = [f"%{needle}%"]
    if project:
        where_extra += " AND p.slug = ?"
        params.append(project)
    if since_iso:
        where_extra += " AND m.timestamp >= ?"
        params.append(since_iso)

    # We need (a) one row per session for the SessionMatch, plus (b)
    # the first matching content_text per session for snippet
    # generation. SQLite's window functions would solve this in one
    # query but we keep the SQL portable: pull
    # (session_fk, content_text) hits sorted by timestamp DESC, dedup
    # in Python keeping the first hit per session.
    hit_rows = conn.execute(
        "SELECT m.session_fk AS sfk, m.content_text AS content_text "  # noqa: S608 — where_extra is built from fixed clauses + parameter placeholders
        "FROM messages m "
        "JOIN sessions s ON s.id = m.session_fk "
        "JOIN projects p ON p.id = s.project_id "
        f"WHERE m.content_text LIKE ?{where_extra} "
        "ORDER BY m.timestamp DESC",
        params,
    ).fetchall()

    snippet_by_sfk: dict[int, str | None] = {}
    for hr in hit_rows:
        sfk = int(hr["sfk"])
        if sfk in snippet_by_sfk:
            continue
        snippet_by_sfk[sfk] = _build_snippet(hr["content_text"] or "", needle)

    if not snippet_by_sfk:
        return []

    placeholders = ",".join("?" for _ in snippet_by_sfk)
    sql = (
        "SELECT "
        + _SESSION_SELECT
        + ", s.id AS session_fk "
        + _SESSION_FROM
        + f" WHERE s.id IN ({placeholders}) ORDER BY s.last_ts DESC"
    )
    rows = conn.execute(sql, list(snippet_by_sfk.keys())).fetchall()

    out: list[SessionMatch] = []
    for r in rows:
        out.append(_row_to_match(r, snippet=snippet_by_sfk.get(int(r["session_fk"]))))
        if limit and limit > 0 and len(out) >= limit:
            break
    _record_loaded(conn, "search_past_decisions", out)
    return out
