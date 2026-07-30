"""P0.1 — ``GET /api/projects`` must never run an unfiltered bulk aggregate.

``project_mart`` covers most but not all projects (91 of 334 on a real
382K-message store, most of them with no messages at all). The uncovered ones
fall back to ``bulk_project_lite_stats`` / ``bulk_project_cost``, and those
helpers used to be called with no id filter — so a single mart-less project
made every ``?include_stats=true`` request GROUP BY over every message row in
the store and hang past 180s. One uncovered project poisoned the whole
request.

Scoping them to "every mart-uncovered project in the store" fixed the hang but
kept the request coupled to the whole store: an uncovered high-traffic project
400 rows down the list still cost ~250ms on every page. The scope is now the
**page's** uncovered ids — computed after the slice, against a mart read that
is itself scoped to the page — so an off-page project costs nothing at all.

These tests pin that contract at the route boundary: the helpers are called
with exactly the ids *on the returned page* that the route can't answer from
the mart, or not at all. All stores are ``tmp_path`` — never the real one.
"""

from __future__ import annotations

import json

import pytest

from stackunderflow.routes.projects import _fragment_costs_usd, get_projects
from stackunderflow.store import db, mart_queries, queries, schema

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
async def test_bulk_helpers_are_scoped_to_the_pages_mart_uncovered_ids(tmp_path, monkeypatch):
    """The scope is the *page's* uncovered ids, not the store's.

    This store is small enough that the default (uncapped) page IS every
    project, so the two coincide here — deliberately, so the assertion below
    keeps pinning the original P0.1 contract. What distinguishes the two is
    pinned by ``test_off_page_uncovered_project_is_never_priced``.
    """
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
    body = json.loads((await get_projects(include_stats=True)).body.decode("utf-8"))

    page_uncovered = {uncovered}
    assert len(body["projects"]) == 3, "whole store on one page — see the docstring"
    assert [set(ids) for ids in calls["lite"]] == [page_uncovered]
    assert [set(ids) for ids in calls["cost"]] == [page_uncovered]
    # The mart-covered ids were never handed to a message-scanning helper.
    assert all(covered_a not in ids and covered_b not in ids for ids in calls["lite"])


@pytest.mark.asyncio
async def test_off_page_uncovered_project_is_never_priced(tmp_path, monkeypatch):
    """An uncovered project the caller didn't ask for costs exactly nothing.

    The regression this pins: ``uncovered_ids`` used to be derived from every
    project row *before* the page slice, so one mart-less high-traffic project
    made every page pay its bulk-helper scan (~250ms measured for a single
    slug on the maintainer's store) even when it wasn't in the response.
    """
    store_db = tmp_path / "store.db"
    conn = _connect(store_db)
    covered_a = _insert_project(conn, slug="-covered-a", last_modified=3.0)
    covered_b = _insert_project(conn, slug="-covered-b", last_modified=2.0)
    uncovered = _insert_project(conn, slug="-uncovered", last_modified=1.0)
    _insert_project_mart(conn, project_id=covered_a, slug="-covered-a", total_cost_usd=1.0)
    _insert_project_mart(conn, project_id=covered_b, slug="-covered-b", total_cost_usd=2.0)
    _insert_billable_message(conn, uncovered)
    conn.commit()
    conn.close()
    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)

    # Page 1 is fully mart-covered; the uncovered project sorts onto page 2.
    calls = _spy_on_bulk_helpers(monkeypatch)
    page1 = json.loads((await get_projects(include_stats=True, limit=2)).body.decode("utf-8"))
    assert [p["dir_name"] for p in page1["projects"]] == ["-covered-a", "-covered-b"]
    assert page1["total_count"] == 3, "the off-page row still counts for the pager"
    assert calls == {"lite": [], "cost": []}, (
        f"off-page uncovered project was priced anyway: {calls}"
    )

    # ...and the cost appears exactly when the caller asks for its page.
    calls2 = _spy_on_bulk_helpers(monkeypatch)
    page2 = json.loads(
        (await get_projects(include_stats=True, limit=2, offset=2)).body.decode("utf-8")
    )
    assert [p["dir_name"] for p in page2["projects"]] == ["-uncovered"]
    assert [set(ids) for ids in calls2["lite"]] == [{uncovered}]
    assert [set(ids) for ids in calls2["cost"]] == [{uncovered}]
    assert page2["projects"][0]["stats"]["total_cost"] > 0.0


