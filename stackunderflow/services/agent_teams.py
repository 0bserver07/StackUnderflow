"""Agent-teams service — surfaces Claude Code parallel-agent topology.

Builds the tree of (lead session → spawned sub-agents) for the "Agents"
dashboard tab.

Since migration ``v013_multi_agent_session_metadata`` the team graph is
**materialised at ingest time** (see
``stackunderflow/adapters/claude_teams.py``): every team gets an
``agent_teams`` row, and each member session carries its ``team_id`` /
``spawned_by_session_id`` / ``spawn_prompt`` / ``agent_role``. When that
metadata is present this module just JOINs — no ``raw_json`` parsing on
the hot path.

When it is *not* present (a store ingested before v013, or one whose
``~/.claude/teams/`` artefacts were never on disk), the service falls
back to the original heuristic: scan ``messages.is_sidechain`` + parse
``raw_json`` for ``teamName`` / ``agentId`` and chain ``parent_uuid``.
The two paths are observationally equivalent for the dashboard; the
indexed one is just faster, and additionally surfaces the richer
``spawn_prompt`` (the verbatim prompt the sub-agent was launched with,
not just the first user message of its transcript).

Public API
----------

* :func:`list_team_sessions` — top-level list view (one row per
  team-leader session).
* :func:`build_team_graph` — full tree for one lead session
  (lead + every spawned agent, with cost + previews).
* :func:`get_agent_transcript` — drill into one agent's full message
  list.

Empty-store behaviour: a store with neither materialised teams nor
sidechain messages → ``list_team_sessions`` returns ``[]`` and the route
surfaces ``{"teams": []}`` cleanly.
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

_ROLE_LEAD = "lead"
_ROLE_SUBAGENT = "subagent"


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
    description: str | None = None  # team description (materialised teams only)


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
    # v013: the verbatim prompt the agent was spawned with (from
    # ~/.claude/teams config or ~/.claude/tasks), and its team role. Both
    # NULL when team metadata hasn't been materialised for this session.
    spawn_prompt: str | None = None
    agent_role: str | None = None


@dataclass(frozen=True, slots=True)
class TeamGraph:
    """Returned by :func:`build_team_graph`. Lead first, agents in order."""

    session_id: str
    team_name: str | None
    project_slug: str
    project_display_name: str
    lead: AgentSummary
    agents: tuple[AgentSummary, ...]
    description: str | None = None  # team description (materialised teams only)


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
        return str(candidate).split("@", 1)[0]
    if fallback_session_id and fallback_session_id.startswith("agent-"):
        # Older path: ``agent-XXXX.jsonl`` — the session id IS the agent id.
        return fallback_session_id.removeprefix("agent-")
    return None


def _session_first_message_raw(conn: sqlite3.Connection, *, session_fk: int) -> str | None:
    """Return the first ``raw_json`` blob in the session, in seq order."""
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


def _session_message_count(conn: sqlite3.Connection, *, session_fk: int) -> int:
    return int(
        conn.execute(
            "SELECT COUNT(*) AS c FROM messages WHERE session_fk = ?",
            (session_fk,),
        ).fetchone()["c"]
    )


def _indexed_teams_available(conn: sqlite3.Connection) -> bool:
    """True when migration v013 ran *and* at least one session is materialised.

    We check ``sessions.team_id`` (the column the indexed queries JOIN
    on) rather than just the existence of ``agent_teams`` rows: an
    ``agent_teams`` row whose lead transcript hasn't been ingested yet is
    an orphan with nothing to JOIN, so for that store we keep the
    heuristic path (which is identical there). Uses the partial
    ``idx_sessions_team`` index, so it's a cheap probe even on a large
    store. A pre-v013 schema (no such column) → ``False``.
    """
    try:
        return conn.execute(
            "SELECT 1 FROM sessions WHERE team_id IS NOT NULL LIMIT 1"
        ).fetchone() is not None
    except sqlite3.OperationalError:
        return False


def _agent_summary_for_session(
    conn: sqlite3.Connection,
    *,
    session_fk: int,
    session_id: str,
    first_ts: str | None,
    last_ts: str | None,
    is_lead: bool,
    parent_session_id: str | None,
    agent_id: str | None,
    agent_name: str | None,
    spawn_prompt: str | None,
    agent_role: str | None,
) -> AgentSummary:
    return AgentSummary(
        session_id=session_id,
        agent_id=agent_id,
        agent_name=agent_name,
        is_lead=is_lead,
        parent_session_id=parent_session_id,
        message_count=_session_message_count(conn, session_fk=session_fk),
        first_ts=first_ts,
        last_ts=last_ts,
        first_user_prompt=_session_first_user_prompt(conn, session_fk=session_fk),
        model=_session_dominant_model(conn, session_fk=session_fk),
        cost_usd=_session_cost_usd(conn, session_fk=session_fk),
        spawn_prompt=spawn_prompt,
        agent_role=agent_role,
    )


# ── public API: list_team_sessions ──────────────────────────────────────────


def list_team_sessions(
    conn: sqlite3.Connection, *, limit: int = 50, project_slug: str | None = None
) -> list[TeamSummary]:
    """List recent teams (one row per lead session that spawned sub-agents).

    Uses the materialised ``agent_teams`` table when populated (a single
    indexed JOIN), otherwise falls back to the ``messages.is_sidechain``
    heuristic. Returns at most ``limit`` rows ordered by most recent
    activity. When ``project_slug`` is set, only teams whose lead
    session lives in that project are returned (per-project Agents-tab
    scoping). Empty store → empty list.
    """
    if _indexed_teams_available(conn):
        return _list_team_sessions_indexed(
            conn, limit=limit, project_slug=project_slug,
        )
    return _list_team_sessions_scan(
        conn, limit=limit, project_slug=project_slug,
    )


def _list_team_sessions_indexed(
    conn: sqlite3.Connection, *, limit: int, project_slug: str | None = None
) -> list[TeamSummary]:
    where = ""
    params: list = []
    if project_slug:
        where = "WHERE p.slug = ?"
        params.append(project_slug)
    params.append(limit)
    rows = conn.execute(
        f"""
        SELECT
          t.team_id,
          t.description,
          t.lead_session_id,
          p.slug          AS project_slug,
          p.display_name  AS project_display_name,
          MIN(s.first_ts) AS first_ts,
          MAX(s.last_ts)  AS last_ts,
          SUM(CASE WHEN COALESCE(s.agent_role, '') = 'subagent' THEN 1 ELSE 0 END) AS agent_count,
          SUM(CASE WHEN COALESCE(s.agent_role, '') = 'subagent' THEN s.message_count ELSE 0 END) AS sub_msgs,
          SUM(CASE WHEN s.session_id = t.lead_session_id THEN s.message_count ELSE 0 END) AS lead_msgs
        FROM agent_teams t
        JOIN projects p ON p.id = t.project_id
        JOIN sessions s ON s.team_id = t.team_id
        {where}
        GROUP BY t.team_id, t.description, t.lead_session_id, p.slug, p.display_name
        ORDER BY MAX(s.last_ts) DESC, t.team_id ASC
        LIMIT ?
        """,
        tuple(params),
    ).fetchall()
    return [
        TeamSummary(
            session_id=r["lead_session_id"] or r["team_id"],
            project_slug=r["project_slug"],
            project_display_name=r["project_display_name"],
            team_name=r["team_id"],
            first_ts=r["first_ts"],
            last_ts=r["last_ts"],
            agent_count=int(r["agent_count"] or 0),
            sub_agent_message_count=int(r["sub_msgs"] or 0),
            lead_message_count=int(r["lead_msgs"] or 0),
            description=r["description"],
        )
        for r in rows
    ]


def _list_team_sessions_scan(
    conn: sqlite3.Connection, *, limit: int, project_slug: str | None = None
) -> list[TeamSummary]:
    """Heuristic list view (pre-v013, or stores without materialised teams).

    Strategy: for every project that has *any* sidechain message, group
    the sidechain rows by ``(project_id, session_id)`` and treat each
    distinct ``session_id`` as one team-lead candidate. We then aggregate
    counts + timestamps + a ``teamName`` peek from the lead session's
    first row. ``project_slug`` filter narrows to a single project's
    teams (per-project Agents-tab scoping).
    """
    has_sidechain = conn.execute(
        "SELECT 1 FROM messages WHERE is_sidechain = 1 LIMIT 1"
    ).fetchone()
    if not has_sidechain:
        return []

    extra_where = ""
    extra_params: list = []
    if project_slug:
        extra_where = "AND p.slug = ?"
        extra_params.append(project_slug)

    rows = conn.execute(
        f"""
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
        {extra_where}
        GROUP BY s.id, s.session_id, s.first_ts, s.last_ts,
                 s.project_id, p.slug, p.display_name
        HAVING lead_msgs > 0
        ORDER BY s.last_ts DESC
        """,
        tuple(extra_params),
    ).fetchall()

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


# ── public API: build_team_graph ────────────────────────────────────────────


def build_team_graph(
    conn: sqlite3.Connection, *, lead_session_id: str
) -> TeamGraph | None:
    """Return the full lead → agents tree for one team.

    ``None`` when the session can't be found / isn't part of a team.

    Uses the materialised ``sessions.team_id`` JOIN when available
    (deterministic membership, includes ``spawn_prompt``), and falls back
    to the ``parent_uuid`` / ``teamName`` heuristic otherwise. Passing a
    *sub-agent's* session id resolves up to its team's lead.
    """
    if _indexed_teams_available(conn):
        graph = _build_team_graph_indexed(conn, lead_session_id=lead_session_id)
        if graph is not None:
            return graph
        # Fall through: the session may belong to a team that hasn't been
        # materialised yet (ingested before v013). Try the heuristic.
    return _build_team_graph_scan(conn, lead_session_id=lead_session_id)


def _build_team_graph_indexed(
    conn: sqlite3.Connection, *, lead_session_id: str
) -> TeamGraph | None:
    # Resolve the team: either the given id IS a known lead, or it's a
    # member session carrying a ``team_id``.
    team_row = conn.execute(
        "SELECT t.team_id, t.description, t.lead_session_id, t.project_id, "
        "       p.slug, p.display_name "
        "FROM agent_teams t JOIN projects p ON p.id = t.project_id "
        "WHERE t.lead_session_id = ?",
        (lead_session_id,),
    ).fetchone()
    if team_row is None:
        member = conn.execute(
            "SELECT team_id FROM sessions WHERE session_id = ? AND team_id IS NOT NULL LIMIT 1",
            (lead_session_id,),
        ).fetchone()
        if member is None:
            return None
        team_row = conn.execute(
            "SELECT t.team_id, t.description, t.lead_session_id, t.project_id, "
            "       p.slug, p.display_name "
            "FROM agent_teams t JOIN projects p ON p.id = t.project_id "
            "WHERE t.team_id = ?",
            (member["team_id"],),
        ).fetchone()
        if team_row is None:
            return None

    team_id = team_row["team_id"]
    lead_session = team_row["lead_session_id"]

    member_rows = conn.execute(
        "SELECT s.id, s.session_id, s.first_ts, s.last_ts, "
        "       s.spawn_prompt, s.agent_role, s.spawned_by_session_id "
        "FROM sessions s WHERE s.team_id = ? "
        "ORDER BY (CASE WHEN s.agent_role = 'lead' THEN 0 ELSE 1 END), s.first_ts ASC, s.session_id ASC",
        (team_id,),
    ).fetchall()
    if not member_rows:
        return None

    lead_summary: AgentSummary | None = None
    agents: list[AgentSummary] = []
    for row in member_rows:
        sfk = int(row["id"])
        sid = row["session_id"]
        is_lead = (row["agent_role"] == _ROLE_LEAD) or (sid == lead_session)
        first_raw = _session_first_message_raw(conn, session_fk=sfk)
        agent_id = None if is_lead else _extract_agent_id(first_raw, fallback_session_id=sid)
        agent_name = "team-lead" if is_lead else (agent_id or sid)
        parent_sid = None if is_lead else (row["spawned_by_session_id"] or lead_session)
        summary = _agent_summary_for_session(
            conn,
            session_fk=sfk,
            session_id=sid,
            first_ts=row["first_ts"],
            last_ts=row["last_ts"],
            is_lead=is_lead,
            parent_session_id=parent_sid,
            agent_id=agent_id,
            agent_name=agent_name,
            spawn_prompt=row["spawn_prompt"],
            agent_role=row["agent_role"] or (_ROLE_LEAD if is_lead else _ROLE_SUBAGENT),
        )
        if is_lead and lead_summary is None:
            lead_summary = summary
        else:
            agents.append(summary)

    if lead_summary is None:
        # Lead transcript not ingested yet — synthesise a placeholder so the
        # graph still renders the sub-agents.
        lead_summary = AgentSummary(
            session_id=lead_session or team_id,
            agent_id=None,
            agent_name="team-lead",
            is_lead=True,
            parent_session_id=None,
            message_count=0,
            first_ts=None,
            last_ts=None,
            first_user_prompt=None,
            model=None,
            cost_usd=0.0,
            spawn_prompt=None,
            agent_role=_ROLE_LEAD,
        )

    return TeamGraph(
        session_id=lead_summary.session_id,
        team_name=team_id,
        project_slug=team_row["slug"],
        project_display_name=team_row["display_name"],
        lead=lead_summary,
        agents=tuple(agents),
        description=team_row["description"],
    )


def _build_team_graph_scan(
    conn: sqlite3.Connection, *, lead_session_id: str
) -> TeamGraph | None:
    """Heuristic graph builder (pre-v013, or un-materialised sessions).

    1. Locate the lead session row + its project.
    2. The lead's ``teamName`` (if any) discriminates sub-agents when a
       project hosted several teams over time.
    3. Sub-agents are sessions in the same project with sidechain rows
       whose ``teamName`` agrees with (or is absent on) the lead's.
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

    lead_summary = _agent_summary_for_session(
        conn,
        session_fk=lead_fk,
        session_id=lead_row["session_id"],
        first_ts=lead_row["first_ts"],
        last_ts=lead_row["last_ts"],
        is_lead=True,
        parent_session_id=None,
        agent_id=None,
        agent_name="team-lead",
        spawn_prompt=None,
        agent_role=_ROLE_LEAD,
    )

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
        if lead_team_name and sub_team_name and lead_team_name != sub_team_name:
            continue
        agent_id = _extract_agent_id(first_raw, fallback_session_id=row["session_id"])
        agents.append(
            _agent_summary_for_session(
                conn,
                session_fk=sub_fk,
                session_id=row["session_id"],
                first_ts=row["first_ts"],
                last_ts=row["last_ts"],
                is_lead=False,
                parent_session_id=lead_row["session_id"],
                agent_id=agent_id,
                agent_name=agent_id or row["session_id"],
                spawn_prompt=None,
                agent_role=_ROLE_SUBAGENT,
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


# ── public API: get_agent_transcript ────────────────────────────────────────


def get_agent_transcript(
    conn: sqlite3.Connection,
    *,
    lead_session_id: str,
    agent_session_id: str,
) -> list[dict[str, Any]] | None:
    """Return raw message rows for one agent in a team.

    ``lead_session_id`` is used as a cheap same-project fence so the URL
    ``/api/agent-teams/{lead}/agent/{sub}`` can't surface arbitrary
    cross-project sessions. ``None`` when either session can't be found
    in the same project.
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
        "description": g.description,
        "project_slug": g.project_slug,
        "project_display_name": g.project_display_name,
        "lead": agent_summary_to_dict(g.lead),
        "agents": [agent_summary_to_dict(a) for a in g.agents],
    }
