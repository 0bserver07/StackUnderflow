"""COST-B concern 1 — ``/api/interaction/{id}`` memoizes the EnrichedDataset.

Every click on a command in the Messages tab rebuilt the whole project's
``EnrichedDataset``: 2,538 ms and ~740 MB transient on the 49K-message slug,
measured, paid again for every click and for every id that was never going to
match. The response only needs a linear scan (0.11-0.20 ms over 1,680
interactions) plus one ``_serialise_interaction``.

Session-scoping the rebuild was refuted (interactions span sessions; the
navigation channel drops session ids), so the fix is to memoize the dataset
itself under the same self-invalidating sessions signature ``_STATS_CACHE``
uses, LRU-bounded at 2 because each entry is a live object graph, not a dict.
"""

from __future__ import annotations

import pytest
from fastapi import HTTPException

from stackunderflow.routes.cost import (
    _DATASET_CACHE,
    _DATASET_CACHE_MAX,
    _invalidate_stats_cache,
    get_interaction,
)
from stackunderflow.stats.enricher import EnrichedDataset, Interaction, Record
from stackunderflow.store import db, schema


@pytest.fixture(autouse=True)
def _clear_memo():
    """Process-global memo — clear around every test so counters are exact."""
    _invalidate_stats_cache()
    yield
    _invalidate_stats_cache()


def _seed_project(store_db, slug: str) -> int:
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


def _record(kind: str, content: str, uuid: str) -> Record:
    return Record(
        session_id="sess-1",
        kind=kind,
        timestamp="2026-04-23T00:00:00Z",
        model="claude-sonnet-4-5",
        content=content,
        tokens={"input": 10, "output": 5, "cache_creation": 0, "cache_read": 0},
        tools=["Read"],
        is_error=False,
        error_category=None,
        is_interruption=False,
        has_tool_result=False,
        uuid=uuid,
        parent_uuid=None,
        is_sidechain=False,
        message_id=uuid,
        cwd="/tmp",
        raw_data={},
    )


def _dataset(interaction_id: str = "IX-1") -> EnrichedDataset:
    cmd = _record("user", "refactor this", "u1")
    resp = _record("assistant", "on it", "u2")
    ix = Interaction(
        interaction_id=interaction_id,
        command=cmd,
        responses=[resp],
        tool_results=[],
        session_id="sess-1",
        start_time=cmd.timestamp,
        end_time=resp.timestamp,
        model="claude-sonnet-4-5",
        tool_count=1,
        assistant_steps=1,
    )
    return EnrichedDataset(records=[cmd, resp], interactions=[ix], sessions={})


def _spy_builder(monkeypatch, dataset, counter: dict):
    def build(conn, *, project_id):  # noqa: ARG001
        counter["n"] += 1
        return dataset, "/fake/log-dir"

    monkeypatch.setattr("stackunderflow.routes.cost.queries.build_enriched_dataset", build)


@pytest.mark.asyncio
async def test_second_click_skips_the_rebuild(tmp_path, monkeypatch):
    """Two clicks, one build — the whole point. The second click's answer must
    be identical, not merely present."""
    store_db = tmp_path / "store.db"
    slug = "-ds-memo"
    _seed_project(store_db, slug)
    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)
    monkeypatch.setattr("stackunderflow.deps.current_log_path", f"/fake/{slug}")

    calls = {"n": 0}
    _spy_builder(monkeypatch, _dataset(), calls)

    first = await get_interaction("IX-1")
    second = await get_interaction("IX-1")

    assert calls["n"] == 1, "the warm click rebuilt the dataset"
    assert first == second
    assert first["command"]["content"] == "refactor this"
    assert first["responses"][0]["model"] == "claude-sonnet-4-5"


@pytest.mark.asyncio
async def test_handler_never_mutates_the_shared_dataset(tmp_path, monkeypatch):
    """The memo hands back the SHARED object graph — copying it would cost more
    than rebuilding. So the handler must only read: the serialised payload
    copies the mutable fields (``tools``, ``tokens``) rather than aliasing them,
    and mutating a response can never reach the cached dataset."""
    store_db = tmp_path / "store.db"
    slug = "-ds-nomutate"
    _seed_project(store_db, slug)
    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)
    monkeypatch.setattr("stackunderflow.deps.current_log_path", f"/fake/{slug}")

    dataset = _dataset()
    calls = {"n": 0}
    _spy_builder(monkeypatch, dataset, calls)

    first = await get_interaction("IX-1")
    first["tools_used"].append("Bash")
    first["command"]["tools"].append("Bash")
    first["command"]["tokens"]["input"] = 999_999
    first["responses"].clear()

    second = await get_interaction("IX-1")
    assert calls["n"] == 1
    assert second["command"]["tools"] == ["Read"]
    assert second["command"]["tokens"]["input"] == 10
    assert len(second["responses"]) == 1
    # …and the cached graph itself is untouched.
    assert dataset.interactions[0].command.tools == ["Read"]
    assert dataset.interactions[0].command.tokens["input"] == 10


