"""Wave 3A — ``/api/dashboard-data`` reads from ``project_mart`` + ``daily_mart``.

The later tests in this module also cover the multi-provider mart fast-path
(RANK 2 — a slug with one project per provider must merge mart rows instead
of falling through to the ~3.1s aggregator pipeline), the mart-sourced
``tools`` / ``cache`` blocks (RANK 7 — previously hard-coded empties), and
the ``hourly_pattern`` dict-shape contract (RANK 46 — was a bare ``[]``).
"""

from __future__ import annotations

import json
import time

import pytest

from stackunderflow.routes import data as data_route
from stackunderflow.store import db, schema


def _connect(store_db):
    conn = db.connect(store_db)
    schema.apply(conn)
    return conn


def _insert_project(conn, provider, slug):
    cur = conn.execute(
        "INSERT INTO projects (provider, slug, display_name, first_seen, last_modified) VALUES (?, ?, ?, ?, ?)",
        (provider, slug, slug, 0.0, 0.0),
    )
    return int(cur.lastrowid)


def _insert_session(conn, project_id, *, session_id, last_ts, n):
    conn.execute(
        "INSERT INTO sessions (project_id, session_id, first_ts, last_ts, message_count) VALUES (?, ?, ?, ?, ?)",
        (project_id, session_id, last_ts, last_ts, n),
    )


def _insert_session_returning_id(conn, project_id, *, session_id, last_ts, n=0):
    cur = conn.execute(
        "INSERT INTO sessions (project_id, session_id, first_ts, last_ts, message_count) VALUES (?, ?, ?, ?, ?)",
        (project_id, session_id, last_ts, last_ts, n),
    )
    return int(cur.lastrowid)


def _insert_tool_mart(conn, *, project_id, day, provider, tool_name, event_count, cost_usd=0.0):
    """One ``tool_mart`` row (v007 columns; calls_total/v012 left at default)."""
    conn.execute(
        "INSERT INTO tool_mart "
        "(day, project_id, provider, tool_name, event_count, cost_usd, "
        " tokens_in, tokens_out, session_count) "
        "VALUES (?, ?, ?, ?, ?, ?, 0, 0, 1)",
        (day, project_id, provider, tool_name, event_count, cost_usd),
    )


def _insert_billable_message(
    conn, *, session_fk, seq, model, timestamp, input_tokens=0, output_tokens=0, cache_read=0, cache_create=0
):
    """A messages row that drives BOTH the mart path and the pipeline.

    * token COLUMNS feed the provider Normalizer → usage_events → marts.
    * Claude-shaped ``raw_json`` (``message.usage.*``) feeds the aggregator
      pipeline's ``enricher._usage_from`` so both data sources see identical
      tokens for the same row.
    """
    raw = {
        "type": "assistant",
        "timestamp": timestamp,
        "message": {
            "role": "assistant",
            "model": model,
            "usage": {
                "input_tokens": input_tokens,
                "output_tokens": output_tokens,
                "cache_creation_input_tokens": cache_create,
                "cache_read_input_tokens": cache_read,
            },
        },
    }
    conn.execute(
        "INSERT INTO messages (session_fk, seq, timestamp, role, model, "
        "input_tokens, output_tokens, cache_create_tokens, cache_read_tokens, "
        "content_text, tools_json, raw_json) "
        "VALUES (?, ?, ?, 'assistant', ?, ?, ?, ?, ?, '', '[]', ?)",
        (session_fk, seq, timestamp, model, input_tokens, output_tokens, cache_create, cache_read, json.dumps(raw)),
    )


