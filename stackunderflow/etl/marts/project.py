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
resets the dim columns to their ``DEFAULT`` on every refresh; the
``_refresh_message_dims`` second pass then UPDATEs the true counts for the
same affected projects (mirrors ``command_mart``'s session_count recompute).

Overview rate dims (v023)
=========================
The same second pass also materialises the numerators behind the Overview's
cache / interruption / errors blocks (the rates the mart fast-path otherwise
showed as 0): ``total_cache_read_messages`` (cache.hit_rate),
``total_commands_followed_by_interruption`` (interruption_rate),
``total_command_tools`` / ``total_command_steps`` (avg tools/steps per
command), and ``total_records`` / ``total_errors`` / ``errors_by_category``
(errors total / rate / by_category). These come from the SAME enricher +
``aggregator._command_analysis`` / ``_CacheCollector`` / ``_ErrorsCollector``
logic the full pipeline runs, so they're identical to ``get_project_stats``
(proven by the equivalence tests). Rates are derived at read time from these
counts so a slug's per-provider rows stay additive.
"""

from __future__ import annotations

import json
import sqlite3
from collections import Counter

from stackunderflow.stats import aggregator, classifier, enricher
from stackunderflow.stats.classifier import RawEntry
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
    """Recompute the message-type + command + rate dims for *project_ids*.

    Scans every ``messages`` row of each project (joined via ``sessions``,
    ordered by timestamp so the interaction grouping matches the pipeline)
    and classifies it with the same functions ``get_project_stats`` runs,
    then UPDATEs the materialised counts onto the project's mart row. A
    project with no mart row (no billable events) is silently skipped — the
    UPDATE matches nothing.

    Two derivations share the single ``messages`` fetch:

    * ``_count_message_dims`` — the v022 per-message-type + command counts.
    * ``_count_interaction_dims`` — the v023 Overview rate numerators
      (cache-read messages, interruption / tools / steps per command,
      errors total + by_category), which need the full enricher +
      ``aggregator._command_analysis`` pass to match the pipeline exactly.
    """
    for pid in project_ids:
        rows = conn.execute(
            "SELECT m.raw_json AS raw_json, s.session_id AS session_id, "
            "       m.timestamp AS timestamp, p.provider AS provider "
            "FROM messages m "
            "JOIN sessions s ON s.id = m.session_fk "
            "JOIN projects p ON p.id = s.project_id "
            "WHERE s.project_id = ? "
            "ORDER BY m.timestamp",
            (pid,),
        ).fetchall()
        dims = _count_message_dims(r["raw_json"] for r in rows)
        rate = _count_interaction_dims(rows)
        conn.execute(
            "UPDATE project_mart SET "
            "total_user_messages = ?, "
            "total_assistant_messages = ?, "
            "total_tool_use_messages = ?, "
            "total_tool_result_messages = ?, "
            "total_commands = ?, "
            "total_records = ?, "
            "total_errors = ?, "
            "errors_by_category = ?, "
            "total_cache_read_messages = ?, "
            "total_commands_followed_by_interruption = ?, "
            "total_command_tools = ?, "
            "total_command_steps = ? "
            "WHERE project_id = ?",
            (
                dims["user"],
                dims["assistant"],
                dims["tool_use"],
                dims["tool_result"],
                dims["commands"],
                rate["records"],
                rate["errors_total"],
                rate["errors_by_category_json"],
                rate["cache_read_messages"],
                rate["commands_followed_by_interruption"],
                rate["command_tools"],
                rate["command_steps"],
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


def _count_interaction_dims(rows) -> dict[str, object]:
    """Materialise the v023 Overview rate numerators for one project.

    Rebuilds the project's ``EnrichedDataset`` the SAME way
    ``queries.build_enriched_dataset`` does — ``classifier.tag`` over
    ``RawEntry`` rows (clean column timestamp wins over any raw payload ts),
    then ``enricher.build`` — and reads the numerators straight off the
    records / interactions and ``aggregator._command_analysis``. Reusing the
    real pipeline functions keeps these counts byte-for-byte identical to
    ``get_project_stats`` (the equivalence tests pin it):

    * ``cache_read_messages`` == ``_CacheCollector.w_read`` — assistant
      records carrying cache-read tokens; ``cache.hit_rate`` numerator.
    * ``errors_total`` / ``errors_by_category_json`` == ``_ErrorsCollector``
      ``_total`` / ``by_category`` — falsy ``error_category`` buckets to
      ``"Other"`` exactly as the collector does.
    * ``commands_followed_by_interruption`` / ``command_tools`` /
      ``command_steps`` == ``_command_analysis``'s
      ``commands_followed_by_interruption`` / ``total_tools_used`` /
      ``total_assistant_steps`` — the interruption_rate and avg
      tools/steps-per-command numerators.
    * ``records`` == ``len(EnrichedDataset.records)`` — the all-kinds record
      count the aggregator's ``errors.rate`` divides by (distinct from
      ``project_mart.total_messages``, which is the billable-event count).

    Defensive against unparseable rows (parity with ``_count_message_dims``):
    a ``raw_json`` that won't decode to a dict is skipped rather than
    breaking the mart refresh.
    """
    raw_entries: list[RawEntry] = []
    for r in rows:
        rj = r["raw_json"]
        try:
            payload = json.loads(rj) if rj else {}
        except (json.JSONDecodeError, TypeError, ValueError):
            continue
        if not isinstance(payload, dict):
            continue
        if r["timestamp"]:
            payload["timestamp"] = r["timestamp"]
        raw_entries.append(
            RawEntry(
                payload=payload,
                session_id=r["session_id"] or "",
                origin=r["session_id"] or "",
                provider=r["provider"] or "anthropic",
            )
        )

    dataset = enricher.build(classifier.tag(raw_entries), "")
    records = dataset.records

    cache_read_messages = sum(
        1
        for rec in records
        if rec.kind == "assistant" and rec.tokens.get("cache_read", 0)
    )

    errors_total = 0
    by_category: Counter[str] = Counter()
    for rec in records:
        if rec.is_error:
            errors_total += 1
            by_category[rec.error_category or "Other"] += 1

    ca = aggregator._command_analysis(records, dataset.interactions)

    return {
        "records": len(records),
        "cache_read_messages": cache_read_messages,
        "errors_total": errors_total,
        "errors_by_category_json": json.dumps(dict(by_category)),
        "commands_followed_by_interruption": int(
            ca.get("commands_followed_by_interruption", 0) or 0
        ),
        "command_tools": int(ca.get("total_tools_used", 0) or 0),
        "command_steps": int(ca.get("total_assistant_steps", 0) or 0),
    }
