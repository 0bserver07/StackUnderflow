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
from collections.abc import Callable, Sequence
from dataclasses import asdict, dataclass
from datetime import UTC, datetime, timedelta
from pathlib import Path
from typing import Any, overload

from stackunderflow.services import discovery_telemetry as _telemetry

__all__ = [
    "SessionMatch",
    "OutcomeMatch",
    "BudgetedResult",
    "DEFAULT_MIN_OUTCOME_CONFIDENCE",
    "find_sessions_in_path",
    "find_sessions_touching_file",
    "search_past_decisions",
    "find_sessions_where_action_worked",
    "find_failure_modes_for_file",
    "pack_within_budget",
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

    ``embedding_score`` is only populated by ``search_past_decisions``
    when called with ``use_embeddings=True``; it carries the cosine
    similarity between the query and the message that contributed the
    snippet, mapped to ``[0, 1]``. ``None`` when semantic mode is off
    (the default) so the JSON contract for substring-mode callers is
    unchanged.

    ``more_matches_in_session`` is the session-clustering count: how many
    *further* messages in this session also matched the query beyond the
    one that produced the ``snippet``. Only ``search_past_decisions``
    populates it (both its FTS and LIKE paths), and only when > 0; it stays
    ``None`` — and is dropped from :meth:`to_dict` — otherwise, so the JSON
    shape for single-hit sessions and for the other discovery functions is
    unchanged.
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
    embedding_score: float | None = None
    more_matches_in_session: int | None = None

    def to_dict(self) -> dict[str, Any]:
        """Serialise to a JSON-friendly dict.

        ``embedding_score`` and ``more_matches_in_session`` are dropped
        when ``None`` so substring-mode / single-hit callers see the
        original 9-key shape; only the results that carry the extra signal
        expose the field.
        """
        out = asdict(self)
        if out.get("embedding_score") is None:
            out.pop("embedding_score", None)
        if out.get("more_matches_in_session") is None:
            out.pop("more_matches_in_session", None)
        return out


@dataclass(frozen=True, kw_only=True)
class OutcomeMatch(SessionMatch):
    """A discovery match annotated with an inferred outcome.

    Extends :class:`SessionMatch` with four fields that say whether the
    matched action *worked*, the evidence for that judgement, and a
    confidence score for the judgement. The inherited ``snippet`` stays
    ``None`` for outcome queries — the evidence string carries the
    relevant excerpt instead.

    ``outcome`` is one of:

    * ``"worked"``    — a following user turn explicitly confirmed
      success (an in-vocabulary positive phrase), or — at lower
      confidence — the session continued/ended with no revert and no
      complaint.
    * ``"failed"``    — a following user turn reported it broke / was
      wrong / wasn't what was asked (or emitted a negative emoji).
    * ``"reverted"``  — the change was undone (the user asked, or the
      agent ran ``git revert`` / ``git reset --hard`` / ``git checkout --``
      / ``git restore``).
    * ``"uncertain"`` — the action was the last recorded turn, or the
      follow-up turns gave no clear signal.

    ``outcome_confidence`` is in ``[0.0, 1.0]``:

    * ``1.0`` — deterministic flag from the ``captured_events`` table
      (future hook integration; not assigned by the transcript fallback).
    * ``0.8`` — explicit in-vocabulary success/failure/revert phrase
      from a user turn within the lookahead window.
    * ``0.5`` — agent revert tool-call (e.g. ``git revert`` on the same
      file) — strong but slightly weaker than an explicit user statement.
    * ``0.3`` — "no complaint before session ended" — surface heuristic
      that over-claims and is filtered out by the default 0.5 threshold.
    * ``0.0`` — no signal at all (anchor was the session's last turn).

    The new fields are keyword-only so they can follow ``SessionMatch``'s
    defaulted ``snippet`` without dataclass field-ordering complaints.
    """

    outcome: str            # "worked" | "failed" | "reverted" | "uncertain"
    outcome_evidence: str   # short human-readable justification + msg ref
    outcome_msg_id: int     # id of the message that established the outcome
    outcome_confidence: float = 0.0  # [0.0, 1.0]; see class docstring


@dataclass(frozen=True)
class BudgetedResult:
    """Outcome of running a discovery query with a token budget applied.

    The three discovery functions return a plain ``list[SessionMatch]``
    when no ``context_budget`` is given (backward-compatible), and a
    ``BudgetedResult`` when one is. ``sessions`` is rank-ordered (most
    useful first), not ``last_ts``-ordered, because the point of the
    budget path is to surface the highest-value rows before the budget
    runs out.

    * ``truncated`` — at least one matched session was dropped to fit.
    * ``more_available`` — how many were dropped (0 when not truncated).
    * ``budget_used_tokens`` — Σ of the chars/4 estimate over kept rows.
    * ``budget_max_tokens`` — the budget that was enforced (``<= 0`` means
      "no enforcement"; everything that the ``--limit`` cap allowed is
      kept).
    """

    sessions: list[SessionMatch]
    truncated: bool
    more_available: int
    budget_used_tokens: int
    budget_max_tokens: int


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


def _normalize_path(p: str) -> str:
    """Return a comparable canonical form for a filesystem path.

    Normalises both POSIX and Windows-style separators to ``/`` and
    strips trailing separators so ``_is_ancestor`` can do a single
    prefix check. On Windows, the original case is preserved (we do
    not lowercase because the on-disk project path may have been
    persisted with a specific case from the source adapter).

    Pure string arithmetic — no filesystem access.
    """
    if not p:
        return p
    # Replace OS-specific separator with ``/`` so the prefix check
    # works regardless of which side of the comparison was produced
    # on which OS. Trailing separators stripped so ``/foo`` and
    # ``/foo/`` compare equal.
    return p.replace("\\", "/").rstrip("/")


def _is_ancestor(ancestor: str, descendant: str) -> bool:
    """True if ``ancestor`` is ``descendant`` itself or a directory
    ancestor of it.

    Pure path arithmetic — no filesystem access. Normalises separators
    (``\\`` → ``/``) before comparing so a Windows-style query path
    can match a POSIX-style stored project path.
    """
    if not ancestor or not descendant:
        return False
    a = _normalize_path(ancestor)
    d = _normalize_path(descendant)
    if a == d:
        return True
    return d.startswith(a + "/")


def _resolve_input_path(path: str | Path) -> str:
    """Resolve the caller's path to an absolute string.

    ``Path.resolve(strict=False)`` works whether or not the path
    exists on disk — we want this because tests / agents may query
    paths that have been deleted or never existed locally.

    On Windows, an input like ``/Users/yad/dev/foo`` (POSIX-shaped)
    gets prepended with the current drive (``C:\\Users\\yad\\dev\\foo``)
    by ``resolve()``. We keep the resolved form because that's the
    canonical local interpretation, and the comparison in
    ``_is_ancestor`` normalises separators on both sides so a stored
    POSIX-shaped project path still matches.
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


def _row_to_match(
    row: sqlite3.Row,
    snippet: str | None = None,
    embedding_score: float | None = None,
    more_matches_in_session: int | None = None,
) -> SessionMatch:
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
        embedding_score=embedding_score,
        more_matches_in_session=more_matches_in_session,
    )


def _ensure_row_factory(conn: sqlite3.Connection) -> None:
    """Discovery code accesses columns by name; force a Row factory.

    Idempotent: only sets the factory if it's still the default. Tests
    that pre-set a factory (e.g. an in-memory connection in CLI tests)
    are honoured.
    """
    if conn.row_factory is None:
        conn.row_factory = sqlite3.Row


# ── token-budgeted output ───────────────────────────────────────────────────
#
# Discovery results default to ``--limit 20`` rows of dumb recency
# truncation. For an agent caller that's noise dumped into a tight
# context window. The machinery below ranks rows (recency + cost +
# command-specific relevance), packs greedily until an estimated token
# budget is exhausted, and tells the caller how many rows were dropped
# so it can emit a tail marker.


def _estimate_tokens(session_dict: dict[str, Any]) -> int:
    """Rough chars/4 token estimate for one serialised session row.

    Avoids an llm-side tokenizer dependency; off by ~10-20% either way,
    which is fine for budget enforcement. If precision ever matters,
    add a ``--precise-tokens`` flag backed by tiktoken (extra dep).
    """
    serialized = json.dumps(session_dict, separators=(",", ":"))
    return (len(serialized) // 4) + 1


# Until citation telemetry exists (separate spec) the rank is a weighted
# sum of three terms in [0, 1]: recency, cost, and a command-specific
# relevance term. The citation-feedback spec appends a fourth
# ``cite_rate`` term — that's why ``_build_rank_fn`` assembles a *list*
# of (weight, score_fn) tuples in one place instead of hard-coding the
# arithmetic: a new term is one appended tuple, no packer rewrite.
_DEFAULT_RANK_WEIGHTS: tuple[float, float, float] = (0.5, 0.2, 0.3)
_COST_SATURATION_USD = 5.0  # sessions ≥ $5 get the full cost score


def _parse_rank_weights(raw: str | None) -> tuple[float, float, float]:
    """Parse ``"recency,cost,relevance"`` leniently.

    A missing/blank/malformed value, the wrong component count, or any
    negative weight falls back to ``_DEFAULT_RANK_WEIGHTS``. A fourth+
    component (reserved for the citation-feedback term) is ignored here.
    """
    if not raw or not isinstance(raw, str):
        return _DEFAULT_RANK_WEIGHTS
    parts = [p.strip() for p in raw.split(",") if p.strip()]
    try:
        vals = [float(p) for p in parts]
    except ValueError:
        return _DEFAULT_RANK_WEIGHTS
    if len(vals) < 3 or any(v < 0 for v in vals[:3]):
        return _DEFAULT_RANK_WEIGHTS
    return (vals[0], vals[1], vals[2])


def _parse_ts(value: str | None) -> datetime | None:
    """Best-effort ISO-timestamp parse; tz-naive strings get UTC."""
    if not value:
        return None
    try:
        parsed = datetime.fromisoformat(value.replace("Z", "+00:00"))
    except ValueError:
        return None
    if parsed.tzinfo is None:
        parsed = parsed.replace(tzinfo=UTC)
    return parsed


def _recency_score(m: SessionMatch, *, now: datetime | None = None) -> float:
    """``1 / (1 + days_since_last_ts)`` — 1.0 today, ~0.05 at 20 days."""
    ts = _parse_ts(m.last_ts)
    if ts is None:
        return 0.0
    ref = now or datetime.now(UTC)
    days = max(0.0, (ref - ts).total_seconds() / 86400.0)
    return 1.0 / (1.0 + days)


def _cost_score(m: SessionMatch) -> float:
    """``min(1.0, cost_usd / 5.0)`` — cheap sessions are less reusable."""
    return min(1.0, max(0.0, float(m.cost_usd)) / _COST_SATURATION_USD)


def _build_rank_fn(
    relevance_fn: Callable[[SessionMatch], float],
    *,
    now: datetime | None = None,
    weights: tuple[float, float, float] | None = None,
) -> Callable[[SessionMatch], float]:
    """Compose the weighted-sum rank function for a discovery command.

    ``relevance_fn`` is the command-specific term (path-relationship,
    tool-vs-content match kind, LIKE-match density). ``weights`` defaults
    to the ``discovery_rank_weights`` setting.

    Extension point: the citation-feedback spec adds a ``cite_rate`` term
    by appending ``(w_cite, _cite_rate_score)`` to ``terms`` below and a
    fourth component to the default weights tuple.
    """
    if weights is None:
        from stackunderflow.settings import Settings

        weights = _parse_rank_weights(Settings().discovery_rank_weights)
    w_recency, w_cost, w_relevance = weights
    terms: list[tuple[float, Callable[[SessionMatch], float]]] = [
        (w_recency, lambda m: _recency_score(m, now=now)),
        (w_cost, _cost_score),
        (w_relevance, relevance_fn),
    ]

    def _rank(m: SessionMatch) -> float:
        return sum(w * fn(m) for w, fn in terms)

    return _rank


def pack_within_budget(
    sessions: Sequence[SessionMatch],
    *,
    budget_tokens: int,
    rank_fn: Callable[[SessionMatch], float] | None = None,
) -> tuple[list[SessionMatch], int, int]:
    """Sort by ``rank_fn``, pack greedily, return ``(kept, dropped, used)``.

    * ``rank_fn`` — higher score sorts first; ``None`` keeps input order
      (use when the caller already sorted). Ties keep input order
      (stable sort), so a recency-sorted SQL result stays recency-ordered
      within an equal-rank band.
    * ``budget_tokens`` — ``<= 0`` disables enforcement (keep everything,
      still re-ordered by ``rank_fn``).
    * ``used`` — Σ of :func:`_estimate_tokens` over the kept rows.

    Greedy + strict: once a row doesn't fit we stop (we do *not* skip it
    and keep scanning for a smaller one — that would reorder by size and
    defeat the ranking). If the top-ranked row alone exceeds the budget,
    zero rows are kept and the caller's marker should tell the agent to
    raise ``--context-budget``.
    """
    ordered = sorted(sessions, key=rank_fn, reverse=True) if rank_fn else list(sessions)

    if budget_tokens is None or budget_tokens <= 0:
        used = sum(_estimate_tokens(m.to_dict()) for m in ordered)
        return ordered, 0, used

    kept: list[SessionMatch] = []
    used = 0
    for m in ordered:
        cost = _estimate_tokens(m.to_dict())
        if used + cost > budget_tokens:
            break
        kept.append(m)
        used += cost
    return kept, len(ordered) - len(kept), used


def _budgeted(
    matches: list[SessionMatch],
    *,
    context_budget: int,
    rank_fn: Callable[[SessionMatch], float],
) -> BudgetedResult:
    """Run ``pack_within_budget`` and wrap the result for a discovery fn."""
    kept, dropped, used = pack_within_budget(
        matches, budget_tokens=context_budget, rank_fn=rank_fn,
    )
    return BudgetedResult(
        sessions=kept,
        truncated=dropped > 0,
        more_available=dropped,
        budget_used_tokens=used,
        budget_max_tokens=context_budget,
    )


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


def _relevance_in_path(resolved: str) -> Callable[[SessionMatch], float]:
    """Relevance term for ``find_sessions_in_path``.

    1.0 when the queried path *is* the project root, 0.5 when the queried
    path is a descendant of it (working in a subdir), 0.25 when the
    project sits below the queried path (defensive — this function only
    returns ancestor-or-equal projects, so that branch is unreachable in
    practice but kept for standalone reuse), 0.0 otherwise.
    """
    target = resolved.rstrip("/")

    def _rel(m: SessionMatch) -> float:
        proj = (m.project_path or "").rstrip("/")
        if not proj:
            return 0.0
        if proj == target:
            return 1.0
        if target.startswith(proj + "/"):
            return 0.5
        if proj.startswith(target + "/"):
            return 0.25
        return 0.0

    return _rel


@overload
def find_sessions_in_path(
    conn: sqlite3.Connection, path: str | Path, *, since: str | None = ...,
    limit: int = ..., provider: str | None = ..., context_budget: None = ...,
) -> list[SessionMatch]: ...
@overload
def find_sessions_in_path(
    conn: sqlite3.Connection, path: str | Path, *, since: str | None = ...,
    limit: int = ..., provider: str | None = ..., context_budget: int,
) -> BudgetedResult: ...
def find_sessions_in_path(
    conn: sqlite3.Connection,
    path: str | Path,
    *,
    since: str | None = None,
    limit: int = 20,
    provider: str | None = None,
    context_budget: int | None = None,
) -> list[SessionMatch] | BudgetedResult:
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
    context_budget:
        When ``None`` (default) the return is a plain ``list[SessionMatch]``
        sorted by ``last_ts DESC`` — today's behaviour. When an int, the
        ``limit``-capped rows are re-ranked (recency + cost + path
        relevance), packed greedily until ~that many estimated tokens are
        used, and a :class:`BudgetedResult` is returned instead. ``limit``
        stays a hard cap; the budget is the additional constraint.

    Returns
    -------
    ``list[SessionMatch]`` (``context_budget`` unset) or
    :class:`BudgetedResult` (``context_budget`` set). ``snippet`` is
    always ``None`` for this query.
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
        if context_budget is not None:
            return _budgeted(
                [], context_budget=context_budget,
                rank_fn=_build_rank_fn(_relevance_in_path(resolved)),
            )
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
    matches = [_row_to_match(r) for r in rows]
    _record_loaded(conn, "find_sessions_in_path", matches)
    if context_budget is not None:
        return _budgeted(
            matches, context_budget=context_budget,
            rank_fn=_build_rank_fn(_relevance_in_path(resolved)),
        )
    return matches


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


# Relevance tiers for ``find_sessions_touching_file``: an exact tool-arg
# match is the strongest signal, a free-form content mention weaker,
# anything else weakest (defensive default — every match falls into one
# of the first two in practice).
_TOUCHING_FILE_RELEVANCE = {"tool": 1.0, "content": 0.5}


def _relevance_touching_file(
    match_kind_by_sid: dict[str, str],
) -> Callable[[SessionMatch], float]:
    def _rel(m: SessionMatch) -> float:
        return _TOUCHING_FILE_RELEVANCE.get(match_kind_by_sid.get(m.session_id, ""), 0.25)

    return _rel


@overload
def find_sessions_touching_file(
    conn: sqlite3.Connection, file_path: str | Path, *, limit: int = ...,
    mode: str = ..., context_budget: None = ...,
) -> list[SessionMatch]: ...
@overload
def find_sessions_touching_file(
    conn: sqlite3.Connection, file_path: str | Path, *, limit: int = ...,
    mode: str = ..., context_budget: int,
) -> BudgetedResult: ...
def find_sessions_touching_file(
    conn: sqlite3.Connection,
    file_path: str | Path,
    *,
    limit: int = 20,
    mode: str = "any",
    context_budget: int | None = None,
) -> list[SessionMatch] | BudgetedResult:
    """Sessions where ``file_path`` shows up in tools or message content.

    ``mode``
        * ``"read"`` — only sessions where ``file_path`` appears as an
          argument to a Read tool call.
        * ``"write"`` — Edit / Write / MultiEdit / NotebookEdit args.
        * ``"any"`` (default) — any of the above OR a free-form mention
          in ``messages.content_text``.

    The match is substring-based on the resolved absolute path. Returns a
    plain ``list[SessionMatch]`` sorted by ``last_ts DESC`` when
    ``context_budget`` is ``None``; when set, the ``limit``-capped rows
    are re-ranked (recency + cost + tool-vs-content match strength),
    packed greedily, and returned as a :class:`BudgetedResult`.
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
        "SELECT s.id AS sfk, s.session_id AS sid, m.tools_json, m.content_text "  # noqa: S608 — sql_filter is a fixed literal selected by mode
        "FROM messages m "
        "JOIN sessions s ON s.id = m.session_fk "
        f"WHERE {sql_filter}",
        sql_params,
    ).fetchall()

    # Group hits per-session, applying the mode-specific tool-match
    # check. Sessions that only had a free-form content_text mention
    # are kept only when ``mode == 'any'``. ``match_kind_by_sid`` records
    # the strongest match seen per session for the relevance term —
    # first-hit-wins (we short-circuit after a session matches), which is
    # fine for a heuristic score.
    matched_session_fks: set[int] = set()
    match_kind_by_sid: dict[str, str] = {}
    for row in rows:
        sfk = int(row["sfk"])
        if sfk in matched_session_fks:
            continue
        sid = row["sid"]
        tools_json = row["tools_json"]
        if mode in {"read", "write"}:
            if _tools_json_mentions_file(
                tools_json, file_path=resolved, mode=mode
            ):
                matched_session_fks.add(sfk)
                match_kind_by_sid[sid] = "tool"
            continue
        # mode == "any"
        if _tools_json_mentions_file(
            tools_json, file_path=resolved, mode="any"
        ):
            matched_session_fks.add(sfk)
            match_kind_by_sid[sid] = "tool"
        else:
            content = row["content_text"] or ""
            if resolved in content:
                matched_session_fks.add(sfk)
                match_kind_by_sid[sid] = "content"

    rank_fn = _build_rank_fn(_relevance_touching_file(match_kind_by_sid))

    if not matched_session_fks:
        if context_budget is not None:
            return _budgeted([], context_budget=context_budget, rank_fn=rank_fn)
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
    matches = [_row_to_match(r) for r in rows2]
    _record_loaded(conn, "find_sessions_touching_file", matches)
    if context_budget is not None:
        return _budgeted(matches, context_budget=context_budget, rank_fn=rank_fn)
    return matches


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


