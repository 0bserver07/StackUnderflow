"""Tests for ``stackunderflow.services.agent_teams``.

Build the agent-team graph from a synthetic in-memory store with seeded
sidechain messages. Locks the contract that:

* a store with no sidechain rows yields an empty list,
* a single-agent fan-out is detected and rendered,
* multi-agent fan-out preserves agent order + counts,
* deep recursion (a sub-agent that itself spawned children) is
  reachable via ``team_name`` matching.
"""

from __future__ import annotations

import json

import pytest

from stackunderflow.services import agent_teams as agent_teams_service
from stackunderflow.store import db, schema

# ── seed helpers ─────────────────────────────────────────────────────────────


def _seed_project(conn, *, slug: str = "test-project") -> int:
    cur = conn.execute(
        "INSERT INTO projects (provider, slug, display_name, first_seen, last_modified) "
        "VALUES (?, ?, ?, ?, ?)",
        ("claude", slug, slug, 0.0, 1_000_000.0),
    )
    return int(cur.lastrowid)


def _seed_session(
    conn, *, project_id: int, session_id: str, first_ts: str, last_ts: str
) -> int:
    cur = conn.execute(
        "INSERT INTO sessions "
        "(project_id, session_id, first_ts, last_ts, message_count) "
        "VALUES (?, ?, ?, ?, ?)",
        (project_id, session_id, first_ts, last_ts, 0),
    )
    return int(cur.lastrowid)


def _seed_message(
    conn,
    *,
    session_fk: int,
    seq: int,
    role: str = "assistant",
    is_sidechain: bool = False,
    team_name: str | None = None,
    agent_id: str | None = None,
    content: str = "hello world",
    model: str | None = "claude-sonnet-4-5",
    input_tokens: int = 1000,
    output_tokens: int = 500,
    uuid: str | None = None,
    parent_uuid: str | None = None,
    timestamp: str = "2026-04-01T00:00:00Z",
) -> int:
    raw: dict[str, object] = {"sessionId": str(session_fk), "type": role}
    if team_name is not None:
        raw["teamName"] = team_name
    if agent_id is not None:
        raw["agentId"] = agent_id
    if uuid is not None:
        raw["uuid"] = uuid
    if parent_uuid is not None:
        raw["parentUuid"] = parent_uuid
    cur = conn.execute(
        "INSERT INTO messages "
        "(session_fk, seq, timestamp, role, model, "
        " input_tokens, output_tokens, cache_create_tokens, cache_read_tokens, "
        " content_text, tools_json, raw_json, is_sidechain, uuid, parent_uuid) "
        "VALUES (?, ?, ?, ?, ?, ?, ?, 0, 0, ?, '[]', ?, ?, ?, ?)",
        (
            session_fk,
            seq,
            timestamp,
            role,
            model,
            input_tokens,
            output_tokens,
            content,
            json.dumps(raw),
            1 if is_sidechain else 0,
            uuid,
            parent_uuid,
        ),
    )
    return int(cur.lastrowid)


@pytest.fixture()
def conn(tmp_path):
    """Fresh schema-applied store for one test."""
    store_path = tmp_path / "store.db"
    c = db.connect(store_path)
    schema.apply(c)
    yield c
    c.close()


# ── empty-store contract ────────────────────────────────────────────────────


def test_list_team_sessions_empty_store_returns_empty(conn):
    assert agent_teams_service.list_team_sessions(conn) == []


def test_list_team_sessions_no_sidechain_returns_empty(conn):
    """Project + sessions exist, but no sidechain rows → still empty."""
    pid = _seed_project(conn)
    sfk = _seed_session(
        conn,
        project_id=pid,
        session_id="lonely-session",
        first_ts="2026-04-01T00:00:00Z",
        last_ts="2026-04-01T01:00:00Z",
    )
    _seed_message(conn, session_fk=sfk, seq=0)
    _seed_message(conn, session_fk=sfk, seq=1)
    assert agent_teams_service.list_team_sessions(conn) == []


def test_build_team_graph_unknown_session_returns_none(conn):
    assert (
        agent_teams_service.build_team_graph(conn, lead_session_id="nope")
        is None
    )


# ── single-agent fan-out ─────────────────────────────────────────────────────