def _run_normalizers(conn):
    """Walk ``messages`` → ``usage_events`` via the registered Normalizers.

    Compact inline mirror of the watcher's ``_normalize_recent`` loop (the
    e2e integration test uses the same shape). Lets a unit-scale fixture
    populate ``usage_events`` so ``refresh_all_marts`` has something to roll
    up — without crafting event rows by hand.
    """
    from stackunderflow.etl import normalize as normalize_registry

    for provider, ncls in normalize_registry.all().items():
        normalizer = ncls()
        rows = conn.execute(
            "SELECT m.id, m.session_fk, m.seq, m.timestamp, m.role, m.model, "
            "m.input_tokens, m.output_tokens, m.cache_create_tokens, "
            "m.cache_read_tokens, m.content_text, m.tools_json, m.raw_json, "
            "m.is_sidechain, m.uuid, m.parent_uuid, m.speed, "
            "s.session_id AS session_id, s.project_id AS project_id, "
            "p.provider AS provider "
            "FROM messages m JOIN sessions s ON s.id = m.session_fk "
            "JOIN projects p ON p.id = s.project_id "
            "LEFT JOIN usage_events e ON e.source_message_fk = m.id "
            "WHERE p.provider = ? AND e.id IS NULL",
            (provider,),
        ).fetchall()
        for row in rows:
            msg_row = dict(row)
            for ev in normalizer.normalize(msg_row):
                conn.execute(
                    "INSERT OR IGNORE INTO usage_events ("
                    "source_message_fk, provider, account, project_id, session_id, "
                    "ts, day, model, speed, input_tokens, output_tokens, "
                    "cache_read_tokens, cache_create_tokens, cost_usd, cost_source, "
                    "role, raw_extras) "
                    "VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                    (
                        msg_row["id"],
                        ev.get("provider", provider),
                        ev.get("account", "default"),
                        ev.get("project_id", msg_row["project_id"]),
                        ev.get("session_id", msg_row["session_id"]),
                        ev.get("ts", msg_row["timestamp"]),
                        ev.get("day", (msg_row["timestamp"] or "")[:10]),
                        ev.get("model", msg_row.get("model") or ""),
                        ev.get("speed", msg_row.get("speed", "standard")),
                        int(ev.get("input_tokens", 0)),
                        int(ev.get("output_tokens", 0)),
                        int(ev.get("cache_read_tokens", 0)),
                        int(ev.get("cache_create_tokens", 0)),
                        float(ev.get("cost_usd", 0.0)),
                        ev.get("cost_source", "rate_card"),
                        ev.get("role", msg_row.get("role", "")),
                        ev.get("raw_extras"),
                    ),
                )
    conn.commit()


def _insert_project_mart(conn, *, project_id, provider, slug, **kw):
    conn.execute(
        "INSERT INTO project_mart "
        "(project_id, provider, slug, display_name, first_ts, last_ts, "
        " total_messages, total_sessions, total_input_tokens, total_output_tokens, "
        " total_cache_read, total_cache_create, total_cost_usd) "
        "VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        (
            project_id,
            provider,
            slug,
            slug,
            kw.get("first_ts"),
            kw.get("last_ts"),
            kw.get("total_messages", 0),
            kw.get("total_sessions", 0),
            kw.get("total_input_tokens", 0),
            kw.get("total_output_tokens", 0),
            kw.get("total_cache_read", 0),
            kw.get("total_cache_create", 0),
            kw.get("total_cost_usd", 0.0),
        ),
    )


def _insert_daily_mart(conn, *, project_id, day, **kw):
    conn.execute(
        "INSERT INTO daily_mart "
        "(day, project_id, provider, model, speed, input_tokens, output_tokens, "
        " cache_read, cache_create, message_count, session_count, cost_usd) "
        "VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        (
            day,
            project_id,
            kw.get("provider", "claude"),
            kw.get("model", "claude-sonnet-4-5"),
            kw.get("speed", "standard"),
            kw.get("input_tokens", 0),
            kw.get("output_tokens", 0),
            kw.get("cache_read", 0),
            kw.get("cache_create", 0),
            kw.get("message_count", 0),
            kw.get("session_count", 0),
            kw.get("cost_usd", 0.0),
        ),
    )


