"""ToolMartBuilder — incremental + idempotent + 1/N attribution.

Locks in the Wave 5 contract: ``tool_mart`` fans each event out across
its ``messages.tools_json`` distinct names, attributes 1/N of cost +
tokens to each, and tracks ``session_count`` correctly across refresh
windows (the additive-mart trap from HANDOFF §"`session_count`
correctness across windows").
"""

from __future__ import annotations

import json

from stackunderflow.etl.marts import tool as tool_mod
from stackunderflow.etl.marts.tool import ToolMartBuilder

from .conftest import insert_event


def test_empty_events_returns_zero(conn) -> None:
    """No events → no tool_mart rows, refresh returns 0."""
    new = ToolMartBuilder().refresh(conn, since_event_id=0)
    assert new == 0
    assert conn.execute("SELECT * FROM tool_mart").fetchall() == []


def test_no_tools_event_creates_no_rows(conn) -> None:
    """An event whose source message has empty tools_json contributes nothing."""
    insert_event(conn, event_id=1, cost_usd=0.10)  # tools_json default '[]'
    ToolMartBuilder().refresh(conn, since_event_id=0)
    assert conn.execute("SELECT * FROM tool_mart").fetchall() == []


def test_single_tool_full_attribution(conn) -> None:
    """One tool in tools_json gets 100% of the event's cost + tokens."""
    insert_event(
        conn, event_id=1, cost_usd=0.20,
        input_tokens=1000, output_tokens=500,
        tools_json=json.dumps(["Read"]),
    )
    ToolMartBuilder().refresh(conn, since_event_id=0)
    rows = conn.execute("SELECT * FROM tool_mart").fetchall()
    assert len(rows) == 1
    r = rows[0]
    assert r["tool_name"] == "Read"
    assert r["event_count"] == 1
    assert r["cost_usd"] == 0.20
    assert r["tokens_in"] == 1000
    assert r["tokens_out"] == 500
    assert r["session_count"] == 1


def test_multi_tool_one_over_n_attribution(conn) -> None:
    """Two distinct tools in one event split cost + tokens 50/50."""
    insert_event(
        conn, event_id=1, cost_usd=0.30,
        input_tokens=1000, output_tokens=400,
        tools_json=json.dumps(["Read", "Edit"]),
    )
    ToolMartBuilder().refresh(conn, since_event_id=0)
    rows = {r["tool_name"]: r for r in conn.execute("SELECT * FROM tool_mart").fetchall()}
    assert set(rows) == {"Read", "Edit"}
    for name in ("Read", "Edit"):
        assert rows[name]["event_count"] == 1
        assert abs(rows[name]["cost_usd"] - 0.15) < 1e-9
        assert rows[name]["tokens_in"] == 500
        assert rows[name]["tokens_out"] == 200


def test_duplicate_tool_in_one_message_collapses(conn) -> None:
    """Read called twice in one turn = one Read bucket (per aggregator §1.3)."""
    insert_event(
        conn, event_id=1, cost_usd=0.10,
        tools_json=json.dumps(["Read", "Read", "Read"]),
    )
    ToolMartBuilder().refresh(conn, since_event_id=0)
    rows = conn.execute("SELECT * FROM tool_mart").fetchall()
    assert len(rows) == 1
    assert rows[0]["tool_name"] == "Read"
    # event_count is 1, not 3 — distinct names is what we count.
    assert rows[0]["event_count"] == 1
    # calls_total IS 3 — total occurrences, not distinct (v012).
    assert rows[0]["calls_total"] == 3
    assert rows[0]["cost_usd"] == 0.10


# ── calls_total — non-distinct occurrence count (v012) ─────────────────────


def test_calls_total_single_tool_single_occurrence(conn) -> None:
    """One tool, one occurrence → event_count == calls_total == 1."""
    insert_event(
        conn, event_id=1, cost_usd=0.10,
        tools_json=json.dumps(["Read"]),
    )
    ToolMartBuilder().refresh(conn, since_event_id=0)
    row = conn.execute("SELECT * FROM tool_mart").fetchone()
    assert row["event_count"] == 1
    assert row["calls_total"] == 1


def test_calls_total_distinct_vs_total_parity(conn) -> None:
    """``["Read", "Read", "Edit"]`` → Read 1/2, Edit 1/1 (event/calls)."""
    insert_event(
        conn, event_id=1, cost_usd=0.30,
        tools_json=json.dumps(["Read", "Read", "Edit"]),
    )
    ToolMartBuilder().refresh(conn, since_event_id=0)
    rows = {r["tool_name"]: r for r in conn.execute("SELECT * FROM tool_mart").fetchall()}
    assert rows["Read"]["event_count"] == 1
    assert rows["Read"]["calls_total"] == 2
    assert rows["Edit"]["event_count"] == 1
    assert rows["Edit"]["calls_total"] == 1
    # Cost still splits 1/N over the *distinct* tools (2 here) — a repeated
    # call must not double cost.
    assert abs(rows["Read"]["cost_usd"] - 0.15) < 1e-9
    assert abs(rows["Edit"]["cost_usd"] - 0.15) < 1e-9