# LIKE-match-density relevance for ``search_past_decisions``: total
# needle occurrences across a session's matching messages, saturating at
# 5 (a stand-in for a BM25 score until an FTS index exists).
_DECISIONS_OCCURRENCE_SATURATION = 5.0


def _relevance_decisions(
    occ_by_sid: dict[str, int],
) -> Callable[[SessionMatch], float]:
    def _rel(m: SessionMatch) -> float:
        return min(1.0, occ_by_sid.get(m.session_id, 0) / _DECISIONS_OCCURRENCE_SATURATION)

    return _rel


def _relevance_embeddings(m: SessionMatch) -> float:
    """Relevance term for ``use_embeddings=True``.

    Reads the cosine-similarity score already attached to the match
    (mapped to ``[0, 1]`` at compute time). Returns ``0.0`` for any
    match whose score wasn't computed — encoder failure on a specific
    message, orphan row whose session id couldn't be resolved, etc.
    Those rows sink to the bottom of the rank but stay in the surface
    so the agent can still see them.
    """
    score = m.embedding_score
    if score is None:
        return 0.0
    return max(0.0, min(1.0, float(score)))


def _load_message_texts(
    conn: sqlite3.Connection, message_ids: list[int],
) -> dict[int, str]:
    """Fetch ``messages.content_text`` for ``message_ids``.

    Returns ``{message_id: content_text}``; ids with no row are absent.
    Reads the same column the substring filter matched on (and the same
    source the retired sentence-transformers backend used), so the
    re-rank scores the text the user actually searched. The IN clause is
    chunked to stay under SQLite's default 999-parameter limit even though
    the candidate set is normally a few dozen ids.
    """
    if not message_ids:
        return {}
    out: dict[int, str] = {}
    chunk_size = 500
    for start in range(0, len(message_ids), chunk_size):
        chunk = message_ids[start:start + chunk_size]
        placeholders = ",".join("?" for _ in chunk)
        rows = conn.execute(
            "SELECT id, content_text FROM messages "  # noqa: S608 — placeholders bound
            f"WHERE id IN ({placeholders})",
            chunk,
        ).fetchall()
        for r in rows:
            mid = int(r["id"] if hasattr(r, "keys") else r[0])
            content = r["content_text"] if hasattr(r, "keys") else r[1]
            out[mid] = content or ""
    return out


