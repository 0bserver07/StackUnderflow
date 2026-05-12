"""Tests for ``stackunderflow.adapters.claude_teams``.

Covers the three pure functions (``discover_teams`` / ``discover_tasks``
/ ``link_sessions_to_team``) against synthetic ``~/.claude/`` fixtures,
plus the ``materialize_team_metadata`` ingest-time orchestrator end to
end against an in-memory store, plus the v013 migration's additivity.
"""

from __future__ import annotations

import json
from pathlib import Path

import pytest

from stackunderflow.adapters import claude_teams as ct
from stackunderflow.store import db, schema

# ── fixture builders ─────────────────────────────────────────────────────────


def _write_team_config(claude_root: Path, name: str, config: dict) -> None:
    team_dir = claude_root / "teams" / name
    team_dir.mkdir(parents=True, exist_ok=True)
    (team_dir / "config.json").write_text(json.dumps(config))


def _write_tasks(claude_root: Path, team: str, tasks: list[dict]) -> None:
    tdir = claude_root / "tasks" / team
    tdir.mkdir(parents=True, exist_ok=True)
    (tdir / ".lock").write_text("")
    (tdir / ".highwatermark").write_text(str(len(tasks)))
    for t in tasks:
        (tdir / f"{t['id']}.json").write_text(json.dumps(t))


def _member(agent_id: str, name: str, *, cwd: str, lead: bool = False, prompt: str | None = None) -> dict:
    m: dict = {"agentId": agent_id, "name": name, "cwd": cwd}
    if lead:
        m["agentType"] = "team-lead"
    if prompt is not None:
        m["prompt"] = prompt
    return m


# ── discover_teams ───────────────────────────────────────────────────────────


def test_discover_teams_missing_dir_returns_empty(tmp_path: Path) -> None:
    assert ct.discover_teams(tmp_path / ".claude") == []


def test_discover_teams_parses_three_teams_with_members(tmp_path: Path) -> None:
    root = tmp_path / ".claude"
    for i in (1, 2, 3):
        _write_team_config(
            root,
            f"team-{i}",
            {
                "name": f"team-{i}",
                "description": f"desc {i}",
                "createdAt": 1_700_000_000_000 + i,
                "leadAgentId": f"team-lead@team-{i}",
                "leadSessionId": f"lead-sess-{i}",
                "members": [
                    _member(f"team-lead@team-{i}", "team-lead", cwd=f"/work/p{i}", lead=True),
                    _member(f"w-a@team-{i}", "w-a", cwd=f"/work/p{i}", prompt=f"prompt a {i}"),
                    _member(f"w-b@team-{i}", "w-b", cwd=f"/work/p{i}", prompt=f"prompt b {i}"),
                ],
            },
        )
    # A team dir with no config.json (the implicit "default" team) is skipped.
    (root / "teams" / "default" / "inboxes").mkdir(parents=True)
    (root / "teams" / "default" / "inboxes" / "lead.json").write_text("[]")

    teams = ct.discover_teams(root)
    assert [t.team_id for t in teams] == ["team-1", "team-2", "team-3"]
    t1 = teams[0]
    assert t1.description == "desc 1"
    assert t1.lead_session_id == "lead-sess-1"
    assert t1.lead_agent_id == "team-lead@team-1"
    assert t1.created_ts.startswith("2023-11-")  # 1.7e12 ms ≈ 2023-11-14
    assert t1.project_path == "/work/p1"
    assert [m.name for m in t1.members] == ["team-lead", "w-a", "w-b"]
    assert next(m for m in t1.members if m.name == "team-lead").is_lead is True
    w_a = next(m for m in t1.members if m.name == "w-a")
    assert w_a.is_lead is False
    assert w_a.prompt == "prompt a 1"
    assert json.loads(t1.config_json)["name"] == "team-1"  # raw blob round-trips


def test_discover_teams_skips_malformed_config(tmp_path: Path) -> None:
    root = tmp_path / ".claude"
    (root / "teams" / "broken").mkdir(parents=True)
    (root / "teams" / "broken" / "config.json").write_text("{not json")
    _write_team_config(root, "ok", {"name": "ok", "createdAt": 1, "members": []})
    assert [t.team_id for t in ct.discover_teams(root)] == ["ok"]


# ── discover_tasks ───────────────────────────────────────────────────────────


def test_discover_tasks_missing_dir_returns_empty(tmp_path: Path) -> None:
    assert ct.discover_tasks(tmp_path / ".claude", "nope") == []


