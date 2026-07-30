"""PROJ-7 — an all-zero ``project_mart`` seed does NOT count as coverage.

``ProjectMartBuilder`` seeds a mart row for every event-less project so no slug
can go missing from the dashboard. That seed writes only the identity columns,
so all its totals stay at ``DEFAULT`` — but the builder's second pass fills the
*message*-derived dims off the ``messages`` table, so a seeded project that has
real message rows ends up with ``total_records > 0`` and ``total_messages ==
0``.

Once the seed covered every project, ``uncovered = id not in mart_rows`` went
empty, the bulk-SQL fallback stopped running, and those projects' list cards
lost the dates / command count / cost they used to show. The predicate under
test restores them without touching the genuinely empty seeds (no messages,
nothing to recover — their zeros are correct).

All stores are ``tmp_path`` — never the real one.
"""

from __future__ import annotations

import json

import pytest

from stackunderflow.routes.projects import get_projects
from stackunderflow.store import db, queries, schema

EVENTS_BACKED = "-events-backed"
SEEDED_WITH_MESSAGES = "-seeded-with-messages"
SEEDED_EMPTY = "-seeded-empty"


def _connect(store_db):
    conn = db.connect(store_db)
    schema.apply(conn)
    return conn


def _insert_project(conn, *, slug, provider="claude", last_modified=0.0):
    cur = conn.execute(
        "INSERT INTO projects (provider, slug, display_name, first_seen, last_modified) "
        "VALUES (?, ?, ?, 0.0, ?)",
        (provider, slug, slug, last_modified),
    )
    return int(cur.lastrowid)


def _insert_project_mart(conn, *, project_id, slug, provider="claude", **kw):
    """Insert a ``project_mart`` row, defaulting every total to 0/NULL.

    Mirrors what the builder writes: an events-backed row carries real totals
    and ``first_ts`` / ``last_ts``; a *seed* carries none of that, but may
    still carry the message-derived ``total_records``.
    """
    conn.execute(
        "INSERT INTO project_mart "
        "(project_id, provider, slug, display_name, first_ts, last_ts, "
        " total_messages, total_sessions, total_input_tokens, total_output_tokens, "
        " total_cache_read, total_cache_create, total_cost_usd, "
        " total_user_messages, total_assistant_messages, total_tool_use_messages, "
        " total_tool_result_messages, total_commands, total_records) "
        "VALUES (?, ?, ?, ?, ?, ?, ?, 0, ?, 0, 0, 0, ?, 0, 0, 0, 0, ?, ?)",
        (
            project_id,
            provider,
            slug,
            slug,
            kw.get("first_ts"),
            kw.get("last_ts"),
            kw.get("total_messages", 0),
            kw.get("total_input_tokens", 0),
            kw.get("total_cost_usd", 0.0),
            kw.get("total_commands", 0),
            kw.get("total_records", 0),
        ),
    )


def _insert_messages(conn, project_id, *, day="01"):
    """One user turn + one billable assistant turn, with real timestamps."""
    sid = conn.execute(
        "INSERT INTO sessions (project_id, session_id) VALUES (?, ?)",
        (project_id, f"s-{project_id}"),
    ).lastrowid
    conn.execute(
        "INSERT INTO messages (session_fk, seq, timestamp, role, raw_json) "
        f"VALUES (?, 0, '2026-05-{day}T10:00:00+00:00', 'user', '{{}}')",
        (sid,),
    )
    conn.execute(
        "INSERT INTO messages (session_fk, seq, timestamp, role, model, "
        " input_tokens, output_tokens, cache_create_tokens, cache_read_tokens, raw_json) "
        f"VALUES (?, 1, '2026-05-{day}T11:00:00+00:00', 'assistant', "
        "'claude-opus-4-6', 4000, 2000, 0, 0, '{}')",
        (sid,),
    )


def _seed_store(store_db):
    """One project of each coverage shape; returns ``{slug: project_id}``."""
    conn = _connect(store_db)
    events_backed = _insert_project(conn, slug=EVENTS_BACKED, last_modified=3.0)
    seeded_msgs = _insert_project(conn, slug=SEEDED_WITH_MESSAGES, last_modified=2.0)
    seeded_empty = _insert_project(conn, slug=SEEDED_EMPTY, last_modified=1.0)

    # (1) Events-backed: the ETL priced it, so the mart row is authoritative.
    _insert_project_mart(
        conn,
        project_id=events_backed,
        slug=EVENTS_BACKED,
        total_messages=8,
        total_records=8,
        total_input_tokens=12345,
        total_cost_usd=2.5,
        total_commands=7,
        first_ts="2026-04-01T00:00:00Z",
        last_ts="2026-04-30T00:00:00Z",
    )
    _insert_messages(conn, events_backed, day="09")

    # (2) Seeded but message-bearing: the all-zero seed plus the second pass'
    #     total_records. Real messages exist; the mart can't speak for them.
    _insert_project_mart(
        conn, project_id=seeded_msgs, slug=SEEDED_WITH_MESSAGES, total_records=2
    )
    _insert_messages(conn, seeded_msgs, day="14")

    # (3) Truly empty seed: no messages anywhere, so total_records stays 0.
    _insert_project_mart(conn, project_id=seeded_empty, slug=SEEDED_EMPTY)

    conn.commit()
    expected_cost = queries.bulk_project_cost(conn, project_ids=[seeded_msgs])[seeded_msgs]
    conn.close()
    assert expected_cost > 0.0
    return {
        EVENTS_BACKED: events_backed,
        SEEDED_WITH_MESSAGES: seeded_msgs,
        SEEDED_EMPTY: seeded_empty,
        "expected_cost": expected_cost,
    }