def _compute_embedding_scores(
    conn: sqlite3.Connection,
    *,
    query: str,
    mid_by_sfk: dict[int, int],
    model_name: str | None = None,
) -> dict[int, float]:
    """Embed query + first-hit messages via Ollama, return ``{session_fk: cosine}``.

    Uses :mod:`stackunderflow.services.embeddings` — the same cloud-first,
    local-fallback Ollama path that powers ``memory ask`` /
    ``hybrid_search``. The candidate set is small (bounded by the
    substring pre-filter), so the messages are embedded on the fly with no
    cache table: we load each candidate's ``content_text``, embed them plus
    the query in one Ollama batch, and score with
    :func:`embeddings.cosine` mapped to ``[0, 1]``.

    Graceful degradation is the whole point. When Ollama is unreachable
    ``embeddings.embed_texts`` returns ``None``; we return an **empty
    dict** so every row's ``embedding_score`` stays ``None``,
    :func:`_relevance_embeddings` yields ``0.0`` for all of them, and
    ``search_past_decisions`` degrades to substring ordering — exactly how
    ``hybrid_search`` falls back to FTS-only. No exception is raised.

    ``model_name`` is an Ollama embed model name; ``None`` →
    ``embeddings.DEFAULT_EMBED_MODEL`` (via the env-var-aware resolver
    inside ``embed_texts``).

    Returns an empty dict when ``mid_by_sfk`` is empty (nothing to score).
    """
    if not mid_by_sfk:
        return {}
    from stackunderflow.services import embeddings as _emb

    message_ids = list(mid_by_sfk.values())
    texts_by_mid = _load_message_texts(conn, message_ids)

    # Build the aligned embed batch: query first, then one entry per
    # candidate that has non-empty text. ``embed_texts`` skips rows whose
    # text is empty/whitespace, so feeding only non-empty texts keeps the
    # returned vectors aligned 1:1 with ``embed_ids`` (a candidate with no
    # embeddable text simply never gets a score → 0.0 at rank time).
    embed_ids: list[int] = []
    batch: list[str] = [query]
    for mid in message_ids:
        text = texts_by_mid.get(mid, "")
        if text and text.strip():
            embed_ids.append(mid)
            batch.append(text)

    # Nothing embeddable (all candidate texts empty) — no cosine to
    # compute, fall through to substring ranking.
    if not embed_ids:
        return {}

    vectors = _emb.embed_texts(batch, model=model_name)
    # Ollama unreachable / whole batch failed → None (or a short result if
    # some rows silently dropped). Degrade to substring ranking: empty
    # dict, no score attached to any row.
    if not vectors or len(vectors) != len(batch):
        return {}

    query_vec = vectors[0]
    score_by_mid: dict[int, float] = {}
    for i, mid in enumerate(embed_ids):
        cand_vec = vectors[i + 1]
        # cosine ∈ [-1, 1]; map to [0, 1] so the score composes with the
        # existing pack_within_budget rank fn (each component in [0, 1]).
        cos = _emb.cosine(query_vec, cand_vec)
        score_by_mid[mid] = (cos + 1.0) / 2.0

    return {
        sfk: score_by_mid.get(mid, 0.0)
        for sfk, mid in mid_by_sfk.items()
    }


