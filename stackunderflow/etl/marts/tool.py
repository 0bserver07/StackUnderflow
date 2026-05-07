"""tool_mart — (day, project_id, provider, tool_name) rollup.

A billable event in ``usage_events`` corresponds to one assistant
``messages`` row. That row's ``tools_json`` carries the names of every
tool the assistant invoked in that turn. ``tool_mart`` fans the event
out across the message's distinct tool names so a Read+Edit message
contributes one row's worth of attribution to ``Read`` and one row's
worth to ``Edit``.

Cost attribution mirrors the legacy
``stats.aggregator._ToolCostCollector`` (spec §1.3): each distinct tool
name in the message gets ``1 / N`` of the message's cost where ``N`` is
the count of distinct tool names. Token columns follow the same 1/N
attribution so SUM(tokens_in) across the mart converges on the source
event's input_tokens (within float rounding).

Pattern selection
=================

This is an **additive** mart — same family as ``daily_mart`` and
``provider_day_mart``. The ``(day, project_id, provider, tool_name)``
key is dense enough that a watermark window only touches a small set
of keys, so ``ON CONFLICT DO UPDATE`` adds the new SUM/COUNT(*)
contribution onto existing rows.

``session_count`` follows the additive-mart trap from
HANDOFF §"`session_count` correctness across windows": a session that
uses ``Read`` on the same day in two refresh windows would naively
count as 2. After the additive upsert we recompute ``session_count``
for the affected keys. The recompute scans the **full day** for any
touched ``(day, project, provider)`` triple — see
``_recompute_session_counts`` for the cost shape.

Sourcing
========

We JOIN ``usage_events`` to ``messages`` on ``source_message_fk`` and
parse ``tools_json`` host-side (Python). Pure-SQL with ``json_each``
would be tighter but ``messages.tools_json`` is sometimes the literal
string ``'[]'`` and we want the same defensive parse the aggregator
uses. The watermark-window scan in ``refresh()`` is always over a
small chunk; the ``session_count`` recompute scan is **not** —
see ``_recompute_session_counts`` for its real cost shape.
"""

from __future__ import annotations

import json
import sqlite3
from collections import Counter
from typing import Any

from .base import MartBuilder

# ── instrumentation ────────────────────────────────────────────────────
#
# Process-local counter: incremented once per
# ``(day, project_id, provider)`` group scanned inside
# ``_recompute_session_counts``. Tests reset it before a refresh and
# assert the post-refresh value to confirm the per-group dedup is
# intact — i.e., a watermark window touching K events all in one
# group costs exactly **one** group-scan, not K.
#
# Not part of the public API. Don't read it from app code; if the
# value is interesting outside tests, promote it to a real metric.
_session_count_recompute_calls = 0


