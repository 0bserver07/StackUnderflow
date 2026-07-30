"""P0.1 — ``GET /api/projects`` must never run an unfiltered bulk aggregate.

``project_mart`` covers most but not all projects (91 of 334 on a real
382K-message store, most of them with no messages at all). The uncovered ones
fall back to ``bulk_project_lite_stats`` / ``bulk_project_cost``, and those
helpers used to be called with no id filter — so a single mart-less project
made every ``?include_stats=true`` request GROUP BY over every message row in
the store and hang past 180s. One uncovered project poisoned the whole
request.

These tests pin the contract at the route boundary: the helpers are called
with exactly the ids the route can't answer from the mart, or not at all.
All stores are ``tmp_path`` — never the real one.
"""

from __future__ import annotations

import json

import pytest

from stackunderflow.routes.projects import get_projects
from stackunderflow.store import db, queries, schema

PARENT = "-Users-me-dev-repo"
FRAGMENT = "-Users-me-dev-repo--claude-worktrees-todo"


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


def _insert_billable_message(conn, project_id, *, model="claude-opus-4-6", tokens=1000):
    sid = conn.execute(
        "INSERT INTO sessions (project_id, session_id) VALUES (?, ?)",
        (project_id, f"s-{project_id}"),
    ).lastrowid
    conn.execute(
        "INSERT INTO messages (session_fk, seq, timestamp, role, model, "
        " input_tokens, output_tokens, cache_create_tokens, cache_read_tokens, raw_json) "
        "VALUES (?, 0, '2026-05-01T00:00:00Z', 'assistant', ?, ?, ?, 0, 0, '{}')",
        (sid, model, tokens, tokens // 2),
    )


def _insert_project_mart(conn, *, project_id, slug, provider="claude", total_cost_usd=0.0, **kw):
    conn.execute(
        "INSERT INTO project_mart "
        "(project_id, provider, slug, display_name, first_ts, last_ts, "
        " total_messages, total_sessions, total_input_tokens, total_output_tokens, "
        " total_cache_read, total_cache_create, total_cost_usd, "
        " total_user_messages, total_assistant_messages, total_tool_use_messages, "
        " total_tool_result_messages, total_commands) "
        "VALUES (?, ?, ?, ?, NULL, NULL, 0, 0, ?, 0, 0, 0, ?, 0, 0, 0, 0, 0)",
        (project_id, provider, slug, slug, kw.get("total_input_tokens", 0), total_cost_usd),
    )


def _spy_on_bulk_helpers(monkeypatch) -> dict[str, list]:
    """Record the ``project_ids`` every bulk-helper call was scoped to."""
    calls: dict[str, list] = {"lite": [], "cost": []}
    real_lite = queries.bulk_project_lite_stats
    real_cost = queries.bulk_project_cost

    def lite_spy(conn, *, project_ids=None):
        calls["lite"].append(project_ids)
        return real_lite(conn, project_ids=project_ids)

    def cost_spy(conn, *, project_ids=None):
        calls["cost"].append(project_ids)
        return real_cost(conn, project_ids=project_ids)

    monkeypatch.setattr(queries, "bulk_project_lite_stats", lite_spy)
    monkeypatch.setattr(queries, "bulk_project_cost", cost_spy)
    return calls


@pytest.mark.asyncio
async def test_bulk_helpers_are_scoped_to_the_mart_uncovered_ids(tmp_path, monkeypatch):
    store_db = tmp_path / "store.db"
    conn = _connect(store_db)
    covered_a = _insert_project(conn, slug="-covered-a")
    covered_b = _insert_project(conn, slug="-covered-b")
    uncovered = _insert_project(conn, slug="-uncovered")
    _insert_project_mart(conn, project_id=covered_a, slug="-covered-a", total_cost_usd=1.0)
    _insert_project_mart(conn, project_id=covered_b, slug="-covered-b", total_cost_usd=2.0)
    _insert_billable_message(conn, uncovered)
    conn.commit()
    conn.close()

    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)
    calls = _spy_on_bulk_helpers(monkeypatch)
    await get_projects(include_stats=True)

    assert [set(ids) for ids in calls["lite"]] == [{uncovered}]
    assert [set(ids) for ids in calls["cost"]] == [{uncovered}]
    # The mart-covered ids were never handed to a message-scanning helper.
    assert all(covered_a not in ids and covered_b not in ids for ids in calls["lite"])