@pytest.mark.asyncio
async def test_mart_read_covers_every_id_uncovered_is_computed_against(tmp_path, monkeypatch):
    """PROJ-5 — the provider-filtered mart read still spans the whole page.

    ``uncovered = page_ids - mart_rows.keys()`` is only meaningful if the mart
    was actually *asked* about every page id. Narrowing that read (by ids, by
    provider, or both) below the page would silently reclassify never-asked
    ids as "uncovered" and send them to the message-scanning fallback. So:
    the mart read's scope must be a superset of the ids the fallback then
    receives, and equal to the page's own ids.
    """
    store_db = tmp_path / "store.db"
    conn = _connect(store_db)
    covered = _insert_project(conn, slug="-covered", last_modified=3.0)
    uncovered = _insert_project(conn, slug="-uncovered", last_modified=2.0)
    other_provider = _insert_project(conn, slug="-codex-only", provider="codex", last_modified=1.0)
    _insert_project_mart(
        conn, project_id=covered, slug="-covered", total_cost_usd=1.0, total_input_tokens=77
    )
    _insert_project_mart(
        conn, project_id=other_provider, slug="-codex-only", provider="codex", total_cost_usd=9.0
    )
    _insert_billable_message(conn, uncovered)
    conn.commit()
    conn.close()

    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)
    calls = _spy_on_bulk_helpers(monkeypatch)
    mart_scopes: list[set[int] | None] = []
    real_list_mart = mart_queries.list_project_mart

    def mart_spy(conn, *, provider_filter=None, project_ids=None):
        mart_scopes.append(set(project_ids) if project_ids is not None else None)
        return real_list_mart(conn, provider_filter=provider_filter, project_ids=project_ids)

    monkeypatch.setattr(mart_queries, "list_project_mart", mart_spy)
    body = json.loads(
        (await get_projects(include_stats=True, provider=["claude"])).body.decode("utf-8")
    )

    page_ids = {covered, uncovered}
    assert mart_scopes == [page_ids], "the mart read must span exactly the page"
    for scope in calls["lite"] + calls["cost"]:
        assert set(scope) <= mart_scopes[0], (
            "the fallback was handed ids the mart was never asked about"
        )
    # The provider filter on the mart read did not drop a row it should keep.
    by_slug = {p["dir_name"]: p for p in body["projects"]}
    assert set(by_slug) == {"-covered", "-uncovered"}
    assert by_slug["-covered"]["stats"]["total_input_tokens"] == 77


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


# ── the fragment-cost fills key off coverage, and MERGE ──────────────────────
#
# ``_fragment_costs_usd`` used to gate both of its lazy loads on "did someone
# else already load this?" flags (``not mart_loaded`` / ``not cost_by_pid``).
# With the page-scoped ordering the caller's dicts are partial by construction,
# so those flags now mean the opposite of what they claimed: they would skip a
# load the fragments still need and roll a fabricated 0.0 into ``worktree_cost``
# — which then gets FX-converted like a real number.


def _one_fragment_fold(*frags: tuple[str, list[int]]) -> dict[str, list[dict]]:
    """A ``folded`` mapping: one parent, N fragment rows with the given ids."""
    return {PARENT: [{"dir_name": name, "_ids": ids} for name, ids in frags]}


def test_fragment_cost_fill_merges_into_a_preseeded_cost_map(tmp_path):
    """A pre-seeded id keeps its cost; the missing one is fetched alongside.

    Rebinding (``cost_by_pid = bulk_project_cost(...)``) instead of merging
    would drop the pre-seeded entry and roll that fragment up as 0.0.
    """
    conn = _connect(tmp_path / "store.db")
    priced = _insert_project(conn, slug=FRAGMENT)
    other = _insert_project(conn, slug=FRAGMENT + "-2")
    _insert_billable_message(conn, other)
    conn.commit()

    costs = _fragment_costs_usd(
        conn,
        _one_fragment_fold((FRAGMENT, [priced]), (FRAGMENT + "-2", [other])),
        mart_rows={},
        cost_by_pid={priced: 7.5},
    )
    conn.close()

    assert costs[FRAGMENT] == pytest.approx(7.5), "pre-seeded cost was replaced, not merged"
    assert costs[FRAGMENT + "-2"] > 0.0, "the missing fragment was never priced"


def test_fragment_mart_read_is_scoped_to_the_ids_not_already_covered(tmp_path, monkeypatch):
    """The mart fill triggers on missing coverage, never on a "loaded" flag."""
    conn = _connect(tmp_path / "store.db")
    already = _insert_project(conn, slug=FRAGMENT)
    absent = _insert_project(conn, slug=FRAGMENT + "-2")
    _insert_project_mart(conn, project_id=absent, slug=FRAGMENT + "-2", total_cost_usd=3.0)
    conn.commit()

    scopes: list[list[int] | None] = []
    real_list_mart = mart_queries.list_project_mart

    def mart_spy(conn, *, provider_filter=None, project_ids=None):
        scopes.append(None if project_ids is None else sorted(project_ids))
        return real_list_mart(conn, provider_filter=provider_filter, project_ids=project_ids)

    monkeypatch.setattr(mart_queries, "list_project_mart", mart_spy)
    preloaded = {already: {"project_id": already, "total_cost_usd": 1.25, "total_records": 4, "total_messages": 4}}
    costs = _fragment_costs_usd(
        conn,
        _one_fragment_fold((FRAGMENT, [already]), (FRAGMENT + "-2", [absent])),
        mart_rows=preloaded,
        cost_by_pid={},
    )
    conn.close()

    assert scopes == [[absent]], "the mart was re-read for an id already covered"
    assert costs[FRAGMENT] == pytest.approx(1.25)  # from the caller's dict
    assert costs[FRAGMENT + "-2"] == pytest.approx(3.0)  # from the scoped fill