# ── FTS lexical path (bm25) ──────────────────────────────────────────────────
#
# ``search_past_decisions`` defaults to the leading-wildcard
# ``content_text LIKE '%needle%'`` full scan above. When the caller injects
# a ``search_service`` (an FTS5 index living in the *separate*
# ``search_index.db``), the candidate-gathering + ranking is routed through
# bm25 instead: the store's leading-wildcard scan disappears from the hot
# path. The store is still the provenance authority — the FTS index yields
# ranked ``session_id`` s + snippets + clustering counts, and we hydrate
# session/date/cost from the store by ``session_id`` (the two databases are
# joined at the id level in Python, never via ``ATTACH``: they have
# independent WAL/lock domains and the existing ``memory ask`` path already
# bridges them the same way).


def _sessions_by_id(
    conn: sqlite3.Connection,
    session_ids: Sequence[str],
    project: str | None,
) -> dict[str, sqlite3.Row]:
    """Hydrate ``{session_id: row}`` provenance for FTS-matched session ids.

    Mirrors the LIKE path's ``sessions ⨯ projects ⨯ session_mart`` join but
    keys on the provider-facing ``session_id`` (what the FTS index stores),
    chunked under SQLite's 999-parameter limit. ``project`` (a slug) is
    re-applied as a belt-and-suspenders scope even though the FTS half
    already filtered on it. Later duplicate ids keep the first row seen.
    """
    out: dict[str, sqlite3.Row] = {}
    ids = list(dict.fromkeys(s for s in session_ids if s))
    if not ids:
        return out
    chunk_size = 500
    for start in range(0, len(ids), chunk_size):
        part = ids[start:start + chunk_size]
        placeholders = ",".join("?" for _ in part)
        params: list[Any] = list(part)
        where_extra = ""
        if project:
            where_extra = " AND p.slug = ?"
            params.append(project)
        sql = (
            "SELECT " + _SESSION_SELECT + " "  # noqa: S608 — placeholders + fixed clause
            + _SESSION_FROM
            + f" WHERE s.session_id IN ({placeholders})"
            + where_extra
        )
        for r in conn.execute(sql, params).fetchall():
            out.setdefault(r["session_id"], r)
    return out


