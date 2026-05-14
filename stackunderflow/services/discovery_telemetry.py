"""Citation-feedback telemetry for the discovery surface.

The three discovery commands (``find-sessions-in-path``,
``find-sessions-touching-file``, ``search-past-decisions``) rank
surfaced sessions purely on metadata (recency, cost). This module
records *outcomes*: which surfaced sessions an agent actually looked up
(via the ``session_query`` MCP tool). That signal feeds the
token-budgeted ranking so sessions that consistently earn citations
climb and uncited noise sinks.

Five entry points, all taking the main store connection
(``~/.stackunderflow/store.db``):

* :func:`record_loaded` — bulk-bump ``loaded_count`` for every session a
  discovery command just surfaced. Called from
  ``services.discovery``'s three ``find_*`` functions.
* :func:`record_cited` — bump ``cited_count`` when a previously-surfaced
  session is looked up. Called from the ``session_query`` MCP tool.
* :func:`cite_rate` — ``cited / loaded`` for a single ``(command,
  session_id)`` pair, ``0.0`` if never loaded. The spec's ranking input.
* :func:`cite_rate_terms` — the same thing for *all* sessions of a
  command, in one query, clamped to ``[0, 1]`` and zeroed for demoted
  sessions. This is the ranking-ready map; see "Ranking integration"
  below.
* :func:`demote_candidates` / :func:`mark_demoted` — the
  ``discovery demote-uncited`` sweep: sessions surfaced ``min_loads``+
  times over ``min_age_days``+ days with zero citations.

Cite attribution is **lax** (spec §"Cite-attribution heuristic"): a
``session_query`` lookup counts as a cite for that session regardless of
which discovery command surfaced it, so :func:`record_cited` bumps every
``(command, session_id)`` row for the session. If the session was never
surfaced, the cite is still recorded (a zero-load row per command) so it
survives for future ranking.

Telemetry is **local-only** — the table stores session ids + counters,
never transcript content, and lives in the same SQLite file as
everything else. Writes are gated behind
``STACKUNDERFLOW_DISCOVERY_TELEMETRY`` (default on; set to ``0`` /
``false`` / ``no`` / ``off`` to disable for ephemeral / scripted use).
All writes are best-effort: a failure (read-only DB, locked store,
pre-v009 schema) is swallowed so telemetry never breaks the discovery
query that produced the results.

Ranking integration (spec §Wiring point 3 — wired at merge time with
spec 03's ``pack_within_budget``)::

    # In services.discovery, the per-command rank_fn composes:
    #   recency 0.40 / cost 0.15 / relevance 0.15 / cite_rate 0.30
    cite_terms = discovery_telemetry.cite_rate_terms(conn, command)
    def rank_fn(m: SessionMatch) -> float:
        return (0.40 * recency_score(m)
                + 0.15 * cost_score(m)
                + 0.15 * relevance_score(m)
                + 0.30 * cite_terms.get(m.session_id, 0.0))

``cite_rate_terms`` is intentionally a flat ``{session_id: score}`` map
(not a closure over ``SessionMatch``) so it composes cleanly into any
ranking shape without this module importing ``services.discovery``.
"""

from __future__ import annotations

import os
import sqlite3
from datetime import UTC, datetime

__all__ = [
    "VALID_COMMANDS",
    "telemetry_enabled",
    "record_loaded",
    "record_cited",
    "cite_rate",
    "cite_rate_terms",
    "demote_candidates",
    "mark_demoted",
    "iter_telemetry",
]

# The three discovery commands that emit telemetry. Used as the
# ``command`` column value and as the fan-out set when a citation lands
# for a session that was never surfaced.
VALID_COMMANDS: tuple[str, ...] = (
    "find_sessions_in_path",
    "find_sessions_touching_file",
    "search_past_decisions",
)

_ENV_FLAG = "STACKUNDERFLOW_DISCOVERY_TELEMETRY"
# Anything in this set (case-insensitive, stripped) means "disabled".
_DISABLED_VALUES = frozenset({"0", "false", "no", "off", "none", ""})


# ── env gate ────────────────────────────────────────────────────────────────


def telemetry_enabled() -> bool:
    """True unless ``STACKUNDERFLOW_DISCOVERY_TELEMETRY`` is set falsy.

    Default-on: the env var only needs setting to *disable*. Accepts
    ``0`` / ``false`` / ``no`` / ``off`` / ``none`` / empty (case-
    insensitive) as "off"; anything else (including ``1`` / ``true``)
    keeps it on.
    """
    raw = os.getenv(_ENV_FLAG)
    if raw is None:
        return True
    return raw.strip().lower() not in _DISABLED_VALUES


def _utcnow_iso() -> str:
    return datetime.now(UTC).isoformat()


# ── write paths ─────────────────────────────────────────────────────────────