def test_discover_tasks_parses_and_sorts(tmp_path: Path) -> None:
    root = tmp_path / ".claude"
    _write_tasks(
        root,
        "team-x",
        [
            {"id": "2", "subject": "second", "description": "do 2", "status": "pending", "owner": "w-b"},
            {"id": "1", "subject": "first", "description": "do 1", "status": "in_progress", "owner": "w-a"},
            {"id": "10", "subject": "tenth", "description": "do 10", "status": "pending"},
        ],
    )
    tasks = ct.discover_tasks(root, "team-x")
    # numeric sort: 1, 2, 10 — and the .lock / .highwatermark files are ignored.
    assert [t.task_id for t in tasks] == ["1", "2", "10"]
    assert tasks[0].owner_name == "w-a"
    assert tasks[0].description == "do 1"
    assert tasks[2].owner_name is None


# ── link_sessions_to_team ────────────────────────────────────────────────────


def _mrec(agent_id: str, name: str, *, lead: bool = False, prompt: str | None = None) -> ct.MemberRecord:
    return ct.MemberRecord(
        agent_id=agent_id, name=name, agent_type="team-lead" if lead else "general-purpose",
        model=None, cwd="/work/p", is_lead=lead, prompt=prompt,
    )


def _team(name: str = "t", lead_session: str | None = "L",
          members: list[ct.MemberRecord] | None = None) -> ct.TeamRecord:
    return ct.TeamRecord(
        team_id=name, created_ts="2026-01-01T00:00:00+00:00", description="d",
        lead_session_id=lead_session, lead_agent_id=f"team-lead@{name}",
        project_path="/work/p", members=tuple(members or []), config_json="{}",
    )


def test_link_maps_lead_and_subagents_via_teamname_and_member_prompt() -> None:
    team = _team(members=[
        _mrec("team-lead@t", "team-lead", lead=True),
        _mrec("w-a@t", "w-a", prompt="PROMPT A"),
        _mrec("w-b@t", "w-b", prompt="PROMPT B"),
    ])
    hints = [
        ct.SessionTeamHint(session_id="L", team_name="t"),
        ct.SessionTeamHint(session_id="s-a", team_name="t", agent_id="w-a@t", has_sidechain=True),
        ct.SessionTeamHint(session_id="s-b", team_name="t", agent_id="w-b", has_sidechain=True),
        # teamName matches but no member agentId matches → linked, no prompt.
        ct.SessionTeamHint(session_id="s-x", team_name="t", agent_id="ghost@t", has_sidechain=True),
    ]
    links = ct.link_sessions_to_team(hints, [team], {})
    assert links["L"].role == ct.ROLE_LEAD
    assert links["L"].parent_session_id is None
    assert links["s-a"].role == ct.ROLE_SUBAGENT
    assert links["s-a"].parent_session_id == "L"
    assert links["s-a"].spawn_prompt == "PROMPT A"
    assert links["s-b"].spawn_prompt == "PROMPT B"  # matched by bare name
    assert links["s-x"].role == ct.ROLE_SUBAGENT
    assert links["s-x"].spawn_prompt is None


def test_link_falls_back_to_task_description_then_parent_uuid_chain() -> None:
    team = _team(members=[
        _mrec("team-lead@t", "team-lead", lead=True),
        _mrec("w-a@t", "w-a"),  # no prompt → spawn_prompt comes from the owning task
    ])
    tasks = [ct.TaskRecord(task_id="1", owner_name="w-a", subject="s", description="TASK A DESC", status="pending")]
    hints = [
        ct.SessionTeamHint(session_id="L", team_name="t", uuids=frozenset({"u-L1", "u-L2"})),
        ct.SessionTeamHint(
            session_id="s-a", team_name="t", agent_id="w-a@t", has_sidechain=True, uuids=frozenset({"u-a1"}),
        ),
        # s-c carries NO teamName, but its first message's parent_uuid points
        # into s-a's uuid set → chain fallback links it as a child of s-a.
        ct.SessionTeamHint(session_id="s-c", team_name=None, has_sidechain=True, parent_uuids=frozenset({"u-a1"})),
    ]
    links = ct.link_sessions_to_team(hints, [team], {"t": tasks})
    assert links["s-a"].spawn_prompt == "TASK A DESC"
    assert "s-c" in links
    assert links["s-c"].role == ct.ROLE_SUBAGENT
    assert links["s-c"].parent_session_id == "s-a"
    assert links["s-c"].team_id == "t"


def test_link_ignores_session_for_unknown_team() -> None:
    team = _team()
    hints = [ct.SessionTeamHint(session_id="lonely", team_name="some-other-team", has_sidechain=True)]
    links = ct.link_sessions_to_team(hints, [team], {})
    assert "lonely" not in links
    assert links == {"L": ct.SessionTeamLink("t", ct.ROLE_LEAD, None, None)}


# ── materialize_team_metadata (ingest-time orchestrator) ─────────────────────


def _conn(tmp_path: Path):
    c = db.connect(tmp_path / "store.db")
    schema.apply(c)
    return c


