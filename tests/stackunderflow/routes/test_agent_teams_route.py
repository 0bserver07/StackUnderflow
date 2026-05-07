"""Tests for ``/api/agent-teams/*`` routes.

Mounts only the agent-teams router against a fresh schema-applied store
and seeds messages directly so each test case is self-contained. Locks
the JSON contract documented in ``docs/specs/agent-teams.md``.
"""

from __future__ import annotations

import json

import pytest
from fastapi import FastAPI
from fastapi.testclient import TestClient

import stackunderflow.deps as deps
from stackunderflow.routes.agent_teams import router as agent_teams_router
from stackunderflow.store import db, schema

# ── fixtures ────────────────────────────────────────────────────────────────


@pytest.fixture()
def app_client(tmp_path, monkeypatch):
    store_db = tmp_path / "store.db"
    conn = db.connect(store_db)
    schema.apply(conn)
    conn.close()

    monkeypatch.setattr(deps, "store_path", store_db)

    app = FastAPI()
    app.include_router(agent_teams_router)
    return TestClient(app), store_db


# ── seed helpers ─────────────────────────────────────────────────────────────


def _seed_project(conn, *, slug: str = "test-project") -> int:
    cur = conn.execute(
        "INSERT INTO projects (provider, slug, display_name, first_seen, last_modified) "
        "VALUES (?, ?, ?, ?, ?)",
        ("claude", slug, slug, 0.0, 1_000_000.0),
    )
    return int(cur.lastrowid)


def _seed_session(conn, *, project_id, session_id, first_ts, last_ts) -> int:
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
    session_fk,
    seq,
    role="assistant",
    is_sidechain=False,
    team_name=None,
    agent_id=None,
    content="hello",
    model="claude-sonnet-4-5",
    timestamp="2026-04-01T00:00:00Z",
):
    raw: dict[str, object] = {"sessionId": str(session_fk), "type": role}
    if team_name is not None:
        raw["teamName"] = team_name
    if agent_id is not None:
        raw["agentId"] = agent_id
    conn.execute(
        "INSERT INTO messages "
        "(session_fk, seq, timestamp, role, model, "
        " input_tokens, output_tokens, cache_create_tokens, cache_read_tokens, "
        " content_text, tools_json, raw_json, is_sidechain) "
        "VALUES (?, ?, ?, ?, ?, 100, 50, 0, 0, ?, '[]', ?, ?)",
        (
            session_fk,
            seq,
            timestamp,
            role,
            model,
            content,
            json.dumps(raw),
            1 if is_sidechain else 0,
        ),
    )


# ── empty store ──────────────────────────────────────────────────────────────


def test_list_returns_empty_on_fresh_store(app_client):
    client, _ = app_client
    res = client.get("/api/agent-teams")
    assert res.status_code == 200
    assert res.json() == {"teams": []}


def test_list_returns_empty_on_store_with_no_sidechain(app_client):
    client, store_db = app_client
    conn = db.connect(store_db)
    pid = _seed_project(conn)
    sfk = _seed_session(
        conn,
        project_id=pid,
        session_id="lonely",
        first_ts="2026-04-01T00:00:00Z",
        last_ts="2026-04-01T01:00:00Z",
    )
    _seed_message(conn, session_fk=sfk, seq=0)
    conn.commit()
    conn.close()

    res = client.get("/api/agent-teams")
    assert res.status_code == 200
    assert res.json() == {"teams": []}


def test_get_team_returns_404_for_unknown_session(app_client):
    client, _ = app_client
    res = client.get("/api/agent-teams/does-not-exist")
    assert res.status_code == 404
    body = res.json()
    assert "Lead session not found" in body["detail"]


def test_get_transcript_returns_404_when_pair_missing(app_client):
    client, _ = app_client
    res = client.get("/api/agent-teams/lead-x/agent/sub-y")
    assert res.status_code == 404