def record_loaded(
    conn: sqlite3.Connection,
    command: str,
    session_ids: list[str],
) -> None:
    """Bulk-increment ``loaded_count`` for the sessions just surfaced.

    Idempotent in the useful sense: re-surfacing a session bumps its
    counter rather than inserting a duplicate (the ``(command,
    session_id)`` PRIMARY KEY + ``ON CONFLICT`` handles it).
    ``first_loaded_ts`` is set on first insert and never touched again;
    ``last_loaded_ts`` updates every call.

    No-op when ``session_ids`` is empty, telemetry is disabled, or the
    write fails (best-effort — see module docstring).
    """
    if not session_ids or not telemetry_enabled():
        return
    now = _utcnow_iso()
    rows = [(command, sid, now, now) for sid in session_ids if sid]
    if not rows:
        return
    try:
        conn.executemany(
            "INSERT INTO discovery_telemetry "
            "  (command, session_id, loaded_count, cited_count, "
            "   first_loaded_ts, last_loaded_ts) "
            "VALUES (?, ?, 1, 0, ?, ?) "
            "ON CONFLICT(command, session_id) DO UPDATE SET "
            "  loaded_count   = loaded_count + 1, "
            "  last_loaded_ts = excluded.last_loaded_ts",
            rows,
        )
    except sqlite3.Error:
        # Read-only DB, locked store, or pre-v009 schema. Telemetry must
        # never break the discovery query that produced the results.
        pass


def record_cited(
    conn: sqlite3.Connection,
    session_id: str,
    *,
    source_command: str | None = None,
) -> None:
    """Increment ``cited_count`` for a session that was just looked up.

    Called from the ``session_query`` MCP tool (and a future
    ``stackunderflow sessions show``). Lax attribution: bumps every
    existing ``(command, session_id)`` row for the session. If the
    session was never surfaced, seeds a zero-load row per known command
    so the cite survives — once a load lands on one of those rows the
    cite is already counted (cite_rate stays ``0.0`` while
    ``loaded_count == 0``; see :func:`cite_rate`).

    ``source_command``, when one of :data:`VALID_COMMANDS`, narrows the
    "never surfaced" seed to that single command — currently only used to
    keep the table tidy; cross-command attribution would build on it.

    No-op when ``session_id`` is empty, telemetry is disabled, or the
    write fails (best-effort).
    """
    if not session_id or not telemetry_enabled():
        return
    now = _utcnow_iso()
    try:
        cur = conn.execute(
            "UPDATE discovery_telemetry "
            "SET cited_count = cited_count + 1, last_cited_ts = ? "
            "WHERE session_id = ?",
            (now, session_id),
        )
        if cur.rowcount and cur.rowcount > 0:
            return
        # Never surfaced for any command — record the cite anyway so a
        # later load picks it up. Seed under source_command if given,
        # else fan across all three so whichever command surfaces it
        # next already has the cite on its row.
        commands = (
            (source_command,)
            if source_command in VALID_COMMANDS
            else VALID_COMMANDS
        )
        conn.executemany(
            "INSERT INTO discovery_telemetry "
            "  (command, session_id, loaded_count, cited_count, last_cited_ts) "
            "VALUES (?, ?, 0, 1, ?) "
            "ON CONFLICT(command, session_id) DO UPDATE SET "
            "  cited_count   = cited_count + 1, "
            "  last_cited_ts = excluded.last_cited_ts",
            [(c, session_id, now) for c in commands],
        )
    except sqlite3.Error:
        pass


def mark_demoted(
    conn: sqlite3.Connection,
    pairs: list[tuple[str, str]],
) -> int:
    """Set ``demoted = 1`` on the given ``(command, session_id)`` rows.

    Returns the number of rows updated. Used by ``discovery
    demote-uncited`` (non-dry-run). Best-effort; ``0`` on failure or
    empty input.
    """
    if not pairs:
        return 0
    try:
        cur = conn.executemany(
            "UPDATE discovery_telemetry SET demoted = 1 "
            "WHERE command = ? AND session_id = ?",
            list(pairs),
        )
        return cur.rowcount if cur.rowcount and cur.rowcount > 0 else 0
    except sqlite3.Error:
        return 0


# ── read paths ──────────────────────────────────────────────────────────────


def cite_rate(
    conn: sqlite3.Connection,
    command: str,
    session_id: str,
) -> float:
    """Return ``cited_count / loaded_count`` for ``(command, session_id)``.

    ``0.0`` when the pair has never been loaded (``loaded_count == 0``),
    which also covers the "row doesn't exist" and "loaded-but-never-
    cited" cases — so the result is always a finite, NaN-free float.
    The raw ratio is returned (not clamped); :func:`cite_rate_terms`
    gives the ``[0, 1]``-clamped, demotion-aware variant the ranking
    actually uses.
    """
    try:
        row = conn.execute(
            "SELECT loaded_count, cited_count FROM discovery_telemetry "
            "WHERE command = ? AND session_id = ?",
            (command, session_id),
        ).fetchone()
    except sqlite3.Error:
        return 0.0
    if row is None:
        return 0.0
    loaded = int(row["loaded_count"] if hasattr(row, "keys") else row[0] or 0)
    cited = int(row["cited_count"] if hasattr(row, "keys") else row[1] or 0)
    if loaded <= 0:
        return 0.0
    return cited / loaded


