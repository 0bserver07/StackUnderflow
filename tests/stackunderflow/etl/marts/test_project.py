"""ProjectMartBuilder — per-project lifetime totals."""

from __future__ import annotations

from stackunderflow.etl.marts.project import ProjectMartBuilder

from .conftest import insert_event


def test_empty(conn) -> None:
    new = ProjectMartBuilder().refresh(conn, since_event_id=0)
    assert new == 0
    assert conn.execute("SELECT COUNT(*) AS n FROM project_mart").fetchone()["n"] == 0


def test_per_project_totals(conn) -> None:
    insert_event(conn, event_id=1, project_id=1, session_id="sess-1",
                 input_tokens=100, output_tokens=50, cost_usd=0.01)
    insert_event(conn, event_id=2, project_id=1, session_id="sess-1",
                 input_tokens=200, output_tokens=100, cost_usd=0.02)
    insert_event(conn, event_id=3, project_id=2, session_id="sess-2",
                 provider="codex", input_tokens=400, cost_usd=0.10)
    ProjectMartBuilder().refresh(conn, since_event_id=0)
    rows = {r["project_id"]: dict(r) for r in conn.execute(
        "SELECT * FROM project_mart"
    )}
    assert rows[1]["total_input_tokens"] == 300
    assert rows[1]["total_output_tokens"] == 150
    assert rows[1]["total_messages"] == 2
    assert rows[1]["total_sessions"] == 1
    assert abs(rows[1]["total_cost_usd"] - 0.03) < 1e-9
    assert rows[1]["display_name"] == "Alpha"
    assert rows[1]["slug"] == "alpha"
    assert rows[1]["provider"] == "claude"
    assert rows[2]["provider"] == "codex"


def test_total_sessions_distinct(conn) -> None:
    """total_sessions is COUNT(DISTINCT session_id) per project."""
    insert_event(conn, event_id=1, project_id=1, session_id="sess-1")
    insert_event(conn, event_id=2, project_id=1, session_id="sess-1")
    insert_event(conn, event_id=3, project_id=1, session_id="sess-X")
    # sess-X uses session_fk=1 (same project) by virtue of the helper
    # — that's fine, the mart only reads from usage_events.
    ProjectMartBuilder().refresh(conn, since_event_id=0)
    row = conn.execute("SELECT * FROM project_mart").fetchone()
    assert row["total_sessions"] == 2
    assert row["total_messages"] == 3


def test_stale_project_row_replaced(conn) -> None:
    """New events for an existing project replace the row, not increment."""
    insert_event(conn, event_id=1, project_id=1, session_id="sess-1",
                 input_tokens=100, cost_usd=0.01)
    b = ProjectMartBuilder()
    w1 = b.refresh(conn, since_event_id=0)
    insert_event(conn, event_id=2, project_id=1, session_id="sess-1",
                 input_tokens=300, cost_usd=0.05)
    b.refresh(conn, since_event_id=w1)
    row = conn.execute("SELECT * FROM project_mart").fetchone()
    # Replace, not increment — the row reflects ALL events, not just w2.
    assert row["total_input_tokens"] == 400
    assert abs(row["total_cost_usd"] - 0.06) < 1e-9
    assert row["total_messages"] == 2


def test_rebuild_matches_incremental(conn) -> None:
    insert_event(conn, event_id=1, project_id=1)
    insert_event(conn, event_id=2, project_id=2, session_id="sess-2",
                 provider="codex")
    b = ProjectMartBuilder()
    b.refresh(conn, since_event_id=0)
    inc = sorted(
        tuple(dict(r).items())
        for r in conn.execute(
            "SELECT * FROM project_mart ORDER BY project_id"
        ).fetchall()
    )
    b.rebuild_from_scratch(conn)
    out = sorted(
        tuple(dict(r).items())
        for r in conn.execute(
            "SELECT * FROM project_mart ORDER BY project_id"
        ).fetchall()
    )
    assert inc == out
