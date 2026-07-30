"""RANK 11 — the project-stats pipeline is memoized across the cost routes.

``/api/cost-data`` and ``/api/tool-distribution`` both need the full
``queries.get_project_stats`` sweep (1.4-4s on big projects). They must share
one memoized result keyed on (store, slug, tz_offset) + a sessions signature,
so the Overview tab stops paying for the same pipeline 2-3x. The signature
must also self-invalidate when ingest writes new rows.
"""

from __future__ import annotations

import pytest

from stackunderflow.routes.commands import get_tool_distribution
from stackunderflow.routes.cost import _invalidate_stats_cache, get_cost_data
from stackunderflow.store import db, schema


@pytest.fixture(autouse=True)
def _clear_memo():
    """Process-global memo — clear around every test so counters are exact."""
    _invalidate_stats_cache()
    yield
    _invalidate_stats_cache()


def _seed_project(store_db, slug: str):
    conn = db.connect(store_db)
    schema.apply(conn)
    cur = conn.execute(
        "INSERT INTO projects (provider, slug, display_name, first_seen, last_modified) VALUES (?, ?, ?, ?, ?)",
        ("claude", slug, slug, 0.0, 0.0),
    )
    pid = cur.lastrowid
    conn.commit()
    conn.close()
    return pid


def _fake_stats() -> dict:
    return {
        "session_costs": [],
        "command_costs": [],
        "tool_costs": {},
        "token_composition": {"daily": {}, "totals": {}, "per_session": {}},
        "outliers": {},
        "retry_signals": [],
        "session_efficiency": [],
        "error_cost": {},
        "trends": {},
        "user_interactions": {"tool_count_distribution": {"0": 1, "3": 2}},
    }


@pytest.mark.asyncio
async def test_repeat_cost_data_calls_skip_recompute(tmp_path, monkeypatch):
    store_db = tmp_path / "store.db"
    slug = "-memo-proj"
    _seed_project(store_db, slug)
    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)
    monkeypatch.setattr("stackunderflow.deps.current_log_path", f"/fake/{slug}")

    calls = {"n": 0}

    def counting_stats(conn, *, project_id, tz_offset=0):  # noqa: ARG001
        calls["n"] += 1
        return [], _fake_stats()

    monkeypatch.setattr("stackunderflow.routes.cost.queries.get_project_stats", counting_stats)

    first = await get_cost_data()
    second = await get_cost_data()
    # Pipeline ran once; the 2nd call came from the memo.
    assert calls["n"] == 1
    # Cached path returns an equivalent payload (deep-copied, not aliased).
    assert first["session_costs"] == second["session_costs"]


@pytest.mark.asyncio
async def test_cost_data_and_tool_distribution_share_one_sweep(tmp_path, monkeypatch):
    """The two routes key on the same (store, slug, tz) entry, so the second
    route to run reuses the first's pipeline output."""
    store_db = tmp_path / "store.db"
    slug = "-memo-shared"
    _seed_project(store_db, slug)
    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)
    monkeypatch.setattr("stackunderflow.deps.current_log_path", f"/fake/{slug}")

    calls = {"n": 0}

    def counting_stats(conn, *, project_id, tz_offset=0):  # noqa: ARG001
        calls["n"] += 1
        return [], _fake_stats()

    monkeypatch.setattr("stackunderflow.routes.cost.queries.get_project_stats", counting_stats)

    await get_cost_data()
    td = await get_tool_distribution()

    assert calls["n"] == 1  # tool-distribution reused cost-data's sweep
    assert td == {"tool_count_distribution": {"0": 1, "3": 2}}


@pytest.mark.asyncio
async def test_memo_busts_when_sessions_signature_changes(tmp_path, monkeypatch):
    """New ingest (a fresh session row bumps message_count) must invalidate the
    memo — staleness can't outlive a refresh."""
    store_db = tmp_path / "store.db"
    slug = "-memo-bust"
    pid = _seed_project(store_db, slug)
    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)
    monkeypatch.setattr("stackunderflow.deps.current_log_path", f"/fake/{slug}")

    calls = {"n": 0}

    def counting_stats(conn, *, project_id, tz_offset=0):  # noqa: ARG001
        calls["n"] += 1
        return [], _fake_stats()

    monkeypatch.setattr("stackunderflow.routes.cost.queries.get_project_stats", counting_stats)

    await get_cost_data()
    await get_cost_data()
    assert calls["n"] == 1  # cached

    # Simulate ingest writing a new session (changes the signature).
    conn = db.connect(store_db)
    conn.execute(
        "INSERT INTO sessions (project_id, session_id, first_ts, last_ts, message_count) "
        "VALUES (?, 's-new', '2026-05-01T00:00:00Z', '2026-05-01T00:00:00Z', 7)",
        (pid,),
    )
    conn.commit()
    conn.close()

    await get_cost_data()
    assert calls["n"] == 2  # signature moved → recomputed


@pytest.mark.asyncio
async def test_api_stats_shares_the_memoized_sweep(tmp_path, monkeypatch):
    """``/api/stats`` was the last consumer still recomputing the pipeline on
    EVERY call — ~4s per request on big projects with zero warm benefit while
    ``/api/cost-data`` sat at 84ms warm. It now rides the same memo entry, and
    its in-place trims (heavy-block strip, currency, include filter) must act
    on the returned copy, never the shared entry."""
    store_db = tmp_path / "store.db"
    slug = "-memo-stats"
    _seed_project(store_db, slug)
    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)
    monkeypatch.setattr("stackunderflow.deps.current_log_path", f"/fake/{slug}")

    calls = {"n": 0}

    def counting_stats(conn, *, project_id, tz_offset=0):  # noqa: ARG001
        calls["n"] += 1
        return [], _fake_stats()

    monkeypatch.setattr("stackunderflow.routes.cost.queries.get_project_stats", counting_stats)

    from stackunderflow.routes.data import get_stats

    first = await get_stats()
    second = await get_stats()
    assert calls["n"] == 1  # warm /api/stats call came from the memo
    # details=False strips the heavy nested lists on the returned copy …
    assert first["user_interactions"]["tool_count_distribution"] == {}
    assert second["user_interactions"]["tool_count_distribution"] == {}

    # … but the SHARED entry must stay intact: tool-distribution reads the
    # same memo afterwards and must still see the full distribution, without
    # a recompute. A stripped shared entry here is the poisoning bug.
    td = await get_tool_distribution()
    assert calls["n"] == 1
    assert td == {"tool_count_distribution": {"0": 1, "3": 2}}