# ── single-agent ─────────────────────────────────────────────────────────────


def test_list_and_graph_for_single_agent_team(app_client):
    client, store_db = app_client
    conn = db.connect(store_db)
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
        team_name="alpha",
        content="kick off the team",
    )
    _seed_message(conn, session_fk=lead_fk, seq=1, team_name="alpha")
    _seed_message(
        conn,
        session_fk=sub_fk,
        seq=0,
        role="user",
        is_sidechain=True,
        team_name="alpha",
        agent_id="alpha-worker",
        content="please research X",
    )
    _seed_message(
        conn,
        session_fk=sub_fk,
        seq=1,
        is_sidechain=True,
        team_name="alpha",
        agent_id="alpha-worker",
    )
    conn.commit()
    conn.close()

    list_res = client.get("/api/agent-teams")
    assert list_res.status_code == 200
    teams = list_res.json()["teams"]
    assert len(teams) == 1
    assert teams[0]["session_id"] == "lead-001"
    assert teams[0]["team_name"] == "alpha"
    assert teams[0]["agent_count"] == 1

    g_res = client.get("/api/agent-teams/lead-001")
    assert g_res.status_code == 200
    g = g_res.json()
    assert g["session_id"] == "lead-001"
    assert g["team_name"] == "alpha"
    assert g["lead"]["session_id"] == "lead-001"
    assert g["lead"]["is_lead"] is True
    assert g["lead"]["first_user_prompt"] == "kick off the team"
    assert len(g["agents"]) == 1
    sub = g["agents"][0]
    assert sub["session_id"] == "sub-001"
    assert sub["agent_id"] == "alpha-worker"
    assert sub["parent_session_id"] == "lead-001"
    assert sub["first_user_prompt"] == "please research X"
    assert isinstance(sub["cost_usd"], int | float)


# ── multi-agent fan-out ─────────────────────────────────────────────────────


def test_multi_agent_fanout_preserves_order(app_client):
    client, store_db = app_client
    conn = db.connect(store_db)
    pid = _seed_project(conn)
    lead_fk = _seed_session(
        conn,
        project_id=pid,
        session_id="lead-multi",
        first_ts="2026-04-01T00:00:00Z",
        last_ts="2026-04-01T03:00:00Z",
    )
    a1 = _seed_session(
        conn, project_id=pid, session_id="agent-a",
        first_ts="2026-04-01T00:10:00Z", last_ts="2026-04-01T01:10:00Z",
    )
    a2 = _seed_session(
        conn, project_id=pid, session_id="agent-b",
        first_ts="2026-04-01T00:20:00Z", last_ts="2026-04-01T02:00:00Z",
    )
    a3 = _seed_session(
        conn, project_id=pid, session_id="agent-c",
        first_ts="2026-04-01T00:30:00Z", last_ts="2026-04-01T02:30:00Z",
    )
    _seed_message(conn, session_fk=lead_fk, seq=0, team_name="multi", role="user", content="boot")
    _seed_message(conn, session_fk=lead_fk, seq=1, team_name="multi")
    for i, sfk in enumerate((a1, a2, a3)):
        _seed_message(
            conn, session_fk=sfk, seq=0, role="user", is_sidechain=True,
            team_name="multi", agent_id=f"worker-{i}",
            content=f"prompt for worker {i}",
        )
        _seed_message(
            conn, session_fk=sfk, seq=1, is_sidechain=True,
            team_name="multi", agent_id=f"worker-{i}",
        )
    conn.commit()
    conn.close()

    g = client.get("/api/agent-teams/lead-multi").json()
    assert [a["session_id"] for a in g["agents"]] == ["agent-a", "agent-b", "agent-c"]
    assert {a["agent_id"] for a in g["agents"]} == {"worker-0", "worker-1", "worker-2"}


# ── deep recursion ─────────────────────────────────────────────────────────


