"""End-to-end fast-mode plumbing test.

Walks one Claude JSONL fixture with ``service_tier="priority"`` records
through the real adapter → real writer → real store → real query path,
then asserts the dashboard cost reflects the 6× Opus multiplier.

The point of this test is structural: every link in the chain
(adapter detects priority, writer persists ``speed`` to the new column,
``get_project_stats`` reconstructs Records with ``speed="fast"`` so the
aggregator's (model, speed) collectors price correctly) has its own unit
test, but only this end-to-end run proves they're wired together.
"""

from __future__ import annotations

import json
import sqlite3
from collections.abc import Generator
from pathlib import Path

import pytest

from stackunderflow.adapters.base import SessionRef
from stackunderflow.adapters.claude import ClaudeAdapter
from stackunderflow.infra.costs import compute_cost
from stackunderflow.ingest.writer import ingest_file
from stackunderflow.store import db, queries, schema


@pytest.fixture
def conn(tmp_path: Path) -> Generator[sqlite3.Connection, None, None]:
    c = db.connect(tmp_path / "store.db")
    schema.apply(c)
    yield c
    c.close()


def _claude_assistant_record(
    *,
    uuid: str,
    parent_uuid: str | None,
    timestamp: str,
    model: str,
    input_tokens: int,
    output_tokens: int,
    service_tier: str,
) -> dict:
    """Synthetic Claude JSONL line — assistant role with usage block."""
    return {
        "type": "assistant",
        "uuid": uuid,
        "parentUuid": parent_uuid,
        "timestamp": timestamp,
        "isSidechain": False,
        "sessionId": "sess-1",
        "message": {
            "role": "assistant",
            "model": model,
            "content": [{"type": "text", "text": "ok"}],
            "usage": {
                "input_tokens": input_tokens,
                "output_tokens": output_tokens,
                "cache_creation_input_tokens": 0,
                "cache_read_input_tokens": 0,
                "service_tier": service_tier,
            },
        },
    }


def _claude_user_record(*, uuid: str, timestamp: str, text: str = "hi") -> dict:
    return {
        "type": "user",
        "uuid": uuid,
        "parentUuid": None,
        "timestamp": timestamp,
        "isSidechain": False,
        "sessionId": "sess-1",
        "message": {
            "role": "user",
            "content": text,
        },
    }


def test_priority_tier_jsonl_ingests_with_speed_fast(conn, tmp_path: Path) -> None:
    """Adapter → writer → DB column round-trip for service_tier='priority'."""
    project_dir = tmp_path / "claude_projects" / "-test-proj"
    project_dir.mkdir(parents=True)
    jsonl = project_dir / "sess-1.jsonl"
    lines = [
        _claude_user_record(uuid="u1", timestamp="2026-04-15T10:00:00.000Z"),
        _claude_assistant_record(
            uuid="a1", parent_uuid="u1",
            timestamp="2026-04-15T10:00:01.000Z",
            model="claude-opus-4-6",
            input_tokens=1000, output_tokens=500,
            service_tier="priority",  # → speed='fast'
        ),
        _claude_assistant_record(
            uuid="a2", parent_uuid="u1",
            timestamp="2026-04-15T10:00:02.000Z",
            model="claude-opus-4-6",
            input_tokens=1000, output_tokens=500,
            service_tier="standard",  # → speed='standard'
        ),
    ]
    jsonl.write_text("\n".join(json.dumps(line) for line in lines) + "\n")

    stat = jsonl.stat()
    ref = SessionRef(
        provider="claude",
        project_slug="-test-proj",
        session_id="sess-1",
        file_path=jsonl,
        file_mtime=stat.st_mtime,
        file_size=stat.st_size,
    )

    ingest_file(conn, ClaudeAdapter(), ref)

    # Speed column must reflect what the adapter parsed.
    rows = conn.execute(
        "SELECT model, speed, input_tokens, output_tokens "
        "FROM messages WHERE role = 'assistant' ORDER BY seq"
    ).fetchall()
    assert len(rows) == 2
    speeds = sorted(r["speed"] for r in rows)
    assert speeds == ["fast", "standard"]


