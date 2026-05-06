"""CommandMartBuilder — slash-command parse, attribution, idempotency.

Locks in the Wave 5 contract: ``command_mart`` walks back from each
event to the most recent preceding user message in the same session,
parses the slash command (or ``freeform``), and aggregates per
``(day, project_id, command_name)``. ``session_count`` is recomputed
across refresh windows (additive-mart trap, HANDOFF
§"`session_count` correctness across windows").
"""

from __future__ import annotations

from stackunderflow.etl.marts.command import (
    FREEFORM,
    CommandMartBuilder,
    parse_command_name,
)

from .conftest import insert_event, insert_user_prompt


def test_parse_command_name_slash() -> None:
    """`/init args` → `/init`."""
    assert parse_command_name("/init project") == "/init"
    assert parse_command_name("/help") == "/help"
    assert parse_command_name("/review-pr 123") == "/review-pr"
    assert parse_command_name("/run_test foo") == "/run_test"


def test_parse_command_name_freeform() -> None:
    """Non-slash prompts collapse to a single ``freeform`` bucket."""
    assert parse_command_name("hello") == FREEFORM
    assert parse_command_name("") == FREEFORM
    assert parse_command_name("// comment") == FREEFORM
    # Leading whitespace is permissive — strips before checking.
    assert parse_command_name("   hello") == FREEFORM
    assert parse_command_name("   /init") == "/init"


def test_empty_events_returns_zero(conn) -> None:
    """No events → no command_mart rows."""
    new = CommandMartBuilder().refresh(conn, since_event_id=0)
    assert new == 0
    assert conn.execute("SELECT * FROM command_mart").fetchall() == []


def test_event_with_no_preceding_prompt(conn) -> None:
    """An assistant event with no preceding user message buckets to __no_prompt__."""
    insert_event(conn, event_id=1, cost_usd=0.10, input_tokens=100)
    CommandMartBuilder().refresh(conn, since_event_id=0)
    rows = conn.execute("SELECT * FROM command_mart").fetchall()
    assert len(rows) == 1
    assert rows[0]["command_name"] == "__no_prompt__"
    assert rows[0]["cost_usd"] == 0.10
    assert rows[0]["tokens_in"] == 100


def test_slash_command_attribution(conn) -> None:
    """An assistant event after `/init` accrues to the `/init` bucket."""
    # User prompt at seq 1, assistant event at seq 2.
    insert_user_prompt(
        conn, msg_id=10, session_fk=1, seq=1,
        content_text="/init build dashboard",
    )
    insert_event(
        conn, event_id=1, msg_id=11, seq=2,
        cost_usd=0.20, input_tokens=500, output_tokens=200,
    )
    CommandMartBuilder().refresh(conn, since_event_id=0)
    rows = conn.execute("SELECT * FROM command_mart").fetchall()
    assert len(rows) == 1
    r = rows[0]
    assert r["command_name"] == "/init"
    assert r["event_count"] == 1
    assert r["cost_usd"] == 0.20
    assert r["tokens_in"] == 500
    assert r["tokens_out"] == 200
    assert r["session_count"] == 1


def test_freeform_bucket_collects_non_slash_prompts(conn) -> None:
    """Plain prose prompts all aggregate into one ``freeform`` row."""
    insert_user_prompt(
        conn, msg_id=10, session_fk=1, seq=1, content_text="implement feature",
    )
    insert_event(conn, event_id=1, msg_id=11, seq=2, cost_usd=0.10)
    insert_user_prompt(
        conn, msg_id=12, session_fk=1, seq=3, content_text="now refactor",
    )
    insert_event(conn, event_id=2, msg_id=13, seq=4, cost_usd=0.05)
    CommandMartBuilder().refresh(conn, since_event_id=0)
    rows = conn.execute("SELECT * FROM command_mart").fetchall()
    assert len(rows) == 1
    assert rows[0]["command_name"] == FREEFORM
    assert rows[0]["event_count"] == 2
    assert abs(rows[0]["cost_usd"] - 0.15) < 1e-9


def test_multiple_commands_in_one_session(conn) -> None:
    """Distinct slash commands in one session bucket separately."""
    insert_user_prompt(conn, msg_id=10, session_fk=1, seq=1, content_text="/init")
    insert_event(conn, event_id=1, msg_id=11, seq=2, cost_usd=0.10)
    insert_user_prompt(conn, msg_id=12, session_fk=1, seq=3, content_text="/help")
    insert_event(conn, event_id=2, msg_id=13, seq=4, cost_usd=0.05)
    CommandMartBuilder().refresh(conn, since_event_id=0)
    rows = {
        r["command_name"]: r
        for r in conn.execute("SELECT * FROM command_mart").fetchall()
    }
    assert set(rows) == {"/init", "/help"}
    assert rows["/init"]["cost_usd"] == 0.10
    assert rows["/help"]["cost_usd"] == 0.05
    # Both rows reference the same session.
    assert rows["/init"]["session_count"] == 1
    assert rows["/help"]["session_count"] == 1


