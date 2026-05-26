"""Unit tests for Claude Code fallback team discovery (JSONL fallback).

Exercises discover_teams_from_jsonl and team/session linking on a
synthetic project workspace directory containing orchestrator and
worker transcripts.
"""

from __future__ import annotations

import json
import sqlite3
from pathlib import Path

import pytest

from stackunderflow.adapters import claude_teams as ct
from stackunderflow.store import db, schema


def _write_jsonl(path: Path, records: list[dict]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text("\n".join(json.dumps(r) for r in records) + "\n", encoding="utf-8")


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


def test_discover_teams_from_jsonl_and_materialize(tmp_path: Path) -> None:
    root = tmp_path / ".claude"

    # 1. Write synthetic orchestrator transcript
    lead_records = [
        {
            "type": "user",
            "timestamp": "2026-05-06T23:00:00Z",
            "message": {"content": "start the orchestration"},
        },
        {
            "type": "assistant",
            "timestamp": "2026-05-06T23:01:00Z",
            "message": {
                "content": [
                    {
                        "type": "tool_use",
                        "id": "tc-1",
                        "name": "TeamCreate",
                        "input": {
                            "team_name": "fallback-team",
                            "description": "reconstructed team from JSONL",
                        },
                    }
                ]
            },
        },
        {
            "type": "assistant",
            "timestamp": "2026-05-06T23:02:00Z",
            "message": {
                "content": [
                    {
                        "type": "tool_use",
                        "id": "ag-1",
                        "name": "Agent",
                        "input": {
                            "team_name": "fallback-team",
                            "name": "worker-a",
                            "subagent_type": "general-purpose",
                            "prompt": "build feature A",
                        },
                    }
                ]
            },
        },
        {
            "type": "assistant",
            "timestamp": "2026-05-06T23:03:00Z",
            "message": {
                "content": [
                    {
                        "type": "tool_use",
                        "id": "ag-2",
                        "name": "Agent",
                        "input": {
                            "team_name": "fallback-team",
                            "name": "worker-b",
                            "subagent_type": "general-purpose",
                            "prompt": "build feature B",
                        },
                    }
                ]
            },
        },
    ]
    _write_jsonl(root / "projects" / "test-proj" / "lead.jsonl", lead_records)

    # 2. Write synthetic worker transcripts
    worker_a_records = [
        {
            "type": "user",
            "timestamp": "2026-05-06T23:02:05Z",
            "message": {
                "content": "You are `worker-a` on `fallback-team`. Implement feature A."
            },
        }
    ]
    _write_jsonl(root / "projects" / "test-proj" / "worker-a.jsonl", worker_a_records)

    worker_b_records = [
        {
            "type": "user",
            "timestamp": "2026-05-06T23:03:05Z",
            "message": {
                "content": [
                    {
                        "type": "text",
                        "text": "You are `worker-b` in team `fallback-team`. Implement feature B.",
                    }
                ]
            },
        }
    ]
    _write_jsonl(root / "projects" / "test-proj" / "worker-b.jsonl", worker_b_records)

    # 3. Discover fallback teams
    teams, worker_map = ct.discover_teams_from_jsonl(root)
    assert len(teams) == 1
    team = teams[0]
    assert team.team_id == "fallback-team"
    assert team.lead_session_id == "lead"
    assert team.description == "reconstructed team from JSONL"
    assert len(team.members) == 3
    assert {m.name for m in team.members} == {"team-lead", "worker-a", "worker-b"}

    config = json.loads(team.config_json)
    assert config["_source"] == "jsonl_fallback"
    assert config["leadSessionId"] == "lead"
    assert len(config["members"]) == 3

    assert worker_map == {
        "worker-a": ("worker-a", "fallback-team"),
        "worker-b": ("worker-b", "fallback-team"),
    }

    # 4. Ingest-time materialization verification
    conn = sqlite3.connect(":memory:")
    conn.row_factory = sqlite3.Row
    schema.apply(conn)

    # Insert projects
    pid = conn.execute(
        "INSERT INTO projects (provider, slug, display_name, first_seen, last_modified) "
        "VALUES ('claude', 'test-proj', 'test-proj', '2026-05-06T23:00:00Z', '2026-05-06T23:10:00Z')"
    ).lastrowid

    # Insert sessions
    lead_fk = conn.execute(
        "INSERT INTO sessions (project_id, session_id, first_ts, last_ts, message_count) "
        "VALUES (?, 'lead', '2026-05-06T23:00:00Z', '2026-05-06T23:10:00Z', 10)",
        (pid,),
    ).lastrowid
    worker_a_fk = conn.execute(
        "INSERT INTO sessions (project_id, session_id, first_ts, last_ts, message_count) "
        "VALUES (?, 'worker-a', '2026-05-06T23:02:00Z', '2026-05-06T23:05:00Z', 1)",
        (pid,),
    ).lastrowid
    worker_b_fk = conn.execute(
        "INSERT INTO sessions (project_id, session_id, first_ts, last_ts, message_count) "
        "VALUES (?, 'worker-b', '2026-05-06T23:03:00Z', '2026-05-06T23:06:00Z', 1)",
        (pid,),
    ).lastrowid

    # Insert messages for lead to satisfy hints peeker
    _seed_msg(conn, session_fk=lead_fk, seq=1, role="user", ts="2026-05-06T23:00:00Z")
    _seed_msg(conn, session_fk=worker_a_fk, seq=1, role="user", ts="2026-05-06T23:02:05Z")
    _seed_msg(conn, session_fk=worker_b_fk, seq=1, role="user", ts="2026-05-06T23:03:05Z")

    # Commit outstanding inserts to close the implicit transaction
    conn.commit()

    report = ct.materialize_team_metadata(conn, claude_root=root)
    assert report.teams_seen == 1
    assert report.teams_materialized == 1
    assert report.sessions_linked == 3

    # Assert database mappings
    db_team = conn.execute(
        "SELECT team_id, lead_session_id, config_json FROM agent_teams WHERE team_id = 'fallback-team'"
    ).fetchone()
    assert db_team is not None
    assert db_team[0] == "fallback-team"
    assert db_team[1] == "lead"
    assert json.loads(db_team[2])["_source"] == "jsonl_fallback"

    sessions_linked = conn.execute(
        "SELECT session_id, team_id, agent_role, spawn_prompt FROM sessions ORDER BY session_id"
    ).fetchall()
    assert len(sessions_linked) == 3

    lead_row = sessions_linked[0]
    assert lead_row[0] == "lead"
    assert lead_row[1] == "fallback-team"
    assert lead_row[2] == "lead"
    assert lead_row[3] is None

    worker_a_row = sessions_linked[1]
    assert worker_a_row[0] == "worker-a"
    assert worker_a_row[1] == "fallback-team"
    assert worker_a_row[2] == "subagent"
    assert worker_a_row[3] == "build feature A"

    worker_b_row = sessions_linked[2]
    assert worker_b_row[0] == "worker-b"
    assert worker_b_row[1] == "fallback-team"
    assert worker_b_row[2] == "subagent"
    assert worker_b_row[3] == "build feature B"

    conn.close()