def _seed_project(conn, *, slug: str) -> int:
    return int(
        conn.execute(
            "INSERT INTO projects (provider, slug, display_name, first_seen, last_modified) "
            "VALUES ('claude', ?, ?, 0.0, 1.0)",
            (slug, slug),
        ).lastrowid
    )


def _seed_session(conn, *, project_id: int, session_id: str, first_ts: str, last_ts: str, msg_count: int) -> int:
    return int(
        conn.execute(
            "INSERT INTO sessions (project_id, session_id, first_ts, last_ts, message_count) "
            "VALUES (?, ?, ?, ?, ?)",
            (project_id, session_id, first_ts, last_ts, msg_count),
        ).lastrowid
    )


def _seed_msg(conn, *, session_fk: int, seq: int, role: str = "assistant",
              is_sidechain: bool = False, raw: dict | None = None,
              content: str = "hi", ts: str = "2026-04-01T00:00:00Z") -> None:
    raw = raw or {}
    conn.execute(
        "INSERT INTO messages "
        "(session_fk, seq, timestamp, role, model, input_tokens, output_tokens, "
        " cache_create_tokens, cache_read_tokens, content_text, tools_json, raw_json, "
        " is_sidechain, uuid, parent_uuid) "
        "VALUES (?, ?, ?, ?, 'claude-sonnet-4-5', 10, 5, 0, 0, ?, '[]', ?, ?, ?, ?)",
        (session_fk, seq, ts, role, content, json.dumps(raw), 1 if is_sidechain else 0,
         raw.get("uuid"), raw.get("parentUuid")),
    )


def test_materialize_links_one_lead_plus_five_subagents(tmp_path: Path) -> None:
    claude_root = tmp_path / ".claude"
    members = [_member("team-lead@myteam", "team-lead", cwd="/work/myproj", lead=True)]
    for i in range(5):
        members.append(_member(f"worker-{i}@myteam", f"worker-{i}", cwd="/work/myproj", prompt=f"do task {i}"))
    _write_team_config(
        claude_root,
        "myteam",
        {
            "name": "myteam", "description": "five-way fanout", "createdAt": 1_700_000_000_000,
            "leadAgentId": "team-lead@myteam", "leadSessionId": "lead-sess", "members": members,
        },
    )
    conn = _conn(tmp_path)
    pid = _seed_project(conn, slug=ct.slug_for_path("/work/myproj"))
    lead_fk = _seed_session(
        conn, project_id=pid, session_id="lead-sess",
        first_ts="2026-04-01T00:00:00Z", last_ts="2026-04-01T05:00:00Z", msg_count=2,
    )
    _seed_msg(conn, session_fk=lead_fk, seq=0, role="user",
              raw={"teamName": "myteam", "uuid": "u-lead-0"}, content="boot")
    _seed_msg(conn, session_fk=lead_fk, seq=1,
              raw={"teamName": "myteam", "uuid": "u-lead-1", "parentUuid": "u-lead-0"})
    for i in range(5):
        sfk = _seed_session(
            conn, project_id=pid, session_id=f"agent-{i}",
            first_ts=f"2026-04-01T0{i}:10:00Z", last_ts=f"2026-04-01T0{i}:30:00Z", msg_count=3,
        )
        _seed_msg(conn, session_fk=sfk, seq=0, role="user", is_sidechain=True,
                  raw={"teamName": "myteam", "agentId": f"worker-{i}@myteam",
                       "uuid": f"u-{i}-0", "parentUuid": "u-lead-1"}, content=f"work {i}")
        _seed_msg(conn, session_fk=sfk, seq=1, is_sidechain=True,
                  raw={"teamName": "myteam", "agentId": f"worker-{i}@myteam"})

    report = ct.materialize_team_metadata(conn, claude_root=claude_root)
    assert report.teams_materialized == 1
    assert report.sessions_linked == 6

    trow = conn.execute(
        "SELECT team_id, project_id, description, lead_session_id FROM agent_teams"
    ).fetchone()
    assert trow["team_id"] == "myteam"
    assert trow["project_id"] == pid
    assert trow["description"] == "five-way fanout"
    assert trow["lead_session_id"] == "lead-sess"

    rows = {
        r["session_id"]: r
        for r in conn.execute(
            "SELECT session_id, team_id, agent_role, spawned_by_session_id, spawn_prompt FROM sessions"
        ).fetchall()
    }
    assert all(rows[s]["team_id"] == "myteam" for s in rows)
    assert rows["lead-sess"]["agent_role"] == "lead"
    assert rows["lead-sess"]["spawned_by_session_id"] is None
    for i in range(5):
        r = rows[f"agent-{i}"]
        assert r["agent_role"] == "subagent"
        assert r["spawned_by_session_id"] == "lead-sess"
        assert r["spawn_prompt"] == f"do task {i}"
    conn.close()