@pytest.mark.asyncio
async def test_signature_move_rebuilds(tmp_path, monkeypatch):
    """Ingest writing a session row moves the sessions signature, so the next
    click must rebuild — a stale dataset would hide the messages the user just
    navigated to."""
    store_db = tmp_path / "store.db"
    slug = "-ds-sig"
    pid = _seed_project(store_db, slug)
    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)
    monkeypatch.setattr("stackunderflow.deps.current_log_path", f"/fake/{slug}")

    calls = {"n": 0}
    _spy_builder(monkeypatch, _dataset(), calls)

    await get_interaction("IX-1")
    await get_interaction("IX-1")
    assert calls["n"] == 1

    conn = db.connect(store_db)
    conn.execute(
        "INSERT INTO sessions (project_id, session_id, first_ts, last_ts, message_count) "
        "VALUES (?, 's-new', '2026-05-01T00:00:00Z', '2026-05-01T00:00:00Z', 7)",
        (pid,),
    )
    conn.commit()
    conn.close()

    await get_interaction("IX-1")
    assert calls["n"] == 2, "signature moved → must rebuild"


@pytest.mark.asyncio
async def test_missing_id_404s_without_rebuilding_on_a_warm_memo(tmp_path, monkeypatch):
    """A guaranteed miss is the worst case of the old code: the full rebuild,
    then a 404. On a warm memo it costs one linear scan and no build at all."""
    store_db = tmp_path / "store.db"
    slug = "-ds-miss"
    _seed_project(store_db, slug)
    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)
    monkeypatch.setattr("stackunderflow.deps.current_log_path", f"/fake/{slug}")

    calls = {"n": 0}
    _spy_builder(monkeypatch, _dataset(), calls)

    await get_interaction("IX-1")  # warms the memo
    for _ in range(5):
        with pytest.raises(HTTPException) as exc:
            await get_interaction("nope")
        assert exc.value.status_code == 404
        assert "nope" in exc.value.detail

    assert calls["n"] == 1, "misses re-ran the pipeline"


@pytest.mark.asyncio
async def test_cache_is_lru_bounded_at_two(tmp_path, monkeypatch):
    """Entries are whole object graphs (+739 MB resident for the 49K-message
    slug, 1,134 MB RSS with two held), so the cap is 2, not ``_STATS_CACHE``'s
    8 — and the entry evicted is the least recently USED, not the least
    recently inserted."""
    store_db = tmp_path / "store.db"
    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)

    slugs = [f"-ds-lru-{i}" for i in range(_DATASET_CACHE_MAX + 2)]
    for s in slugs:
        _seed_project(store_db, s)

    calls = {"n": 0}
    _spy_builder(monkeypatch, _dataset(), calls)

    for s in slugs:
        await get_interaction("IX-1", log_path=f"/fake/{s}")
        assert len(_DATASET_CACHE) <= _DATASET_CACHE_MAX

    assert [k[1] for k in _DATASET_CACHE] == slugs[-_DATASET_CACHE_MAX:]

    # Re-read the oldest survivor so it becomes the newest; the next insert
    # then evicts what is now the oldest instead of it.
    protected = slugs[-_DATASET_CACHE_MAX]
    doomed = slugs[-_DATASET_CACHE_MAX + 1]
    await get_interaction("IX-1", log_path=f"/fake/{protected}")
    _seed_project(store_db, "-ds-lru-final")
    await get_interaction("IX-1", log_path="/fake/-ds-lru-final")

    surviving = [k[1] for k in _DATASET_CACHE]
    assert protected in surviving, "move_to_end on a hit must protect a re-read entry"
    assert doomed not in surviving, "the LRU entry is the one evicted"


@pytest.mark.asyncio
async def test_invalidator_drops_datasets_too(tmp_path, monkeypatch):
    """A model-alias edit / refresh calls ``_invalidate_stats_cache``; that one
    entry point must clear BOTH memos, scoped the same way, or a new
    invalidation site silently wires up only half the caches."""
    store_db = tmp_path / "store.db"
    keep, drop = "-ds-keep", "-ds-drop"
    _seed_project(store_db, keep)
    _seed_project(store_db, drop)
    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)

    calls = {"n": 0}
    _spy_builder(monkeypatch, _dataset(), calls)

    await get_interaction("IX-1", log_path=f"/fake/{keep}")
    await get_interaction("IX-1", log_path=f"/fake/{drop}")
    assert calls["n"] == 2

    _invalidate_stats_cache(drop)  # per-slug scope, same predicate as the stats memo
    await get_interaction("IX-1", log_path=f"/fake/{keep}")
    assert calls["n"] == 2, "the untouched slug's dataset was dropped too"
    await get_interaction("IX-1", log_path=f"/fake/{drop}")
    assert calls["n"] == 3, "the invalidated slug kept serving a stale dataset"

    _invalidate_stats_cache()  # full clear
    assert not _DATASET_CACHE
    await get_interaction("IX-1", log_path=f"/fake/{keep}")
    assert calls["n"] == 4