@pytest.mark.asyncio
async def test_scoping_does_not_change_the_payload(tmp_path, monkeypatch):
    """The uncovered project still reports real numbers after scoping."""
    store_db = tmp_path / "store.db"
    conn = _connect(store_db)
    covered = _insert_project(conn, slug="-covered")
    uncovered = _insert_project(conn, slug="-uncovered")
    _insert_project_mart(
        conn, project_id=covered, slug="-covered", total_cost_usd=1.5, total_input_tokens=99
    )
    _insert_billable_message(conn, uncovered, tokens=4000)
    conn.commit()
    expected_cost = queries.bulk_project_cost(conn, project_ids=[uncovered])[uncovered]
    conn.close()
    assert expected_cost > 0.0

    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)
    response = await get_projects(include_stats=True)
    body = json.loads(response.body.decode("utf-8"))
    by_slug = {p["dir_name"]: p for p in body["projects"]}
    assert by_slug["-covered"]["stats"]["total_input_tokens"] == 99
    assert by_slug["-covered"]["stats"]["total_cost"] == pytest.approx(1.5)
    assert by_slug["-uncovered"]["stats"]["total_input_tokens"] == 4000
    assert by_slug["-uncovered"]["stats"]["total_cost"] == pytest.approx(expected_cost)


@pytest.mark.asyncio
async def test_full_mart_coverage_never_touches_the_bulk_helpers(tmp_path, monkeypatch):
    store_db = tmp_path / "store.db"
    conn = _connect(store_db)
    for slug in ("-a", "-b"):
        pid = _insert_project(conn, slug=slug)
        _insert_project_mart(conn, project_id=pid, slug=slug, total_cost_usd=1.0)
    conn.commit()
    conn.close()

    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)
    calls = _spy_on_bulk_helpers(monkeypatch)
    await get_projects(include_stats=True)
    assert calls == {"lite": [], "cost": []}


@pytest.mark.asyncio
async def test_worktree_fragment_fallback_is_scoped_to_the_fragment_ids(tmp_path, monkeypatch):
    """The ``include_stats=false`` roll-up path had the same unscoped scan.

    ``_fragment_costs_usd`` re-priced every message in the store just to
    answer "what did this worktree fragment cost?" whenever the fragment had
    no ``project_mart`` row.
    """
    store_db = tmp_path / "store.db"
    conn = _connect(store_db)
    parent = _insert_project(conn, slug=PARENT, last_modified=2.0)
    fragment = _insert_project(conn, slug=FRAGMENT, last_modified=1.0)
    other = _insert_project(conn, slug="-unrelated", last_modified=3.0)
    _insert_project_mart(conn, project_id=parent, slug=PARENT, total_cost_usd=5.0)
    _insert_billable_message(conn, fragment)
    _insert_billable_message(conn, other)
    conn.commit()
    conn.close()

    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)
    calls = _spy_on_bulk_helpers(monkeypatch)
    response = await get_projects(include_stats=False)
    body = json.loads(response.body.decode("utf-8"))

    assert [set(ids) for ids in calls["cost"]] == [{fragment}]
    assert calls["lite"] == []
    folded = {p["dir_name"] for p in body["projects"]}
    assert FRAGMENT not in folded and PARENT in folded
    parent_row = next(p for p in body["projects"] if p["dir_name"] == PARENT)
    assert parent_row["worktree_cost"] > 0.0
