"""ModelDayMartBuilder — (day, model, speed) rollup across all projects."""

from __future__ import annotations

from stackunderflow.etl.marts.model_day import ModelDayMartBuilder

from .conftest import insert_event


def test_empty(conn) -> None:
    new = ModelDayMartBuilder().refresh(conn, since_event_id=0)
    assert new == 0
    assert conn.execute("SELECT COUNT(*) AS n FROM model_day_mart").fetchone()["n"] == 0


def test_per_model_per_speed(conn) -> None:
    insert_event(conn, event_id=1, model="sonnet", speed="standard",
                 input_tokens=100)
    insert_event(conn, event_id=2, model="sonnet", speed="standard",
                 input_tokens=200)
    insert_event(conn, event_id=3, model="sonnet", speed="fast",
                 input_tokens=400)
    insert_event(conn, event_id=4, model="opus", speed="standard",
                 input_tokens=800)
    ModelDayMartBuilder().refresh(conn, since_event_id=0)
    rows = {(r["model"], r["speed"]): dict(r) for r in conn.execute(
        "SELECT * FROM model_day_mart"
    )}
    assert rows[("sonnet", "standard")]["input_tokens"] == 300
    assert rows[("sonnet", "fast")]["input_tokens"] == 400
    assert rows[("opus", "standard")]["input_tokens"] == 800


def test_session_count_recompute_across_windows(conn) -> None:
    insert_event(conn, event_id=1, session_id="sess-1", model="sonnet")
    b = ModelDayMartBuilder()
    w1 = b.refresh(conn, since_event_id=0)
    insert_event(conn, event_id=2, session_id="sess-1", model="sonnet")
    b.refresh(conn, since_event_id=w1)
    row = conn.execute("SELECT * FROM model_day_mart").fetchone()
    assert row["session_count"] == 1
    assert row["message_count"] == 2


def test_rebuild_matches_incremental(conn) -> None:
    insert_event(conn, event_id=1, model="sonnet")
    insert_event(conn, event_id=2, model="opus")
    b = ModelDayMartBuilder()
    b.refresh(conn, since_event_id=0)
    inc = sorted(
        tuple(dict(r).items())
        for r in conn.execute(
            "SELECT * FROM model_day_mart ORDER BY model"
        ).fetchall()
    )
    b.rebuild_from_scratch(conn)
    out = sorted(
        tuple(dict(r).items())
        for r in conn.execute(
            "SELECT * FROM model_day_mart ORDER BY model"
        ).fetchall()
    )
    assert inc == out
