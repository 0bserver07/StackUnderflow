"""RANK 11 — the project-stats pipeline is memoized across the cost routes.

``/api/cost-data`` and ``/api/tool-distribution`` both need the full
``queries.get_project_stats`` sweep (1.4-4s on big projects). They must share
one memoized result keyed on (store, slug, tz_offset) + a sessions signature,
so the Overview tab stops paying for the same pipeline 2-3x. The signature
must also self-invalidate when ingest writes new rows.
"""

from __future__ import annotations

from unittest.mock import patch

import pytest

from stackunderflow.routes.commands import get_tool_distribution
from stackunderflow.routes.cost import (
    COST_KEYS,
    _invalidate_stats_cache,
    _project_stats_cached,
    _STATS_CACHE,
    _STATS_CACHE_MAX,
    get_cost_data,
)
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


# ── the memo invalidator is actually wired (stale-data fix) ──────────────────


@pytest.fixture
def isolated_settings(tmp_path):
    """Redirect ``Settings.persist`` I/O at a tmp config.json.

    ``_APP_DIR`` / ``_CFG_FILE`` are bound at import, so patching the module
    attributes is the only way to keep a route test off the real
    ``~/.stackunderflow/config.json``.
    """
    app_dir = tmp_path / "cfg-home"
    app_dir.mkdir(parents=True, exist_ok=True)
    with (
        patch("stackunderflow.settings._APP_DIR", app_dir),
        patch("stackunderflow.settings._CFG_FILE", app_dir / "config.json"),
    ):
        yield app_dir


@pytest.mark.asyncio
async def test_model_alias_write_busts_the_stats_memo(tmp_path, monkeypatch, isolated_settings):
    """A model-alias edit changes how the pipeline GROUPS models, but it does
    not move the sessions signature — so the memo can't self-invalidate.

    ``_invalidate_stats_cache`` existed for exactly this and had zero production
    callers: editing an alias dropped the dashboard cache (routes/cfg.py already
    called ``invalidate_dashboard_cache``) while ``/api/cost-data`` kept serving
    pre-alias aggregation until the next ingest. Both cfg write paths must now
    drop both caches.
    """
    from stackunderflow.routes.cfg import delete_model_alias, set_model_alias

    store_db = tmp_path / "store.db"
    slug = "-memo-alias"
    _seed_project(store_db, slug)
    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)
    monkeypatch.setattr("stackunderflow.deps.current_log_path", f"/fake/{slug}")

    calls = {"n": 0}

    def counting_stats(conn, *, project_id, tz_offset=0):  # noqa: ARG001
        calls["n"] += 1
        return [], _fake_stats()

    monkeypatch.setattr("stackunderflow.routes.cost.queries.get_project_stats", counting_stats)

    await get_cost_data()
    await get_cost_data()
    assert calls["n"] == 1  # warm

    await set_model_alias({"from": "proxy/opus", "to": "opus-4-8"})
    await get_cost_data()
    assert calls["n"] == 2, "alias SET must drop the memo, not just the dashboard cache"

    await get_cost_data()
    assert calls["n"] == 2  # warm again

    await delete_model_alias(src="proxy/opus")
    await get_cost_data()
    assert calls["n"] == 3, "alias DELETE must drop the memo too"


@pytest.mark.asyncio
async def test_refresh_paths_drop_the_stats_memo(tmp_path, monkeypatch):
    """The two ``/api/refresh`` handlers invalidate the dashboard cache; the
    stats memo now rides along at the same two sites, with matching scope —
    per-slug for the single-project refresh, full clear for refresh-all."""
    from stackunderflow.routes import data as data_routes

    store_db = tmp_path / "store.db"
    slug = "-memo-refresh"
    _seed_project(store_db, slug)
    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)
    monkeypatch.setattr("stackunderflow.deps.current_log_path", f"/fake/{slug}")

    calls = {"n": 0}

    def counting_stats(conn, *, project_id, tz_offset=0):  # noqa: ARG001
        calls["n"] += 1
        return [], _fake_stats()

    monkeypatch.setattr("stackunderflow.routes.cost.queries.get_project_stats", counting_stats)
    # Pretend ingest found new rows for this slug without actually writing any,
    # so the sessions signature stays put and ONLY the explicit invalidation can
    # bust the memo.
    monkeypatch.setattr(data_routes, "run_ingest", lambda conn, adapters: {slug: 3})
    monkeypatch.setattr(data_routes, "registered", lambda: {})
    monkeypatch.setattr(data_routes, "_reindex_services", lambda *a, **kw: None)
    # ``get_project_messages`` runs ``get_project_stats`` internally (queries.py),
    # so leaving it live would add a phantom tick to the counter from the
    # reindex block rather than from the memo.
    monkeypatch.setattr(
        "stackunderflow.routes.data.queries.get_project_messages",
        lambda conn, *, project_id, limit=None: [],
    )

    await get_cost_data()
    await get_cost_data()
    assert calls["n"] == 1

    await data_routes.refresh_data({})
    await get_cost_data()
    assert calls["n"] == 2

    await get_cost_data()
    assert calls["n"] == 2
    await data_routes.refresh_all_projects({})
    await get_cost_data()
    assert calls["n"] == 3


