"""project_mart — one row per project, lifetime totals.

Replace-from-scratch-for-affected-keys pattern. New events for an
existing project invalidate the prior aggregate (``total_sessions``
especially is a DISTINCT count that can't be summed across windows),
so we recompute the project row from all of its events. ``INSERT OR
REPLACE`` on the ``project_id`` PRIMARY KEY does the swap atomically.

``provider``, ``slug``, ``display_name`` come from the ``projects``
table, joined in by id.

Message-type + command dims
===========================
``usage_events`` is assistant-only (the normalizers skip non-billable
rows), so the token/cost/message totals above can't see user turns,
tool-result turns, or commands. The Overview's User / Assistant /
Tool-Use / Tool-Results cards and the Commands KPI need those, so we
materialise five extra columns (v022) computed straight off the
project's ``messages.raw_json`` — running the **same** classifier +
enricher field-extraction ``get_project_stats`` uses, so the counts are
identical to the full-pipeline aggregator (proven by the equivalence
tests). This is a build-time cost (a full ``messages`` scan per affected
project); the dashboard read path stays a single indexed ``project_mart``
lookup, well inside the <100ms budget.

The ``INSERT OR REPLACE`` above lists only the original 13 columns, so it
resets the five dim columns to their ``DEFAULT 0`` on every refresh; the
``_refresh_message_dims`` second pass then UPDATEs the true counts for the
same affected projects (mirrors ``command_mart``'s session_count recompute).
"""

from __future__ import annotations

import json
import sqlite3

from stackunderflow.stats import classifier
from stackunderflow.stats.enricher import (
    _has_result_block,
    _text_from,
    _tools_from,
)

from .base import MartBuilder

# Interruption markers the aggregator's ``_command_analysis`` excludes from
# the command tally (``aggregator._is_interrupt_text``). ``str.startswith``
# accepts a tuple, so this drives the same prefix test the pipeline runs.
_INTERRUPT_MARKERS = (classifier.INTERRUPT_PREFIX, classifier.INTERRUPT_API)


class ProjectMartBuilder(MartBuilder):
    """Per-project lifetime aggregates."""

    name = "project"

    def refresh(self, conn: sqlite3.Connection, since_event_id: int) -> int:
        max_id = _max_event_id(conn)
        if max_id <= since_event_id:
            return since_event_id

        affected = [
            int(r[0])
            for r in conn.execute(
                "SELECT DISTINCT project_id FROM usage_events "
                "WHERE id > ? AND id <= ?",
                (since_event_id, max_id),
            ).fetchall()
        ]

        conn.execute(
            """
            INSERT OR REPLACE INTO project_mart (
                project_id, provider, slug, display_name,
                first_ts, last_ts,
                total_messages, total_sessions,
                total_input_tokens, total_output_tokens,
                total_cache_read, total_cache_create,
                total_cost_usd
            )
            SELECT
                e.project_id,
                p.provider,
                p.slug,
                p.display_name,
                MIN(e.ts),
                MAX(e.ts),
                COUNT(*),
                COUNT(DISTINCT e.session_id),
                SUM(e.input_tokens),
                SUM(e.output_tokens),
                SUM(e.cache_read_tokens),
                SUM(e.cache_create_tokens),
                SUM(e.cost_usd)
            FROM usage_events e
            JOIN projects p ON p.id = e.project_id
            WHERE e.project_id IN (
                SELECT DISTINCT project_id
                FROM usage_events
                WHERE id > ? AND id <= ?
            )
            GROUP BY e.project_id, p.provider, p.slug, p.display_name
            """,
            (since_event_id, max_id),
        )

        # Second pass: materialise the message-type + command dims from the
        # raw ``messages`` (the INSERT above reset them to DEFAULT 0).
        _refresh_message_dims(conn, affected)

        return max_id

    def rebuild_from_scratch(self, conn: sqlite3.Connection) -> None:
        conn.execute("DELETE FROM project_mart")
        self.refresh(conn, since_event_id=0)


def _max_event_id(conn: sqlite3.Connection) -> int:
    row = conn.execute("SELECT MAX(id) AS m FROM usage_events").fetchone()
    if row is None:
        return 0
    val = row["m"] if hasattr(row, "keys") else row[0]
    return int(val) if val is not None else 0


def _refresh_message_dims(
    conn: sqlite3.Connection, project_ids: list[int]
) -> None:
    """Recompute the message-type + command dims for *project_ids*.

    Scans every ``messages`` row of each project (joined via ``sessions``)
    and classifies it with the same functions ``get_project_stats`` runs,
    then UPDATEs the materialised counts onto the project's mart row. A
    project with no mart row (no billable events) is silently skipped — the
    UPDATE matches nothing.
    """
    for pid in project_ids:
        rows = conn.execute(
            "SELECT m.raw_json AS raw_json "
            "FROM messages m "
            "JOIN sessions s ON s.id = m.session_fk "
            "WHERE s.project_id = ?",
            (pid,),
        ).fetchall()
        dims = _count_message_dims(
            (r["raw_json"] if hasattr(r, "keys") else r[0]) for r in rows
        )
        conn.execute(
            "UPDATE project_mart SET "
            "total_user_messages = ?, "
            "total_assistant_messages = ?, "
            "total_tool_use_messages = ?, "
            "total_tool_result_messages = ?, "
            "total_commands = ? "
            "WHERE project_id = ?",
            (
                dims["user"],
                dims["assistant"],
                dims["tool_use"],
                dims["tool_result"],
                dims["commands"],
                pid,
            ),
        )


def _count_message_dims(raw_jsons) -> dict[str, int]:
    """Count message-type + command dims over an iterable of ``raw_json`` strings.

    Mirrors ``aggregator.summarise`` exactly by reusing the classifier's
    ``_determine_kind`` and the enricher's field extraction:

    * ``user`` / ``assistant`` == ``overview.message_types`` kind counts.
    * ``tool_use`` == assistant records carrying ``tool_use`` blocks
      (``message_dict['type']=='assistant' and message_dict['tools']``).
    * ``tool_result`` == records carrying a ``tool_result`` block
      (``message_dict['has_tool_result']``).
    * ``commands`` == ``user_interactions.user_commands_analyzed``: a real
      user turn — kind ``user``, not a tool_result, not an interruption.

    Defensive against unparseable rows (a poison row must never break the
    mart refresh / ingest): a row whose ``raw_json`` won't decode to a dict
    is skipped, same coverage the per-message marts already tolerate.
    """
    user = assistant = tool_use = tool_result = commands = 0
    for rj in raw_jsons:
        try:
            payload = json.loads(rj) if rj else {}
        except (json.JSONDecodeError, TypeError, ValueError):
            continue
        if not isinstance(payload, dict):
            continue
        kind = classifier._determine_kind(payload)
        raw_msg = payload.get("message")
        msg = raw_msg if isinstance(raw_msg, dict) else {}
        has_tool_result = _has_result_block(msg)
        if has_tool_result:
            tool_result += 1
        if kind == "user":
            user += 1
            if not has_tool_result and not _text_from(payload).startswith(
                _INTERRUPT_MARKERS
            ):
                commands += 1
        elif kind == "assistant":
            assistant += 1
            if _tools_from(msg):
                tool_use += 1
    return {
        "user": user,
        "assistant": assistant,
        "tool_use": tool_use,
        "tool_result": tool_result,
        "commands": commands,
    }
