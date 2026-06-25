"""``/api/messages`` pushes pagination into SQL.

The companion ``test_messages_pagination.py`` locks the envelope *contract*
(keys, clamping, legacy ``?limit=``). This suite locks the *implementation*
the performance fix introduced: a page request must

* reconstruct ONLY the requested page — never materialise + enrich + aggregate
  the whole project (the old behaviour, O(total) per request), and
* keep the page globally timestamp-ordered across sessions (the ordering the
  old in-Python ``to_dicts`` sort produced), with correct items/total for the
  first, middle, last/partial, empty and past-the-end cases.

Pre-fix, the Messages tab on a 44K-message project took ~4.8s per page because
``get_project_messages`` ran the full stats pipeline on every request; the SQL
page path brings that to ~50ms.
"""

from __future__ import annotations

import json

import pytest

from stackunderflow.routes import data as data_route
from stackunderflow.store import db, queries, schema


def _connect(store_db):
    conn = db.connect(store_db)
    schema.apply(conn)
    return conn


def _insert_project(conn, *, provider="claude", slug="-sql-pg"):
    cur = conn.execute(
        "INSERT INTO projects (provider, slug, display_name, first_seen, last_modified) VALUES (?, ?, ?, ?, ?)",
        (provider, slug, slug, 0.0, 0.0),
    )
    return int(cur.lastrowid)


def _insert_session(conn, *, project_id, session_id, ts="2026-05-01T00:00:00Z"):
    cur = conn.execute(
        "INSERT INTO sessions (project_id, session_id, first_ts, last_ts, message_count) VALUES (?, ?, ?, ?, 0)",
        (project_id, session_id, ts, ts),
    )
    return int(cur.lastrowid)


def _ts(total_seconds: int) -> str:
    """``2026-05-01T HH:MM:SS Z`` for a second offset into the day."""
    h, rem = divmod(total_seconds, 3600)
    m, s = divmod(rem, 60)
    return f"2026-05-01T{h:02d}:{m:02d}:{s:02d}Z"


def _insert_message(conn, *, session_fk, seq, ts, role="assistant", model=None, content=""):
    """Insert a message with ``raw_json`` consistent with its columns."""
    msg: dict = {"role": role, "content": content}
    if model:
        msg["model"] = model
    raw = {"type": role, "timestamp": ts, "uuid": f"u{session_fk}-{seq}", "message": msg}
    conn.execute(
        "INSERT INTO messages (session_fk, seq, timestamp, role, model, "
        "input_tokens, output_tokens, content_text, tools_json, raw_json, "
        "is_sidechain, uuid, parent_uuid) "
        "VALUES (?, ?, ?, ?, ?, 0, 0, ?, '[]', ?, 0, ?, NULL)",
        (session_fk, seq, ts, role, model, content, json.dumps(raw), f"u{session_fk}-{seq}"),
    )


@pytest.fixture
def two_session_store(tmp_path, monkeypatch):
    """250 messages split across two sessions with interleaved timestamps.

    Interleaving means global timestamp order differs from per-session order,
    so a page that respects the contract has to merge the two sessions.
    """
    store_db = tmp_path / "msgs.db"
    slug = "-sql-pg"
    conn = _connect(store_db)
    pid = _insert_project(conn, slug=slug)
    s1 = _insert_session(conn, project_id=pid, session_id="s1")
    s2 = _insert_session(conn, project_id=pid, session_id="s2")
    # 125 per session; s1 on even seconds, s2 on odd → strictly increasing, no ties.
    for i in range(125):
        _insert_message(conn, session_fk=s1, seq=i, ts=_ts(2 * i), content=f"s1-{i}", model="claude-opus-4-6")
        _insert_message(conn, session_fk=s2, seq=i, ts=_ts(2 * i + 1), content=f"s2-{i}", model="claude-sonnet-4-6")
    conn.commit()
    conn.close()
    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)
    monkeypatch.setattr("stackunderflow.deps.current_log_path", f"/fake/{slug}")
    return slug


# ── only the page is reconstructed ────────────────────────────────────────


@pytest.mark.asyncio
async def test_route_reconstructs_only_the_page(two_session_store, monkeypatch):
    """A page request parses exactly ``per_page`` records, regardless of the
    250-row total — the behavioural proof pagination is in SQL, not Python."""
    from stackunderflow.stats import enricher

    calls = {"n": 0}
    real = enricher.parse_record
    monkeypatch.setattr(
        enricher,
        "parse_record",
        lambda te: (calls.__setitem__("n", calls["n"] + 1) or real(te)),
    )
    out = await data_route.get_messages(page=1, per_page=50)
    assert len(out["messages"]) == 50
    assert out["total"] == 250
    assert calls["n"] == 50  # not 250


