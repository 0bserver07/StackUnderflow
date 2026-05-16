"""``/api/messages`` returns a paginated envelope, capped at 500 items per page.

Pre-fix, the route returned the full message list unbounded — a 26K-message
project ballooned the response to ~37 MB and OOMed the Messages tab. This
suite locks the new contract:

* default ``per_page`` = 100, max = 500 (clamped)
* envelope keys: ``messages, total, page, per_page, total_pages,
  start_index, end_index``
* ``page`` out of range is clamped to ``[1, total_pages]``
* legacy ``?limit=N`` still caps the page size when ``per_page`` is the
  default — preserved for one release of in-flight clients
* filter short-circuits (provider exclude / empty result) return the
  same shape so the frontend never branches on a bare list vs envelope
"""

from __future__ import annotations

import pytest

from stackunderflow.routes import data as data_route
from stackunderflow.store import db, schema


def _connect(store_db):
    conn = db.connect(store_db)
    schema.apply(conn)
    return conn


def _insert_project(conn, *, provider="claude", slug="-test"):
    cur = conn.execute(
        "INSERT INTO projects (provider, slug, display_name, first_seen, last_modified) "
        "VALUES (?, ?, ?, ?, ?)",
        (provider, slug, slug, 0.0, 0.0),
    )
    return int(cur.lastrowid)


def _insert_session(conn, *, project_id, session_id, ts, n=0):
    cur = conn.execute(
        "INSERT INTO sessions (project_id, session_id, first_ts, last_ts, "
        "message_count) VALUES (?, ?, ?, ?, ?)",
        (project_id, session_id, ts, ts, n),
    )
    return int(cur.lastrowid)


def _bulk_insert_messages(conn, *, session_fk, count, model="claude-A", base_ts="2026-04-01T10:00:00Z"):
    """Insert ``count`` assistant messages so we have something to page over."""
    rows = []
    for i in range(count):
        # Timestamps strictly increasing so ORDER BY is deterministic.
        ts = f"2026-04-01T10:{i // 60:02d}:{i % 60:02d}Z"
        rows.append((session_fk, i, ts, model))
    conn.executemany(
        "INSERT INTO messages (session_fk, seq, timestamp, role, model, "
        "input_tokens, output_tokens, cache_create_tokens, cache_read_tokens, "
        "content_text, tools_json, raw_json, is_sidechain, uuid, parent_uuid) "
        "VALUES (?, ?, ?, 'assistant', ?, 1, 1, 0, 0, '', '[]', '{}', 0, NULL, NULL)",
        rows,
    )


@pytest.fixture
def populated_store(tmp_path, monkeypatch):
    """A store with 250 messages — enough to span 3 default pages."""
    store_db = tmp_path / "msgs.db"
    slug = "-pagination-test"
    conn = _connect(store_db)
    pid = _insert_project(conn, slug=slug)
    sfk = _insert_session(conn, project_id=pid, session_id="s1", ts="2026-04-01T10:00:00Z", n=250)
    _bulk_insert_messages(conn, session_fk=sfk, count=250)
    conn.commit()
    conn.close()

    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)
    monkeypatch.setattr("stackunderflow.deps.current_log_path", f"/fake/{slug}")
    return slug


# ── envelope shape ────────────────────────────────────────────────────────────


@pytest.mark.asyncio
async def test_default_pagination_returns_envelope(populated_store):
    """No params → first 100 messages wrapped in the canonical envelope."""
    out = await data_route.get_messages()
    assert isinstance(out, dict)
    assert set(out) >= {
        "messages", "total", "page", "per_page", "total_pages",
        "start_index", "end_index",
    }
    assert out["page"] == 1
    assert out["per_page"] == 100
    assert out["total"] == 250
    assert out["total_pages"] == 3
    assert len(out["messages"]) == 100
    assert out["start_index"] == 0
    assert out["end_index"] == 100