def test_materialize_is_idempotent(tmp_path: Path) -> None:
    claude_root = tmp_path / ".claude"
    _write_team_config(
        claude_root, "t1",
        {
            "name": "t1", "createdAt": 1_700_000_000_000, "leadAgentId": "team-lead@t1",
            "leadSessionId": "lead-1",
            "members": [
                _member("team-lead@t1", "team-lead", cwd="/w/p", lead=True),
                _member("a@t1", "a", cwd="/w/p", prompt="P"),
            ],
        },
    )
    _write_tasks(claude_root, "t1",
                 [{"id": "1", "subject": "s", "description": "TD", "status": "done", "owner": "a"}])
    conn = _conn(tmp_path)
    pid = _seed_project(conn, slug=ct.slug_for_path("/w/p"))
    lead_fk = _seed_session(
        conn, project_id=pid, session_id="lead-1",
        first_ts="2026-04-01T00:00:00Z", last_ts="2026-04-01T01:00:00Z", msg_count=1,
    )
    _seed_msg(conn, session_fk=lead_fk, seq=0, role="user", raw={"teamName": "t1"}, content="boot")
    a_fk = _seed_session(
        conn, project_id=pid, session_id="sub-a",
        first_ts="2026-04-01T00:10:00Z", last_ts="2026-04-01T00:30:00Z", msg_count=1,
    )
    _seed_msg(conn, session_fk=a_fk, seq=0, role="user", is_sidechain=True,
              raw={"teamName": "t1", "agentId": "a@t1"})

    ct.materialize_team_metadata(conn, claude_root=claude_root)
    ct.materialize_team_metadata(conn, claude_root=claude_root)
    assert conn.execute("SELECT COUNT(*) FROM agent_teams").fetchone()[0] == 1
    # the member prompt is preferred over the task description
    assert conn.execute("SELECT spawn_prompt FROM sessions WHERE session_id = 'sub-a'").fetchone()[0] == "P"
    conn.close()


def test_materialize_noop_when_no_teams_dir(tmp_path: Path) -> None:
    conn = _conn(tmp_path)
    report = ct.materialize_team_metadata(conn, claude_root=tmp_path / ".claude")
    assert report.teams_seen == 0 and report.teams_materialized == 0
    assert conn.execute("SELECT COUNT(*) FROM agent_teams").fetchone()[0] == 0
    conn.close()


def test_materialize_skips_team_with_no_ingested_sessions(tmp_path: Path) -> None:
    claude_root = tmp_path / ".claude"
    _write_team_config(claude_root, "ghost", {
        "name": "ghost", "createdAt": 1, "leadAgentId": "team-lead@ghost",
        "leadSessionId": "never-ingested",
        "members": [_member("team-lead@ghost", "team-lead", cwd="/nowhere", lead=True)],
    })
    conn = _conn(tmp_path)
    report = ct.materialize_team_metadata(conn, claude_root=claude_root)
    assert report.teams_seen == 1
    assert report.teams_materialized == 0
    conn.close()


# ── v013 migration additivity ───────────────────────────────────────────────


def test_v013_columns_and_table_present_and_nullable(tmp_path: Path) -> None:
    conn = _conn(tmp_path)
    cols = {r["name"] for r in conn.execute("PRAGMA table_info(sessions)")}
    assert {"team_id", "spawned_by_session_id", "spawn_prompt", "agent_role"}.issubset(cols)
    assert conn.execute("SELECT COUNT(*) FROM agent_teams").fetchone()[0] == 0
    pid = _seed_project(conn, slug="-w-p")
    _seed_session(conn, project_id=pid, session_id="s1",
                  first_ts="2026-04-01T00:00:00Z", last_ts="2026-04-01T01:00:00Z", msg_count=0)
    schema.apply(conn)  # idempotent re-run must not clobber
    r = conn.execute(
        "SELECT team_id, spawned_by_session_id, spawn_prompt, agent_role FROM sessions WHERE session_id = 's1'"
    ).fetchone()
    assert r["team_id"] is None and r["spawned_by_session_id"] is None
    assert r["spawn_prompt"] is None and r["agent_role"] is None
    assert conn.execute("PRAGMA user_version").fetchone()[0] == schema.CURRENT_VERSION
    conn.close()


# ── ClaudeAdapter.materialize_metadata smoke (the run_ingest hook) ───────────


def test_claude_adapter_exposes_materialize_metadata(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    from stackunderflow.adapters.claude import ClaudeAdapter

    # Point HOME at a tmp dir with no ~/.claude — the hook must be a clean no-op.
    monkeypatch.setenv("HOME", str(tmp_path))
    conn = _conn(tmp_path / "store-home")
    ClaudeAdapter().materialize_metadata(conn)  # must not raise
    assert conn.execute("SELECT COUNT(*) FROM agent_teams").fetchone()[0] == 0
    conn.close()