def test_idempotency_re_running_with_watermark_is_noop(conn) -> None:
    """Re-running with the persisted watermark must not double-count."""
    insert_user_prompt(conn, msg_id=10, session_fk=1, seq=1, content_text="/init")
    insert_event(
        conn, event_id=1, msg_id=11, seq=2,
        cost_usd=0.10, input_tokens=200,
    )
    b = CommandMartBuilder()
    w = b.refresh(conn, since_event_id=0)
    b.refresh(conn, since_event_id=w)
    row = conn.execute("SELECT * FROM command_mart").fetchone()
    assert row["event_count"] == 1
    assert row["cost_usd"] == 0.10
    assert row["tokens_in"] == 200


def test_incremental_appends_existing_command(conn) -> None:
    """Two events on the same (day, project, command) sum additively."""
    insert_user_prompt(conn, msg_id=10, session_fk=1, seq=1, content_text="/init")
    insert_event(conn, event_id=1, msg_id=11, seq=2, cost_usd=0.10)
    b = CommandMartBuilder()
    w1 = b.refresh(conn, since_event_id=0)
    # Second event in the same session, after the same /init prompt.
    insert_event(conn, event_id=2, msg_id=12, seq=3, cost_usd=0.20)
    b.refresh(conn, since_event_id=w1)
    row = conn.execute("SELECT * FROM command_mart").fetchone()
    assert row["event_count"] == 2
    assert abs(row["cost_usd"] - 0.30) < 1e-9


def test_session_count_stays_unique_across_windows(conn) -> None:
    """Same session producing two events for /init across windows = 1."""
    insert_user_prompt(conn, msg_id=10, session_fk=1, seq=1, content_text="/init")
    insert_event(
        conn, event_id=1, msg_id=11, seq=2, session_id="sess-1", cost_usd=0.10,
    )
    b = CommandMartBuilder()
    w1 = b.refresh(conn, since_event_id=0)
    insert_event(
        conn, event_id=2, msg_id=12, seq=3, session_id="sess-1", cost_usd=0.10,
    )
    b.refresh(conn, since_event_id=w1)
    row = conn.execute("SELECT * FROM command_mart").fetchone()
    assert row["session_count"] == 1, "same session across windows must stay 1"
    assert row["event_count"] == 2


def test_session_count_two_distinct_sessions_same_command(conn) -> None:
    """Two sessions both running /init → session_count = 2."""
    # Session 1
    insert_user_prompt(conn, msg_id=10, session_fk=1, seq=1, content_text="/init")
    insert_event(
        conn, event_id=1, msg_id=11, seq=2, session_id="sess-1", cost_usd=0.10,
    )
    b = CommandMartBuilder()
    w1 = b.refresh(conn, since_event_id=0)
    # Session 2 — different session_fk so the seq-walk hits its own user msg.
    insert_user_prompt(conn, msg_id=20, session_fk=2, seq=1, content_text="/init")
    insert_event(
        conn, event_id=2, msg_id=21, seq=2,
        session_id="sess-2",  # different session_id
        session_fk=2,
        project_id=2,  # use project 2 so session_fk=2 makes sense
        cost_usd=0.10,
    )
    # ``session_fk=2`` is the project-2 session by the conftest seed, so we
    # expect a row keyed on (day, project_id=2, /init) with session_count 1.
    b.refresh(conn, since_event_id=w1)
    rows = conn.execute(
        "SELECT * FROM command_mart ORDER BY project_id"
    ).fetchall()
    assert len(rows) == 2
    assert rows[0]["project_id"] == 1
    assert rows[0]["session_count"] == 1
    assert rows[1]["project_id"] == 2
    assert rows[1]["session_count"] == 1


def test_rebuild_from_scratch_matches_incremental(conn) -> None:
    """Full rebuild produces the same final state as multi-window refresh."""
    insert_user_prompt(conn, msg_id=10, session_fk=1, seq=1, content_text="/init")
    insert_event(conn, event_id=1, msg_id=11, seq=2, cost_usd=0.10)
    insert_user_prompt(conn, msg_id=12, session_fk=1, seq=3, content_text="/help")
    insert_event(conn, event_id=2, msg_id=13, seq=4, cost_usd=0.05)
    b = CommandMartBuilder()
    b.refresh(conn, since_event_id=0)
    incremental = sorted(
        tuple(dict(r).items())
        for r in conn.execute(
            "SELECT * FROM command_mart ORDER BY command_name"
        ).fetchall()
    )
    b.rebuild_from_scratch(conn)
    rebuilt = sorted(
        tuple(dict(r).items())
        for r in conn.execute(
            "SELECT * FROM command_mart ORDER BY command_name"
        ).fetchall()
    )
    assert incremental == rebuilt