@pytest.mark.asyncio
async def test_dashboard_data_overview_from_project_mart(tmp_path, monkeypatch):
    store_db = tmp_path / "store.db"
    slug = "-mart-proj"
    conn = _connect(store_db)
    pid = _insert_project(conn, "claude", slug)
    _insert_session(conn, pid, session_id="s1", last_ts="2026-04-25T00:00:00Z", n=3)
    _insert_project_mart(
        conn,
        project_id=pid,
        provider="claude",
        slug=slug,
        total_messages=42,
        total_sessions=3,
        total_input_tokens=1000,
        total_output_tokens=500,
        total_cache_read=100,
        total_cache_create=50,
        total_cost_usd=1.25,
        first_ts="2026-04-01T00:00:00Z",
        last_ts="2026-04-30T00:00:00Z",
    )
    conn.commit()
    conn.close()
    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)
    monkeypatch.setattr("stackunderflow.deps.current_log_path", f"/fake/{slug}")
    data_route.invalidate_dashboard_cache()
    payload = await data_route.get_dashboard_data()
    stats = payload["statistics"]
    assert stats["overview"]["total_tokens"]["input"] == 1000
    assert stats["overview"]["total_cost"] == pytest.approx(1.25)
    assert stats["tools"] == {"usage_counts": {}, "error_counts": {}, "error_rates": {}}
    assert stats["errors"] == {"total": 0}


@pytest.mark.asyncio
async def test_dashboard_data_daily_stats_from_daily_mart(tmp_path, monkeypatch):
    store_db = tmp_path / "store.db"
    slug = "-daily-proj"
    conn = _connect(store_db)
    pid = _insert_project(conn, "claude", slug)
    _insert_session(conn, pid, session_id="s1", last_ts="2026-04-02T00:00:00Z", n=1)
    _insert_project_mart(
        conn, project_id=pid, provider="claude", slug=slug, total_messages=2, total_input_tokens=300, total_cost_usd=0.6
    )
    _insert_daily_mart(
        conn, project_id=pid, day="2026-04-01", input_tokens=100, output_tokens=50, message_count=1, cost_usd=0.2
    )
    _insert_daily_mart(
        conn, project_id=pid, day="2026-04-02", input_tokens=200, output_tokens=100, message_count=1, cost_usd=0.4
    )
    conn.commit()
    conn.close()
    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)
    monkeypatch.setattr("stackunderflow.deps.current_log_path", f"/fake/{slug}")
    data_route.invalidate_dashboard_cache()
    payload = await data_route.get_dashboard_data()
    daily = payload["statistics"]["daily_stats"]
    # daily_mart_by_day emits the legacy `Record<string, DailyData>` shape
    # the frontend type contract (and the legacy aggregator) expects.
    assert sorted(daily.keys()) == ["2026-04-01", "2026-04-02"]
    bucket_d2 = daily["2026-04-02"]
    assert bucket_d2["cost"]["total"] == pytest.approx(0.4)
    assert bucket_d2["tokens"]["input"] == 200
    assert bucket_d2["tokens"]["output"] == 100


@pytest.mark.asyncio
async def test_dashboard_data_models_from_daily_mart(tmp_path, monkeypatch):
    store_db = tmp_path / "store.db"
    slug = "-models-proj"
    conn = _connect(store_db)
    pid = _insert_project(conn, "claude", slug)
    _insert_session(conn, pid, session_id="s1", last_ts="2026-04-01T00:00:00Z", n=1)
    _insert_project_mart(conn, project_id=pid, provider="claude", slug=slug, total_messages=3)
    _insert_daily_mart(conn, project_id=pid, day="2026-04-01", model="claude-sonnet-4-5", message_count=2, cost_usd=0.5)
    _insert_daily_mart(conn, project_id=pid, day="2026-04-01", model="gpt-5", message_count=1, cost_usd=0.3)
    conn.commit()
    conn.close()
    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)
    monkeypatch.setattr("stackunderflow.deps.current_log_path", f"/fake/{slug}")
    data_route.invalidate_dashboard_cache()
    payload = await data_route.get_dashboard_data()
    models = payload["statistics"]["models"]
    assert set(models) == {"claude-sonnet-4-5", "gpt-5"}
    assert models["claude-sonnet-4-5"]["count"] == 2
    assert models["claude-sonnet-4-5"]["cost"] == pytest.approx(0.5)


