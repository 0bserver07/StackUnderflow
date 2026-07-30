"""ProjectMartBuilder — per-project lifetime totals."""

from __future__ import annotations

import json

from stackunderflow.etl.marts import project as project_mod
from stackunderflow.etl.marts.project import (
    ProjectMartBuilder,
    _seed_uncovered_projects,
)

from .conftest import insert_event, insert_message


def test_empty(conn) -> None:
    """No events → the watermark stays 0, but every project still gets a row.

    The coverage seed runs whether or not events arrived: a project that
    will never produce a ``usage_event`` (history-only sessions, a provider
    that emits none) must not be invisible to the mart-backed read paths,
    so it lands with its DEFAULT — truthfully zero — totals.
    """
    new = ProjectMartBuilder().refresh(conn, since_event_id=0)
    assert new == 0
    rows = conn.execute(
        "SELECT project_id, total_messages, total_cost_usd "
        "FROM project_mart ORDER BY project_id"
    ).fetchall()
    assert [r["project_id"] for r in rows] == [1, 2]
    assert all(r["total_messages"] == 0 for r in rows)
    assert all(r["total_cost_usd"] == 0.0 for r in rows)


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


# ── coverage seed: projects with no usage_events ─────────────────────────────
#
# The events-driven aggregate is ``FROM usage_events``, so a project with
# zero events could never win a row and stayed invisible everywhere the
# dashboard reads the mart. These pin the seed that closes that hole — and,
# just as importantly, that it stays inert once coverage is complete.


def _user_turn(conn, *, msg_id: int, session_fk: int, seq: int, text: str) -> None:
    """Insert a raw user message that the dims pass will classify as a command."""
    insert_message(
        conn,
        msg_id=msg_id,
        session_fk=session_fk,
        seq=seq,
        role="user",
        content_text=text,
        raw_json=json.dumps(
            {"type": "user", "message": {"role": "user", "content": text}}
        ),
    )


def _spy_on_dims(monkeypatch) -> list[list[int]]:
    """Record the project-id list handed to each ``_refresh_message_dims`` call."""
    calls: list[list[int]] = []
    real = project_mod._refresh_message_dims

    def _spy(conn, project_ids):
        calls.append(list(project_ids))
        return real(conn, project_ids)

    monkeypatch.setattr(project_mod, "_refresh_message_dims", _spy)
    return calls


def test_event_less_project_gets_zero_cost_row(conn) -> None:
    """A project with real messages but no events lands a zero-cost row."""
    insert_event(conn, event_id=1, project_id=1, input_tokens=100, cost_usd=0.01)
    # Project 2 owns session_fk 2 and a genuine user turn — but no normalizer
    # ever produced a billable event for it.
    _user_turn(conn, msg_id=900, session_fk=2, seq=1, text="ship it")

    ProjectMartBuilder().refresh(conn, since_event_id=0)

    row = conn.execute(
        "SELECT * FROM project_mart WHERE project_id = 2"
    ).fetchone()
    assert row is not None
    # Identity columns come from ``projects``, same as the events path.
    assert (row["provider"], row["slug"], row["display_name"]) == (
        "codex", "beta", "Beta",
    )
    # Totals are truthfully zero, not missing.
    assert row["total_messages"] == 0
    assert row["total_sessions"] == 0
    assert row["total_cost_usd"] == 0.0
    assert row["first_ts"] is None
    assert row["last_ts"] is None
    # Dims are derived from the raw messages, so the user turn is counted.
    assert row["total_user_messages"] == 1
    assert row["total_commands"] == 1
    assert row["total_records"] == 1


def test_seed_returns_only_ids_it_inserted(conn) -> None:
    """The seed reports newly-covered ids only — never already-covered ones."""
    assert _seed_uncovered_projects(conn) == [1, 2]
    assert _seed_uncovered_projects(conn) == []


def test_steady_state_seed_adds_nothing_to_affected(conn, monkeypatch) -> None:
    """Already-covered projects must not re-enter the message-dims pass.

    Otherwise every watcher cycle would re-scan ``messages`` for every
    project in the store — the dims pass is the expensive half of the mart.
    """
    insert_event(conn, event_id=1, project_id=1)
    b = ProjectMartBuilder()
    w1 = b.refresh(conn, since_event_id=0)  # covers 1 (events) + 2 (seed)

    calls = _spy_on_dims(monkeypatch)

    # A new event for the already-covered project 1: only project 1 is
    # affected; the seed contributes nothing.
    insert_event(conn, event_id=2, project_id=1)
    w2 = b.refresh(conn, since_event_id=w1)
    assert calls == [[1]]

    # Nothing new at all — the whole refresh is a no-op.
    calls.clear()
    assert b.refresh(conn, since_event_id=w2) == w2
    assert calls == [[]]


def test_rebuild_from_scratch_covers_every_project(conn) -> None:
    insert_event(conn, event_id=1, project_id=1)
    ProjectMartBuilder().rebuild_from_scratch(conn)
    ids = [
        r["project_id"]
        for r in conn.execute(
            "SELECT project_id FROM project_mart ORDER BY project_id"
        )
    ]
    assert ids == [1, 2]


def test_rebuild_from_scratch_covers_projects_with_no_events_at_all(conn) -> None:
    """A store that has projects but has never produced an event still covers."""
    ProjectMartBuilder().rebuild_from_scratch(conn)
    ids = [
        r["project_id"]
        for r in conn.execute(
            "SELECT project_id FROM project_mart ORDER BY project_id"
        )
    ]
    assert ids == [1, 2]
