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
from collections.abc import Sequence
from dataclasses import asdict, dataclass
from datetime import UTC, datetime, timedelta
from pathlib import Path
from typing import Any

__all__ = [
    "SessionMatch",
    "OutcomeMatch",
    "find_sessions_in_path",
    "find_sessions_touching_file",
    "search_past_decisions",
    "find_sessions_where_action_worked",
    "find_failure_modes_for_file",
    "parse_since",
    "decode_slug_to_path",
    "load_messages_for_project",
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


@dataclass(frozen=True, kw_only=True)
class OutcomeMatch(SessionMatch):
    """A discovery match annotated with an inferred outcome.

    Extends :class:`SessionMatch` with three fields that say whether the
    matched action *worked*, and the evidence for that judgement. The
    inherited ``snippet`` stays ``None`` for outcome queries — the
    evidence string carries the relevant excerpt instead.

    ``outcome`` is one of:

    * ``"worked"``    — a following user turn confirmed success, or the
      session continued/ended with no revert and no complaint.
    * ``"failed"``    — a following user turn reported it broke / was
      wrong / wasn't what was asked.
    * ``"reverted"``  — the change was undone (the user asked, or the
      agent ran ``git revert`` / ``git reset --hard`` / ``git checkout --``).
    * ``"uncertain"`` — the action was the last recorded turn, or the
      follow-up turns gave no clear signal.

    The new fields are keyword-only so they can follow ``SessionMatch``'s
    defaulted ``snippet`` without dataclass field-ordering complaints.
    """

    outcome: str            # "worked" | "failed" | "reverted" | "uncertain"
    outcome_evidence: str   # short human-readable justification + msg ref
    outcome_msg_id: int     # id of the message that established the outcome


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
    return [_row_to_match(r) for r in rows]


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
    return [_row_to_match(r) for r in rows2]


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
    return out


# ── outcome-aware discovery ──────────────────────────────────────────────────
#
# The three functions above answer "which sessions touched X". These two
# answer "which sessions touched X *and it worked* / *and it broke*" — a
# qualitatively different signal. We don't store outcomes; we infer them
# by walking forward from the message that performed the action and
# reading the next few user turns for confirmation / complaint / revert.
#
# No schema change: ``messages.role`` + ``seq`` + ``tools_json`` +
# ``content_text`` + ``is_sidechain`` are all we need.
#
# This is heuristic. False positives (especially "silence ⇒ worked") are
# expected; the keyword lists and the lookahead window are deliberately
# kept in one place so they can be tuned against a real store later.


# Keyword classifiers. One module-level dict so they can be tuned or
# localised later — the initial set is English-only. Phrases are matched
# case-insensitively on word boundaries (so bare ``no`` doesn't fire on
# "another" / "node" / "notes"); ``revert`` wins over ``negative`` wins
# over ``positive`` when more than one class matches a message.
OUTCOME_KEYWORDS: dict[str, tuple[str, ...]] = {
    "revert": (
        "undo", "undo that", "undo it",
        "revert", "revert that", "revert it",
        "roll back", "rollback", "roll that back",
        "take that back", "back it out", "back that out",
        "git revert", "git reset --hard", "git checkout --",
    ),
    "negative": (
        "no", "nope",
        "that broke", "you broke", "broke it", "broke the build", "broke the tests",
        "still broken", "still failing", "still fails", "still errors",
        "doesn't work", "does not work", "didn't work", "did not work",
        "not working", "isn't working", "won't work", "wont work", "stopped working",
        "wrong", "that's wrong", "thats wrong", "incorrect",
        "not what i asked", "not what i wanted", "not what i meant",
        "that's not right", "thats not right", "that's not it", "thats not it",
        "no good", "doesn't help", "didn't help",
    ),
    "positive": (
        "thanks", "thank you", "thx",
        "that worked", "it worked", "works now", "working now",
        "that works", "it works", "works great", "works perfectly",
        "perfect", "nice", "great", "awesome", "excellent",
        "ship it", "lgtm", "looks good", "looks great", "love it",
        "exactly right", "that's it", "thats it", "nailed it",
    ),
}

# Benign ``no <noun>`` bigrams — "no problem", "no worries" etc. are not
# complaints, so a bare-``no`` negative match immediately followed by one
# of these is suppressed (we keep scanning for a real signal).
_BENIGN_NO_SUFFIXES = (
    "problem", "problems", "worries", "worry", "rush", "biggie", "prob",
    "issue", "issues", "need", "need to", "doubt",
)

# Bash commands that revert work — matched as substrings inside a
# whitespace-normalised Bash tool-call ``command`` arg.
_REVERT_COMMAND_PATTERNS = (
    "git revert",
    "git reset --hard",
    "git reset --merge",
    "git reset head",
    "git checkout --",
    "git checkout .",
    "git restore ",
    "git stash",
)

# Tool names (across providers) that run a shell command.
_SHELL_TOOL_NAMES = ("Bash", "shell", "run_command", "execute_command", "RunCommand")

# How many follow-up user turns to inspect past the anchor before giving
# up. Open question in the spec — start at 5, tune empirically.
_OUTCOME_LOOKAHEAD = 5


def _compile_keyword_re(words: tuple[str, ...]) -> re.Pattern[str]:
    """Build a word-boundary alternation regex, longest phrase first."""
    parts = sorted({w.strip() for w in words if w.strip()}, key=len, reverse=True)
    return re.compile(
        r"\b(?:" + "|".join(re.escape(p) for p in parts) + r")\b",
        re.IGNORECASE,
    )


_KW_RE: dict[str, re.Pattern[str]] = {
    klass: _compile_keyword_re(words) for klass, words in OUTCOME_KEYWORDS.items()
}


def _trim_inline(text: str, limit: int) -> str:
    """Collapse whitespace and clip to ``limit`` chars with an ellipsis."""
    one_line = " ".join(text.split())
    if len(one_line) <= limit:
        return one_line
    return one_line[: max(1, limit - 1)].rstrip() + "…"


def _classify_user_text(text: str) -> str | None:
    """Classify a single user message.

    Returns ``"revert"`` / ``"negative"`` / ``"positive"`` for the first
    class that fires, or ``None`` if nothing matched. Precedence: revert >
    negative > positive (a "thanks, but revert that" is a revert request,
    not approval).
    """
    if not text:
        return None
    if _KW_RE["revert"].search(text):
        return "revert"
    for m in _KW_RE["negative"].finditer(text):
        if m.group(0).lower() == "no":
            tail = text[m.end():m.end() + 16].lower().lstrip(" ,.!:;-")
            if tail.startswith(_BENIGN_NO_SUFFIXES):
                continue  # "no problem" / "no worries" — not a complaint
        return "negative"
    if _KW_RE["positive"].search(text):
        return "positive"
    return None


def _row_value(row: Any, key: str, default: Any = None) -> Any:
    """Read ``key`` from a ``sqlite3.Row`` / dict-ish row, tolerating absence."""
    try:
        return row[key]
    except (IndexError, KeyError, TypeError):
        if isinstance(row, dict):
            return row.get(key, default)
        return default


def _is_sidechain_row(row: Any) -> bool:
    return bool(_row_value(row, "is_sidechain", 0))


def _revert_command_in_tools(tools_json: str | None) -> str | None:
    """Return the first revert-y shell command in ``tools_json``, or ``None``."""
    if not tools_json or tools_json == "[]":
        return None
    try:
        tools = json.loads(tools_json)
    except (json.JSONDecodeError, ValueError):
        return None
    if not isinstance(tools, list):
        return None
    for entry in tools:
        if not isinstance(entry, dict):
            continue
        name = entry.get("name") or entry.get("tool") or ""
        if name not in _SHELL_TOOL_NAMES:
            continue
        candidate = entry.get("input") or entry.get("arguments") or entry
        cmd = ""
        if isinstance(candidate, dict):
            cmd = candidate.get("command") or candidate.get("cmd") or ""
        if not isinstance(cmd, str) or not cmd:
            continue
        norm = " ".join(cmd.lower().split())
        if any(pat in norm for pat in _REVERT_COMMAND_PATTERNS):
            return cmd
    return None


def _classify_outcome(
    messages: Sequence[Any],
    anchor_idx: int,
    lookahead: int = _OUTCOME_LOOKAHEAD,
) -> tuple[str, str, int]:
    """Infer the outcome of the action at ``messages[anchor_idx]``.

    ``messages`` must be the session's rows in conversation order (sorted
    by ``seq``); each row exposes ``id``, ``role``, ``content_text``,
    ``tools_json`` and ``is_sidechain``. We walk strictly forward from the
    anchor, skipping sidechain rows (a Task sub-agent's transcript doesn't
    speak for the parent session), and look at up to ``lookahead`` real
    user turns plus any agent revert command in between.

    Returns ``(outcome, evidence, outcome_msg_id)``. ``outcome`` is one of
    ``"worked"`` / ``"failed"`` / ``"reverted"`` / ``"uncertain"``.
    """
    anchor_id = int(_row_value(messages[anchor_idx], "id", 0) or 0)

    tail = [m for m in list(messages)[anchor_idx + 1:] if not _is_sidechain_row(m)]
    if not tail:
        return (
            "uncertain",
            "action is the last recorded turn in the session — no follow-up to judge",
            anchor_id,
        )

    user_turns_seen = 0
    last_user_id = anchor_id
    for m in tail:
        mid = int(_row_value(m, "id", 0) or 0)
        role = str(_row_value(m, "role", "") or "").lower()
        if role == "assistant":
            cmd = _revert_command_in_tools(_row_value(m, "tools_json"))
            if cmd is not None:
                return (
                    "reverted",
                    f"agent ran `{_trim_inline(cmd, 120)}` after the action",
                    mid,
                )
            continue
        if role != "user":
            continue
        text = str(_row_value(m, "content_text", "") or "").strip()
        if not text:
            # A tool-result user message — not a real turn. Walk further.
            continue
        klass = _classify_user_text(text)
        if klass is None:
            user_turns_seen += 1
            last_user_id = mid
            if user_turns_seen >= lookahead:
                break
            continue
        excerpt = _trim_inline(text, 160)
        if klass == "revert":
            return ("reverted", f"user wrote: '{excerpt}'", mid)
        if klass == "negative":
            return ("failed", f"user wrote: '{excerpt}'", mid)
        return ("worked", f"user wrote: '{excerpt}'", mid)

    # Window exhausted (or session ended) with no explicit signal.
    if user_turns_seen == 0:
        # The session continued — more agent work, tool calls, maybe a
        # sub-agent — but the user never came back to complain or ask for
        # a revert. Treat that silence as acceptance.
        return (
            "worked",
            "session continued after the action with no user complaint or revert",
            int(_row_value(messages[-1], "id", anchor_id) or anchor_id),
        )
    return (
        "uncertain",
        f"{user_turns_seen} follow-up user turn(s) but none confirmed or rejected the action",
        last_user_id,
    )


def _row_to_outcome_match(
    row: sqlite3.Row, outcome: str, evidence: str, outcome_msg_id: int
) -> OutcomeMatch:
    return OutcomeMatch(
        session_id=row["session_id"],
        project_slug=row["project_slug"],
        project_path=_project_fs_path(row["stored_path"], row["project_slug"]),
        provider=row["provider"],
        first_ts=row["first_ts"] or "",
        last_ts=row["last_ts"] or "",
        message_count=int(row["message_count"] or 0),
        cost_usd=float(row["cost_usd"] or 0.0),
        snippet=None,
        outcome=outcome,
        outcome_evidence=evidence,
        outcome_msg_id=int(outcome_msg_id),
    )


def _outcome_matches_for(
    conn: sqlite3.Connection,
    anchor_seq_by_fk: dict[int, int],
    *,
    wanted_outcomes: set[str],
    limit: int,
) -> list[OutcomeMatch]:
    """Back half shared by the two outcome functions.

    ``anchor_seq_by_fk`` maps a candidate ``sessions.id`` to the ``seq``
    of its anchor message (the *last* one that matched the query). For
    each session — newest ``last_ts`` first — we pull the rows from the
    anchor's ``seq`` onward, classify the outcome (the first row is the
    anchor), and keep the session when ``outcome`` is in
    ``wanted_outcomes``. ``limit`` (> 0) caps the count and lets us stop
    loading early once we've got enough.
    """
    if not anchor_seq_by_fk:
        return []
    fks = list(anchor_seq_by_fk)
    placeholders = ",".join("?" for _ in fks)
    meta_sql = (
        "SELECT " + _SESSION_SELECT + ", s.id AS session_fk " + _SESSION_FROM
        + f" WHERE s.id IN ({placeholders}) ORDER BY s.last_ts DESC"
    )
    meta_rows = conn.execute(meta_sql, fks).fetchall()

    out: list[OutcomeMatch] = []
    for meta in meta_rows:
        sfk = int(meta["session_fk"])
        msg_rows = conn.execute(
            "SELECT id, seq, role, content_text, tools_json, is_sidechain "
            "FROM messages WHERE session_fk = ? AND seq >= ? ORDER BY seq, id",
            (sfk, anchor_seq_by_fk[sfk]),
        ).fetchall()
        if not msg_rows:
            continue
        outcome, evidence, msg_id = _classify_outcome(msg_rows, 0)
        if outcome not in wanted_outcomes:
            continue
        out.append(_row_to_outcome_match(meta, outcome, evidence, msg_id))
        if limit and limit > 0 and len(out) >= limit:
            break
    return out


def find_sessions_where_action_worked(
    conn: sqlite3.Connection,
    *,
    action: str,
    project: str | None = None,
    file_path: str | None = None,
    since: str | None = None,
    limit: int = 20,
) -> list[OutcomeMatch]:
    """Sessions where ``action`` was performed and the next user turn confirmed success.

    ``action`` is matched as a case-insensitive substring against both the
    serialised tool calls (``messages.tools_json``) and the message text
    (``messages.content_text``) — so it can be a tool name (``"Edit"``), a
    file fragment (``"cost.py"``), or a phrase from the conversation
    (``"add caching"``). Empty/whitespace ``action`` returns no matches.

    For each candidate session, the *last* message matching ``action`` is
    the anchor; the outcome is inferred by walking forward from it (see
    :func:`_classify_outcome`). Only sessions whose inferred outcome is
    ``"worked"`` are returned, sorted by ``last_ts`` DESC.

    Parameters
    ----------
    conn:
        Main store connection (``~/.stackunderflow/store.db``).
    action:
        Free-text action descriptor.
    project:
        Optional ``projects.slug`` filter.
    file_path:
        Optional — narrow to sessions that *also* touch this file (any
        Read/Edit/Write tool arg or a free-form mention). Resolved to an
        absolute path before matching.
    since:
        Optional cutoff on ``messages.timestamp``. Same forms as
        :func:`parse_since` accepts. Raises ``ValueError`` if malformed.
    limit:
        Max rows returned. Negative or zero means no limit.
    """
    _ensure_row_factory(conn)
    if not action or not action.strip():
        return []
    needle = action.strip()
    since_iso = parse_since(since)

    where = ["(m.tools_json LIKE ? OR m.content_text LIKE ?)"]
    params: list[Any] = [f"%{needle}%", f"%{needle}%"]
    if project:
        where.append("p.slug = ?")
        params.append(project)
    if since_iso:
        where.append("m.timestamp >= ?")
        params.append(since_iso)
    cand_sql = (
        "SELECT s.id AS sfk, MAX(m.seq) AS anchor_seq "
        "FROM messages m "
        "JOIN sessions s ON s.id = m.session_fk "
        "JOIN projects p ON p.id = s.project_id "
        "WHERE " + " AND ".join(where) + " GROUP BY s.id"
    )
    cand_rows = conn.execute(cand_sql, params).fetchall()
    anchor_seq_by_fk = {int(r["sfk"]): int(r["anchor_seq"]) for r in cand_rows}

    # Optional file narrowing — intersect with sessions touching the file
    # (loose: any tools_json / content_text mention of the resolved path).
    if file_path and anchor_seq_by_fk:
        resolved = _resolve_input_path(file_path)
        like = f"%{resolved}%"
        touch_rows = conn.execute(
            "SELECT DISTINCT s.id AS sfk FROM messages m "
            "JOIN sessions s ON s.id = m.session_fk "
            "WHERE m.tools_json LIKE ? OR m.content_text LIKE ?",
            [like, like],
        ).fetchall()
        touched = {int(r["sfk"]) for r in touch_rows}
        anchor_seq_by_fk = {
            fk: seq for fk, seq in anchor_seq_by_fk.items() if fk in touched
        }

    return _outcome_matches_for(
        conn, anchor_seq_by_fk, wanted_outcomes={"worked"}, limit=limit,
    )


def find_failure_modes_for_file(
    conn: sqlite3.Connection,
    file_path: str,
    *,
    since: str | None = None,
    limit: int = 20,
) -> list[OutcomeMatch]:
    """Sessions where editing ``file_path`` led to a follow-up correction.

    Candidate sessions are those with at least one Edit / Write /
    MultiEdit / NotebookEdit tool call whose arguments reference
    ``file_path``. The anchor is the *last* such edit; the outcome is
    inferred forward from it. Sessions whose inferred outcome is
    ``"failed"`` or ``"reverted"`` are returned, sorted by ``last_ts``
    DESC — i.e. "here's where touching this file went wrong, and why".

    Parameters
    ----------
    conn:
        Main store connection.
    file_path:
        File to look up. Resolved to an absolute path before matching.
    since:
        Optional cutoff on ``messages.timestamp``. Same forms as
        :func:`parse_since`. Raises ``ValueError`` if malformed.
    limit:
        Max rows returned. Negative or zero means no limit.
    """
    _ensure_row_factory(conn)
    resolved = _resolve_input_path(file_path)
    since_iso = parse_since(since)

    where = ["m.tools_json LIKE ?"]
    params: list[Any] = [f"%{resolved}%"]
    if since_iso:
        where.append("m.timestamp >= ?")
        params.append(since_iso)
    cand_sql = (
        "SELECT s.id AS sfk, m.seq AS seq, m.tools_json AS tools_json "
        "FROM messages m JOIN sessions s ON s.id = m.session_fk "
        "WHERE " + " AND ".join(where)
    )
    # The SQL ``LIKE`` is generous (it would match a Read of the file, or
    # the path mentioned inside some unrelated arg). Pin the anchor to the
    # *last* genuine write-mode mention per session; sessions with none
    # drop out.
    anchor_seq_by_fk: dict[int, int] = {}
    for r in conn.execute(cand_sql, params).fetchall():
        if not _tools_json_mentions_file(
            r["tools_json"], file_path=resolved, mode="write",
        ):
            continue
        sfk, seq = int(r["sfk"]), int(r["seq"])
        if seq > anchor_seq_by_fk.get(sfk, -1):
            anchor_seq_by_fk[sfk] = seq

    return _outcome_matches_for(
        conn, anchor_seq_by_fk, wanted_outcomes={"failed", "reverted"}, limit=limit,
    )


# ── bulk message loading (used by skill synthesis) ──────────────────────────


def load_messages_for_project(
    conn: sqlite3.Connection,
    project_id: int,
    *,
    since: str | None = None,
) -> list[sqlite3.Row]:
    """Return every message row for one project, ordered for sequence walking.

    Joined to ``sessions`` so each row carries ``session_id`` (the
    provider-facing id, not the internal fk) plus the project ``slug``;
    callers that need to group by session — pattern mining in
    ``services.skill_synth`` is the first — get a self-contained row.

    Parameters
    ----------
    conn:
        Main store connection.
    project_id:
        Internal ``projects.id`` (not a slug — resolve the slug first).
    since:
        Optional cutoff. Same accepted forms as the rest of this module
        (``"7d"`` / ``"1w"`` / ``"1m"`` / ``"24h"`` / ISO). Filters on
        ``messages.timestamp``.

    Returns
    -------
    Rows ordered by ``(session_fk, seq)`` so a consumer can walk each
    session's turns in order. Columns: ``message_id``, ``session_fk``,
    ``session_id``, ``project_slug``, ``provider``, ``seq``,
    ``timestamp``, ``role``, ``model``, ``content_text``, ``tools_json``,
    ``raw_json``, ``is_sidechain``.
    """
    _ensure_row_factory(conn)
    since_iso = parse_since(since)

    where_extra = ""
    params: list[Any] = [int(project_id)]
    if since_iso:
        where_extra = " AND m.timestamp >= ?"
        params.append(since_iso)

    return conn.execute(
        "SELECT m.id            AS message_id, "  # noqa: S608 — where_extra is a fixed literal + placeholder
        "       m.session_fk    AS session_fk, "
        "       s.session_id    AS session_id, "
        "       p.slug          AS project_slug, "
        "       p.provider      AS provider, "
        "       m.seq           AS seq, "
        "       m.timestamp     AS timestamp, "
        "       m.role          AS role, "
        "       m.model         AS model, "
        "       m.content_text  AS content_text, "
        "       m.tools_json    AS tools_json, "
        "       m.raw_json      AS raw_json, "
        "       m.is_sidechain  AS is_sidechain "
        "FROM messages m "
        "JOIN sessions s ON s.id = m.session_fk "
        "JOIN projects p ON p.id = s.project_id "
        f"WHERE s.project_id = ?{where_extra} "
        "ORDER BY m.session_fk, m.seq",
        params,
    ).fetchall()