@pytest.mark.asyncio
async def test_dashboard_data_under_100ms_with_100k_daily_mart_rows(tmp_path, monkeypatch):
    store_db = tmp_path / "store.db"
    slug = "-perf-proj"
    conn = _connect(store_db)
    pid = _insert_project(conn, "claude", slug)
    _insert_session(conn, pid, session_id="s1", last_ts="2026-04-25T00:00:00Z", n=100000)
    _insert_project_mart(
        conn,
        project_id=pid,
        provider="claude",
        slug=slug,
        total_messages=100000,
        total_input_tokens=10_000_000,
        total_cost_usd=42.0,
    )
    rows = []
    for d in range(1000):
        for m in range(100):
            day_str = f"2024-{((d // 30) % 12) + 1:02d}-{(d % 28) + 1:02d}"
            rows.append((day_str, pid, "claude", f"model-{m}", "standard", 10, 5, 0, 0, 1, 1, 0.001))
    conn.executemany(
        "INSERT OR IGNORE INTO daily_mart "
        "(day, project_id, provider, model, speed, input_tokens, output_tokens, "
        " cache_read, cache_create, message_count, session_count, cost_usd) "
        "VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        rows,
    )
    conn.commit()
    conn.close()
    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)
    monkeypatch.setattr("stackunderflow.deps.current_log_path", f"/fake/{slug}")
    data_route.invalidate_dashboard_cache()
    await data_route.get_dashboard_data()
    t0 = time.perf_counter()
    payload = await data_route.get_dashboard_data()
    elapsed_ms = (time.perf_counter() - t0) * 1000
    assert payload["statistics"]["overview"]["total_cost"] == pytest.approx(42.0)
    assert elapsed_ms < 100, f"slow: {elapsed_ms:.1f}ms"


# ── RANK 2 — multi-provider mart fast-path (merge, don't fall through) ────────


def _boom_pipeline(*_a, **_k):
    raise AssertionError(
        "queries.get_project_stats ran — the multi-provider mart fast-path "
        "missed and fell through to the full ~3.1s pipeline"
    )