def test_deep_recursion_grandchildren_in_same_team(app_client):
    client, store_db = app_client
    conn = db.connect(store_db)
    pid = _seed_project(conn)
    lead_fk = _seed_session(
        conn, project_id=pid, session_id="lead-d",
        first_ts="2026-04-01T00:00:00Z", last_ts="2026-04-01T05:00:00Z",
    )
    child_fk = _seed_session(
        conn, project_id=pid, session_id="child",
        first_ts="2026-04-01T00:30:00Z", last_ts="2026-04-01T01:30:00Z",
    )
    grandchild_fk = _seed_session(
        conn, project_id=pid, session_id="grandchild",
        first_ts="2026-04-01T01:00:00Z", last_ts="2026-04-01T01:20:00Z",
    )
    _seed_message(conn, session_fk=lead_fk, seq=0, team_name="deep", role="user", content="boot")
    _seed_message(conn, session_fk=lead_fk, seq=1, team_name="deep")
    _seed_message(
        conn, session_fk=child_fk, seq=0, is_sidechain=True,
        team_name="deep", agent_id="child-agent", role="user", content="child prompt",
    )
    _seed_message(
        conn, session_fk=grandchild_fk, seq=0, is_sidechain=True,
        team_name="deep", agent_id="grandchild-agent", role="user",
        content="grandchild prompt",
    )
    conn.commit()
    conn.close()

    g = client.get("/api/agent-teams/lead-d").json()
    assert len(g["agents"]) == 2
    assert {a["agent_id"] for a in g["agents"]} == {"child-agent", "grandchild-agent"}


# ── transcript drill-in ─────────────────────────────────────────────────────


def test_transcript_returns_messages_in_seq_order(app_client):
    client, store_db = app_client
    conn = db.connect(store_db)
    pid = _seed_project(conn)
    lead_fk = _seed_session(
        conn, project_id=pid, session_id="lead-tx",
        first_ts="2026-04-01T00:00:00Z", last_ts="2026-04-01T01:00:00Z",
    )
    sub_fk = _seed_session(
        conn, project_id=pid, session_id="sub-tx",
        first_ts="2026-04-01T00:10:00Z", last_ts="2026-04-01T00:30:00Z",
    )
    _seed_message(conn, session_fk=lead_fk, seq=0, role="user", content="boot")
    _seed_message(
        conn, session_fk=sub_fk, seq=0, is_sidechain=True, role="user",
        agent_id="tx-1", content="first",
    )
    _seed_message(
        conn, session_fk=sub_fk, seq=1, is_sidechain=True, agent_id="tx-1", content="second",
    )
    _seed_message(
        conn, session_fk=sub_fk, seq=2, is_sidechain=True, agent_id="tx-1", content="third",
    )
    conn.commit()
    conn.close()

    res = client.get("/api/agent-teams/lead-tx/agent/sub-tx")
    assert res.status_code == 200
    body = res.json()
    assert body["session_id"] == "lead-tx"
    assert body["agent_session_id"] == "sub-tx"
    assert body["message_count"] == 3
    assert [m["content_text"] for m in body["messages"]] == ["first", "second", "third"]
    assert all(m["is_sidechain"] is True for m in body["messages"])


def test_transcript_rejects_cross_project_pair(app_client):
    client, store_db = app_client
    conn = db.connect(store_db)
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
    conn.commit()
    conn.close()

    res = client.get("/api/agent-teams/lead-x/agent/sub-y")
    assert res.status_code == 404


# ── limit param bounds ──────────────────────────────────────────────────────


def test_list_rejects_zero_limit(app_client):
    client, _ = app_client
    res = client.get("/api/agent-teams?limit=0")
    assert res.status_code == 422  # FastAPI Query(ge=1) violation


def test_list_rejects_too_large_limit(app_client):
    client, _ = app_client
    res = client.get("/api/agent-teams?limit=10000")
    assert res.status_code == 422
