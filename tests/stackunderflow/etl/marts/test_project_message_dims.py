"""Equivalence: ``project_mart`` message-type + command dims == full pipeline.

The North Star for the v022 materialisation (ui-perf-audit #7/#26): the
counts ``ProjectMartBuilder`` writes onto ``project_mart`` must EXACTLY equal
what ``get_project_stats`` (the full classifier → enricher → aggregator pass)
computes for the same project — single- and multi-provider. We prove it by
building a synthetic store with real ``messages.raw_json``, running the mart
refresh, and asserting the mart row's dims against the aggregator's
``overview.message_types`` / ``user_interactions.user_commands_analyzed`` and
the tool counts derived from the pipeline's own formatted message list.
"""

from __future__ import annotations

import json
from pathlib import Path

import pytest

from stackunderflow.etl.marts.project import ProjectMartBuilder
from stackunderflow.store import db, queries, schema

# ── store builders ───────────────────────────────────────────────────────────


def _connect(tmp_path: Path):
    conn = db.connect(tmp_path / "store.db")
    schema.apply(conn)
    return conn


def _insert_project(conn, *, pid: int, provider: str, slug: str) -> None:
    conn.execute(
        "INSERT INTO projects (id, provider, slug, display_name, "
        "first_seen, last_modified) VALUES (?, ?, ?, ?, 0, 0)",
        (pid, provider, slug, slug),
    )


def _insert_session(conn, *, sid: int, pid: int, session_id: str) -> None:
    conn.execute(
        "INSERT INTO sessions (id, project_id, session_id) VALUES (?, ?, ?)",
        (sid, pid, session_id),
    )


def _insert_message(
    conn, *, mid: int, session_fk: int, seq: int, ts: str, role: str, payload: dict
) -> None:
    conn.execute(
        "INSERT INTO messages (id, session_fk, seq, timestamp, role, raw_json) "
        "VALUES (?, ?, ?, ?, ?, ?)",
        (mid, session_fk, seq, ts, role, json.dumps(payload)),
    )


def _insert_assistant_event(
    conn, *, eid: int, mid: int, pid: int, provider: str, session_id: str, ts: str
) -> None:
    """Mirror the normalizer: only billable assistant rows become events."""
    conn.execute(
        "INSERT INTO usage_events (id, source_message_fk, provider, project_id, "
        "session_id, ts, day, model, speed, input_tokens, output_tokens, "
        "cache_read_tokens, cache_create_tokens, cost_usd, cost_source, role) "
        "VALUES (?, ?, ?, ?, ?, ?, ?, 'm', 'standard', 10, 5, 0, 0, 0.0, "
        "'rate_card', 'assistant')",
        (eid, mid, provider, pid, session_id, ts, ts[:10]),
    )


def _user(content) -> dict:
    return {"type": "user", "message": {"role": "user", "content": content}}


def _assistant(content) -> dict:
    return {
        "type": "assistant",
        "message": {"role": "assistant", "model": "m", "content": content,
                    "usage": {"input_tokens": 10, "output_tokens": 5}},
    }


def _seed_project(conn, *, pid: int, provider: str, slug: str, sid: int,
                  session_id: str, base_eid: int) -> None:
    """Seed one project with a realistic mix of every message kind.

    Returns nothing; the caller knows the expected shape by construction:
    3 user (1 command, 1 tool_result, 1 interruption) + 2 assistant
    (1 with a tool_use, 1 text-only) + 1 summary + 1 more command.
    """
    _insert_project(conn, pid=pid, provider=provider, slug=slug)
    _insert_session(conn, sid=sid, pid=pid, session_id=session_id)
    rows = [
        ("user", _user("do X")),                                    # command
        ("assistant", _assistant([
            {"type": "text", "text": "ok"},
            {"type": "tool_use", "name": "Read", "id": f"t{pid}", "input": {"file_path": "/a"}},
        ])),                                                          # tool_use
        ("user", _user([
            {"type": "tool_result", "tool_use_id": f"t{pid}", "content": "data"},
        ])),                                                          # tool_result
        ("assistant", _assistant([{"type": "text", "text": "done"}])),  # text only
        ("user", _user("[Request interrupted by user for tool use]")),  # interruption
        ("user", _user("do Y")),                                    # command #2
        ("summary", {"type": "summary", "summary": "recap"}),       # summary
    ]
    eid = base_eid
    for i, (role, payload) in enumerate(rows):
        mid = pid * 100 + i
        ts = f"2026-04-0{i + 1}T00:00:00+00:00"
        _insert_message(conn, mid=mid, session_fk=sid, seq=i, ts=ts,
                        role=role, payload=payload)
        if role == "assistant":
            _insert_assistant_event(conn, eid=eid, mid=mid, pid=pid,
                                    provider=provider, session_id=session_id, ts=ts)
            eid += 1


def _mart_row(conn, pid: int) -> dict:
    row = conn.execute(
        "SELECT * FROM project_mart WHERE project_id = ?", (pid,)
    ).fetchone()
    return dict(row)