def cite_rate_terms(
    conn: sqlite3.Connection,
    command: str,
) -> dict[str, float]:
    """Return ``{session_id: cite_rate_score}`` for every row of ``command``.

    The ranking-ready map: each score is ``min(1.0, cited / loaded)``
    (so the composed rank stays in ``[0, 1]``) and ``0.0`` for any
    session flagged ``demoted`` — the ``demote-uncited`` sweep's whole
    point is to drop those out of default ranking. Sessions with
    ``loaded_count == 0`` (cite recorded before any load) are omitted
    rather than mapped to ``0.0``, so a caller's ``.get(sid, 0.0)``
    treats "unknown" and "loaded-but-uncited" identically.

    Empty dict on any read failure (pre-v009 schema, etc.).
    """
    try:
        rows = conn.execute(
            "SELECT session_id, loaded_count, cited_count, demoted "
            "FROM discovery_telemetry WHERE command = ?",
            (command,),
        ).fetchall()
    except sqlite3.Error:
        return {}
    out: dict[str, float] = {}
    for r in rows:
        if hasattr(r, "keys"):
            sid = r["session_id"]
            loaded = int(r["loaded_count"] or 0)
            cited = int(r["cited_count"] or 0)
            demoted = int(r["demoted"] or 0)
        else:  # pragma: no cover - defensive; default row_factory
            sid, loaded, cited, demoted = r[0], int(r[1] or 0), int(r[2] or 0), int(r[3] or 0)
        if loaded <= 0:
            continue
        out[sid] = 0.0 if demoted else min(1.0, cited / loaded)
    return out


def demote_candidates(
    conn: sqlite3.Connection,
    *,
    min_loads: int = 20,
    min_age_days: int = 7,
) -> list[tuple[str, str, int]]:
    """Sessions surfaced a lot, for a while, and never cited.

    A ``(command, session_id)`` pair is a candidate when *all* hold:

    * ``loaded_count >= min_loads`` — surfaced often enough to matter;
    * ``cited_count == 0`` — never once looked up;
    * ``first_loaded_ts`` is at least ``min_age_days`` old — it's had a
      fair chance (a session surfaced 20× *today* isn't stale yet);
    * not already ``demoted``.

    Returns ``(command, session_id, loaded_count)`` tuples sorted by
    ``loaded_count`` descending (worst offenders first). Empty list when
    neither threshold bites (or on read failure).
    """
    try:
        rows = conn.execute(
            "SELECT command, session_id, loaded_count FROM discovery_telemetry "
            "WHERE loaded_count >= ? "
            "  AND cited_count = 0 "
            "  AND demoted = 0 "
            "  AND first_loaded_ts IS NOT NULL "
            "  AND julianday('now') - julianday(first_loaded_ts) >= ? "
            "ORDER BY loaded_count DESC, command, session_id",
            (int(min_loads), float(min_age_days)),
        ).fetchall()
    except sqlite3.Error:
        return []
    out: list[tuple[str, str, int]] = []
    for r in rows:
        if hasattr(r, "keys"):
            out.append((r["command"], r["session_id"], int(r["loaded_count"] or 0)))
        else:  # pragma: no cover - defensive
            out.append((r[0], r[1], int(r[2] or 0)))
    return out


def iter_telemetry(
    conn: sqlite3.Connection,
    *,
    command: str | None = None,
    session_id: str | None = None,
    limit: int = 50,
) -> list[dict]:
    """Return telemetry rows for the ``discovery telemetry`` CLI introspection.

    Each dict has: ``command``, ``session_id``, ``loaded_count``,
    ``cited_count``, ``cite_rate`` (raw ratio), ``first_loaded_ts``,
    ``last_loaded_ts``, ``last_cited_ts``, ``demoted`` (bool). Ordered
    by most-recently-loaded first. ``limit <= 0`` means no limit. Empty
    list on read failure.
    """
    where: list[str] = []
    params: list[object] = []
    if command:
        where.append("command = ?")
        params.append(command)
    if session_id:
        where.append("session_id = ?")
        params.append(session_id)
    sql = "SELECT * FROM discovery_telemetry"
    if where:
        sql += " WHERE " + " AND ".join(where)
    sql += " ORDER BY last_loaded_ts IS NULL, last_loaded_ts DESC, command, session_id"
    if limit and limit > 0:
        sql += " LIMIT ?"
        params.append(int(limit))
    try:
        rows = conn.execute(sql, params).fetchall()
    except sqlite3.Error:
        return []
    out: list[dict] = []
    for r in rows:
        d = dict(r) if hasattr(r, "keys") else {
            "command": r[0], "session_id": r[1], "loaded_count": r[2],
            "cited_count": r[3], "first_loaded_ts": r[4],
            "last_loaded_ts": r[5], "last_cited_ts": r[6], "demoted": r[7],
        }
        loaded = int(d.get("loaded_count") or 0)
        cited = int(d.get("cited_count") or 0)
        d["cite_rate"] = (cited / loaded) if loaded > 0 else 0.0
        d["demoted"] = bool(d.get("demoted"))
        out.append(d)
    return out
