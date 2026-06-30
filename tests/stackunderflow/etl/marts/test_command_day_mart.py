"""command_day_mart (v025) — per-(day, project_id) user-command count.

The windowed-Commands-KPI source (ui-perf-audit #25). Locks in the contract
that ``CommandMartBuilder`` materialises a per-day count of *real user command*
turns (kind ``user``, not a tool_result, not an interruption) that:

* reconciles EXACTLY with ``project_mart.total_commands`` when summed over all
  days (the same classifier rule the v022 project-mart dims use), and
* is idempotent / replace-correct across incremental refresh windows.

These build a synthetic store with real ``messages.raw_json`` and run the mart
refresh, mirroring the v022 ``test_project_message_dims`` equivalence tests.
"""

from __future__ import annotations

import json
from pathlib import Path

from stackunderflow.etl.marts.command import CommandMartBuilder
from stackunderflow.etl.marts.project import ProjectMartBuilder
from stackunderflow.store import db, schema


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


def _insert_assistant_event(conn, *, eid: int, mid: int, pid: int, session_id: str, ts: str) -> None:
    conn.execute(
        "INSERT INTO usage_events (id, source_message_fk, provider, project_id, "
        "session_id, ts, day, model, speed, input_tokens, output_tokens, "
        "cache_read_tokens, cache_create_tokens, cost_usd, cost_source, role) "
        "VALUES (?, ?, 'claude', ?, ?, ?, ?, 'm', 'standard', 10, 5, 0, 0, 0.0, "
        "'rate_card', 'assistant')",
        (eid, mid, pid, session_id, ts, ts[:10]),
    )


def _user(content) -> dict:
    return {"type": "user", "message": {"role": "user", "content": content}}


def _assistant(content) -> dict:
    return {
        "type": "assistant",
        "message": {"role": "assistant", "model": "m", "content": content,
                    "usage": {"input_tokens": 10, "output_tokens": 5}},
    }


def _seed_two_day_project(conn, *, pid: int = 1, sid: int = 1) -> None:
    """2 commands on 2026-04-01, 1 on 2026-04-03; a tool_result + interruption
    are present but must NOT count.

    Day 01: "/init" (command) + assistant + "do X" (command) + assistant
    Day 02: tool_result user turn (NOT a command)
    Day 03: interruption user turn (NOT a command) + "fix it" (command) + assistant
    """
    _insert_project(conn, pid=pid, provider="claude", slug="alpha")
    _insert_session(conn, sid=sid, pid=pid, session_id="s1")
    rows = [
        ("2026-04-01T01:00:00+00:00", "user", _user("/init")),                 # cmd (day 01)
        ("2026-04-01T01:00:01+00:00", "assistant", _assistant([{"type": "text", "text": "ok"}])),
        ("2026-04-01T02:00:00+00:00", "user", _user("do X")),                  # cmd (day 01)
        ("2026-04-01T02:00:01+00:00", "assistant", _assistant([{"type": "text", "text": "ok"}])),
        ("2026-04-02T01:00:00+00:00", "user", _user([                          # tool_result (NOT cmd)
            {"type": "tool_result", "tool_use_id": "t1", "content": "data"}])),
        ("2026-04-03T01:00:00+00:00", "user", _user("[Request interrupted by user for tool use]")),  # NOT cmd
        ("2026-04-03T02:00:00+00:00", "user", _user("fix it")),                # cmd (day 03)
        ("2026-04-03T02:00:01+00:00", "assistant", _assistant([{"type": "text", "text": "done"}])),
    ]
    eid = 1
    for i, (ts, role, payload) in enumerate(rows):
        mid = pid * 100 + i
        _insert_message(conn, mid=mid, session_fk=sid, seq=i, ts=ts, role=role, payload=payload)
        if role == "assistant":
            _insert_assistant_event(conn, eid=eid, mid=mid, pid=pid, session_id="s1", ts=ts)
            eid += 1
    conn.commit()


def _day_counts(conn, pid: int) -> dict[str, int]:
    return {
        r["day"]: int(r["command_count"])
        for r in conn.execute(
            "SELECT day, command_count FROM command_day_mart WHERE project_id = ? ORDER BY day",
            (pid,),
        ).fetchall()
    }


# ── tests ────────────────────────────────────────────────────────────────────


def test_per_day_counts_exclude_tool_results_and_interruptions(tmp_path):
    conn = _connect(tmp_path)
    _seed_two_day_project(conn)
    CommandMartBuilder().rebuild_from_scratch(conn)

    counts = _day_counts(conn, 1)
    # 2 commands on day 01, none on day 02 (only a tool_result) or for the
    # interruption on day 03, 1 command ("fix it") on day 03.
    assert counts == {"2026-04-01": 2, "2026-04-03": 1}
    conn.close()


def test_sum_reconciles_with_project_mart_total_commands(tmp_path):
    """SUM(command_day_mart.command_count) == project_mart.total_commands."""
    conn = _connect(tmp_path)
    _seed_two_day_project(conn)
    ProjectMartBuilder().rebuild_from_scratch(conn)
    CommandMartBuilder().rebuild_from_scratch(conn)

    total_commands = conn.execute(
        "SELECT total_commands FROM project_mart WHERE project_id = 1"
    ).fetchone()[0]
    day_sum = conn.execute(
        "SELECT COALESCE(SUM(command_count), 0) FROM command_day_mart WHERE project_id = 1"
    ).fetchone()[0]
    assert total_commands == 3
    assert day_sum == total_commands
    conn.close()


def test_incremental_refresh_matches_rebuild(tmp_path):
    """A watermarked incremental refresh produces the same per-day counts."""
    conn = _connect(tmp_path)
    _seed_two_day_project(conn)

    b = CommandMartBuilder()
    b.refresh(conn, since_event_id=0)
    inc = _day_counts(conn, 1)
    b.rebuild_from_scratch(conn)
    full = _day_counts(conn, 1)
    assert inc == full
    conn.close()


def test_rebuild_from_scratch_clears_both_tables(tmp_path):
    """rebuild_from_scratch wipes command_day_mart too (no stale rows)."""
    conn = _connect(tmp_path)
    _seed_two_day_project(conn)
    CommandMartBuilder().rebuild_from_scratch(conn)
    # Inject a stale row that a fresh rebuild must remove.
    conn.execute(
        "INSERT INTO command_day_mart (day, project_id, command_count) VALUES "
        "('2099-01-01', 1, 999)"
    )
    conn.commit()
    CommandMartBuilder().rebuild_from_scratch(conn)
    counts = _day_counts(conn, 1)
    assert "2099-01-01" not in counts
    assert counts == {"2026-04-01": 2, "2026-04-03": 1}
    conn.close()


def test_empty_store_leaves_command_day_mart_empty(tmp_path):
    conn = _connect(tmp_path)
    CommandMartBuilder().rebuild_from_scratch(conn)
    assert conn.execute("SELECT COUNT(*) FROM command_day_mart").fetchone()[0] == 0
    conn.close()