async def _payload_by_slug(monkeypatch, store_db):
    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)
    response = await get_projects(include_stats=True)
    body = json.loads(response.body.decode("utf-8"))
    return {p["dir_name"]: p for p in body["projects"]}


@pytest.mark.asyncio
async def test_seeded_row_with_messages_recovers_dates_and_commands(tmp_path, monkeypatch):
    """The whole point: an all-zero seed must not blank a real project."""
    store_db = tmp_path / "store.db"
    ids = _seed_store(store_db)
    by_slug = await _payload_by_slug(monkeypatch, store_db)

    stats = by_slug[SEEDED_WITH_MESSAGES]["stats"]
    assert stats["first_message_date"] == "2026-05-14T10:00:00+00:00"
    assert stats["last_message_date"] == "2026-05-14T11:00:00+00:00"
    assert stats["total_commands"] == 1
    assert stats["total_input_tokens"] == 4000
    assert stats["total_cost"] == pytest.approx(ids["expected_cost"])


@pytest.mark.asyncio
async def test_truly_empty_seed_stays_zero(tmp_path, monkeypatch):
    """``total_records == 0`` means there is nothing to recover — no dates."""
    store_db = tmp_path / "store.db"
    _seed_store(store_db)
    by_slug = await _payload_by_slug(monkeypatch, store_db)

    stats = by_slug[SEEDED_EMPTY]["stats"]
    assert stats["first_message_date"] is None
    assert stats["last_message_date"] is None
    assert stats["total_commands"] == 0
    assert stats["total_input_tokens"] == 0
    assert stats["total_cost"] == pytest.approx(0.0)


@pytest.mark.asyncio
async def test_events_backed_row_still_takes_the_mart_path(tmp_path, monkeypatch):
    """A populated mart row wins even though the project also has messages."""
    store_db = tmp_path / "store.db"
    _seed_store(store_db)
    by_slug = await _payload_by_slug(monkeypatch, store_db)

    stats = by_slug[EVENTS_BACKED]["stats"]
    # Mart figures, not the message rows' (4000 in / 2000 out / 1 command).
    assert stats["total_input_tokens"] == 12345
    assert stats["total_commands"] == 7
    assert stats["total_cost"] == pytest.approx(2.5)
    assert stats["first_message_date"] == "2026-04-01T00:00:00Z"


@pytest.mark.asyncio
async def test_bulk_helpers_are_scoped_to_the_placeholder_ids_only(tmp_path, monkeypatch):
    """The fallback stays scoped to exactly the recoverable ids.

    Neither the events-backed project (the mart answers for it) nor the truly
    empty seed (``total_records == 0`` — there is nothing to recover) may enter
    the message-scanning helpers' scope.
    """
    store_db = tmp_path / "store.db"
    ids = _seed_store(store_db)

    calls: dict[str, list] = {"lite": [], "cost": []}
    real_lite = queries.bulk_project_lite_stats
    real_cost = queries.bulk_project_cost

    def lite_spy(conn, *, project_ids=None):
        calls["lite"].append(set(project_ids) if project_ids is not None else None)
        return real_lite(conn, project_ids=project_ids)

    def cost_spy(conn, *, project_ids=None):
        calls["cost"].append(set(project_ids) if project_ids is not None else None)
        return real_cost(conn, project_ids=project_ids)

    monkeypatch.setattr(queries, "bulk_project_lite_stats", lite_spy)
    monkeypatch.setattr(queries, "bulk_project_cost", cost_spy)
    await _payload_by_slug(monkeypatch, store_db)

    expected = {ids[SEEDED_WITH_MESSAGES]}
    assert calls["lite"] == [expected]
    assert calls["cost"] == [expected]
    for scope in calls["lite"] + calls["cost"]:
        assert ids[EVENTS_BACKED] not in scope
        assert ids[SEEDED_EMPTY] not in scope