@pytest.mark.asyncio
async def test_dashboard_data_merges_multi_provider_mart_rows(tmp_path, monkeypatch):
    """A slug with claude + codex ids serves a MERGED mart payload.

    The old gate (``len(project_ids) == 1``) bailed to the aggregator for any
    multi-provider slug. We patch ``get_project_stats`` to explode so the test
    fails loudly if the fast-path ever falls through again.
    """
    store_db = tmp_path / "store.db"
    slug = "-multi-prov"
    conn = _connect(store_db)
    pid_c = _insert_project(conn, "claude", slug)
    pid_x = _insert_project(conn, "codex", slug)
    _insert_session(conn, pid_c, session_id="c1", last_ts="2026-04-02T00:00:00Z", n=2)
    _insert_session(conn, pid_x, session_id="x1", last_ts="2026-04-02T00:00:00Z", n=1)
    _insert_project_mart(
        conn,
        project_id=pid_c,
        provider="claude",
        slug=slug,
        total_messages=10,
        total_sessions=2,
        total_input_tokens=1000,
        total_output_tokens=400,
        total_cache_read=900,
        total_cache_create=100,
        total_cost_usd=2.0,
        first_ts="2026-04-01T00:00:00Z",
        last_ts="2026-04-03T00:00:00Z",
    )
    _insert_project_mart(
        conn,
        project_id=pid_x,
        provider="codex",
        slug=slug,
        total_messages=5,
        total_sessions=1,
        total_input_tokens=500,
        total_output_tokens=200,
        total_cache_read=300,
        total_cache_create=50,
        total_cost_usd=1.5,
        first_ts="2026-03-30T00:00:00Z",
        last_ts="2026-04-02T00:00:00Z",
    )
    # Shared day so daily_stats must SUM both providers in one bucket.
    _insert_daily_mart(
        conn,
        project_id=pid_c,
        day="2026-04-02",
        provider="claude",
        model="claude-sonnet-4-5",
        input_tokens=1000,
        output_tokens=400,
        cache_read=900,
        cache_create=100,
        message_count=10,
        session_count=2,
        cost_usd=2.0,
    )
    _insert_daily_mart(
        conn,
        project_id=pid_x,
        day="2026-04-02",
        provider="codex",
        model="gpt-5",
        input_tokens=500,
        output_tokens=200,
        cache_read=300,
        cache_create=50,
        message_count=5,
        session_count=1,
        cost_usd=1.5,
    )
    # tool_mart spread across both providers — usage_counts must merge.
    _insert_tool_mart(conn, project_id=pid_c, day="2026-04-02", provider="claude", tool_name="Read", event_count=7)
    _insert_tool_mart(conn, project_id=pid_x, day="2026-04-02", provider="codex", tool_name="Read", event_count=3)
    _insert_tool_mart(conn, project_id=pid_x, day="2026-04-02", provider="codex", tool_name="Edit", event_count=4)
    conn.commit()
    conn.close()

    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)
    monkeypatch.setattr("stackunderflow.deps.current_log_path", f"/fake/{slug}")
    monkeypatch.setattr("stackunderflow.routes.data.queries.get_project_stats", _boom_pipeline)
    data_route.invalidate_dashboard_cache()

    stats = (await data_route.get_dashboard_data())["statistics"]

    ov = stats["overview"]
    assert ov["total_tokens"]["input"] == 1500  # 1000 + 500
    assert ov["total_tokens"]["output"] == 600  # 400 + 200
    assert ov["total_tokens"]["cache_read"] == 1200  # 900 + 300
    assert ov["total_tokens"]["cache_creation"] == 150  # 100 + 50
    assert ov["total_cost"] == pytest.approx(3.5)  # 2.0 + 1.5
    assert ov["total_messages"] == 15  # 10 + 5
    assert ov["total_sessions"] == 3  # 2 + 1
    # date_range spans the earliest start / latest end across providers.
    assert ov["date_range"]["start"] == "2026-03-30T00:00:00Z"
    assert ov["date_range"]["end"] == "2026-04-03T00:00:00Z"
    assert stats["sessions"]["count"] == 3

    # models + daily merged across providers
    assert set(stats["models"]) == {"claude-sonnet-4-5", "gpt-5"}
    bucket = stats["daily_stats"]["2026-04-02"]
    assert bucket["tokens"]["input"] == 1500
    assert bucket["messages"] == 15
    assert bucket["cost"]["total"] == pytest.approx(3.5)


@pytest.mark.asyncio
async def test_dashboard_data_merge_equals_sum_of_single_provider_paths(tmp_path):
    """Merged overview == per-provider mart overviews summed (the merge math)."""
    store_db = tmp_path / "store.db"
    slug = "-merge-math"
    conn = _connect(store_db)
    pid_c = _insert_project(conn, "claude", slug)
    pid_x = _insert_project(conn, "codex", slug)
    _insert_project_mart(
        conn,
        project_id=pid_c,
        provider="claude",
        slug=slug,
        total_messages=10,
        total_sessions=2,
        total_input_tokens=1000,
        total_output_tokens=400,
        total_cache_read=900,
        total_cache_create=100,
        total_cost_usd=2.0,
    )
    _insert_project_mart(
        conn,
        project_id=pid_x,
        provider="codex",
        slug=slug,
        total_messages=5,
        total_sessions=1,
        total_input_tokens=500,
        total_output_tokens=200,
        total_cache_read=300,
        total_cache_create=50,
        total_cost_usd=1.5,
    )
    conn.commit()

    merged = data_route._stats_from_marts(conn, project_ids=[pid_c, pid_x])
    only_c = data_route._stats_from_marts(conn, project_ids=[pid_c])
    only_x = data_route._stats_from_marts(conn, project_ids=[pid_x])
    conn.close()

    mo, co, xo = merged["overview"], only_c["overview"], only_x["overview"]
    for key in ("input", "output", "cache_read", "cache_creation"):
        assert mo["total_tokens"][key] == (co["total_tokens"][key] + xo["total_tokens"][key]), key
    assert mo["total_cost"] == pytest.approx(co["total_cost"] + xo["total_cost"])
    assert mo["total_messages"] == co["total_messages"] + xo["total_messages"]
    assert mo["total_sessions"] == co["total_sessions"] + xo["total_sessions"]
    # cache block is summed from the merged project_mart cache totals
    assert merged["cache"]["total_created"] == 150
    assert merged["cache"]["total_read"] == 1200


