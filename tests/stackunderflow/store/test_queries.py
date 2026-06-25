import json
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


def test_cross_project_daily_totals_carries_speed(conn) -> None:
    """The speed flag is appended at the end of each tuple (v003)."""
    pa = _seed_project(conn, slug="proj-a")
    sa = _seed_session(conn, pa, "s-a")
    conn.execute(
        "INSERT INTO messages (session_fk, seq, timestamp, role, model, "
        "input_tokens, output_tokens, speed, raw_json) "
        "VALUES (?,?,?,?,?,?,?,?,?)",
        (sa, 0, "2026-04-15T10:00:00+00:00", "assistant",
         "claude-opus-4-6", 100, 50, "fast", "{}"),
    )
    conn.execute(
        "INSERT INTO messages (session_fk, seq, timestamp, role, model, "
        "input_tokens, output_tokens, speed, raw_json) "
        "VALUES (?,?,?,?,?,?,?,?,?)",
        (sa, 1, "2026-04-15T11:00:00+00:00", "assistant",
         "claude-opus-4-6", 100, 50, "standard", "{}"),
    )
    rows = queries.cross_project_daily_totals(conn)
    speeds = sorted(r[6] for r in rows)
    assert speeds == ["fast", "standard"]


# ── fast-mode cost path ──────────────────────────────────────────────────

def _seed_assistant_message(
    conn: sqlite3.Connection,
    *,
    session_fk: int,
    seq: int,
    timestamp: str,
    model: str,
    input_tokens: int,
    output_tokens: int,
    speed: str = "standard",
) -> None:
    conn.execute(
        "INSERT INTO messages (session_fk, seq, timestamp, role, model, "
        "input_tokens, output_tokens, speed, raw_json) "
        "VALUES (?, ?, ?, 'assistant', ?, ?, ?, ?, '{}')",
        (session_fk, seq, timestamp, model, input_tokens, output_tokens, speed),
    )


def test_get_global_stats_applies_fast_mode_multiplier(conn) -> None:
    """Opus rows tagged speed='fast' must price at 6× via compute_cost.

    Seeds two assistant messages that are identical *except* for ``speed``.
    The fast row's slice of ``daily_costs`` and ``models[opus].cost`` must
    be 6× the standard row's slice — closing the SQL-path gap PR #44 left.
    """
    from stackunderflow.infra.costs import compute_cost

    pa = _seed_project(conn, slug="proj-a")
    sa = _seed_session(conn, pa, "s-a")
    _seed_assistant_message(
        conn, session_fk=sa, seq=0,
        timestamp="2026-04-15T10:00:00+00:00",
        model="claude-opus-4-6",
        input_tokens=1000, output_tokens=500,
        speed="standard",
    )
    _seed_assistant_message(
        conn, session_fk=sa, seq=1,
        timestamp="2026-04-15T11:00:00+00:00",
        model="claude-opus-4-6",
        input_tokens=1000, output_tokens=500,
        speed="fast",
    )

    stats = queries.get_global_stats(conn)
    # Both rows roll into the same day & model.
    daily_cost = stats["daily_costs"][0]["cost"]
    model_cost = stats["models"]["claude-opus-4-6"]["cost"]

    # Compute the expected value using the same compute_cost the query
    # uses, so this test pins behavior, not absolute dollar figures.
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
    expected = standard_cost + fast_cost

    assert daily_cost == pytest.approx(expected)
    assert model_cost == pytest.approx(expected)
    # And the priority-tier slice is 6× the standard slice — that's the
    # whole point of the fast-mode multiplier.
    assert fast_cost == pytest.approx(standard_cost * 6.0)


def test_get_global_stats_standard_only_unchanged(conn) -> None:
    """Sessions without any fast rows must produce the same numbers as
    pre-v003 — no regression for the common case."""
    from stackunderflow.infra.costs import compute_cost

    pa = _seed_project(conn, slug="proj-a")
    sa = _seed_session(conn, pa, "s-a")
    _seed_assistant_message(
        conn, session_fk=sa, seq=0,
        timestamp="2026-04-15T10:00:00+00:00",
        model="claude-sonnet-4-6",
        input_tokens=2000, output_tokens=1000,
    )
    stats = queries.get_global_stats(conn)
    expected = compute_cost(
        {"input": 2000, "output": 1000, "cache_creation": 0, "cache_read": 0},
        "claude-sonnet-4-6",
        speed="standard",
    )["total_cost"]
    assert stats["models"]["claude-sonnet-4-6"]["cost"] == pytest.approx(expected)