@pytest.mark.asyncio
async def test_route_never_uses_full_materialise_path(two_session_store, monkeypatch):
    """The O(total) ``get_project_messages`` helper must not be on the
    ``/api/messages`` path any more."""

    def _boom(*a, **k):
        raise AssertionError("get_project_messages (full materialise) must not be called")

    monkeypatch.setattr(queries, "get_project_messages", _boom)
    out = await data_route.get_messages(page=2, per_page=50)
    assert out["page"] == 2
    assert len(out["messages"]) == 50


# ── global ordering + page correctness ────────────────────────────────────


@pytest.mark.asyncio
async def test_page_is_globally_timestamp_ordered(two_session_store):
    out = await data_route.get_messages(page=1, per_page=10)
    ts = [m["timestamp"] for m in out["messages"]]
    assert ts == sorted(ts)
    # First 10 by global time alternate the two sessions.
    assert [m["content"] for m in out["messages"]] == [
        "s1-0",
        "s2-0",
        "s1-1",
        "s2-1",
        "s1-2",
        "s2-2",
        "s1-3",
        "s2-3",
        "s1-4",
        "s2-4",
    ]


@pytest.mark.asyncio
async def test_pages_do_not_overlap_or_skip(two_session_store):
    """Concatenating every page reproduces the full ordered stream exactly."""
    seen: list[str] = []
    for page in range(1, 6):  # 250 / 50 = 5 pages
        out = await data_route.get_messages(page=page, per_page=50)
        seen.extend(m["uuid"] for m in out["messages"])
    assert len(seen) == 250
    assert len(set(seen)) == 250  # no duplicates → no overlap, no skips


@pytest.mark.asyncio
async def test_first_middle_last_partial_pages(two_session_store):
    first = await data_route.get_messages(page=1, per_page=100)
    middle = await data_route.get_messages(page=2, per_page=100)
    last = await data_route.get_messages(page=3, per_page=100)
    assert (first["start_index"], first["end_index"], len(first["messages"])) == (0, 100, 100)
    assert (middle["start_index"], middle["end_index"], len(middle["messages"])) == (100, 200, 100)
    # Last page is partial: 250 total → 50 rows.
    assert (last["page"], last["end_index"], len(last["messages"])) == (3, 250, 50)
    # No overlap between adjacent pages.
    assert {m["uuid"] for m in first["messages"]}.isdisjoint(m["uuid"] for m in middle["messages"])


@pytest.mark.asyncio
async def test_page_beyond_end_clamps_to_last(two_session_store):
    out = await data_route.get_messages(page=999, per_page=100)
    assert out["page"] == 3
    assert len(out["messages"]) == 50
    assert out["end_index"] == 250


# ── model filter is pushed into SQL alongside pagination ──────────────────


@pytest.mark.asyncio
async def test_model_filter_paginates_in_sql(two_session_store):
    """Total + page both reflect the model filter, so indices stay aligned."""
    out = await data_route.get_messages(page=1, per_page=100, model=["claude-opus-4-6"])
    assert out["total"] == 125  # only the s1 (opus) messages
    assert len(out["messages"]) == 100
    assert all(m["model"] == "claude-opus-4-6" for m in out["messages"])
    out2 = await data_route.get_messages(page=2, per_page=100, model=["claude-opus-4-6"])
    assert len(out2["messages"]) == 25
    assert out2["end_index"] == 125


# ── empty (but existing) project ──────────────────────────────────────────


@pytest.mark.asyncio
async def test_existing_project_with_zero_messages(tmp_path, monkeypatch):
    """A project row that exists but has no messages returns the same empty
    envelope the old materialise-then-slice path produced (page clamps to 0
    when total_pages is 0)."""
    store_db = tmp_path / "empty.db"
    slug = "-empty"
    conn = _connect(store_db)
    _insert_project(conn, slug=slug)
    conn.commit()
    conn.close()
    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)
    monkeypatch.setattr("stackunderflow.deps.current_log_path", f"/fake/{slug}")

    out = await data_route.get_messages(page=1, per_page=100)
    assert out["messages"] == []
    assert out["total"] == 0
    assert out["total_pages"] == 0
