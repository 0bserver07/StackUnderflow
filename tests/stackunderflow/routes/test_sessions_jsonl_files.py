"""``GET /api/jsonl-files`` builds the session list in O(1) queries.

The route used to run an N+1 — ``list_sessions`` then, per session, a
``get_session_stats`` query + a first-user-message query + a ``compute_cost``
call (~3.7K queries for ~1.8K sessions, stalling the Sessions tab linearly).
This suite locks the fix:

* the per-session aggregates + titles + costs are byte-identical to the old
  per-session path (parity), and
* the number of ``messages``-touching SQL statements is a small constant,
  independent of the session count (the O(1) proof, asserted via a tracing
  cursor).
"""
from __future__ import annotations

import json

import pytest

from stackunderflow.infra.costs import compute_cost
from stackunderflow.routes import sessions as sessions_route
from stackunderflow.routes.sessions import get_jsonl_files
from stackunderflow.store import db, queries, schema


def _connect(store_db):
    conn = db.connect(store_db)
    schema.apply(conn)
    return conn


def _insert_project(conn, *, provider="claude", slug="-jf"):
    cur = conn.execute(
        "INSERT INTO projects (provider, slug, display_name, first_seen, last_modified) "
        "VALUES (?, ?, ?, ?, ?)",
        (provider, slug, slug, 0.0, 0.0),
    )
    return int(cur.lastrowid)


def _insert_session(conn, *, project_id, session_id, ts="2026-05-01T00:00:00Z"):
    cur = conn.execute(
        "INSERT INTO sessions (project_id, session_id, first_ts, last_ts, message_count) "
        "VALUES (?, ?, ?, ?, 0)",
        (project_id, session_id, ts, ts),
    )
    return int(cur.lastrowid)


def _insert_message(
    conn,
    *,
    session_fk,
    seq,
    ts,
    role,
    model=None,
    input_tokens=0,
    output_tokens=0,
    content="",
    n_tools=0,
):
    tools_json = json.dumps([{"name": f"t{i}"} for i in range(n_tools)])
    raw = {"type": role, "timestamp": ts, "uuid": f"u{session_fk}-{seq}",
           "message": {"role": role, "content": content}}
    conn.execute(
        "INSERT INTO messages (session_fk, seq, timestamp, role, model, "
        "input_tokens, output_tokens, content_text, tools_json, raw_json, "
        "is_sidechain, uuid, parent_uuid) "
        "VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 0, ?, NULL)",
        (session_fk, seq, ts, role, model, input_tokens, output_tokens,
         content, tools_json, json.dumps(raw), f"u{session_fk}-{seq}"),
    )


def _expected_file_for_session(conn, session):
    """Recompute one file row the OLD per-session way — the parity oracle."""
    stats = queries.get_session_stats(conn, session_fk=session.id)
    first = conn.execute(
        "SELECT content_text FROM messages "
        "WHERE session_fk = ? AND role = 'user' "
        "  AND content_text IS NOT NULL AND content_text != '' "
        "ORDER BY seq LIMIT 1",
        (session.id,),
    ).fetchone()
    title = first["content_text"][:150] if first else None
    est = 0.0
    if stats["model"] and (stats["input_tokens"] or stats["output_tokens"]):
        est = compute_cost(
            {"input": stats["input_tokens"], "output": stats["output_tokens"]},
            stats["model"],
        ).get("total_cost", 0.0)
    return {
        "user_messages": stats["user_messages"],
        "assistant_messages": stats["assistant_messages"],
        "input_tokens": stats["input_tokens"],
        "output_tokens": stats["output_tokens"],
        "model": stats["model"],
        "title": title,
        "tool_calls": stats["tool_calls"],
        "estimated_cost": round(est, 4),
    }


@pytest.fixture
def seeded(tmp_path, monkeypatch):
    """Three sessions with deliberately varied shapes to exercise parity:

    - s1: user + assistant, tokens, tools, a title.
    - s2: bigger, two models (MAX(model) wins), no tools.
    - s3: a session row with ZERO messages (all-zero / null-model branch).
    """
    store_db = tmp_path / "jf.db"
    slug = "-jf"
    conn = _connect(store_db)
    pid = _insert_project(conn, slug=slug)

    s1 = _insert_session(conn, project_id=pid, session_id="s1", ts="2026-05-01T00:00:00Z")
    _insert_message(conn, session_fk=s1, seq=0, ts="2026-05-01T00:00:00Z", role="user",
                    content="first prompt one")
    _insert_message(conn, session_fk=s1, seq=1, ts="2026-05-01T00:01:00Z", role="assistant",
                    model="claude-sonnet-4-20250514", input_tokens=1200, output_tokens=340, n_tools=3)

    s2 = _insert_session(conn, project_id=pid, session_id="s2", ts="2026-05-02T00:00:00Z")
    _insert_message(conn, session_fk=s2, seq=0, ts="2026-05-02T00:00:00Z", role="user",
                    content="second session prompt")
    _insert_message(conn, session_fk=s2, seq=1, ts="2026-05-02T00:01:00Z", role="assistant",
                    model="claude-3-5-sonnet-20241022", input_tokens=5000, output_tokens=900)
    _insert_message(conn, session_fk=s2, seq=2, ts="2026-05-02T00:02:00Z", role="assistant",
                    model="claude-opus-4-6", input_tokens=2000, output_tokens=600)

    # s3: zero messages — exercises the missing-aggregate (all-zero) branch.
    _insert_session(conn, project_id=pid, session_id="s3", ts="2026-05-03T00:00:00Z")

    conn.commit()
    conn.close()
    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)
    monkeypatch.setattr("stackunderflow.deps.current_log_path", f"/fake/{slug}")
    return store_db, slug