def test_get_global_stats_sonnet_fast_no_multiplier(conn) -> None:
    """Sonnet on the priority tier still bills at 1× — only Opus families
    get the multiplier per the AnthropicPricer contract.
    """
    from stackunderflow.infra.costs import compute_cost

    pa = _seed_project(conn, slug="proj-a")
    sa = _seed_session(conn, pa, "s-a")
    _seed_assistant_message(
        conn, session_fk=sa, seq=0,
        timestamp="2026-04-15T10:00:00+00:00",
        model="claude-sonnet-4-6",
        input_tokens=1000, output_tokens=500,
        speed="fast",
    )
    stats = queries.get_global_stats(conn)
    standard_expected = compute_cost(
        {"input": 1000, "output": 500, "cache_creation": 0, "cache_read": 0},
        "claude-sonnet-4-6",
        speed="standard",
    )["total_cost"]
    assert stats["models"]["claude-sonnet-4-6"]["cost"] == pytest.approx(standard_expected)


# ── SQL-paginated /api/messages store path ───────────────────────────────
#
# ``count_project_messages`` + ``get_project_messages_page`` push the
# Messages-tab pagination into SQL: the old path materialised, enriched AND
# aggregated every message in the project on every page request. These tests
# pin (a) byte-equivalence with the legacy full-list slice and (b) that only
# the requested page is ever reconstructed — never the whole table.


def _insert_full_message(
    conn: sqlite3.Connection,
    *,
    session_fk: int,
    seq: int,
    timestamp: str,
    role: str = "assistant",
    model: str | None = None,
    content: str = "",
) -> None:
    """Insert a message whose ``raw_json`` is consistent with its columns.

    The reconstruction path reads ``raw_json`` (not the scalar columns), so the
    fixture writes a realistic payload; ``model`` is mirrored into both the
    column (used by the SQL model filter) and ``message.model`` (parsed back
    into the dict) so the two stay in lockstep, as the ingest writer keeps them.
    """
    msg: dict = {"role": role, "content": content}
    if model:
        msg["model"] = model
    raw = {"type": role, "timestamp": timestamp, "uuid": f"u{session_fk}-{seq}", "message": msg}
    conn.execute(
        "INSERT INTO messages (session_fk, seq, timestamp, role, model, "
        "input_tokens, output_tokens, content_text, tools_json, raw_json, "
        "is_sidechain, uuid, parent_uuid) "
        "VALUES (?, ?, ?, ?, ?, 0, 0, ?, '[]', ?, 0, ?, NULL)",
        (session_fk, seq, timestamp, role, model, content, json.dumps(raw), f"u{session_fk}-{seq}"),
    )


def test_count_project_messages_total_and_model_filter(conn) -> None:
    pid = _seed_project(conn)
    sid = _seed_session(conn, pid, "s1")
    for i in range(7):
        _insert_full_message(
            conn, session_fk=sid, seq=i, timestamp=f"2026-05-01T00:00:{i:02d}Z",
            model="claude-opus-4-6" if i < 3 else "claude-sonnet-4-6",
        )
    conn.commit()
    assert queries.count_project_messages(conn, project_id=pid) == 7
    # Model filter is case-insensitive on the model column.
    assert queries.count_project_messages(conn, project_id=pid, model_filter={"claude-opus-4-6"}) == 3
    assert queries.count_project_messages(conn, project_id=pid, model_filter={"claude-sonnet-4-6"}) == 4
    # Empty project id list → 0, no query needed.
    assert queries.count_project_messages(conn, project_id=[]) == 0


