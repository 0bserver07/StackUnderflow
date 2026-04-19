import sqlite3
from collections.abc import Generator
from pathlib import Path

import pytest

from stackunderflow.store import db, queries, schema


@pytest.fixture
def conn(tmp_path: Path) -> Generator[sqlite3.Connection, None, None]:
    c = db.connect(tmp_path / "store.db")
    schema.apply(c)
    yield c
    c.close()


def _seed_project(conn: sqlite3.Connection, *, slug: str = "-a", provider: str = "claude") -> int:
    cur = conn.execute(
        "INSERT INTO projects (provider, slug, display_name, first_seen, last_modified) "
        "VALUES (?, ?, ?, ?, ?)",
        (provider, slug, slug, 0.0, 0.0),
    )
    assert cur.lastrowid is not None
    return cur.lastrowid


def _seed_session(conn: sqlite3.Connection, project_id: int, session_id: str = "s1") -> int:
    cur = conn.execute(
        "INSERT INTO sessions (project_id, session_id) VALUES (?, ?)",
        (project_id, session_id),
    )
    assert cur.lastrowid is not None
    return cur.lastrowid


def test_list_projects_empty(conn) -> None:
    assert queries.list_projects(conn) == []


def test_list_projects_returns_one(conn) -> None:
    _seed_project(conn, slug="-a")
    out = queries.list_projects(conn)
    assert len(out) == 1
    assert out[0].slug == "-a"


def test_get_project_by_slug(conn) -> None:
    _seed_project(conn, slug="-a")
    p = queries.get_project(conn, slug="-a")
    assert p is not None and p.slug == "-a"


def test_get_project_missing_returns_none(conn) -> None:
    assert queries.get_project(conn, slug="-nope") is None


def test_list_sessions_filters_by_project(conn) -> None:
    pid1 = _seed_project(conn, slug="-a")
    pid2 = _seed_project(conn, slug="-b")
    _seed_session(conn, pid1, "s-a1")
    _seed_session(conn, pid2, "s-b1")
    out = queries.list_sessions(conn, project_id=pid1)
    assert {s.session_id for s in out} == {"s-a1"}


def test_get_messages_paginates(conn) -> None:
    pid = _seed_project(conn)
    sid = _seed_session(conn, pid, "s1")
    for i in range(5):
        conn.execute(
            "INSERT INTO messages (session_fk, seq, timestamp, role, raw_json) "
            "VALUES (?, ?, ?, ?, ?)",
            (sid, i, f"2026-01-01T00:0{i}:00+00:00", "user", "{}"),
        )
    page = queries.get_messages(conn, session_fk=sid, limit=2, offset=1)
    assert [m.seq for m in page] == [1, 2]


def test_get_session_messages(conn) -> None:
    pid = _seed_project(conn)
    sid = _seed_session(conn, pid, "s1")
    for i in range(3):
        conn.execute(
            "INSERT INTO messages (session_fk, seq, timestamp, role, raw_json) "
            "VALUES (?, ?, ?, ?, ?)",
            (sid, i, f"2026-01-01T00:0{i}:00+00:00", "user", "{}"),
        )
    msgs = queries.get_session_messages(conn, session_fk=sid)
    assert len(msgs) == 3
    assert [m.seq for m in msgs] == [0, 1, 2]


def test_get_session_stats(conn) -> None:
    pid = _seed_project(conn)
    sid = _seed_session(conn, pid, "s1")
    conn.execute(
        "INSERT INTO messages (session_fk, seq, timestamp, role, model, "
        "input_tokens, output_tokens, tools_json, raw_json) "
        "VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        (sid, 0, "2026-01-01T00:00:00+00:00", "user", None, 10, 0, "[]", "{}"),
    )
    conn.execute(
        "INSERT INTO messages (session_fk, seq, timestamp, role, model, "
        "input_tokens, output_tokens, tools_json, raw_json) "
        "VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        (sid, 1, "2026-01-01T00:00:01+00:00", "assistant", "claude-sonnet-4-6",
         5, 20, '[{"name":"bash"}]', "{}"),
    )
    stats = queries.get_session_stats(conn, session_fk=sid)
    assert stats["user_messages"] == 1
    assert stats["assistant_messages"] == 1
    assert stats["input_tokens"] == 15
    assert stats["output_tokens"] == 20
    assert stats["model"] == "claude-sonnet-4-6"
    assert stats["tool_calls"] == 1


def test_cross_project_daily_totals(conn) -> None:
    # Two projects, messages on different days
    pa = _seed_project(conn, slug="proj-a")
    pb = _seed_project(conn, slug="proj-b")
    sa = _seed_session(conn, pa, "s-a")
    sb = _seed_session(conn, pb, "s-b")
    for seq, (ts, session_fk, model, inp, out) in enumerate([
        ("2026-04-15T10:00:00+00:00", sa, "claude-3", 100, 50),
        ("2026-04-16T10:00:00+00:00", sa, "claude-3", 200, 80),
        ("2026-04-16T11:00:00+00:00", sb, "claude-3", 40, 20),
    ]):
        conn.execute(
            "INSERT INTO messages (session_fk, seq, timestamp, role, model, "
            "input_tokens, output_tokens, raw_json) VALUES (?,?,?,?,?,?,?,?)",
            (session_fk, seq, ts, "assistant", model, inp, out, "{}"),
        )
    rows = queries.cross_project_daily_totals(conn)
    slugs = {r[0] for r in rows}
    assert slugs == {"proj-a", "proj-b"}
    # proj-a 2026-04-15: 100 in, 50 out; proj-a 2026-04-16: 200 in, 80 out
    pa_totals = [(r[3], r[4]) for r in rows if r[0] == "proj-a"]
    assert sum(inp for inp, _ in pa_totals) == 300
    assert sum(out for _, out in pa_totals) == 130


def test_cross_project_daily_totals_since_filter(conn) -> None:
    pa = _seed_project(conn, slug="proj-a")
    sa = _seed_session(conn, pa, "s-a")
    for seq, (ts, inp) in enumerate([
        ("2026-04-14T10:00:00+00:00", 10),
        ("2026-04-15T10:00:00+00:00", 20),
        ("2026-04-16T10:00:00+00:00", 30),
    ]):
        conn.execute(
            "INSERT INTO messages (session_fk, seq, timestamp, role, model, "
            "input_tokens, output_tokens, raw_json) VALUES (?,?,?,?,?,?,?,?)",
            (sa, seq, ts, "user", "", inp, 0, "{}"),
        )
    rows = queries.cross_project_daily_totals(conn, since="2026-04-15T00:00:00+00:00")
    total_in = sum(r[3] for r in rows)
    assert total_in == 50  # 20 + 30, not 10
