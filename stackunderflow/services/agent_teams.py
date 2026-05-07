"""Agent-teams service — surfaces Claude Code parallel-agent topology.

Builds, on demand, a tree of (lead session → spawned sub-agents) for the
"Agents" dashboard tab. The signal we use is already in the ``messages``
table:

* ``messages.is_sidechain``  — true for any message belonging to a
  spawned sub-agent rather than the main session.
* ``messages.uuid`` / ``parent_uuid`` — chain back to the spawning
  message in the parent transcript.
* ``messages.raw_json`` — carries the optional ``teamName`` and
  ``agentId`` fields written by Claude Code 2.1.x+.

We deliberately do **not** add a schema migration. The maintainer's
dashboard reads small windows of agent-rich sessions on demand, and the
on-disk agent-team artefacts under ``~/.claude/teams/`` and
``~/.claude/tasks/`` add no information that isn't already in the JSONL
(and thus already in the ``messages`` table). See
``docs/specs/agent-teams.md`` for the full design rationale.

Public API
----------

* :func:`list_team_sessions` — top-level list view (one row per
  team-leader session).
* :func:`build_team_graph` — full tree for one lead session
  (lead + every spawned agent, with cost + previews).
* :func:`get_agent_transcript` — drill into one agent's full message
  list (thin wrapper over :func:`store.queries.get_session_messages`).

Empty-store behaviour: if the store has no sidechain messages,
``list_team_sessions`` returns ``[]`` and the route surfaces
``{"teams": []}`` cleanly.
"""

from __future__ import annotations

import json
import sqlite3
from dataclasses import asdict, dataclass
from typing import Any

from stackunderflow.infra.costs import compute_cost

__all__ = [
    "TeamSummary",
    "AgentSummary",
    "TeamGraph",
    "list_team_sessions",
    "build_team_graph",
    "get_agent_transcript",
]


# ── dataclasses ──────────────────────────────────────────────────────────────


@dataclass(frozen=True, slots=True)
class TeamSummary:
    """One row of the ``GET /api/agent-teams`` list view."""

    session_id: str
    project_slug: str
    project_display_name: str
    team_name: str | None
    first_ts: str | None
    last_ts: str | None
    agent_count: int
    sub_agent_message_count: int
    lead_message_count: int


@dataclass(frozen=True, slots=True)
class AgentSummary:
    """One agent (lead or spawned) within a :class:`TeamGraph`."""

    session_id: str
    agent_id: str | None
    agent_name: str | None
    is_lead: bool
    parent_session_id: str | None
    message_count: int
    first_ts: str | None
    last_ts: str | None
    first_user_prompt: str | None
    model: str | None
    cost_usd: float


@dataclass(frozen=True, slots=True)
class TeamGraph:
    """Returned by :func:`build_team_graph`. Lead first, agents in order."""

    session_id: str
    team_name: str | None
    project_slug: str
    project_display_name: str
    lead: AgentSummary
    agents: tuple[AgentSummary, ...]


# ── helpers (private) ────────────────────────────────────────────────────────


def _safe_json_loads(blob: str | None) -> dict[str, Any]:
    """Parse ``raw_json`` defensively — never raise on malformed rows."""
    if not blob:
        return {}
    try:
        loaded = json.loads(blob)
    except (json.JSONDecodeError, TypeError):
        return {}
    return loaded if isinstance(loaded, dict) else {}


def _extract_team_name(raw_json: str | None) -> str | None:
    return _safe_json_loads(raw_json).get("teamName")


def _extract_agent_id(raw_json: str | None, *, fallback_session_id: str | None = None) -> str | None:
    """Pull ``agentId`` out of the raw JSON, falling back to the
    ``agent-<id>`` filename convention some older transcripts use.
    """
    candidate = _safe_json_loads(raw_json).get("agentId")
    if candidate:
        return str(candidate)
    if fallback_session_id and fallback_session_id.startswith("agent-"):
        # Older path: ``agent-XXXX.jsonl`` — the session id IS the agent id.
        return fallback_session_id.removeprefix("agent-")
    return None


def _session_first_message_raw(conn: sqlite3.Connection, *, session_fk: int) -> str | None:
    """Return the first ``raw_json`` blob in the session, in seq order.

    Used to derive ``team_name`` / ``agent_id`` for sessions where the
    lead message carries those metadata fields.
    """
    row = conn.execute(
        "SELECT raw_json FROM messages WHERE session_fk = ? ORDER BY seq LIMIT 1",
        (session_fk,),
    ).fetchone()
    return row["raw_json"] if row else None


