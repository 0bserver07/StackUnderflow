"""SessionMartBuilder — per-session lifetime aggregates.

Replace-from-scratch-for-affected-keys: a new event for an existing
session must invalidate the prior aggregate and recompute the row from
all of its events.
"""

from __future__ import annotations

from stackunderflow.etl.marts.session import SessionMartBuilder

from .conftest import insert_event


def test_empty(conn) -> None:
    new = SessionMartBuilder().refresh(conn, since_event_id=0)
    assert new == 0
    assert conn.execute("SELECT COUNT(*) AS n FROM session_mart").fetchone()["n"] == 0


def test_one_session_one_event(conn) -> None:
    insert_event(conn, event_id=1, session_id="sess-1",
                 input_tokens=100, output_tokens=50, cost_usd=0.05,
                 role="assistant", ts="2024-01-01T00:00:00Z")
    SessionMartBuilder().refresh(conn, since_event_id=0)
    row = conn.execute("SELECT * FROM session_mart").fetchone()
    assert row["session_id"] == "sess-1"
    assert row["input_tokens"] == 100
    assert row["output_tokens"] == 50
    assert abs(row["cost_usd"] - 0.05) < 1e-9
    assert row["message_count"] == 1
    assert row["assistant_message_count"] == 1
    assert row["user_message_count"] == 0
    assert row["is_one_shot"] == 0  # need 1 user + 1 assistant
    assert row["primary_model"] == "sonnet"


def test_one_shot_flag(conn) -> None:
    insert_event(conn, event_id=1, session_id="sess-1", role="user",
                 ts="2024-01-01T00:00:00Z")
    insert_event(conn, event_id=2, session_id="sess-1", role="assistant",
                 ts="2024-01-01T00:00:01Z")
    SessionMartBuilder().refresh(conn, since_event_id=0)
    row = conn.execute("SELECT * FROM session_mart").fetchone()
    assert row["is_one_shot"] == 1
    assert row["user_message_count"] == 1
    assert row["assistant_message_count"] == 1


def test_primary_model_picks_majority(conn) -> None:
    """primary_model = the assistant model with the most messages."""
    insert_event(conn, event_id=1, session_id="sess-1", role="assistant",
                 model="sonnet")
    insert_event(conn, event_id=2, session_id="sess-1", role="assistant",
                 model="sonnet")
    insert_event(conn, event_id=3, session_id="sess-1", role="assistant",
                 model="opus")
    SessionMartBuilder().refresh(conn, since_event_id=0)
    row = conn.execute("SELECT * FROM session_mart").fetchone()
    assert row["primary_model"] == "sonnet"


def test_stale_row_replaced_when_new_events_arrive(conn) -> None:
    """A new event for an existing session forces full recomputation.

    Without replace-from-scratch, ``message_count`` and ``cost_usd``
    would drift from reality the moment a session got more events.
    """
    insert_event(conn, event_id=1, session_id="sess-1",
                 input_tokens=100, cost_usd=0.01, role="assistant")
    b = SessionMartBuilder()
    w1 = b.refresh(conn, since_event_id=0)
    row = conn.execute("SELECT * FROM session_mart").fetchone()
    assert row["message_count"] == 1
    assert row["input_tokens"] == 100

    # New event arrives for sess-1 — the row must be recomputed, not
    # incrementally added (the per-session aggregates aren't summable
    # because is_one_shot, primary_model, first_ts, last_ts depend on
    # the full history).
    insert_event(conn, event_id=2, session_id="sess-1",
                 input_tokens=300, cost_usd=0.05, role="assistant",
                 ts="2024-01-01T00:05:00Z")
    b.refresh(conn, since_event_id=w1)
    row = conn.execute("SELECT * FROM session_mart").fetchone()
    assert row["message_count"] == 2
    assert row["input_tokens"] == 400
    assert abs(row["cost_usd"] - 0.06) < 1e-9
    assert row["last_ts"] == "2024-01-01T00:05:00Z"


def test_two_sessions(conn) -> None:
    insert_event(conn, event_id=1, session_id="sess-1", project_id=1)
    insert_event(conn, event_id=2, session_id="sess-2", project_id=2,
                 provider="codex")
    SessionMartBuilder().refresh(conn, since_event_id=0)
    rows = {r["session_id"]: dict(r) for r in conn.execute(
        "SELECT * FROM session_mart"
    )}
    assert set(rows) == {"sess-1", "sess-2"}
    assert rows["sess-1"]["provider"] == "claude"
    assert rows["sess-2"]["provider"] == "codex"


def test_rebuild_matches_incremental(conn) -> None:
    insert_event(conn, event_id=1, session_id="sess-1", role="user")
    insert_event(conn, event_id=2, session_id="sess-1", role="assistant")
    insert_event(conn, event_id=3, session_id="sess-2", project_id=2,
                 role="assistant")
    b = SessionMartBuilder()
    b.refresh(conn, since_event_id=0)
    inc = sorted(
        tuple(dict(r).items())
        for r in conn.execute(
            "SELECT * FROM session_mart ORDER BY session_id"
        ).fetchall()
    )
    b.rebuild_from_scratch(conn)
    out = sorted(
        tuple(dict(r).items())
        for r in conn.execute(
            "SELECT * FROM session_mart ORDER BY session_id"
        ).fetchall()
    )
    assert inc == out
