"""command_mart — (day, project_id, command_name) rollup.

A "command" here is a slash command issued by the user
(``/init``, ``/review``, ``/help``, ...) plus a synthetic ``freeform``
bucket for non-slash prompts. Each user prompt potentially triggers
0..N billable assistant turns; the mart attributes each assistant
event back to the most recent preceding user message in the same
session, then groups by the parsed slash-command of that message.

User messages themselves are NOT in ``usage_events`` — only billable
assistant rows are. The mart builder therefore JOINs ``usage_events``
to ``messages`` on ``source_message_fk`` and uses the per-session
``seq`` order in ``messages`` to find each event's parent prompt.

Pattern selection
=================

Additive — same family as ``daily_mart``. Each refresh window touches
a small set of ``(day, project, command_name)`` keys, and the
ON CONFLICT path adds new SUM/COUNT(*) contributions onto existing
rows.

``session_count`` follows the additive-mart trap from
HANDOFF §"`session_count` correctness across windows" — recomputed
for affected keys after the additive upsert.

Caveats
=======

* The "preceding user message" is found per session_fk × seq. If the
  conversation has tool-result rows or summary entries between the
  user prompt and the assistant turn, we still attribute correctly
  because we walk back to the most recent ``role='user'`` row.
* If an event has no preceding user message in the same session
  (rare — orphaned assistant turn from a malformed source file), we
  attribute it to the synthetic ``__no_prompt__`` command name so
  cost-conservation across the mart still holds.
"""

from __future__ import annotations

import re
import sqlite3
from typing import Any

from .base import MartBuilder

# Slash-command parser: ``/`` followed by 1..64 letters/digits/dashes/
# underscores at the very start of the prompt. Matches ``/init``,
# ``/review-pr``, ``/help`` but not ``// comment`` or ``/abs/path``.
# Length cap is defensive against pathological inputs.
_SLASH_RE = re.compile(r"^/([A-Za-z][A-Za-z0-9_-]{0,63})\b")

# Synthetic bucket for prompts that don't start with a slash command.
FREEFORM = "freeform"

# Synthetic bucket for assistant events with no preceding user message
# in the same session (data integrity escape hatch).
_NO_PROMPT = "__no_prompt__"