def test_calls_total_idempotent_across_windows(conn) -> None:
    """Re-running refresh with the persisted watermark leaves calls_total stable."""
    insert_event(
        conn, event_id=1, cost_usd=0.10,
        tools_json=json.dumps(["Read", "Read"]),
    )
    b = ToolMartBuilder()
    w = b.refresh(conn, since_event_id=0)
    b.refresh(conn, since_event_id=w)  # idempotent re-run
    row = conn.execute("SELECT * FROM tool_mart").fetchone()
    assert row["event_count"] == 1
    assert row["calls_total"] == 2


def test_calls_total_accumulates_additively(conn) -> None:
    """Two events on the same key sum calls_total additively."""
    insert_event(
        conn, event_id=1, cost_usd=0.10,
        tools_json=json.dumps(["Read", "Read"]),  # +2 calls
    )
    b = ToolMartBuilder()
    w1 = b.refresh(conn, since_event_id=0)
    insert_event(
        conn, event_id=2, cost_usd=0.20,
        tools_json=json.dumps(["Read", "Read", "Read"]),  # +3 calls
    )
    b.refresh(conn, since_event_id=w1)
    row = conn.execute("SELECT * FROM tool_mart").fetchone()
    assert row["event_count"] == 2
    assert row["calls_total"] == 5  # 2 + 3


def test_idempotency_re_running_with_watermark_is_noop(conn) -> None:
    """Re-running with the persisted watermark must not double-count."""
    insert_event(
        conn, event_id=1, cost_usd=0.10,
        input_tokens=200,
        tools_json=json.dumps(["Read"]),
    )
    b = ToolMartBuilder()
    w = b.refresh(conn, since_event_id=0)
    # Pretend the watermark advanced; re-running with it must be idempotent.
    b.refresh(conn, since_event_id=w)
    row = conn.execute("SELECT * FROM tool_mart").fetchone()
    assert row["event_count"] == 1
    assert row["cost_usd"] == 0.10
    assert row["tokens_in"] == 200


def test_incremental_appends_existing_key(conn) -> None:
    """Two events on the same (day, project, provider, tool) sum additively."""
    insert_event(
        conn, event_id=1, cost_usd=0.10, input_tokens=100,
        tools_json=json.dumps(["Read"]),
    )
    b = ToolMartBuilder()
    w1 = b.refresh(conn, since_event_id=0)
    insert_event(
        conn, event_id=2, cost_usd=0.20, input_tokens=300,
        tools_json=json.dumps(["Read"]),
    )
    b.refresh(conn, since_event_id=w1)
    row = conn.execute("SELECT * FROM tool_mart").fetchone()
    assert row["event_count"] == 2
    assert abs(row["cost_usd"] - 0.30) < 1e-9
    assert row["tokens_in"] == 400


def test_session_count_stays_unique_across_windows(conn) -> None:
    """COUNT(DISTINCT session_id) must be recomputed, not summed.

    Without recompute, the same session producing Read events on the
    same day in two refresh windows would count as 2.
    """
    insert_event(
        conn, event_id=1, session_id="sess-1",
        tools_json=json.dumps(["Read"]),
    )
    b = ToolMartBuilder()
    w1 = b.refresh(conn, since_event_id=0)
    insert_event(
        conn, event_id=2, session_id="sess-1",
        tools_json=json.dumps(["Read"]),
    )
    b.refresh(conn, since_event_id=w1)
    row = conn.execute("SELECT * FROM tool_mart").fetchone()
    assert row["session_count"] == 1, "same session across windows must stay 1"
    assert row["event_count"] == 2


def test_session_count_two_distinct_sessions_across_windows(conn) -> None:
    """Two sessions on the same (day, tool) → session_count == 2."""
    insert_event(
        conn, event_id=1, session_id="sess-1",
        tools_json=json.dumps(["Read"]),
    )
    b = ToolMartBuilder()
    w1 = b.refresh(conn, since_event_id=0)
    insert_event(
        conn, event_id=2, session_id="sess-2",
        tools_json=json.dumps(["Read"]),
    )
    b.refresh(conn, since_event_id=w1)
    row = conn.execute("SELECT * FROM tool_mart").fetchone()
    assert row["session_count"] == 2