# ── COST-2: keys= narrows the copy without ever aliasing the cache ──────────


def _seed_memo(store_db, slug, monkeypatch, stats: dict | None = None):
    """Run one uncached sweep so ``_STATS_CACHE`` holds ``stats`` for ``slug``."""
    pid = _seed_project(store_db, slug)
    payload = stats if stats is not None else _fake_stats()
    monkeypatch.setattr(
        "stackunderflow.routes.cost.queries.get_project_stats",
        lambda conn, *, project_id, tz_offset=0: ([], payload),
    )
    conn = db.connect(store_db)
    try:
        _project_stats_cached(conn, project_ids=[pid], slug=slug, tz_offset=0)
    finally:
        conn.close()
    return pid


def test_keys_returns_only_requested_keys(tmp_path, monkeypatch):
    """Rule 3: unrequested keys are OMITTED, never handed back as references."""
    store_db = tmp_path / "store.db"
    slug = "-keys-subset"
    pid = _seed_memo(store_db, slug, monkeypatch)
    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)

    conn = db.connect(store_db)
    try:
        got = _project_stats_cached(
            conn, project_ids=[pid], slug=slug, tz_offset=0, keys=("user_interactions",)
        )
    finally:
        conn.close()

    assert set(got) == {"user_interactions"}
    assert "session_costs" not in got
    assert "tool_costs" not in got


def test_keys_subtrees_are_deep_copied(tmp_path, monkeypatch):
    """Rule 2: mutating a returned subtree must not reach the shared entry.

    The mart overlay rebinds ``token_composition`` fields in place and
    ``_convert_in_place`` rewrites every cost leaf under a non-USD currency —
    both act on whatever this returns.
    """
    store_db = tmp_path / "store.db"
    slug = "-keys-deepcopy"
    stats = _fake_stats()
    stats["tool_costs"] = {"Read": {"calls": 5, "cost": 1.0}}
    pid = _seed_memo(store_db, slug, monkeypatch, stats)
    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)

    conn = db.connect(store_db)
    try:
        first = _project_stats_cached(conn, project_ids=[pid], slug=slug, tz_offset=0, keys=COST_KEYS)
        first["tool_costs"]["Read"]["cost"] = 999.0
        first["token_composition"]["totals"]["input"] = 12345
        first["_tool_costs_windowed"] = True  # rule 1: the outer dict is ours

        second = _project_stats_cached(conn, project_ids=[pid], slug=slug, tz_offset=0, keys=COST_KEYS)
    finally:
        conn.close()

    assert second["tool_costs"]["Read"]["cost"] == 1.0
    assert second["token_composition"]["totals"] == {}
    assert "_tool_costs_windowed" not in second