def _bm25_relevance(bm25_by_sid: dict[str, float]) -> dict[str, float]:
    """Map raw SQLite bm25 ``rank`` values to a ``[0, 1]`` relevance score.

    FTS5 ``rank`` is negative and *lower is better*. We min-max normalise
    across the candidate set so the best-ranked session scores ``1.0`` and
    the worst ``0.0`` — the shape the ``pack_within_budget`` rank fn wants
    (each term in ``[0, 1]``, higher better). All-equal candidates all
    score ``1.0``.
    """
    if not bm25_by_sid:
        return {}
    vals = list(bm25_by_sid.values())
    lo, hi = min(vals), max(vals)          # lo = best (most negative)
    span = hi - lo
    if span <= 0:
        return dict.fromkeys(bm25_by_sid, 1.0)
    return {sid: 1.0 - (v - lo) / span for sid, v in bm25_by_sid.items()}


def _fts_decisions(
    conn: sqlite3.Connection,
    search_service: Any,
    needle: str,
    *,
    project: str | None,
    since_iso: str | None,
    limit: int,
) -> tuple[list[SessionMatch], Callable[[SessionMatch], float]] | None:
    """FTS5/bm25 candidate-gathering + ranking for ``search_past_decisions``.

    Returns ``(matches, rank_fn)`` on success, or ``None`` when the FTS
    index isn't populated — the caller then falls back to the LIKE scan.
    A *populated* index that matched nothing returns ``([], rank_fn)`` so
    the caller does **not** silently reintroduce the full scan.

    Matches carry the Python snippet (built from the FTS row's content, so
    it's byte-for-byte the format the LIKE path emits) plus the
    per-session ``more_matches_in_session`` clustering count. The rank_fn's
    relevance term is the bm25 score mapped to ``[0, 1]``. The returned
    list is ``last_ts DESC`` (the function's documented plain-list order);
    the bm25 signal rides in ``rank_fn`` for the budgeted path.
    """
    candidate_k = max(int(limit) * 10, 200) if limit and limit > 0 else 500
    try:
        hits = search_service.lexical_session_hits(
            needle, project=project, date_from=since_iso, candidate_k=candidate_k,
        )
    except Exception:  # noqa: BLE001 — the lexical half must never break the query
        return None
    if hits is None:
        return None  # index not populated → LIKE fallback

    rel_by_sid = _bm25_relevance({h["session_id"]: h["bm25"] for h in hits})
    rank_fn = _build_rank_fn(lambda m: rel_by_sid.get(m.session_id, 0.0))

    if not hits:
        return [], rank_fn  # populated + genuinely no match — never LIKE-scan

    order = [h["session_id"] for h in hits]
    rows_by_sid = _sessions_by_id(conn, order, project)
    snippet_by_sid = {
        h["session_id"]: _build_snippet(h["content"], needle) for h in hits
    }
    more_by_sid = {h["session_id"]: int(h["more_matches_in_session"] or 0) for h in hits}

    out: list[SessionMatch] = []
    for sid in order:
        row = rows_by_sid.get(sid)
        if row is None:
            continue  # FTS hit with no store provenance (index/store drift) — skip
        more = more_by_sid.get(sid, 0)
        out.append(
            _row_to_match(
                row,
                snippet=snippet_by_sid.get(sid),
                more_matches_in_session=more or None,
            )
        )
    out.sort(key=lambda m: m.last_ts or "", reverse=True)
    if limit and limit > 0:
        out = out[:limit]
    return out, rank_fn