@pytest.mark.asyncio
async def test_dashboard_data_falls_through_when_a_provider_is_unmaterialised(tmp_path, monkeypatch):
    """If ANY provider id lacks a ``project_mart`` row, use the full pipeline.

    Serving a merge that silently drops the un-materialised provider would
    undercount, so the gate requires EVERY id to be materialised.
    """
    store_db = tmp_path / "store.db"
    slug = "-partial-mart"
    conn = _connect(store_db)
    pid_c = _insert_project(conn, "claude", slug)
    pid_x = _insert_project(conn, "codex", slug)
    _insert_session(conn, pid_c, session_id="c1", last_ts="2026-04-02T00:00:00Z", n=1)
    _insert_session(conn, pid_x, session_id="x1", last_ts="2026-04-02T00:00:00Z", n=1)
    # Only claude is materialised; codex has NO project_mart row.
    _insert_project_mart(conn, project_id=pid_c, provider="claude", slug=slug, total_messages=10)
    conn.commit()
    conn.close()

    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)
    monkeypatch.setattr("stackunderflow.deps.current_log_path", f"/fake/{slug}")
    calls: list = []

    def _fake(conn, *, project_id, tz_offset=0):  # noqa: ARG001
        calls.append(project_id)
        return ([], {"overview": {"total_messages": 99}})

    monkeypatch.setattr("stackunderflow.routes.data.queries.get_project_stats", _fake)
    data_route.invalidate_dashboard_cache()

    stats = (await data_route.get_dashboard_data())["statistics"]

    assert len(calls) == 1, "expected the full pipeline to run exactly once"
    assert sorted(calls[0]) == sorted([pid_c, pid_x]), "both ids passed to pipeline"
    assert stats["overview"]["total_messages"] == 99


# ── RANK 7 — tools + cache now carry real values (not hard-coded empties) ─────


@pytest.mark.asyncio
async def test_dashboard_data_tools_and_cache_sourced_from_marts(tmp_path, monkeypatch):
    """tools.usage_counts comes from tool_mart; cache ROI from project_mart."""
    store_db = tmp_path / "store.db"
    slug = "-tools-cache"
    conn = _connect(store_db)
    pid = _insert_project(conn, "claude", slug)
    _insert_session(conn, pid, session_id="s1", last_ts="2026-04-02T00:00:00Z", n=4)
    _insert_project_mart(
        conn,
        project_id=pid,
        provider="claude",
        slug=slug,
        total_messages=12,
        total_cache_read=5000,
        total_cache_create=1000,
        total_cost_usd=3.0,
    )
    _insert_tool_mart(conn, project_id=pid, day="2026-04-02", provider="claude", tool_name="Read", event_count=9)
    _insert_tool_mart(conn, project_id=pid, day="2026-04-02", provider="claude", tool_name="Bash", event_count=5)
    # #40: cost_saved is now priced from daily_mart's per-model cache tokens
    # (real rates), not flat 0.9/0.25 constants. Seed daily rows whose cache
    # totals match the project_mart lifetime totals (5000 read / 1000 create).
    _insert_daily_mart(
        conn,
        project_id=pid,
        day="2026-04-02",
        provider="claude",
        model="claude-sonnet-4-20250514",
        cache_read=5000,
        cache_create=1000,
        message_count=12,
        cost_usd=3.0,
    )
    conn.commit()
    conn.close()

    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)
    monkeypatch.setattr("stackunderflow.deps.current_log_path", f"/fake/{slug}")
    data_route.invalidate_dashboard_cache()

    stats = (await data_route.get_dashboard_data())["statistics"]

    # RANK 7: Tool-use charts read stats.tools.usage_counts — now real.
    assert stats["tools"]["usage_counts"] == {"Read": 9, "Bash": 5}
    # RANK 7: Cache-ROI hero card reads total_created / tokens_saved / break-even.
    cache = stats["cache"]
    assert cache["total_created"] == 1000
    assert cache["total_read"] == 5000
    assert cache["tokens_saved"] == 4000  # 5000 - 1000
    assert cache["break_even_achieved"] is True  # read > created
    # #40: cost_saved is priced through compute_cost (real per-model rates),
    # NOT the old flat read*0.9 - created*0.25 magic constants. Pin to the
    # pricer basis: input_cost - cache_read_cost - cache_creation_cost, in the
    # frontend's base-units convention (USD * 1e6).
    from stackunderflow.infra.costs import compute_cost

    cb = compute_cost(
        {"input": 6000, "output": 0, "cache_read": 5000, "cache_creation": 1000},
        "claude-sonnet-4-20250514",
        provider="claude",
        speed="standard",
    )
    expected = round((cb["input_cost"] - cb["cache_read_cost"] - cb["cache_creation_cost"]) * 1_000_000, 2)
    assert cache["cost_saved_base_units"] == pytest.approx(expected)
    # Regression guard: the new basis must differ from the retired constants.
    assert cache["cost_saved_base_units"] != pytest.approx(5000 * 0.9 - 1000 * 0.25)


