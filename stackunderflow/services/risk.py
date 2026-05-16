"""File-risk summarisation — "this file has caused N reverts in M days".

A small, query-time aggregator on top of the existing outcome-aware
discovery functions in :mod:`stackunderflow.services.discovery`. Nothing
new is materialised; we re-use the v0.7.2 outcome heuristic and count
the buckets.

Public surface
--------------

``file_risk_summary(conn, path, *, since=None, recent_limit=5)``
    Returns a dict::

        {
          "path": "<resolved absolute path>",
          "since": "30d" | "2026-04-01" | null,
          "total_sessions":   int,
          "reverted":         int,
          "failed":           int,
          "worked":           int,
          "recent_session_ids": [<up to ``recent_limit`` ids>],
        }

    The buckets count *distinct sessions*. ``total_sessions`` covers
    every session that touched the file (Read or Write or free-form
    mention); the ``reverted`` / ``failed`` / ``worked`` slices look at
    sessions whose *last write-mode mention* of the file matches the
    corresponding outcome (under the same v0.7.2 confidence ladder used
    by :func:`discovery.find_failure_modes_for_file`). Sessions whose
    outcome was ``"uncertain"`` or sat below the default ``0.5``
    confidence threshold are not classified — they stay in
    ``total_sessions`` but don't appear in the three slices.
    ``recent_session_ids`` is the failure-mode (reverted ∪ failed)
    sessions sorted by ``last_ts`` DESC, capped at ``recent_limit``,
    so a consumer that wants "show me what went wrong here last" can
    pull the first id without a follow-up query.

Why not just store the outcome on ``session_mart``?
---------------------------------------------------

The outcome heuristic operates on per-message context (the *anchor* is
"the last write-mode mention of file X" — different per question). A
mart row would have to be per (session, file, action) which is a much
bigger surface. Until that materialisation exists we infer the four
counts at query time; on real stores this stays well under 50 ms for a
typical file because the SQL ``tools_json LIKE`` pre-filter is cheap.

This module is read-only and never touches ``~/.stackunderflow/store.db``
in a way the caller didn't already.
"""

from __future__ import annotations

import sqlite3
from typing import Any

from stackunderflow.services import discovery as _discovery

__all__ = ["file_risk_summary"]


def file_risk_summary(
    conn: sqlite3.Connection,
    path: str,
    *,
    since: str | None = None,
    recent_limit: int = 5,
) -> dict[str, Any]:
    """Summarise the risk of editing ``path`` based on past sessions.

    Parameters
    ----------
    conn:
        Main store connection (``~/.stackunderflow/store.db``).
    path:
        File path to look up. Resolved with the same logic as the rest
        of :mod:`stackunderflow.services.discovery` — ``~`` expanded,
        relative paths resolved against the cwd.
    since:
        Optional cutoff. Same accepted forms as
        :func:`discovery.parse_since` (``"7d"`` / ``"24h"`` / ``"1m"`` /
        ISO date or datetime). Raises ``ValueError`` if malformed.
    recent_limit:
        Maximum number of session ids to surface in
        ``recent_session_ids``. Defaults to ``5`` — the spec's 4 KB
        meta-agent tool cap is comfortable at this size (each id is
        ~36 bytes).
    """
    # Validate ``since`` early via the discovery helper. ValueError on
    # bad input bubbles to the caller (CLI surfaces it as
    # ``--since ...`` Click error; MCP / meta-agent re-raise it).
    since_iso = _discovery.parse_since(since)

    # Resolve path once via the discovery helper so the surfaced
    # ``path`` field matches the file the heuristic actually looked at.
    resolved = _discovery._resolve_input_path(path)
    _discovery._ensure_row_factory(conn)

    # ── total_sessions: every session touching the file (any mode) ──────
    # We can't use ``find_sessions_touching_file`` directly because it
    # doesn't accept ``since`` and we want to apply the cutoff inside
    # SQL when present. Re-implement the cheap LIKE pre-filter inline;
    # match logic mirrors ``discovery.find_sessions_touching_file`` with
    # ``mode='any'``.
    pattern = f"%{resolved}%"
    where = ["(m.tools_json LIKE ? OR m.content_text LIKE ?)"]
    params: list[Any] = [pattern, pattern]
    if since_iso:
        where.append("m.timestamp >= ?")
        params.append(since_iso)
    rows = conn.execute(
        "SELECT DISTINCT s.id AS sfk "  # noqa: S608 — `where` is built from fixed clauses + parameter placeholders
        "FROM messages m JOIN sessions s ON s.id = m.session_fk "
        "WHERE " + " AND ".join(where),
        params,
    ).fetchall()
    total_sessions = len(rows)

    # ── failure-mode classification (reverted + failed) ────────────────
    # Mirrors ``find_failure_modes_for_file``: anchor on the last
    # write-mode mention per session, classify forward, keep only
    # rows whose outcome ∈ wanted with confidence ≥ default threshold.
    fail_mode = _discovery.find_failure_modes_for_file(
        conn, resolved, since=since, limit=0,
    )
    reverted_count = sum(1 for m in fail_mode if m.outcome == "reverted")
    failed_count = sum(1 for m in fail_mode if m.outcome == "failed")

    # ── worked classification ──────────────────────────────────────────
    # We anchor on the same last-write-mode-mention per session and ask
    # ``_outcome_matches_for`` for the ``"worked"`` slice. Inlining the
    # candidate pass (vs. ``find_sessions_where_action_worked``) avoids
    # the substring "Edit" pre-filter, which would miss ``Write`` /
    # ``MultiEdit`` only sessions on the file.
    cand_sql = (
        "SELECT s.id AS sfk, m.seq AS seq, m.tools_json AS tools_json "
        "FROM messages m JOIN sessions s ON s.id = m.session_fk "
        "WHERE m.tools_json LIKE ?"
    )
    cand_params: list[Any] = [pattern]
    if since_iso:
        cand_sql += " AND m.timestamp >= ?"
        cand_params.append(since_iso)
    anchor_seq_by_fk: dict[int, int] = {}
    for r in conn.execute(cand_sql, cand_params).fetchall():
        if not _discovery._tools_json_mentions_file(
            r["tools_json"], file_path=resolved, mode="write",
        ):
            continue
        sfk, seq = int(r["sfk"]), int(r["seq"])
        if seq > anchor_seq_by_fk.get(sfk, -1):
            anchor_seq_by_fk[sfk] = seq
    worked_matches = _discovery._outcome_matches_for(
        conn, anchor_seq_by_fk, wanted_outcomes={"worked"}, limit=0,
    )
    worked_count = len(worked_matches)

    # ── recent_session_ids ─────────────────────────────────────────────
    # Failure-mode sessions sorted newest first (the discovery helper
    # already returns them ``last_ts`` DESC). Distinct by id.
    recent: list[str] = []
    seen: set[str] = set()
    for m in fail_mode:
        if m.session_id in seen:
            continue
        recent.append(m.session_id)
        seen.add(m.session_id)
        if recent_limit > 0 and len(recent) >= recent_limit:
            break

    return {
        "path": resolved,
        "since": since,
        "total_sessions": int(total_sessions),
        "reverted": int(reverted_count),
        "failed": int(failed_count),
        "worked": int(worked_count),
        "recent_session_ids": recent,
    }