def _session_first_user_prompt(conn: sqlite3.Connection, *, session_fk: int) -> str | None:
    row = conn.execute(
        "SELECT content_text FROM messages "
        "WHERE session_fk = ? AND role = 'user' "
        "  AND content_text IS NOT NULL AND content_text != '' "
        "ORDER BY seq LIMIT 1",
        (session_fk,),
    ).fetchone()
    if not row:
        return None
    text = row["content_text"]
    return text[:300] if isinstance(text, str) else None


def _session_token_totals(
    conn: sqlite3.Connection, *, session_fk: int
) -> list[dict[str, Any]]:
    """Return per-(model, speed) token totals for one session.

    Mirrors the shape that ``compute_cost`` consumes; one entry per
    distinct model so a session that switched mid-run still prices
    correctly.
    """
    rows = conn.execute(
        "SELECT COALESCE(model, '') AS model, "
        "       COALESCE(speed, 'standard') AS speed, "
        "       SUM(input_tokens) AS input, "
        "       SUM(output_tokens) AS output, "
        "       SUM(cache_create_tokens) AS cache_create, "
        "       SUM(cache_read_tokens) AS cache_read "
        "FROM messages "
        "WHERE session_fk = ? AND model IS NOT NULL AND model != '' "
        "  AND model != '<synthetic>' "
        "GROUP BY model, speed",
        (session_fk,),
    ).fetchall()
    return [dict(r) for r in rows]


def _session_dominant_model(conn: sqlite3.Connection, *, session_fk: int) -> str | None:
    """Return the model that touched the most assistant messages."""
    row = conn.execute(
        "SELECT model, COUNT(*) AS c FROM messages "
        "WHERE session_fk = ? AND role = 'assistant' "
        "  AND model IS NOT NULL AND model != '' AND model != '<synthetic>' "
        "GROUP BY model ORDER BY c DESC LIMIT 1",
        (session_fk,),
    ).fetchone()
    return row["model"] if row else None


def _session_cost_usd(conn: sqlite3.Connection, *, session_fk: int) -> float:
    total = 0.0
    for r in _session_token_totals(conn, session_fk=session_fk):
        if not r["model"]:
            continue
        cost = compute_cost(
            {
                "input": int(r["input"] or 0),
                "output": int(r["output"] or 0),
                "cache_creation": int(r["cache_create"] or 0),
                "cache_read": int(r["cache_read"] or 0),
            },
            r["model"],
            speed=r["speed"] or "standard",
        )
        total += float(cost.get("total_cost", 0.0) or 0.0)
    return round(total, 4)


# ── public API ───────────────────────────────────────────────────────────────