def test_single_agent_fanout_lists_one_team(conn):
    pid = _seed_project(conn)
    lead_fk = _seed_session(
        conn,
        project_id=pid,
        session_id="lead-001",
        first_ts="2026-04-01T00:00:00Z",
        last_ts="2026-04-01T02:00:00Z",
    )
    sub_fk = _seed_session(
        conn,
        project_id=pid,
        session_id="sub-001",
        first_ts="2026-04-01T00:30:00Z",
        last_ts="2026-04-01T01:30:00Z",
    )
    # Lead messages — non-sidechain
    _seed_message(
        conn,
        session_fk=lead_fk,
        seq=0,
        role="user",
        team_name="team-alpha",
        content="kick off the team",
    )
    _seed_message(conn, session_fk=lead_fk, seq=1, team_name="team-alpha")
    # Sub-agent messages — sidechain, with agentId + teamName
    _seed_message(
        conn,
        session_fk=sub_fk,
        seq=0,
        role="user",
        is_sidechain=True,
        team_name="team-alpha",
        agent_id="alpha-worker",
        content="please research X",
    )
    _seed_message(
        conn,
        session_fk=sub_fk,
        seq=1,
        is_sidechain=True,
        team_name="team-alpha",
        agent_id="alpha-worker",
    )

    teams = agent_teams_service.list_team_sessions(conn)
    assert len(teams) == 1
    t = teams[0]
    assert t.session_id == "lead-001"
    assert t.team_name == "team-alpha"
    assert t.agent_count == 1
    assert t.lead_message_count == 2
    # sidechain rows live on the sub session — the count rolls up from
    # every OTHER session in the project that contains sidechain rows.
    assert t.sub_agent_message_count == 2


def test_build_team_graph_single_agent_returns_lead_plus_one(conn):
    pid = _seed_project(conn)
    lead_fk = _seed_session(
        conn,
        project_id=pid,
        session_id="lead-001",
        first_ts="2026-04-01T00:00:00Z",
        last_ts="2026-04-01T02:00:00Z",
    )
    sub_fk = _seed_session(
        conn,
        project_id=pid,
        session_id="sub-001",
        first_ts="2026-04-01T00:30:00Z",
        last_ts="2026-04-01T01:30:00Z",
    )
    _seed_message(
        conn,
        session_fk=lead_fk,
        seq=0,
        role="user",
        team_name="team-alpha",
        content="kick off the team",
    )
    _seed_message(conn, session_fk=lead_fk, seq=1, team_name="team-alpha")
    _seed_message(
        conn,
        session_fk=sub_fk,
        seq=0,
        role="user",
        is_sidechain=True,
        team_name="team-alpha",
        agent_id="alpha-worker",
        content="please research X",
    )
    _seed_message(
        conn,
        session_fk=sub_fk,
        seq=1,
        is_sidechain=True,
        team_name="team-alpha",
        agent_id="alpha-worker",
    )

    g = agent_teams_service.build_team_graph(conn, lead_session_id="lead-001")
    assert g is not None
    assert g.team_name == "team-alpha"
    assert g.lead.is_lead is True
    assert g.lead.session_id == "lead-001"
    assert g.lead.message_count == 2
    assert g.lead.first_user_prompt == "kick off the team"
    assert len(g.agents) == 1
    sub = g.agents[0]
    assert sub.session_id == "sub-001"
    assert sub.agent_id == "alpha-worker"
    assert sub.parent_session_id == "lead-001"
    assert sub.is_lead is False
    assert sub.first_user_prompt == "please research X"


# ── multi-agent fan-out ─────────────────────────────────────────────────────