@overload
def search_past_decisions(
    conn: sqlite3.Connection, query: str, *, project: str | None = ...,
    since: str | None = ..., limit: int = ..., context_budget: None = ...,
    use_embeddings: bool = ..., model_name: str | None = ...,
    search_service: Any = ...,
) -> list[SessionMatch]: ...
@overload
def search_past_decisions(
    conn: sqlite3.Connection, query: str, *, project: str | None = ...,
    since: str | None = ..., limit: int = ..., context_budget: int,
    use_embeddings: bool = ..., model_name: str | None = ...,
    search_service: Any = ...,
) -> BudgetedResult: ...
def search_past_decisions(
    conn: sqlite3.Connection,
    query: str,
    *,
    project: str | None = None,
    since: str | None = None,
    limit: int = 20,
    context_budget: int | None = None,
    use_embeddings: bool = False,
    model_name: str | None = None,
    search_service: Any = None,
) -> list[SessionMatch] | BudgetedResult:
    """Search past decisions — FTS5/bm25 when indexed, LIKE otherwise.

    When the caller injects a ``search_service`` (an FTS5 index in the
    separate ``search_index.db``) and it is populated, candidate-gathering
    and ranking run through bm25: the leading-wildcard
    ``content_text LIKE '%needle%'`` full scan disappears from the hot
    path, results carry a bm25 relevance signal, and each session is
    clustered to one representative hit plus a ``more_matches_in_session``
    count so a chatty session can't fill the page. The store stays the
    provenance authority (session / date / cost are hydrated from it by
    ``session_id``).

    When no ``search_service`` is given, or its index isn't populated (a
    fresh install, or ``use_embeddings=True`` — that path keeps its own
    substring+cosine pipeline), it degrades gracefully to the original
    ``LIKE`` scan with Python-built snippets. Both paths return the same
    ``SessionMatch`` shape and honour ``pack_within_budget``.

    Parameters
    ----------
    conn:
        Main store connection.
    query:
        Free-form text. Empty/whitespace strings return no matches. (The
        ``memory`` CLI additionally gates empty / punctuation-only queries
        with :func:`services.search_service.search_has_intent` before the
        store is even opened; this function's empty-return is the floor.)
    project:
        Optional ``projects.slug`` filter.
    since:
        Same accepted forms as ``find_sessions_in_path``.
    limit:
        Max rows returned. Sorted by ``last_ts DESC`` so the most
        recent matching session is first.
    context_budget:
        ``None`` (default) → plain ``list[SessionMatch]`` as above. An
        int → the ``limit``-capped rows are re-ranked (recency + cost +
        LIKE-match density) and packed greedily into a
        :class:`BudgetedResult`.
    use_embeddings:
        When ``True``, the LIKE-density relevance term in the rank
        function is **replaced** by an Ollama cosine similarity score
        between the query and the first matching message per session
        (mapped to ``[0, 1]``). Each returned ``SessionMatch`` carries the
        score on its ``embedding_score`` field. The substring filter still
        runs first — embeddings only re-rank the candidate set; they never
        widen it. Uses the same cloud-first, local-fallback Ollama backend
        as ``memory ask`` (:mod:`stackunderflow.services.embeddings`). When
        Ollama is unreachable this **degrades silently** to substring
        ranking (no score attached, no error raised) — exactly how
        ``hybrid_search`` falls back to FTS-only.
    model_name:
        Override the Ollama embed model. ``None`` (default) →
        ``STACKUNDERFLOW_EMBED_MODEL`` env var, or
        ``embeddings.DEFAULT_EMBED_MODEL`` (``nomic-embed-text``). Ignored
        when ``use_embeddings`` is ``False``.
    search_service:
        Optional lexical FTS retriever (a
        :class:`services.search_service.SearchService`). When supplied and
        ``use_embeddings`` is ``False``, the bm25 path above is used; when
        ``None`` (the default, and what every non-CLI caller passes) the
        original ``LIKE`` scan runs, so hooks / meta-agent / test callers
        are byte-compatible. Duck-typed: any object exposing
        ``lexical_session_hits(...)`` works.
    """
    _ensure_row_factory(conn)
    if not query or not query.strip():
        if context_budget is not None:
            return _budgeted(
                [], context_budget=context_budget,
                rank_fn=_build_rank_fn(_relevance_decisions({})),
            )
        return []

    needle = query.strip()
    since_iso = parse_since(since)

    # FTS5/bm25 fast path — only when a lexical index is injected and we're
    # not in embeddings mode (that path keeps its substring+cosine
    # pipeline). ``None`` return ⇒ index not populated ⇒ fall through to
    # the LIKE scan below.
    if search_service is not None and not use_embeddings:
        fts = _fts_decisions(
            conn, search_service, needle,
            project=project, since_iso=since_iso, limit=limit,
        )
        if fts is not None:
            out, rank_fn = fts
            _record_loaded(conn, "search_past_decisions", out)
            if context_budget is not None:
                return _budgeted(out, context_budget=context_budget, rank_fn=rank_fn)
            return out

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
    # generation, plus (c) total needle occurrences per session for the
    # relevance term, plus (d) — under ``use_embeddings`` — the
    # message id behind that first-hit content_text so embeddings can
    # be computed at the message grain (the unique key on the
    # embedding cache). SQLite's window functions would solve this in
    # one query but we keep the SQL portable: pull
    # ``(message_id, session_fk, session_id, content_text)`` hits sorted
    # by timestamp DESC and fold in Python.
    hit_rows = conn.execute(
        "SELECT m.id AS mid, m.session_fk AS sfk, s.session_id AS sid, "  # noqa: S608 — where_extra is built from fixed clauses + parameter placeholders
        "m.content_text AS content_text "
        "FROM messages m "
        "JOIN sessions s ON s.id = m.session_fk "
        "JOIN projects p ON p.id = s.project_id "
        f"WHERE m.content_text LIKE ?{where_extra} "
        "ORDER BY m.timestamp DESC",
        params,
    ).fetchall()

    needle_lower = needle.lower()
    snippet_by_sfk: dict[int, str | None] = {}
    # first-hit message_id per session_fk — drives the embedding compute
    # when ``use_embeddings`` is set (one row per session, not one per
    # raw needle hit).
    first_mid_by_sfk: dict[int, int] = {}
    occ_by_sid: dict[str, int] = {}
    # matching *messages* per session_fk → the session-clustering count
    # (``more_matches_in_session`` = this - 1). Promoted into the shared
    # path so the LIKE branch clusters exactly like the FTS branch.
    msg_count_by_sfk: dict[int, int] = {}
    for hr in hit_rows:
        sfk = int(hr["sfk"])
        content = hr["content_text"] or ""
        occ_by_sid[hr["sid"]] = occ_by_sid.get(hr["sid"], 0) + content.lower().count(needle_lower)
        msg_count_by_sfk[sfk] = msg_count_by_sfk.get(sfk, 0) + 1
        if sfk in snippet_by_sfk:
            continue
        snippet_by_sfk[sfk] = _build_snippet(content, needle)
        first_mid_by_sfk[sfk] = int(hr["mid"])

    # Compute embedding scores up-front so they can both populate the
    # ``embedding_score`` field on each match AND replace the LIKE-
    # density signal in the rank fn. ``score_by_sfk`` maps session_fk
    # → cosine in [0, 1]. Empty when ``use_embeddings`` is False.
    score_by_sfk: dict[int, float] = {}
    if use_embeddings and first_mid_by_sfk:
        score_by_sfk = _compute_embedding_scores(
            conn,
            query=needle,
            mid_by_sfk=first_mid_by_sfk,
            model_name=model_name,
        )

    if use_embeddings:
        # Replace the LIKE-density relevance term with the cosine score
        # carried on each match. The fallback when ``embedding_score``
        # is None (orphan rows, encoder failure on one message) is 0.0
        # — that row sinks to the bottom of the rank but stays in the
        # surface so the agent can still see it.
        rank_fn = _build_rank_fn(_relevance_embeddings)
    else:
        rank_fn = _build_rank_fn(_relevance_decisions(occ_by_sid))

    if not snippet_by_sfk:
        if context_budget is not None:
            return _budgeted([], context_budget=context_budget, rank_fn=rank_fn)
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

    # ``rows`` is ``last_ts DESC``; ``limit`` is a hard cap applied here
    # before any budget re-ranking (the budget is the *additional*
    # constraint, never a way to see past ``--limit``).
    out: list[SessionMatch] = []
    for r in rows:
        sfk = int(r["session_fk"])
        emb_score = score_by_sfk.get(sfk) if use_embeddings else None
        more = msg_count_by_sfk.get(sfk, 1) - 1
        out.append(
            _row_to_match(
                r,
                snippet=snippet_by_sfk.get(sfk),
                embedding_score=emb_score,
                more_matches_in_session=more or None,
            )
        )
        if limit and limit > 0 and len(out) >= limit:
            break
    _record_loaded(conn, "search_past_decisions", out)
    if context_budget is not None:
        return _budgeted(out, context_budget=context_budget, rank_fn=rank_fn)
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
#
# A user-turn match against any of these phrases counts as an *explicit*
# signal — see ``_OUTCOME_CONF_EXPLICIT`` in :func:`_classify_outcome`.
# Silence in the same window is treated separately and earns a much lower
# confidence score so callers can filter it out.
OUTCOME_KEYWORDS: dict[str, tuple[str, ...]] = {
    "revert": (
        "undo", "undo that", "undo it",
        "revert", "revert that", "revert it",
        "roll back", "rollback", "roll that back",
        "take that back", "back it out", "back that out",
        "try again", "try a different",
        "git revert", "git reset --hard", "git checkout --",
    ),
    "negative": (
        "no", "nope",
        "that broke", "you broke", "broke it", "broke the build", "broke the tests",
        "broke", "broken",
        "still broken", "still failing", "still fails", "still errors",
        "doesn't work", "does not work", "didn't work", "did not work",
        "not working", "isn't working", "won't work", "wont work", "stopped working",
        "failing", "tests fail", "test failed", "build failed",
        "wrong", "that's wrong", "thats wrong", "incorrect",
        "mistake", "error",
        "not what i asked", "not what i wanted", "not what i meant",
        "that's not right", "thats not right", "that's not it", "thats not it",
        "no good", "doesn't help", "didn't help",
        "regression", "regressed",
        "❌", "👎",
    ),
    "positive": (
        "thanks", "thank you", "thx", "ty",
        "that worked", "it worked", "works now", "working now",
        "that works", "it works", "works great", "works perfectly",
        "tests pass", "tests passed", "tests passing", "passes",
        "fixed", "solved",
        "perfect", "nice", "great", "awesome", "excellent",
        "ship it", "lgtm", "looks good", "looks great", "love it",
        "exactly right", "that's it", "thats it", "nailed it",
        "correct", "+1",
        "👍", "🎉", "✅", "✓",
    ),
}