def test_rebuild_from_scratch_matches_incremental(conn) -> None:
    """Full rebuild produces the same final state as a multi-window refresh."""
    insert_event(
        conn, event_id=1, cost_usd=0.10,
        tools_json=json.dumps(["Read", "Edit"]),
    )
    insert_event(
        conn, event_id=2, cost_usd=0.20,
        tools_json=json.dumps(["Bash"]),
    )
    b = ToolMartBuilder()
    b.refresh(conn, since_event_id=0)
    incremental = sorted(
        tuple(dict(r).items())
        for r in conn.execute("SELECT * FROM tool_mart ORDER BY tool_name").fetchall()
    )
    b.rebuild_from_scratch(conn)
    rebuilt = sorted(
        tuple(dict(r).items())
        for r in conn.execute("SELECT * FROM tool_mart ORDER BY tool_name").fetchall()
    )
    assert incremental == rebuilt


def test_malformed_tools_json_does_not_crash(conn) -> None:
    """A broken tools_json is silently skipped, not raised."""
    insert_event(conn, event_id=1, cost_usd=0.10, tools_json="not json{")
    insert_event(
        conn, event_id=2, cost_usd=0.20,
        tools_json=json.dumps(["Read"]),
    )
    ToolMartBuilder().refresh(conn, since_event_id=0)
    rows = conn.execute("SELECT * FROM tool_mart").fetchall()
    # Only the well-formed event contributes a row.
    assert len(rows) == 1
    assert rows[0]["tool_name"] == "Read"


def test_separate_keys_create_separate_rows(conn) -> None:
    """Different (day, project, provider, tool) combos produce distinct rows."""
    insert_event(
        conn, event_id=1, project_id=1, provider="claude",
        tools_json=json.dumps(["Read"]),
    )
    insert_event(
        conn, event_id=2, project_id=2, provider="codex",
        session_id="sess-2",
        tools_json=json.dumps(["Read"]),
    )
    insert_event(
        conn, event_id=3, day="2024-01-02",
        tools_json=json.dumps(["Read"]),
    )
    ToolMartBuilder().refresh(conn, since_event_id=0)
    n = conn.execute("SELECT COUNT(*) AS n FROM tool_mart").fetchone()["n"]
    assert n == 3


def test_recompute_bounded_by_distinct_groups_not_window_events(conn) -> None:
    """Regression: the per-group recompute scan must dedup tool keys.

    A watermark window that touches K tool-keys all in the same
    ``(day, project_id, provider)`` group must run exactly **one**
    per-group SQL scan, not K. Documents the real cost shape called
    out in ``_recompute_session_counts``: bounded by distinct
    *groups* touched, not by ``len(keys)`` (which is what the prior
    docstring misleadingly claimed).
    """
    # Seed 1000 events on day 2024-01-01 / project 1 / provider claude,
    # cycling through 10 distinct tool names. All in the SAME
    # (day, project, provider) group — so 10 distinct tool-keys but
    # one underlying group.
    tool_names = [f"Tool{i}" for i in range(10)]
    for event_id in range(1, 1001):
        insert_event(
            conn,
            event_id=event_id,
            session_id="sess-1",
            cost_usd=0.001,
            input_tokens=1, output_tokens=1,
            tools_json=json.dumps([tool_names[event_id % 10]]),
        )
    b = ToolMartBuilder()
    w1 = b.refresh(conn, since_event_id=0)
    assert w1 == 1000

    # Insert a small new window of events on the same group — touches
    # all 10 tool keys but still only one (day, project, provider) group.
    for event_id in range(1001, 1011):
        insert_event(
            conn,
            event_id=event_id,
            session_id="sess-1",
            cost_usd=0.001,
            input_tokens=1, output_tokens=1,
            tools_json=json.dumps([tool_names[event_id % 10]]),
        )

    # Reset counter and run the incremental refresh.
    tool_mod._session_count_recompute_calls = 0
    w2 = b.refresh(conn, since_event_id=w1)
    assert w2 == 1010

    # Window touched 10 distinct tool keys, but they all share one
    # (day, project_id, provider) group → exactly one group scan.
    assert tool_mod._session_count_recompute_calls == 1, (
        f"Expected 1 per-group recompute call, got "
        f"{tool_mod._session_count_recompute_calls}. The recompute "
        f"must dedup tool keys to their (day, project, provider) "
        f"group, not run once per touched tool key."
    )


def test_recompute_call_count_matches_distinct_groups(conn) -> None:
    """Two distinct groups touched in one window → exactly two calls."""
    # Group A: (2024-01-01, project=1, provider=claude)
    insert_event(
        conn, event_id=1, project_id=1, provider="claude",
        day="2024-01-01", session_id="sess-1",
        tools_json=json.dumps(["Read", "Edit"]),
    )
    # Group B: (2024-01-02, project=1, provider=claude) — different day
    insert_event(
        conn, event_id=2, project_id=1, provider="claude",
        day="2024-01-02", session_id="sess-1",
        tools_json=json.dumps(["Bash"]),
    )

    tool_mod._session_count_recompute_calls = 0
    ToolMartBuilder().refresh(conn, since_event_id=0)

    # Window touched 3 distinct tool keys across 2 groups → 2 scans.
    assert tool_mod._session_count_recompute_calls == 2