def test_build_team_graph_multi_agent_fanout(conn):
    pid = _seed_project(conn)
    lead_fk = _seed_session(
        conn,
        project_id=pid,
        session_id="lead-multi",
        first_ts="2026-04-01T00:00:00Z",
        last_ts="2026-04-01T03:00:00Z",
    )
    a1_fk = _seed_session(
        conn,
        project_id=pid,
        session_id="agent-a",
        first_ts="2026-04-01T00:10:00Z",
        last_ts="2026-04-01T01:10:00Z",
    )
    a2_fk = _seed_session(
        conn,
        project_id=pid,
        session_id="agent-b",
        first_ts="2026-04-01T00:20:00Z",
        last_ts="2026-04-01T02:00:00Z",
    )
    a3_fk = _seed_session(
        conn,
        project_id=pid,
        session_id="agent-c",
        first_ts="2026-04-01T00:30:00Z",
        last_ts="2026-04-01T02:30:00Z",
    )
    _seed_message(conn, session_fk=lead_fk, seq=0, team_name="multi", role="user", content="lead boot")
    _seed_message(conn, session_fk=lead_fk, seq=1, team_name="multi")
    for i, sfk in enumerate((a1_fk, a2_fk, a3_fk)):
        _seed_message(
            conn,
            session_fk=sfk,
            seq=0,
            role="user",
            is_sidechain=True,
            team_name="multi",
            agent_id=f"worker-{i}",
            content=f"prompt for worker {i}",
        )
        _seed_message(
            conn,
            session_fk=sfk,
            seq=1,
            is_sidechain=True,
            team_name="multi",
            agent_id=f"worker-{i}",
        )

    g = agent_teams_service.build_team_graph(conn, lead_session_id="lead-multi")
    assert g is not None
    # Sub-agents come back ordered by first_ts ascending — locks the
    # tree-rendering order the dashboard depends on.
    assert [a.session_id for a in g.agents] == ["agent-a", "agent-b", "agent-c"]
    assert all(a.parent_session_id == "lead-multi" for a in g.agents)
    assert {a.agent_id for a in g.agents} == {"worker-0", "worker-1", "worker-2"}

    summary = agent_teams_service.list_team_sessions(conn)
    assert len(summary) == 1
    assert summary[0].agent_count == 3


# ── team_name discriminator: keep two teams in one project distinct ─────────


def test_team_name_discriminator_keeps_unrelated_team_out(conn):
    pid = _seed_project(conn)
    lead_fk = _seed_session(
        conn,
        project_id=pid,
        session_id="lead-alpha",
        first_ts="2026-04-01T00:00:00Z",
        last_ts="2026-04-01T03:00:00Z",
    )
    sub_alpha_fk = _seed_session(
        conn,
        project_id=pid,
        session_id="sub-alpha",
        first_ts="2026-04-01T00:30:00Z",
        last_ts="2026-04-01T01:30:00Z",
    )
    # A second team in the same project — its sub-agent should NOT show
    # up under the lead-alpha graph.
    sub_beta_fk = _seed_session(
        conn,
        project_id=pid,
        session_id="sub-beta",
        first_ts="2026-04-02T00:30:00Z",
        last_ts="2026-04-02T01:30:00Z",
    )

    _seed_message(conn, session_fk=lead_fk, seq=0, team_name="alpha", role="user", content="alpha lead")
    _seed_message(conn, session_fk=lead_fk, seq=1, team_name="alpha")
    _seed_message(
        conn,
        session_fk=sub_alpha_fk,
        seq=0,
        is_sidechain=True,
        team_name="alpha",
        agent_id="alpha-1",
        role="user",
        content="alpha worker prompt",
    )
    _seed_message(
        conn,
        session_fk=sub_beta_fk,
        seq=0,
        is_sidechain=True,
        team_name="beta",
        agent_id="beta-1",
        role="user",
        content="beta worker prompt",
    )

    g = agent_teams_service.build_team_graph(conn, lead_session_id="lead-alpha")
    assert g is not None
    assert [a.session_id for a in g.agents] == ["sub-alpha"]


# ── older transcript path: agent-XXX session id, no teamName ─────────────────


def test_agent_id_falls_back_to_session_filename_convention(conn):
    """Older Task-spawned agents have no ``teamName`` field but their
    session_id is ``agent-<short>`` — the service should still surface
    the agent_id so the UI has a label to render.
    """
    pid = _seed_project(conn)
    lead_fk = _seed_session(
        conn,
        project_id=pid,
        session_id="lead-old",
        first_ts="2026-01-01T00:00:00Z",
        last_ts="2026-01-01T01:00:00Z",
    )
    sub_fk = _seed_session(
        conn,
        project_id=pid,
        session_id="agent-deadbeef",
        first_ts="2026-01-01T00:10:00Z",
        last_ts="2026-01-01T00:30:00Z",
    )
    _seed_message(conn, session_fk=lead_fk, seq=0, role="user", content="kick off")
    _seed_message(conn, session_fk=lead_fk, seq=1)
    _seed_message(
        conn,
        session_fk=sub_fk,
        seq=0,
        is_sidechain=True,
        role="user",
        content="warmup prompt",
    )
    _seed_message(conn, session_fk=sub_fk, seq=1, is_sidechain=True)

    g = agent_teams_service.build_team_graph(conn, lead_session_id="lead-old")
    assert g is not None
    assert len(g.agents) == 1
    sub = g.agents[0]
    assert sub.session_id == "agent-deadbeef"
    assert sub.agent_id == "deadbeef"
    assert sub.first_user_prompt == "warmup prompt"