# The confidence we attribute to each signal kind. Kept as module
# constants so tests can assert against them and a future tuning pass
# can shift one without grepping the body of ``_classify_outcome``.
_OUTCOME_CONF_DETERMINISTIC = 1.0  # captured_events flag (reserved)
_OUTCOME_CONF_EXPLICIT = 0.8       # in-vocabulary phrase from a user turn
_OUTCOME_CONF_TOOL_REVERT = 0.5    # agent ran a git revert / reset / restore
_OUTCOME_CONF_SILENCE = 0.3        # "no complaint before session ended"
_OUTCOME_CONF_NONE = 0.0           # anchor was the last recorded turn

# Default minimum confidence for the two public outcome functions.
# Surface a confirmed outcome only when we have explicit or tool-level
# evidence (≥ 0.5). The 0.3 "silence ⇒ worked" rows stay in the data but
# are filtered out by default so the discovery commands stop pretending
# every quiet session was a success. Power-users opt back in with
# ``--min-confidence 0.3`` / ``min_confidence=0.3``.
DEFAULT_MIN_OUTCOME_CONFIDENCE = 0.5

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
    """Build an alternation regex, longest phrase first.

    Phrases that start or end with a word character get a ``\\b`` boundary
    on that side (so bare ``no`` doesn't fire on "another" / "node"). The
    boundary is dropped for emoji and punctuation tokens (``❌`` / ``+1``)
    where ``\\b`` would never match in Python's regex.
    """
    parts = sorted({w.strip() for w in words if w.strip()}, key=len, reverse=True)
    alts: list[str] = []
    for p in parts:
        body = re.escape(p)
        left = r"\b" if p[0].isalnum() else ""
        right = r"\b" if p[-1].isalnum() else ""
        alts.append(left + body + right)
    return re.compile("(?:" + "|".join(alts) + ")", re.IGNORECASE)


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
) -> tuple[str, str, int, float]:
    """Infer the outcome of the action at ``messages[anchor_idx]``.

    ``messages`` must be the session's rows in conversation order (sorted
    by ``seq``); each row exposes ``id``, ``role``, ``content_text``,
    ``tools_json`` and ``is_sidechain``. We walk strictly forward from the
    anchor, skipping sidechain rows (a Task sub-agent's transcript doesn't
    speak for the parent session), and look at up to ``lookahead`` real
    user turns plus any agent revert command in between.

    Returns ``(outcome, evidence, outcome_msg_id, outcome_confidence)``.
    ``outcome`` is one of ``"worked"`` / ``"failed"`` / ``"reverted"`` /
    ``"uncertain"``. ``outcome_confidence`` is in ``[0.0, 1.0]`` — see the
    :class:`OutcomeMatch` docstring for the rubric.

    Confidence ladder (transcript fallback only — deterministic 1.0 is
    reserved for ``captured_events``):

    * explicit in-vocabulary user phrase ⇒ ``_OUTCOME_CONF_EXPLICIT``
      (currently 0.8)
    * agent revert tool call (``git revert`` / ``reset`` / ``restore``)
      ⇒ ``_OUTCOME_CONF_TOOL_REVERT`` (0.5)
    * session continued / ended with no signal ⇒
      ``_OUTCOME_CONF_SILENCE`` (0.3) for the ``"worked"`` reading,
      ``_OUTCOME_CONF_NONE`` (0.0) when the anchor was already the last
      turn.
    """
    anchor_id = int(_row_value(messages[anchor_idx], "id", 0) or 0)

    tail = [m for m in list(messages)[anchor_idx + 1:] if not _is_sidechain_row(m)]
    if not tail:
        return (
            "uncertain",
            "action is the last recorded turn in the session — no follow-up to judge",
            anchor_id,
            _OUTCOME_CONF_NONE,
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
                    _OUTCOME_CONF_TOOL_REVERT,
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
            return ("reverted", f"user wrote: '{excerpt}'", mid, _OUTCOME_CONF_EXPLICIT)
        if klass == "negative":
            return ("failed", f"user wrote: '{excerpt}'", mid, _OUTCOME_CONF_EXPLICIT)
        return ("worked", f"user wrote: '{excerpt}'", mid, _OUTCOME_CONF_EXPLICIT)

    # Window exhausted (or session ended) with no explicit signal.
    if user_turns_seen == 0:
        # The session continued — more agent work, tool calls, maybe a
        # sub-agent — but the user never came back to complain or ask
        # for a revert. The old heuristic called that "worked"
        # confidently; we still return ``"worked"`` for backward
        # compatibility but stamp it with ``_OUTCOME_CONF_SILENCE`` so
        # callers filter it out by default.
        return (
            "worked",
            "session continued after the action with no user complaint or revert",
            int(_row_value(messages[-1], "id", anchor_id) or anchor_id),
            _OUTCOME_CONF_SILENCE,
        )
    return (
        "uncertain",
        f"{user_turns_seen} follow-up user turn(s) but none confirmed or rejected the action",
        last_user_id,
        _OUTCOME_CONF_NONE,
    )


def _row_to_outcome_match(
    row: sqlite3.Row,
    outcome: str,
    evidence: str,
    outcome_msg_id: int,
    outcome_confidence: float = 0.0,
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
        outcome_confidence=float(outcome_confidence),
    )


def _outcome_matches_for(
    conn: sqlite3.Connection,
    anchor_seq_by_fk: dict[int, int],
    *,
    wanted_outcomes: set[str],
    limit: int,
    min_confidence: float = DEFAULT_MIN_OUTCOME_CONFIDENCE,
) -> list[OutcomeMatch]:
    """Back half shared by the two outcome functions.

    ``anchor_seq_by_fk`` maps a candidate ``sessions.id`` to the ``seq``
    of its anchor message (the *last* one that matched the query). For
    each session — newest ``last_ts`` first — we pull the rows from the
    anchor's ``seq`` onward, classify the outcome (the first row is the
    anchor), and keep the session when ``outcome`` is in
    ``wanted_outcomes`` *and* ``outcome_confidence >= min_confidence``.
    ``limit`` (> 0) caps the count and lets us stop loading early once
    we've got enough.
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
        outcome, evidence, msg_id, confidence = _classify_outcome(msg_rows, 0)
        if outcome not in wanted_outcomes:
            continue
        if confidence < min_confidence:
            continue
        out.append(
            _row_to_outcome_match(meta, outcome, evidence, msg_id, confidence),
        )
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
    min_confidence: float = DEFAULT_MIN_OUTCOME_CONFIDENCE,
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
    ``"worked"`` AND whose ``outcome_confidence >= min_confidence`` are
    returned, sorted by ``last_ts`` DESC. Default ``min_confidence`` is
    ``0.5`` — explicit-phrase confirmations clear it, "silence ⇒ worked"
    rows (confidence 0.3) do not. Pass ``0.0`` to restore the old
    "anything that didn't break is a success" behaviour.

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
    min_confidence:
        Minimum ``outcome_confidence`` for a row to be returned. Defaults
        to :data:`DEFAULT_MIN_OUTCOME_CONFIDENCE` (``0.5``). Clamped into
        ``[0.0, 1.0]``.
    """
    _ensure_row_factory(conn)
    if not action or not action.strip():
        return []
    min_confidence = max(0.0, min(1.0, float(min_confidence)))
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
        min_confidence=min_confidence,
    )