class CommandMartBuilder(MartBuilder):
    """Per-(day, project_id, command_name) cost + token rollup."""

    name = "command"

    def refresh(self, conn: sqlite3.Connection, since_event_id: int) -> int:
        max_id = _max_event_id(conn)
        if max_id <= since_event_id:
            return since_event_id

        rows = _fetch_window(conn, since_event_id=since_event_id, max_id=max_id)

        # Cache: (session_fk, event_seq) → command_name. Many events share
        # the same parent user message — caching the seq lookup avoids a
        # per-event scan. The cache is per-refresh and bounded by the
        # window size.
        prompt_cache: dict[tuple[int, int], str] = {}
        buckets: dict[tuple[str, int, str], dict[str, float]] = {}

        for r in rows:
            session_fk = r["session_fk"]
            seq = r["event_seq"]
            if session_fk is None or seq is None:
                command_name = _NO_PROMPT
            else:
                key = (int(session_fk), int(seq))
                if key in prompt_cache:
                    command_name = prompt_cache[key]
                else:
                    command_name = _find_command_for(conn, session_fk, seq)
                    prompt_cache[key] = command_name

            agg_key = (r["day"], int(r["project_id"]), command_name)
            bucket = buckets.setdefault(
                agg_key,
                {
                    "event_count": 0,
                    "cost_usd": 0.0,
                    "tokens_in": 0,
                    "tokens_out": 0,
                },
            )
            bucket["event_count"] += 1
            bucket["cost_usd"] += float(r["cost_usd"] or 0.0)
            bucket["tokens_in"] += int(r["input_tokens"] or 0)
            bucket["tokens_out"] += int(r["output_tokens"] or 0)

        if buckets:
            conn.executemany(
                """
                INSERT INTO command_mart (
                    day, project_id, command_name,
                    event_count, cost_usd, tokens_in, tokens_out,
                    session_count
                ) VALUES (?, ?, ?, ?, ?, ?, ?, 0)
                ON CONFLICT (day, project_id, command_name) DO UPDATE SET
                    event_count = event_count + excluded.event_count,
                    cost_usd    = cost_usd    + excluded.cost_usd,
                    tokens_in   = tokens_in   + excluded.tokens_in,
                    tokens_out  = tokens_out  + excluded.tokens_out
                """,
                [
                    (
                        k[0], k[1], k[2],
                        v["event_count"], v["cost_usd"],
                        v["tokens_in"], v["tokens_out"],
                    )
                    for k, v in buckets.items()
                ],
            )

            # ── recompute session_count for affected keys ──────────
            _recompute_session_counts(conn, list(buckets.keys()))

        return max_id

    def rebuild_from_scratch(self, conn: sqlite3.Connection) -> None:
        conn.execute("DELETE FROM command_mart")
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

    JOINs ``usage_events`` to ``messages`` to surface ``session_fk`` +
    ``seq`` per event. Those two columns are the lookup key for the
    parent user message — we can't get them off ``usage_events``
    directly because the events table only stores ``session_id`` (the
    string from the source file), not the ``messages.session_fk``
    integer used to walk siblings.
    """
    sql = """
        SELECT e.id            AS event_id,
               e.day           AS day,
               e.project_id    AS project_id,
               e.session_id    AS session_id,
               e.cost_usd      AS cost_usd,
               e.input_tokens  AS input_tokens,
               e.output_tokens AS output_tokens,
               m.session_fk    AS session_fk,
               m.seq           AS event_seq
          FROM usage_events e
          LEFT JOIN messages m ON m.id = e.source_message_fk
         WHERE e.id > ? AND e.id <= ?
         ORDER BY e.id
    """
    return [dict(r) for r in conn.execute(sql, (since_event_id, max_id)).fetchall()]


def _find_command_for(
    conn: sqlite3.Connection,
    session_fk: int,
    event_seq: int,
) -> str:
    """Find the command_name attached to the most recent user message.

    Walks back in ``messages`` ORDER BY seq DESC, filtered by
    ``session_fk`` + ``role='user'`` + ``seq < event_seq``. Returns the
    parsed slash-command (or ``FREEFORM``) of that user message's
    ``content_text``. If no preceding user message exists,
    returns ``_NO_PROMPT`` so cost still accounts somewhere.
    """
    row = conn.execute(
        """
        SELECT content_text
          FROM messages
         WHERE session_fk = ?
           AND role = 'user'
           AND seq < ?
         ORDER BY seq DESC
         LIMIT 1
        """,
        (session_fk, event_seq),
    ).fetchone()
    if row is None:
        return _NO_PROMPT
    text = row["content_text"] if hasattr(row, "keys") else row[0]
    return parse_command_name(text or "")


def parse_command_name(content_text: str) -> str:
    """Return the slash-command name from a user prompt or ``FREEFORM``.

    ``"/init args..."`` → ``"/init"``. ``"hello"`` → ``"freeform"``.
    Public so tests + the routes layer can normalise consumer-side
    inputs the same way the mart does.
    """
    if not content_text:
        return FREEFORM
    stripped = content_text.lstrip()
    m = _SLASH_RE.match(stripped)
    if not m:
        return FREEFORM
    return f"/{m.group(1)}"


def _recompute_session_counts(
    conn: sqlite3.Connection,
    keys: list[tuple[str, int, str]],
) -> None:
    """Set ``command_mart.session_count`` for affected keys to true DISTINCT.

    Bounded by ``len(keys)``. We re-derive the per-command session set
    by re-walking the join for each ``(day, project_id)`` group so we
    don't re-parse the same user_text per command_name.
    """
    # Group by (day, project_id) so we run one scan per project-day.
    groups: set[tuple[str, int]] = {(k[0], k[1]) for k in keys}

    for day, project_id in groups:
        rows = conn.execute(
            """
            SELECT e.session_id    AS session_id,
                   m.session_fk    AS session_fk,
                   m.seq           AS event_seq
              FROM usage_events e
              LEFT JOIN messages m ON m.id = e.source_message_fk
             WHERE e.day = ? AND e.project_id = ?
            """,
            (day, project_id),
        ).fetchall()

        per_command_sessions: dict[str, set[str]] = {}
        local_cache: dict[tuple[int, int], str] = {}
        for r in rows:
            session_fk = r["session_fk"]
            seq = r["event_seq"]
            if session_fk is None or seq is None:
                cmd = _NO_PROMPT
            else:
                cache_key = (int(session_fk), int(seq))
                if cache_key in local_cache:
                    cmd = local_cache[cache_key]
                else:
                    cmd = _find_command_for(conn, session_fk, seq)
                    local_cache[cache_key] = cmd
            per_command_sessions.setdefault(cmd, set()).add(
                str(r["session_id"] or "")
            )

        for cmd, session_ids in per_command_sessions.items():
            conn.execute(
                """
                UPDATE command_mart
                   SET session_count = ?
                 WHERE day = ? AND project_id = ? AND command_name = ?
                """,
                (len(session_ids), day, project_id, cmd),
            )