def test_priority_tier_session_cost_reflects_6x_multiplier(
    conn, tmp_path: Path,
) -> None:
    """End-to-end: ingest a priority-tier session and verify get_global_stats
    reports the 6×-multiplied cost on the Opus fast subset.

    The before/after framing: PR #44 fixed the in-process pipeline but the
    SQL store had no ``speed`` column, so this exact query path silently
    returned 1× cost. After v003 lands, the same query returns 6× on the
    fast slice — that's the gap this PR closes.
    """
    project_dir = tmp_path / "claude_projects" / "-test-proj"
    project_dir.mkdir(parents=True)
    jsonl = project_dir / "sess-1.jsonl"

    # Two assistant messages, identical token counts. One on the priority
    # tier, one standard. Cost difference must be exactly 6× on the fast row.
    lines = [
        _claude_user_record(uuid="u1", timestamp="2026-04-15T10:00:00.000Z"),
        _claude_assistant_record(
            uuid="a1", parent_uuid="u1",
            timestamp="2026-04-15T10:00:01.000Z",
            model="claude-opus-4-6",
            input_tokens=1000, output_tokens=500,
            service_tier="priority",
        ),
        _claude_user_record(uuid="u2", timestamp="2026-04-15T10:01:00.000Z"),
        _claude_assistant_record(
            uuid="a2", parent_uuid="u2",
            timestamp="2026-04-15T10:01:01.000Z",
            model="claude-opus-4-6",
            input_tokens=1000, output_tokens=500,
            service_tier="standard",
        ),
    ]
    jsonl.write_text("\n".join(json.dumps(line) for line in lines) + "\n")

    stat = jsonl.stat()
    ref = SessionRef(
        provider="claude",
        project_slug="-test-proj",
        session_id="sess-1",
        file_path=jsonl,
        file_mtime=stat.st_mtime,
        file_size=stat.st_size,
    )
    ingest_file(conn, ClaudeAdapter(), ref)

    stats = queries.get_global_stats(conn)
    opus_total = stats["models"]["claude-opus-4-6"]["cost"]

    standard_cost = compute_cost(
        {"input": 1000, "output": 500, "cache_creation": 0, "cache_read": 0},
        "claude-opus-4-6",
        speed="standard",
    )["total_cost"]
    fast_cost = compute_cost(
        {"input": 1000, "output": 500, "cache_creation": 0, "cache_read": 0},
        "claude-opus-4-6",
        speed="fast",
    )["total_cost"]

    # The Opus fast slice is exactly 6× the standard slice.
    assert fast_cost == pytest.approx(standard_cost * 6.0)
    # Total reflects both rows priced at their right tier.
    assert opus_total == pytest.approx(standard_cost + fast_cost)


def test_full_pipeline_get_project_stats_threads_speed(conn, tmp_path: Path) -> None:
    """The richer per-project pipeline (build_enriched_dataset →
    classifier → enricher → aggregator) also threads speed end-to-end —
    the (model, speed) bucket in stats['models'] picks up the fast-tier
    multiplier.
    """
    project_dir = tmp_path / "claude_projects" / "-proj"
    project_dir.mkdir(parents=True)
    jsonl = project_dir / "sess-1.jsonl"
    lines = [
        _claude_user_record(uuid="u1", timestamp="2026-04-15T10:00:00.000Z"),
        _claude_assistant_record(
            uuid="a1", parent_uuid="u1",
            timestamp="2026-04-15T10:00:01.000Z",
            model="claude-opus-4-6",
            input_tokens=2000, output_tokens=1000,
            service_tier="priority",
        ),
    ]
    jsonl.write_text("\n".join(json.dumps(line) for line in lines) + "\n")
    stat = jsonl.stat()
    ref = SessionRef(
        provider="claude",
        project_slug="-proj",
        session_id="sess-1",
        file_path=jsonl,
        file_mtime=stat.st_mtime,
        file_size=stat.st_size,
    )
    ingest_file(conn, ClaudeAdapter(), ref)

    project_id = conn.execute(
        "SELECT id FROM projects WHERE slug = '-proj'"
    ).fetchone()["id"]
    _msgs, stats = queries.get_project_stats(conn, project_id=project_id)

    # ``session_costs`` is the aggregator's canonical cost-per-session
    # output; the (model, speed) buckets inside _SessionCostCollector
    # already apply the fast-tier multiplier.
    fast_cost = compute_cost(
        {"input": 2000, "output": 1000, "cache_creation": 0, "cache_read": 0},
        "claude-opus-4-6",
        speed="fast",
    )["total_cost"]
    standard_cost = compute_cost(
        {"input": 2000, "output": 1000, "cache_creation": 0, "cache_read": 0},
        "claude-opus-4-6",
        speed="standard",
    )["total_cost"]

    session_costs = stats.get("session_costs") or []
    assert session_costs, "session_costs missing from stats"
    summed = sum(s["cost"] for s in session_costs)
    # The single session must reflect the fast-tier multiplier — not the
    # standard-tier number that the SQL path returned before v003.
    assert summed == pytest.approx(fast_cost)
    assert summed != pytest.approx(standard_cost)