def find_failure_modes_for_file(
    conn: sqlite3.Connection,
    file_path: str,
    *,
    since: str | None = None,
    limit: int = 20,
    min_confidence: float = DEFAULT_MIN_OUTCOME_CONFIDENCE,
) -> list[OutcomeMatch]:
    """Sessions where editing ``file_path`` led to a follow-up correction.

    Candidate sessions are those with at least one Edit / Write /
    MultiEdit / NotebookEdit tool call whose arguments reference
    ``file_path``. The anchor is the *last* such edit; the outcome is
    inferred forward from it. Sessions whose inferred outcome is
    ``"failed"`` or ``"reverted"`` AND whose ``outcome_confidence >=
    min_confidence`` are returned, sorted by ``last_ts`` DESC — i.e.
    "here's where touching this file went wrong, and why". Default
    ``min_confidence`` is ``0.5`` — both ``failed`` (explicit phrase,
    confidence 0.8) and ``reverted`` (explicit phrase 0.8 or agent
    revert tool call 0.5) clear it.

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
    min_confidence:
        Minimum ``outcome_confidence`` for a row to be returned. Defaults
        to :data:`DEFAULT_MIN_OUTCOME_CONFIDENCE` (``0.5``). Clamped into
        ``[0.0, 1.0]``.
    """
    _ensure_row_factory(conn)
    resolved = _resolve_input_path(file_path)
    since_iso = parse_since(since)
    min_confidence = max(0.0, min(1.0, float(min_confidence)))

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
        min_confidence=min_confidence,
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