def _pipeline_dims(messages: list[dict], stats: dict) -> dict:
    """The dims the full pipeline produces, read from its own output."""
    mt = stats["overview"]["message_types"]
    return {
        "user": int(mt.get("user", 0)),
        "assistant": int(mt.get("assistant", 0)),
        "tool_use": sum(1 for m in messages if m["type"] == "assistant" and m["tools"]),
        "tool_result": sum(1 for m in messages if m.get("has_tool_result")),
        "commands": int(stats["user_interactions"]["user_commands_analyzed"]),
    }


# ── tests ────────────────────────────────────────────────────────────────────


def test_dims_match_full_pipeline_single_provider(tmp_path):
    conn = _connect(tmp_path)
    _seed_project(conn, pid=1, provider="claude", slug="alpha", sid=1,
                  session_id="s1", base_eid=1)
    conn.commit()

    ProjectMartBuilder().rebuild_from_scratch(conn)

    messages, stats = queries.get_project_stats(conn, project_id=1)
    expected = _pipeline_dims(messages, stats)
    row = _mart_row(conn, 1)

    # Sanity on the fixture so a future edit can't make the test vacuous.
    # 4 user turns (2 commands + 1 tool_result + 1 interruption), 2 assistant.
    assert expected == {
        "user": 4, "assistant": 2, "tool_use": 1, "tool_result": 1, "commands": 2,
    }

    assert row["total_user_messages"] == expected["user"]
    assert row["total_assistant_messages"] == expected["assistant"]
    assert row["total_tool_use_messages"] == expected["tool_use"]
    assert row["total_tool_result_messages"] == expected["tool_result"]
    assert row["total_commands"] == expected["commands"]
    conn.close()


def test_dims_match_full_pipeline_multi_provider(tmp_path):
    """Same slug under two providers → summed mart rows == combined pipeline."""
    conn = _connect(tmp_path)
    _seed_project(conn, pid=1, provider="claude", slug="alpha", sid=1,
                  session_id="s1", base_eid=1)
    _seed_project(conn, pid=2, provider="codex", slug="alpha", sid=2,
                  session_id="s2", base_eid=100)
    conn.commit()

    ProjectMartBuilder().rebuild_from_scratch(conn)

    # Full pipeline over BOTH provider ids (how routes/data merges a slug).
    messages, stats = queries.get_project_stats(conn, project_id=[1, 2])
    expected = _pipeline_dims(messages, stats)

    r1, r2 = _mart_row(conn, 1), _mart_row(conn, 2)
    for key, col in (
        ("user", "total_user_messages"),
        ("assistant", "total_assistant_messages"),
        ("tool_use", "total_tool_use_messages"),
        ("tool_result", "total_tool_result_messages"),
        ("commands", "total_commands"),
    ):
        assert r1[col] + r2[col] == expected[key], key
    conn.close()


def test_incremental_refresh_matches_rebuild_dims(tmp_path):
    """A watermarked incremental refresh produces the same dims as a rebuild."""
    conn = _connect(tmp_path)
    _seed_project(conn, pid=1, provider="claude", slug="alpha", sid=1,
                  session_id="s1", base_eid=1)
    conn.commit()

    b = ProjectMartBuilder()
    b.refresh(conn, since_event_id=0)          # incremental from empty watermark
    inc = _mart_row(conn, 1)
    b.rebuild_from_scratch(conn)               # full rebuild
    full = _mart_row(conn, 1)

    dim_cols = (
        "total_user_messages", "total_assistant_messages",
        "total_tool_use_messages", "total_tool_result_messages", "total_commands",
    )
    assert {c: inc[c] for c in dim_cols} == {c: full[c] for c in dim_cols}
    conn.close()


@pytest.mark.parametrize("interrupt", [
    "[Request interrupted by user for tool use]",
    "API Error: Request was aborted.",
])
def test_interruptions_excluded_from_commands(tmp_path, interrupt):
    """Both interruption markers are excluded from the command tally."""
    conn = _connect(tmp_path)
    _insert_project(conn, pid=1, provider="claude", slug="alpha")
    _insert_session(conn, sid=1, pid=1, session_id="s1")
    _insert_message(conn, mid=1, session_fk=1, seq=0,
                    ts="2026-04-01T00:00:00+00:00", role="user",
                    payload=_user("real command"))
    _insert_message(conn, mid=2, session_fk=1, seq=1,
                    ts="2026-04-02T00:00:00+00:00", role="user",
                    payload=_user(interrupt))
    _insert_message(conn, mid=3, session_fk=1, seq=2,
                    ts="2026-04-03T00:00:00+00:00", role="assistant",
                    payload=_assistant([{"type": "text", "text": "hi"}]))
    _insert_assistant_event(conn, eid=1, mid=3, pid=1, provider="claude",
                            session_id="s1", ts="2026-04-03T00:00:00+00:00")
    conn.commit()

    ProjectMartBuilder().rebuild_from_scratch(conn)
    _, stats = queries.get_project_stats(conn, project_id=1)
    row = _mart_row(conn, 1)

    assert row["total_commands"] == 1
    assert row["total_commands"] == stats["user_interactions"]["user_commands_analyzed"]
    assert row["total_user_messages"] == 2  # both user turns counted as 'user'
    conn.close()