def test_get_project_messages_page_matches_full_slice(conn) -> None:
    """Each SQL page equals the matching slice of the full materialised list."""
    pid = _seed_project(conn)
    sid = _seed_session(conn, pid, "s1")
    for i in range(50):
        _insert_full_message(
            conn, session_fk=sid, seq=i, timestamp=f"2026-05-01T00:{i:02d}:00Z",
            content=f"msg {i}",
        )
    conn.commit()

    full = queries.get_project_messages(conn, project_id=pid)
    assert len(full) == 50
    # first / middle / last-partial pages
    for offset, limit in [(0, 20), (20, 20), (40, 20)]:
        page = queries.get_project_messages_page(
            conn, project_id=pid, offset=offset, limit=limit
        )
        expected = full[offset:offset + limit]
        assert [m["uuid"] for m in page] == [m["uuid"] for m in expected]
        assert [m["content"] for m in page] == [m["content"] for m in expected]
    # last page is partial
    assert len(queries.get_project_messages_page(conn, project_id=pid, offset=40, limit=20)) == 10
    # offset past the end → empty
    assert queries.get_project_messages_page(conn, project_id=pid, offset=99, limit=20) == []
    # limit <= 0 → empty
    assert queries.get_project_messages_page(conn, project_id=pid, offset=0, limit=0) == []


def test_get_project_messages_page_orders_by_timestamp_across_sessions(conn) -> None:
    """Pages are globally timestamp-ordered, not per-session — the property the
    old in-Python ``to_dicts`` sort guaranteed and SQL pagination must keep."""
    pid = _seed_project(conn)
    s1 = _seed_session(conn, pid, "s1")
    s2 = _seed_session(conn, pid, "s2")
    # Interleave timestamps across the two sessions: s1 on even seconds, s2 odd.
    for i in range(10):
        _insert_full_message(conn, session_fk=s1, seq=i, timestamp=f"2026-05-01T00:00:{2*i:02d}Z", content=f"s1-{i}")
        _insert_full_message(conn, session_fk=s2, seq=i, timestamp=f"2026-05-01T00:00:{2*i+1:02d}Z", content=f"s2-{i}")
    conn.commit()

    page = queries.get_project_messages_page(conn, project_id=pid, offset=0, limit=8)
    ts = [m["timestamp"] for m in page]
    assert ts == sorted(ts)
    # The first 8 by global time alternate s1/s2 — proves cross-session ordering.
    assert [m["content"] for m in page] == ["s1-0", "s2-0", "s1-1", "s2-1", "s1-2", "s2-2", "s1-3", "s2-3"]
    # Page 2 continues without overlap or gaps.
    page2 = queries.get_project_messages_page(conn, project_id=pid, offset=8, limit=8)
    assert [m["timestamp"] for m in page2] == sorted(m["timestamp"] for m in page2)
    assert {m["uuid"] for m in page}.isdisjoint(m["uuid"] for m in page2)


def test_get_project_messages_page_reconstructs_only_the_page(conn, monkeypatch) -> None:
    """The whole point: a page request reconstructs only ``per_page`` records,
    never the full table. Counts calls into the record parser as the probe."""
    from stackunderflow.stats import enricher

    pid = _seed_project(conn)
    sid = _seed_session(conn, pid, "s1")
    for i in range(300):
        _insert_full_message(conn, session_fk=sid, seq=i, timestamp=f"2026-05-01T{i // 60:02d}:{i % 60:02d}:00Z")
    conn.commit()

    calls = {"n": 0}
    real = enricher.parse_record
    monkeypatch.setattr(
        enricher, "parse_record",
        lambda te: (calls.__setitem__("n", calls["n"] + 1) or real(te)),
    )
    page = queries.get_project_messages_page(conn, project_id=pid, offset=100, limit=25)
    assert len(page) == 25
    # 25 reconstructed, not 300 — pagination happened in SQL.
    assert calls["n"] == 25


def test_get_project_messages_page_model_filter_aligns_with_count(conn) -> None:
    pid = _seed_project(conn)
    sid = _seed_session(conn, pid, "s1")
    for i in range(20):
        _insert_full_message(
            conn, session_fk=sid, seq=i, timestamp=f"2026-05-01T00:{i:02d}:00Z",
            model="claude-opus-4-6" if i % 2 == 0 else "claude-sonnet-4-6",
        )
    conn.commit()
    mf = {"claude-opus-4-6"}
    total = queries.count_project_messages(conn, project_id=pid, model_filter=mf)
    assert total == 10
    page = queries.get_project_messages_page(conn, project_id=pid, offset=0, limit=100, model_filter=mf)
    assert len(page) == total
    assert all(m["model"] == "claude-opus-4-6" for m in page)
    assert [m["timestamp"] for m in page] == sorted(m["timestamp"] for m in page)