@pytest.mark.asyncio
async def test_cost_data_narrows_its_copy_to_cost_keys(tmp_path, monkeypatch):
    """``/api/cost-data`` reads only COST_KEYS, so that's all it copies — while
    still SHARING the full cached entry with the other two consumers."""
    store_db = tmp_path / "store.db"
    slug = "-keys-route"
    _seed_project(store_db, slug)
    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)
    monkeypatch.setattr("stackunderflow.deps.current_log_path", f"/fake/{slug}")

    seen: list[tuple[str, ...] | None] = []
    real = _project_stats_cached

    def spy(conn, **kw):
        seen.append(kw.get("keys"))
        return real(conn, **kw)

    monkeypatch.setattr(
        "stackunderflow.routes.cost.queries.get_project_stats",
        lambda conn, *, project_id, tz_offset=0: ([], _fake_stats()),
    )
    monkeypatch.setattr("stackunderflow.routes.cost._project_stats_cached", spy)

    payload = await get_cost_data()
    assert seen == [COST_KEYS]
    # The route fills its own shape-stable defaults, so the body still has
    # every COST_KEY plus the two envelope fields — and nothing else.
    assert set(payload) == {*COST_KEYS, "currency", "tool_costs_windowed"}
    assert "user_interactions" not in payload

    # The cached entry is still the FULL stats dict — tool-distribution reads
    # user_interactions off it with no recompute.
    td = await get_tool_distribution()
    assert td == {"tool_count_distribution": {"0": 1, "3": 2}}


# ── COST-5b: the memo is bounded and the tz offset is clamped ───────────────


def test_stats_cache_is_lru_bounded(tmp_path, monkeypatch):
    """Entries are 5.5-19 MB each; an unbounded dict was a slow leak. Cap holds
    and the least-recently-used entry is the one evicted."""
    store_db = tmp_path / "store.db"
    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)
    monkeypatch.setattr(
        "stackunderflow.routes.cost.queries.get_project_stats",
        lambda conn, *, project_id, tz_offset=0: ([], _fake_stats()),
    )

    pids = [_seed_project(store_db, f"-lru-{i}") for i in range(_STATS_CACHE_MAX + 3)]
    conn = db.connect(store_db)
    try:
        for i, pid in enumerate(pids):
            _project_stats_cached(conn, project_ids=[pid], slug=f"-lru-{i}", tz_offset=0)
            assert len(_STATS_CACHE) <= _STATS_CACHE_MAX

        slugs = [k[1] for k in _STATS_CACHE]
        assert len(slugs) == _STATS_CACHE_MAX
        # First three inserted are gone; the most recent MAX survive in order.
        assert slugs == [f"-lru-{i}" for i in range(3, _STATS_CACHE_MAX + 3)]

        # Touching the oldest survivor makes it the newest, so the NEXT
        # insertion evicts what is now the oldest instead.
        _project_stats_cached(conn, project_ids=[pids[3]], slug="-lru-3", tz_offset=0)
        new_pid = _seed_project(store_db, "-lru-final")
        _project_stats_cached(conn, project_ids=[new_pid], slug="-lru-final", tz_offset=0)
    finally:
        conn.close()

    surviving = [k[1] for k in _STATS_CACHE]
    assert "-lru-3" in surviving, "move_to_end on a hit must protect a re-read entry"
    assert "-lru-4" not in surviving, "the LRU entry is the one evicted"


def test_absurd_timezone_offsets_clamp_to_one_shared_entry(tmp_path, monkeypatch):
    """``timezone_offset`` is an unvalidated client int. Unclamped, every value
    minted a fresh multi-MB entry. Real offsets span UTC-12:00 … UTC+14:00
    (minutes EAST of UTC), so anything outside collapses onto the boundary."""
    store_db = tmp_path / "store.db"
    slug = "-tz-clamp"
    pid = _seed_project(store_db, slug)
    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)

    calls = {"n": 0}

    def counting_stats(conn, *, project_id, tz_offset=0):
        calls["n"] += 1
        assert -720 <= tz_offset <= 840, f"unclamped tz reached the pipeline: {tz_offset}"
        return [], _fake_stats()

    monkeypatch.setattr("stackunderflow.routes.cost.queries.get_project_stats", counting_stats)

    conn = db.connect(store_db)
    try:
        for absurd in (999_999, 10**9, 5000):
            _project_stats_cached(conn, project_ids=[pid], slug=slug, tz_offset=absurd)
        for absurd in (-999_999, -(10**9), -5000):
            _project_stats_cached(conn, project_ids=[pid], slug=slug, tz_offset=absurd)
        # A real offset still gets its own entry.
        _project_stats_cached(conn, project_ids=[pid], slug=slug, tz_offset=60)
    finally:
        conn.close()

    # Three entries total: +840 (all the huge positives), -720 (all the huge
    # negatives), +60 — not seven.
    assert calls["n"] == 3
    assert sorted(k[2] for k in _STATS_CACHE) == [-720, 60, 840]