class ToolMartBuilder(MartBuilder):
    """Per-(day, project, provider, tool_name) cost + token rollup."""

    name = "tool"

    def refresh(self, conn: sqlite3.Connection, since_event_id: int) -> int:
        max_id = _max_event_id(conn)
        if max_id <= since_event_id:
            return since_event_id

        rows = _fetch_window(conn, since_event_id=since_event_id, max_id=max_id)

        # Bucket by (day, project_id, provider, tool_name) and accumulate
        # the additive measures (event_count, cost_usd, tokens_in/out).
        buckets: dict[tuple[str, int, str, str], dict[str, float]] = {}
        for r in rows:
            tool_names = _parse_tool_names(r["tools_json"])
            if not tool_names:
                continue
            n = len(tool_names)
            cost_share = float(r["cost_usd"] or 0.0) / n
            in_share = int(r["input_tokens"] or 0) / n
            out_share = int(r["output_tokens"] or 0) / n
            for tool_name in tool_names:
                key = (
                    r["day"], int(r["project_id"]),
                    r["provider"], tool_name,
                )
                bucket = buckets.setdefault(
                    key,
                    {"event_count": 0, "cost_usd": 0.0,
                     "tokens_in": 0.0, "tokens_out": 0.0},
                )
                bucket["event_count"] += 1
                bucket["cost_usd"] += cost_share
                bucket["tokens_in"] += in_share
                bucket["tokens_out"] += out_share

        if buckets:
            conn.executemany(
                """
                INSERT INTO tool_mart (
                    day, project_id, provider, tool_name,
                    event_count, cost_usd, tokens_in, tokens_out,
                    session_count
                ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, 0)
                ON CONFLICT (day, project_id, provider, tool_name) DO UPDATE SET
                    event_count = event_count + excluded.event_count,
                    cost_usd    = cost_usd    + excluded.cost_usd,
                    tokens_in   = tokens_in   + excluded.tokens_in,
                    tokens_out  = tokens_out  + excluded.tokens_out
                """,
                [
                    (
                        k[0], k[1], k[2], k[3],
                        v["event_count"], v["cost_usd"],
                        int(v["tokens_in"]), int(v["tokens_out"]),
                    )
                    for k, v in buckets.items()
                ],
            )

        # ── recompute session_count for affected keys ──────────────────
        # COUNT(DISTINCT session_id) is not additive across refresh
        # windows. Recompute from the full join for the (day, project,
        # provider, tool_name) buckets touched by this refresh. The
        # cost is **not** bounded by the watermark window — see
        # ``_recompute_session_counts`` for the real cost shape.
        if buckets:
            _recompute_session_counts(conn, list(buckets.keys()))

        return max_id

    def rebuild_from_scratch(self, conn: sqlite3.Connection) -> None:
        conn.execute("DELETE FROM tool_mart")
        self.refresh(conn, since_event_id=0)


# ── helpers ────────────────────────────────────────────────────────────


def _max_event_id(conn: sqlite3.Connection) -> int:
    row = conn.execute("SELECT MAX(id) AS m FROM usage_events").fetchone()
    if row is None:
        return 0
    val = row["m"] if hasattr(row, "keys") else row[0]
    return int(val) if val is not None else 0


def _fetch_window(
    conn: sqlite3.Connection, *, since_event_id: int, max_id: int,
) -> list[dict[str, Any]]:
    """Return joined event+message rows in (since, max] for this refresh.

    JOINs ``usage_events`` to ``messages`` so we get ``tools_json`` per
    event without a second round-trip. ``LEFT JOIN`` defends against
    events whose source message was deleted (shouldn't happen given the
    FK with ON DELETE CASCADE, but the mart layer is read-only against
    arbitrary stores so we tolerate it).
    """
    sql = """
        SELECT e.id            AS event_id,
               e.day           AS day,
               e.project_id    AS project_id,
               e.provider      AS provider,
               e.session_id    AS session_id,
               e.cost_usd      AS cost_usd,
               e.input_tokens  AS input_tokens,
               e.output_tokens AS output_tokens,
               m.tools_json    AS tools_json
          FROM usage_events e
          LEFT JOIN messages m ON m.id = e.source_message_fk
         WHERE e.id > ? AND e.id <= ?
         ORDER BY e.id
    """
    return [dict(r) for r in conn.execute(sql, (since_event_id, max_id)).fetchall()]


def _parse_tool_names(tools_json: str | None) -> list[str]:
    """Parse ``messages.tools_json`` into a deduped list of tool names.

    The writer stores ``json.dumps(list(rec.tools))`` so the value is a
    JSON array of strings. The legacy aggregator's tool-cost collector
    keys on the **distinct** set of names per message (1/N attribution
    across the distinct names). We match that contract here — a turn
    that calls ``Read`` three times is one ``Read`` bucket, not three.

    Defensive: malformed JSON, empty arrays, and non-string entries all
    return an empty list. The mart silently drops events with no
    parseable tool list — they contribute nothing to the per-tool
    rollup.
    """
    if not tools_json:
        return []
    try:
        parsed = json.loads(tools_json)
    except (json.JSONDecodeError, TypeError):
        return []
    if not isinstance(parsed, list):
        return []
    counts: Counter[str] = Counter()
    for entry in parsed:
        if isinstance(entry, str) and entry:
            counts[entry] += 1
    return list(counts.keys())