@pytest.mark.asyncio
async def test_jsonl_files_matches_old_per_session_path(seeded):
    """Every aggregate/title/cost equals the old per-session computation."""
    store_db, slug = seeded
    resp = await get_jsonl_files(project=slug)
    body = json.loads(resp.body)
    files = {f["name"]: f for f in body["files"]}
    assert set(files) == {"s1.jsonl", "s2.jsonl", "s3.jsonl"}

    conn = _connect(store_db)
    try:
        rows = queries.get_projects_by_slug(conn, slug=slug)
        sessions = queries.list_sessions(conn, project_id=[r.id for r in rows])
        for session in sessions:
            expected = _expected_file_for_session(conn, session)
            got = files[f"{session.session_id}.jsonl"]
            for key, val in expected.items():
                assert got[key] == val, f"{session.session_id}.{key}: {got[key]!r} != {val!r}"
    finally:
        conn.close()

    # MAX(model) tie-break parity: s2 saw two models; the lexical max wins
    # ('claude-opus-4-6' > 'claude-3-5-sonnet-20241022').
    assert files["s2.jsonl"]["model"] == "claude-opus-4-6"
    # s1's title is its first user message; tool_calls came from json_array_length.
    assert files["s1.jsonl"]["title"] == "first prompt one"
    assert files["s1.jsonl"]["tool_calls"] == 3
    # s3 has no messages → all-zero / null model.
    assert files["s3.jsonl"]["model"] is None
    assert files["s3.jsonl"]["input_tokens"] == 0
    assert files["s3.jsonl"]["title"] is None


def _trace_connect(monkeypatch, sink):
    """Patch the route's ``db.connect`` to trace every SQL statement it runs.

    The callback is installed AFTER the real connect (so the connection's
    own PRAGMAs aren't counted) — only the route's own statements land in
    ``sink``.
    """
    real = db.connect

    def wrapped(path):
        conn = real(path)
        conn.set_trace_callback(lambda stmt: sink.append(stmt))
        return conn

    monkeypatch.setattr(sessions_route.db, "connect", wrapped)


def _count_message_queries(sink):
    return sum(1 for s in sink if "from messages" in " ".join(s.lower().split()))


def _build_store(tmp_path, monkeypatch, *, n_sessions, name):
    store_db = tmp_path / name
    slug = f"-{name}"
    conn = _connect(store_db)
    pid = _insert_project(conn, slug=slug)
    for s in range(n_sessions):
        sk = _insert_session(conn, project_id=pid, session_id=f"s{s}",
                             ts=f"2026-05-01T00:{s // 60:02d}:{s % 60:02d}Z")
        _insert_message(conn, session_fk=sk, seq=0, ts=f"2026-05-01T00:{s // 60:02d}:{s % 60:02d}Z",
                        role="user", content=f"prompt {s}")
        _insert_message(conn, session_fk=sk, seq=1, ts=f"2026-05-01T01:{s // 60:02d}:{s % 60:02d}Z",
                        role="assistant", model="claude-sonnet-4-20250514",
                        input_tokens=100, output_tokens=50)
    conn.commit()
    conn.close()
    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)
    monkeypatch.setattr("stackunderflow.deps.current_log_path", f"/fake/{slug}")
    return slug


@pytest.mark.asyncio
async def test_message_query_count_is_constant(tmp_path, monkeypatch):
    """The messages-touching query count does NOT scale with session count:
    2 (one GROUP BY aggregate + one ROW_NUMBER title pass) for any N."""
    # Small project — 3 sessions.
    slug_small = _build_store(tmp_path, monkeypatch, n_sessions=3, name="small")
    sink_small: list[str] = []
    _trace_connect(monkeypatch, sink_small)
    resp_small = await get_jsonl_files(project=slug_small)
    assert len(json.loads(resp_small.body)["files"]) == 3
    small_count = _count_message_queries(sink_small)

    # Big project — 40 sessions. Old N+1 path would issue ~80 messages queries.
    slug_big = _build_store(tmp_path, monkeypatch, n_sessions=40, name="big")
    sink_big: list[str] = []
    _trace_connect(monkeypatch, sink_big)
    resp_big = await get_jsonl_files(project=slug_big)
    assert len(json.loads(resp_big.body)["files"]) == 40
    big_count = _count_message_queries(sink_big)

    assert small_count == 2, f"expected 2 messages queries, got {small_count}: {sink_small}"
    assert big_count == 2, f"expected 2 messages queries, got {big_count}: {sink_big}"
    # The point of the fix: identical query count regardless of N sessions.
    assert big_count == small_count


@pytest.mark.asyncio
async def test_jsonl_files_empty_project_returns_currency_envelope(tmp_path, monkeypatch):
    store_db = tmp_path / "empty.db"
    slug = "-empty"
    conn = _connect(store_db)
    _insert_project(conn, slug=slug)
    conn.commit()
    conn.close()
    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)
    monkeypatch.setattr("stackunderflow.deps.current_log_path", f"/fake/{slug}")

    resp = await get_jsonl_files(project=slug)
    body = json.loads(resp.body)
    assert body["files"] == []
    assert "currency" in body