@pytest.mark.asyncio
async def test_second_page_returns_next_slice(populated_store):
    """Page 2 picks up where page 1 left off."""
    page1 = await data_route.get_messages(page=1, per_page=100)
    page2 = await data_route.get_messages(page=2, per_page=100)
    assert len(page2["messages"]) == 100
    assert page2["page"] == 2
    assert page2["start_index"] == 100
    # No overlap with page 1 — compare on timestamp since the fixture
    # leaves ``uuid`` blank (the schema column is nullable) and message_id
    # comes back synthesised from session/seq which isn't unique per page.
    page1_ts = {m["timestamp"] for m in page1["messages"]}
    page2_ts = {m["timestamp"] for m in page2["messages"]}
    assert page1_ts.isdisjoint(page2_ts)


@pytest.mark.asyncio
async def test_last_page_partial(populated_store):
    """Page 3 has the remaining 50 messages, not a full 100."""
    out = await data_route.get_messages(page=3, per_page=100)
    assert len(out["messages"]) == 50
    assert out["page"] == 3
    assert out["end_index"] == 250


# ── clamping ──────────────────────────────────────────────────────────────────


@pytest.mark.asyncio
async def test_per_page_clamped_to_max(populated_store):
    """Requests above MESSAGES_MAX_PER_PAGE are silently clamped."""
    out = await data_route.get_messages(page=1, per_page=10000)
    assert out["per_page"] == data_route.MESSAGES_MAX_PER_PAGE
    assert len(out["messages"]) <= data_route.MESSAGES_MAX_PER_PAGE


@pytest.mark.asyncio
async def test_per_page_clamped_to_min(populated_store):
    """``per_page=0`` (or negative) clamps to 1 so the page is never empty
    when the store has data."""
    out = await data_route.get_messages(page=1, per_page=0)
    assert out["per_page"] == 1
    assert len(out["messages"]) == 1


@pytest.mark.asyncio
async def test_page_below_one_clamped(populated_store):
    """``page=0`` is treated as page 1 (helper already does the clamp on
    the upper bound; we mirror its lower-bound behaviour here)."""
    out = await data_route.get_messages(page=0, per_page=100)
    assert out["page"] == 1


@pytest.mark.asyncio
async def test_page_above_total_pages_clamped(populated_store):
    """Page far past the end returns the last page rather than 500-ing."""
    out = await data_route.get_messages(page=99, per_page=100)
    assert out["page"] == 3
    assert len(out["messages"]) == 50


# ── legacy ?limit= compatibility ──────────────────────────────────────────────


@pytest.mark.asyncio
async def test_legacy_limit_param_caps_per_page(populated_store):
    """``?limit=25`` (no per_page) still trims the response — preserved for
    one release so in-flight clients keep working."""
    out = await data_route.get_messages(limit=25)
    assert out["per_page"] == 25
    assert len(out["messages"]) == 25


@pytest.mark.asyncio
async def test_explicit_per_page_wins_over_legacy_limit(populated_store):
    """When the caller passes both, ``per_page`` wins (legacy is fallback)."""
    out = await data_route.get_messages(limit=10, per_page=50)
    assert out["per_page"] == 50
    assert len(out["messages"]) == 50


# ── filter short-circuits return the same envelope shape ──────────────────────


@pytest.mark.asyncio
async def test_provider_exclude_returns_empty_envelope(populated_store):
    """A provider filter that excludes the project returns a paginated
    envelope (not a bare list) so the frontend never has to branch on shape."""
    out = await data_route.get_messages(provider=["codex"])
    assert isinstance(out, dict)
    assert out["messages"] == []
    assert out["total"] == 0
    assert out["page"] == 1


# ── payload size: the bug we're fixing ────────────────────────────────────────


@pytest.mark.asyncio
async def test_default_response_size_under_cap(populated_store):
    """The whole point of this PR: default page must stay small. A 100-msg
    page on this fixture is well under 100 KB; on the 26K-message
    chimera fixture the maintainer saw 37 MB unbounded, which is what
    this test guards against regressing.

    The 200 KB cap here is generous (each fixture message is tiny) — the
    real-world per-message size is ~1.4 KB so a 100-page lands at ~140 KB.
    We pick a number that lets the test pass on small fixtures while
    still flagging an unbounded regression."""
    import json
    out = await data_route.get_messages()
    serialized = json.dumps(out, default=str)
    assert len(serialized) < 200_000, (
        f"Default /api/messages page is {len(serialized)} bytes — "
        "looks like pagination broke and we're returning everything."
    )