def list_team_sessions(
    conn: sqlite3.Connection, *, limit: int = 50
) -> list[TeamSummary]:
    """List recent sessions that spawned at least one sub-agent.

    Strategy: for every project that has *any* sidechain message, group
    the sidechain rows by ``(project_id, session_id)`` and treat each
    distinct ``session_id`` as one team-lead candidate. We then
    aggregate counts + timestamps + a team_name peek (from the first
    row of the lead session that carries ``teamName``).

    Returns at most ``limit`` rows ordered by most recent activity.
    Empty input → empty list.
    """
    # Fast bail-out: if the store has zero sidechain rows, return [] without
    # the more expensive aggregate query. This is the common case on a
    # fresh install / non-team project.
    has_sidechain = conn.execute(
        "SELECT 1 FROM messages WHERE is_sidechain = 1 LIMIT 1"
    ).fetchone()
    if not has_sidechain:
        return []

    # Lead-candidate definition: a session whose own message stream is
    # primarily non-sidechain AND lives in a project where some OTHER
    # session contributes sidechain messages. The lead's own messages
    # are not sidechain (it's the parent transcript); the sub-agents
    # land in distinct ``sessions`` rows that are predominantly
    # sidechain.
    #
    # We rank lead candidates by ``last_ts DESC`` so the dashboard
    # surfaces the most recently active team first.
    rows = conn.execute(
        """
        SELECT
          s.id AS session_fk,
          s.session_id,
          s.first_ts,
          s.last_ts,
          s.project_id,
          p.slug AS project_slug,
          p.display_name AS project_display_name,
          COALESCE(SUM(CASE WHEN m.is_sidechain = 0 THEN 1 ELSE 0 END), 0) AS lead_msgs,
          COALESCE(SUM(CASE WHEN m.is_sidechain = 1 THEN 1 ELSE 0 END), 0) AS own_sub_msgs
        FROM sessions s
        JOIN projects p ON p.id = s.project_id
        JOIN messages m ON m.session_fk = s.id
        WHERE s.project_id IN (
          SELECT DISTINCT s2.project_id
          FROM sessions s2
          JOIN messages m2 ON m2.session_fk = s2.id
          WHERE m2.is_sidechain = 1
        )
        GROUP BY s.id, s.session_id, s.first_ts, s.last_ts,
                 s.project_id, p.slug, p.display_name
        HAVING lead_msgs > 0
        ORDER BY s.last_ts DESC
        """
    ).fetchall()

    # Pre-compute per-(project_id) sub-agent sessions so we don't
    # re-query inside the loop. ``sub_sessions_by_project[pid]`` is the
    # list of (session_fk, message_count_sidechain) for every session in
    # the project that contains sidechain rows.
    sub_session_rows = conn.execute(
        """
        SELECT s.project_id,
               s.id AS session_fk,
               COUNT(*) AS sub_msgs
        FROM sessions s
        JOIN messages m ON m.session_fk = s.id
        WHERE m.is_sidechain = 1
        GROUP BY s.project_id, s.id
        """
    ).fetchall()
    sub_by_project: dict[int, list[tuple[int, int]]] = {}
    for sr in sub_session_rows:
        sub_by_project.setdefault(int(sr["project_id"]), []).append(
            (int(sr["session_fk"]), int(sr["sub_msgs"] or 0))
        )

    out: list[TeamSummary] = []
    seen_session_ids: set[str] = set()

    for r in rows:
        if len(out) >= limit:
            break
        # A lead session must have OTHER sessions in its project with
        # sidechain rows. Sessions whose only sidechain content is their
        # own (e.g. a sub-agent transcript that contains both sidechain
        # and non-sidechain rows) are skipped here so they don't appear
        # as a "team-lead".
        pid = int(r["project_id"])
        own_fk = int(r["session_fk"])
        other_subs = [
            (sfk, sub_count)
            for sfk, sub_count in sub_by_project.get(pid, [])
            if sfk != own_fk
        ]
        if not other_subs:
            continue
        if r["session_id"] in seen_session_ids:
            continue
        seen_session_ids.add(r["session_id"])

        team_name = _extract_team_name(
            _session_first_message_raw(conn, session_fk=own_fk)
        )

        # agent_count: distinct ``agentId`` values in the project's
        # other-session sidechain rows. The fallback (when no agentId
        # is recorded) is the number of distinct sub-agent sessions —
        # a strict lower bound that's correct for older transcripts.
        agent_ids: set[str] = set()
        for sfk, _ in other_subs:
            for ar in conn.execute(
                "SELECT raw_json FROM messages "
                "WHERE session_fk = ? AND is_sidechain = 1",
                (sfk,),
            ).fetchall():
                aid = _extract_agent_id(ar["raw_json"])
                if aid:
                    agent_ids.add(aid)
        agent_count = len(agent_ids) if agent_ids else len(other_subs)
        sub_msg_total = sum(c for _, c in other_subs)

        out.append(
            TeamSummary(
                session_id=r["session_id"],
                project_slug=r["project_slug"],
                project_display_name=r["project_display_name"],
                team_name=team_name,
                first_ts=r["first_ts"],
                last_ts=r["last_ts"],
                agent_count=agent_count,
                sub_agent_message_count=sub_msg_total,
                lead_message_count=int(r["lead_msgs"]),
            )
        )
    return out