@pytest.mark.asyncio
async def test_dashboard_data_hourly_pattern_is_dict_not_list(tmp_path, monkeypatch):
    """RANK 46 — hourly_pattern must be the ``{messages, tokens}`` dict.

    A bare ``[]`` is truthy, so the frontend's ``stats.hourly_pattern ?? {...}``
    fallback never fires and HourlyPatternChart renders a blank hole. The dict
    shape lets the chart read ``.tokens`` / ``.messages`` and show its empty
    state instead.
    """
    store_db = tmp_path / "store.db"
    slug = "-hourly-shape"
    conn = _connect(store_db)
    pid = _insert_project(conn, "claude", slug)
    _insert_session(conn, pid, session_id="s1", last_ts="2026-04-02T00:00:00Z", n=1)
    _insert_project_mart(conn, project_id=pid, provider="claude", slug=slug, total_messages=1)
    conn.commit()
    conn.close()

    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)
    monkeypatch.setattr("stackunderflow.deps.current_log_path", f"/fake/{slug}")
    data_route.invalidate_dashboard_cache()

    hourly = (await data_route.get_dashboard_data())["statistics"]["hourly_pattern"]
    assert isinstance(hourly, dict)
    assert hourly == {"messages": {}, "tokens": {}}


# ── parity — mart fast-path reproduces the full pipeline on a real store ──────


@pytest.mark.asyncio
async def test_single_provider_mart_path_matches_full_pipeline(tmp_path, monkeypatch):
    """Mart overview == aggregator overview (tokens + messages + cost) for one provider.

    Builds real ``messages`` (so the pipeline can run), normalises them into
    ``usage_events`` and refreshes the marts, then compares the route's mart
    payload against ``queries.get_project_stats`` over the same store. Single
    provider, so the aggregator prices correctly and cost parity is exact.
    """
    from stackunderflow.etl.watermark import refresh_all_marts
    from stackunderflow.store import queries

    store_db = tmp_path / "store.db"
    slug = "-parity-single"
    conn = _connect(store_db)
    pid = _insert_project(conn, "claude", slug)
    sfk = _insert_session_returning_id(conn, pid, session_id="s1", last_ts="2026-04-02T05:00:00+00:00")
    _insert_billable_message(
        conn,
        session_fk=sfk,
        seq=1,
        model="claude-sonnet-4-5-20250929",
        timestamp="2026-04-02T01:00:00+00:00",
        input_tokens=1000,
        output_tokens=300,
        cache_read=500,
        cache_create=100,
    )
    _insert_billable_message(
        conn,
        session_fk=sfk,
        seq=2,
        model="claude-sonnet-4-5-20250929",
        timestamp="2026-04-02T02:00:00+00:00",
        input_tokens=800,
        output_tokens=200,
        cache_read=400,
        cache_create=50,
    )
    conn.commit()
    _run_normalizers(conn)
    refresh_all_marts(conn)
    _, pipeline_stats = queries.get_project_stats(conn, project_id=[pid])
    conn.close()

    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)
    monkeypatch.setattr("stackunderflow.deps.current_log_path", f"/fake/{slug}")
    data_route.invalidate_dashboard_cache()

    mart_ov = (await data_route.get_dashboard_data())["statistics"]["overview"]
    pipe_ov = pipeline_stats["overview"]

    for key in ("input", "output", "cache_read", "cache_creation"):
        assert mart_ov["total_tokens"][key] == pipe_ov["total_tokens"][key], key
    # Every fixture row is a billable assistant row, so the mart's billable
    # event count equals the pipeline's record count.
    assert mart_ov["total_messages"] == pipe_ov["total_messages"]
    assert mart_ov["total_cost"] == pytest.approx(pipe_ov["total_cost"], rel=1e-6)
    assert mart_ov["total_cost"] > 0.0