def _recompute_session_counts(
    conn: sqlite3.Connection,
    keys: list[tuple[str, int, str, str]],
) -> None:
    """Set ``tool_mart.session_count`` for the given keys to the true DISTINCT.

    Cost shape — read this carefully
    --------------------------------

    The keys are first deduped down to their distinct
    ``(day, project_id, provider)`` groups. For each group we run one
    SQL scan over **every** ``usage_events`` row matching that
    ``(day, project_id, provider)`` triple — *not* just the rows in
    this refresh's watermark window — because
    ``COUNT(DISTINCT session_id)`` cannot be reconstructed from the
    window alone (a session that touched the tool in an earlier
    window would be invisible).

    Concrete cost::

        O(distinct (day, project, provider) groups touched
          ×  events-per-day-of-touched-groups)

    On the maintainer's real store, a busy day with 10K+ events for
    a single ``(day, project, provider)`` triple forces a full 10K-row
    re-scan + ``tools_json`` reparse on every refresh cycle that
    touches that triple. ``len(keys)`` (the per-tool key fanout) does
    *not* bound the work; only the underlying group fanout does.

    Practical bounds:

    * Distinct ``(day, project, provider)`` groups touched in a
      refresh window are typically 1..10 — the watermark advances
      often enough that one window only covers a handful of
      project-days. The watcher's 200 ms debounce keeps it small.
    * Per-group event count tracks how busy the day was. The
      ``idx_events_project ON usage_events(project_id, day)`` index
      covers the predicate so SQLite reads only the relevant slice
      of the events table — no full-table scan — but it still walks
      every row in that slice and parses each ``tools_json``.

    Why this design (option (d) from the design discussion)
    -------------------------------------------------------

    Alternatives considered and rejected:

    (a) Add per-window distinct counts to a stored running total —
        wrong; ``COUNT(DISTINCT)`` does not compose that way.
    (b) Scan only the watermark window per group — undercounts; a
        session that touched the tool in a previous window vanishes.
    (c) Maintain a per-(day, project, provider, tool, session)
        presence table — would make the recompute O(touched-keys)
        but adds a table whose row count is potentially larger than
        ``tool_mart`` itself, plus an extra write on every refresh.
        Not worth the storage today.
    (d) **Current approach** — accept the full-day-per-touched-group
        scan and document its real cost honestly.

    Tests inspect the module-level
    ``_session_count_recompute_calls`` counter (incremented once per
    group below) to verify the per-group dedup is intact — i.e., we
    never fan out worse than O(distinct groups touched).
    """
    # Group by (day, project_id, provider) so we parse each event's
    # tools_json once per group rather than once per tool_name × group.
    # Also collect the wanted tool_names per group so we don't bother
    # building session sets for tools we won't be updating.
    group_tools: dict[tuple[str, int, str], set[str]] = {}
    for k in keys:
        group_tools.setdefault((k[0], k[1], k[2]), set()).add(k[3])

    global _session_count_recompute_calls

    # For each group, fetch every event in scope + its tools_json; build
    # a tool_name → set(session_id) map for the wanted tools only.
    for (day, project_id, provider), wanted_tools in group_tools.items():
        _session_count_recompute_calls += 1
        rows = conn.execute(
            """
            SELECT e.session_id, m.tools_json
              FROM usage_events e
              LEFT JOIN messages m ON m.id = e.source_message_fk
             WHERE e.day = ? AND e.project_id = ? AND e.provider = ?
            """,
            (day, project_id, provider),
        ).fetchall()
        per_tool_sessions: dict[str, set[str]] = {}
        for row in rows:
            tools = _parse_tool_names(row["tools_json"])
            if not tools:
                continue
            sid = str(row["session_id"] or "")
            for t in tools:
                if t in wanted_tools:
                    per_tool_sessions.setdefault(t, set()).add(sid)

        # Update the matching tool_mart rows for this group.
        for tool_name, session_ids in per_tool_sessions.items():
            conn.execute(
                """
                UPDATE tool_mart
                   SET session_count = ?
                 WHERE day = ? AND project_id = ?
                   AND provider = ? AND tool_name = ?
                """,
                (len(session_ids), day, project_id, provider, tool_name),
            )