# ── deep recursion — sub-agent itself spawned a grandchild ──────────────────


def test_team_graph_includes_grandchild_via_team_name(conn):
    """Claude Code sub-agents can themselves spawn more sub-agents (the
    ``Agent`` tool inside an agent). Those grand-children share the
    same ``team_name`` so they land in the same flat agents list — we
    don't try to render them as nested children today, but the count
    must include them.
    """
    pid = _seed_project(conn)
    lead_fk = _seed_session(
        conn,
        project_id=pid,
        session_id="lead-deep",
        first_ts="2026-04-01T00:00:00Z",
        last_ts="2026-04-01T05:00:00Z",
    )
    child_fk = _seed_session(
        conn,
        project_id=pid,
        session_id="child",
        first_ts="2026-04-01T00:30:00Z",
        last_ts="2026-04-01T01:30:00Z",
    )
    grandchild_fk = _seed_session(
        conn,
        project_id=pid,
        session_id="grandchild",
        first_ts="2026-04-01T01:00:00Z",
        last_ts="2026-04-01T01:20:00Z",
    )
    _seed_message(conn, session_fk=lead_fk, seq=0, team_name="deep", role="user", content="boot")
    _seed_message(conn, session_fk=lead_fk, seq=1, team_name="deep")
    _seed_message(
        conn,
        session_fk=child_fk,
        seq=0,
        is_sidechain=True,
        team_name="deep",
        agent_id="child-agent",
        role="user",
        content="child prompt",
    )
    _seed_message(
        conn,
        session_fk=grandchild_fk,
        seq=0,
        is_sidechain=True,
        team_name="deep",
        agent_id="grandchild-agent",
        role="user",
        content="grandchild prompt",
    )

    g = agent_teams_service.build_team_graph(conn, lead_session_id="lead-deep")
    assert g is not None
    assert len(g.agents) == 2
    assert {a.agent_id for a in g.agents} == {"child-agent", "grandchild-agent"}


# ── transcript drill-in ─────────────────────────────────────────────────────


def test_get_agent_transcript_returns_messages_in_order(conn):
    pid = _seed_project(conn)
    lead_fk = _seed_session(
        conn,
        project_id=pid,
        session_id="lead-tx",
        first_ts="2026-04-01T00:00:00Z",
        last_ts="2026-04-01T01:00:00Z",
    )
    sub_fk = _seed_session(
        conn,
        project_id=pid,
        session_id="sub-tx",
        first_ts="2026-04-01T00:10:00Z",
        last_ts="2026-04-01T00:30:00Z",
    )
    _seed_message(conn, session_fk=lead_fk, seq=0, role="user", content="boot")
    _seed_message(
        conn,
        session_fk=sub_fk,
        seq=0,
        role="user",
        is_sidechain=True,
        agent_id="tx-1",
        content="first",
    )
    _seed_message(
        conn,
        session_fk=sub_fk,
        seq=1,
        is_sidechain=True,
        agent_id="tx-1",
        content="second",
    )
    _seed_message(
        conn,
        session_fk=sub_fk,
        seq=2,
        is_sidechain=True,
        agent_id="tx-1",
        content="third",
    )

    rows = agent_teams_service.get_agent_transcript(
        conn, lead_session_id="lead-tx", agent_session_id="sub-tx"
    )
    assert rows is not None
    assert [r["content_text"] for r in rows] == ["first", "second", "third"]
    assert all(r["is_sidechain"] is True for r in rows)


def test_get_agent_transcript_cross_project_returns_none(conn):
    """Reject ``/api/agent-teams/{lead}/agent/{sub}`` when the two
    sessions live in different projects — defensive fence.
    """
    pid_a = _seed_project(conn, slug="proj-a")
    pid_b = _seed_project(conn, slug="proj-b")
    _seed_session(
        conn, project_id=pid_a, session_id="lead-x",
        first_ts="2026-04-01T00:00:00Z", last_ts="2026-04-01T01:00:00Z",
    )
    _seed_session(
        conn, project_id=pid_b, session_id="sub-y",
        first_ts="2026-04-01T00:00:00Z", last_ts="2026-04-01T01:00:00Z",
    )
    assert (
        agent_teams_service.get_agent_transcript(
            conn, lead_session_id="lead-x", agent_session_id="sub-y"
        )
        is None
    )