@pytest.mark.asyncio
async def test_multi_provider_mart_path_matches_pipeline_token_totals(tmp_path, monkeypatch):
    """Multi-provider mart overview reproduces the pipeline's token + message totals.

    Cost is asserted via the mart-internal sum rather than the aggregator:
    the aggregator prices a mixed-provider dataset under a single provider, so
    only token/message totals (pure, provider-independent aggregation) are a
    fair cross-source equality. The merge-math test above pins cost summation.
    """
    from stackunderflow.etl.watermark import refresh_all_marts
    from stackunderflow.store import mart_queries, queries

    store_db = tmp_path / "store.db"
    slug = "-parity-multi"
    conn = _connect(store_db)
    pid_c = _insert_project(conn, "claude", slug)
    pid_x = _insert_project(conn, "codex", slug)
    sfk_c = _insert_session_returning_id(conn, pid_c, session_id="c1", last_ts="2026-04-02T05:00:00+00:00")
    sfk_x = _insert_session_returning_id(conn, pid_x, session_id="x1", last_ts="2026-04-02T05:00:00+00:00")
    _insert_billable_message(
        conn,
        session_fk=sfk_c,
        seq=1,
        model="claude-sonnet-4-5-20250929",
        timestamp="2026-04-02T01:00:00+00:00",
        input_tokens=1000,
        output_tokens=300,
        cache_read=500,
        cache_create=100,
    )
    _insert_billable_message(
        conn,
        session_fk=sfk_c,
        seq=2,
        model="claude-sonnet-4-5-20250929",
        timestamp="2026-04-02T02:00:00+00:00",
        input_tokens=800,
        output_tokens=200,
        cache_read=400,
        cache_create=50,
    )
    # codex row: cache_create stays 0 (OpenAI doesn't bill prompt-cache writes).
    _insert_billable_message(
        conn,
        session_fk=sfk_x,
        seq=1,
        model="gpt-5",
        timestamp="2026-04-02T03:00:00+00:00",
        input_tokens=600,
        output_tokens=150,
        cache_read=0,
        cache_create=0,
    )
    conn.commit()
    _run_normalizers(conn)
    refresh_all_marts(conn)
    _, pipeline_stats = queries.get_project_stats(conn, project_id=[pid_c, pid_x])
    # Mart-internal cost reference: sum of per-provider project_mart totals.
    mart_cost = sum(
        (mart_queries.get_project_mart_row(conn, project_id=pid) or {}).get("total_cost_usd", 0.0)
        for pid in (pid_c, pid_x)
    )
    conn.close()

    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)
    monkeypatch.setattr("stackunderflow.deps.current_log_path", f"/fake/{slug}")
    monkeypatch.setattr("stackunderflow.routes.data.queries.get_project_stats", _boom_pipeline)
    data_route.invalidate_dashboard_cache()

    mart_ov = (await data_route.get_dashboard_data())["statistics"]["overview"]
    pipe_ov = pipeline_stats["overview"]

    for key in ("input", "output", "cache_read", "cache_creation"):
        assert mart_ov["total_tokens"][key] == pipe_ov["total_tokens"][key], key
    assert mart_ov["total_messages"] == pipe_ov["total_messages"]
    assert mart_ov["total_cost"] == pytest.approx(mart_cost)
    assert mart_ov["total_cost"] > 0.0