def build_team_graph(
    conn: sqlite3.Connection, *, lead_session_id: str
) -> TeamGraph | None:
    """Return the full lead → agents tree for one team.

    ``None`` if the lead session can't be found.

    Algorithm:
    1. Locate the lead session row + its project.
    2. Lead's ``team_name`` (if any) comes from the first message's
       ``raw_json``. Used as an additional discriminator when grouping
       sub-agent sessions.
    3. Sub-agents are sessions in the same project whose sidechain
       messages share the lead's ``team_name`` OR whose first message's
       ``parent_uuid`` resolves to a uuid in the lead session (older
       fallback path).
    """
    lead_row = conn.execute(
        "SELECT s.id, s.session_id, s.first_ts, s.last_ts, "
        "       p.id AS project_id, p.slug, p.display_name "
        "FROM sessions s JOIN projects p ON p.id = s.project_id "
        "WHERE s.session_id = ?",
        (lead_session_id,),
    ).fetchone()
    if not lead_row:
        return None

    lead_fk = int(lead_row["id"])
    project_id = int(lead_row["project_id"])
    lead_team_name = _extract_team_name(
        _session_first_message_raw(conn, session_fk=lead_fk)
    )

    lead_summary = AgentSummary(
        session_id=lead_row["session_id"],
        agent_id=None,
        agent_name="team-lead",
        is_lead=True,
        parent_session_id=None,
        message_count=conn.execute(
            "SELECT COUNT(*) AS c FROM messages WHERE session_fk = ?",
            (lead_fk,),
        ).fetchone()["c"],
        first_ts=lead_row["first_ts"],
        last_ts=lead_row["last_ts"],
        first_user_prompt=_session_first_user_prompt(conn, session_fk=lead_fk),
        model=_session_dominant_model(conn, session_fk=lead_fk),
        cost_usd=_session_cost_usd(conn, session_fk=lead_fk),
    )

    # Candidate sub-agent sessions: every other session in the same
    # project that has sidechain rows. We then filter to those whose
    # team_name matches the lead's team_name (when present) — this keeps
    # the graph tight when one project hosts multiple teams over time.
    candidate_rows = conn.execute(
        "SELECT DISTINCT s.id, s.session_id, s.first_ts, s.last_ts "
        "FROM sessions s "
        "JOIN messages m ON m.session_fk = s.id "
        "WHERE s.project_id = ? AND s.id != ? AND m.is_sidechain = 1 "
        "ORDER BY s.first_ts ASC",
        (project_id, lead_fk),
    ).fetchall()

    agents: list[AgentSummary] = []
    for row in candidate_rows:
        sub_fk = int(row["id"])
        first_raw = _session_first_message_raw(conn, session_fk=sub_fk)
        sub_team_name = _extract_team_name(first_raw)
        # team_name discriminator: only filter out when both sides
        # have a name AND they disagree. When the sub-agent doesn't
        # carry teamName (older transcript), include it — the
        # project-scoped sidechain heuristic is good enough.
        if (
            lead_team_name
            and sub_team_name
            and lead_team_name != sub_team_name
        ):
            continue
        agent_id = _extract_agent_id(first_raw, fallback_session_id=row["session_id"])
        agents.append(
            AgentSummary(
                session_id=row["session_id"],
                agent_id=agent_id,
                agent_name=agent_id or row["session_id"],
                is_lead=False,
                parent_session_id=lead_row["session_id"],
                message_count=conn.execute(
                    "SELECT COUNT(*) AS c FROM messages WHERE session_fk = ?",
                    (sub_fk,),
                ).fetchone()["c"],
                first_ts=row["first_ts"],
                last_ts=row["last_ts"],
                first_user_prompt=_session_first_user_prompt(conn, session_fk=sub_fk),
                model=_session_dominant_model(conn, session_fk=sub_fk),
                cost_usd=_session_cost_usd(conn, session_fk=sub_fk),
            )
        )

    return TeamGraph(
        session_id=lead_row["session_id"],
        team_name=lead_team_name,
        project_slug=lead_row["slug"],
        project_display_name=lead_row["display_name"],
        lead=lead_summary,
        agents=tuple(agents),
    )


def get_agent_transcript(
    conn: sqlite3.Connection,
    *,
    lead_session_id: str,
    agent_session_id: str,
) -> list[dict[str, Any]] | None:
    """Return raw message rows for one agent in a team.

    The ``lead_session_id`` parameter is currently used to validate the
    agent belongs to the same project as the lead (cheap sanity check
    so the URL ``/api/agent-teams/{lead}/agent/{sub}`` can't be used to
    surface arbitrary cross-project sessions). Returns ``None`` when
    either session can't be found in the same project.
    """
    pair = conn.execute(
        "SELECT s1.id AS lead_fk, s2.id AS agent_fk, s1.project_id "
        "FROM sessions s1 JOIN sessions s2 "
        "  ON s2.project_id = s1.project_id "
        "WHERE s1.session_id = ? AND s2.session_id = ?",
        (lead_session_id, agent_session_id),
    ).fetchone()
    if not pair:
        return None

    rows = conn.execute(
        "SELECT id, seq, timestamp, role, model, "
        "       input_tokens, output_tokens, "
        "       cache_create_tokens, cache_read_tokens, "
        "       content_text, tools_json, raw_json, "
        "       is_sidechain, uuid, parent_uuid, speed "
        "FROM messages WHERE session_fk = ? ORDER BY seq",
        (int(pair["agent_fk"]),),
    ).fetchall()
    return [
        {**dict(r), "is_sidechain": bool(r["is_sidechain"])}
        for r in rows
    ]


# ── serialisation helpers (used by the route module) ────────────────────────


def team_summary_to_dict(t: TeamSummary) -> dict[str, Any]:
    return asdict(t)


def agent_summary_to_dict(a: AgentSummary) -> dict[str, Any]:
    return asdict(a)


def team_graph_to_dict(g: TeamGraph) -> dict[str, Any]:
    return {
        "session_id": g.session_id,
        "team_name": g.team_name,
        "project_slug": g.project_slug,
        "project_display_name": g.project_display_name,
        "lead": agent_summary_to_dict(g.lead),
        "agents": [agent_summary_to_dict(a) for a in g.agents],
    }
