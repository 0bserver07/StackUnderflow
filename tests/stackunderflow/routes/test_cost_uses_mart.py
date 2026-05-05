"""Wave 3A — ``/api/cost-data`` and ``/api/cost-data/by-provider`` mart paths."""

from __future__ import annotations

import time

import pytest

from stackunderflow.routes.cost import get_cost_by_provider, get_cost_data
from stackunderflow.store import db, schema


def _connect(store_db):
    conn = db.connect(store_db)
    schema.apply(conn)
    return conn


def _insert_project(conn, provider, slug):
    cur = conn.execute(
        "INSERT INTO projects (provider, slug, display_name, first_seen, last_modified) "
        "VALUES (?, ?, ?, ?, ?)", (provider, slug, slug, 0.0, 0.0))
    return int(cur.lastrowid)


def _insert_project_mart(conn, *, project_id, provider, slug):
    conn.execute(
        "INSERT INTO project_mart "
        "(project_id, provider, slug, display_name, first_ts, last_ts, "
        " total_messages, total_sessions, total_input_tokens, total_output_tokens, "
        " total_cache_read, total_cache_create, total_cost_usd) "
        "VALUES (?, ?, ?, ?, '2026-04-01', '2026-04-30', 1, 1, 100, 50, 0, 0, 0.5)",
        (project_id, provider, slug, slug))


def _insert_daily_mart(conn, *, project_id, day, **kw):
    conn.execute(
        "INSERT INTO daily_mart "
        "(day, project_id, provider, model, speed, input_tokens, output_tokens, "
        " cache_read, cache_create, message_count, session_count, cost_usd) "
        "VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        (day, project_id, kw.get("provider", "claude"),
         kw.get("model", "claude-sonnet-4-5"), kw.get("speed", "standard"),
         kw.get("input_tokens", 0), kw.get("output_tokens", 0),
         kw.get("cache_read", 0), kw.get("cache_create", 0),
         kw.get("message_count", 0), kw.get("session_count", 0),
         kw.get("cost_usd", 0.0)))


def _insert_provider_day(conn, *, day, provider, **kw):
    conn.execute(
        "INSERT INTO provider_day_mart "
        "(day, provider, cost_usd, message_count, session_count, project_count) "
        "VALUES (?, ?, ?, ?, ?, ?)",
        (day, provider, kw.get("cost_usd", 0.0), kw.get("message_count", 0),
         kw.get("session_count", 0), kw.get("project_count", 0)))


@pytest.mark.asyncio
async def test_cost_data_overlays_token_composition_from_daily_mart(tmp_path, monkeypatch):
    store_db = tmp_path / "store.db"
    slug = "-cost-overlay"
    conn = _connect(store_db)
    pid = _insert_project(conn, "claude", slug)
    _insert_project_mart(conn, project_id=pid, provider="claude", slug=slug)
    _insert_daily_mart(conn, project_id=pid, day="2026-04-01",
        input_tokens=10, output_tokens=5, cache_read=2, cache_create=1, cost_usd=0.05)
    _insert_daily_mart(conn, project_id=pid, day="2026-04-02",
        input_tokens=20, output_tokens=10, cache_read=4, cache_create=2, cost_usd=0.1)
    conn.commit()
    conn.close()
    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)
    monkeypatch.setattr("stackunderflow.deps.current_log_path", f"/fake/{slug}")
    stub_stats = {
        "session_costs": [{"session_id": "s1", "cost": 0.5}],
        "command_costs": [], "tool_costs": {"Read": {"calls": 2, "cost": 0.0}},
        "token_composition": {
            "daily": {"BOGUS": {"input": 999}},
            "totals": {"input": 999},
            "per_session": {"s1": {"input": 100}},
        },
        "outliers": {}, "retry_signals": [], "session_efficiency": [],
        "error_cost": {}, "trends": {},
    }
    monkeypatch.setattr(
        "stackunderflow.routes.cost.queries.get_project_stats",
        lambda conn, *, project_id, tz_offset=0: ([], stub_stats))
    payload = await get_cost_data()
    tc = payload["token_composition"]
    assert "BOGUS" not in tc["daily"]
    assert tc["daily"] == {
        "2026-04-01": {"input": 10, "output": 5, "cache_read": 2, "cache_creation": 1},
        "2026-04-02": {"input": 20, "output": 10, "cache_read": 4, "cache_creation": 2},
    }
    assert tc["totals"] == {"input": 30, "output": 15, "cache_read": 6, "cache_creation": 3}
    assert tc["per_session"] == {"s1": {"input": 100}}
    assert payload["session_costs"] == [{"session_id": "s1", "cost": 0.5}]


@pytest.mark.asyncio
async def test_cost_data_no_overlay_when_mart_empty(tmp_path, monkeypatch):
    store_db = tmp_path / "store.db"
    slug = "-cost-fallback"
    conn = _connect(store_db)
    _insert_project(conn, "claude", slug)
    conn.commit()
    conn.close()
    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)
    monkeypatch.setattr("stackunderflow.deps.current_log_path", f"/fake/{slug}")
    expected_tc = {"daily": {"2026-04-01": {"input": 7}}, "totals": {"input": 7}, "per_session": {}}
    monkeypatch.setattr(
        "stackunderflow.routes.cost.queries.get_project_stats",
        lambda conn, *, project_id, tz_offset=0: ([], {
            "session_costs": [], "command_costs": [], "tool_costs": {},
            "token_composition": expected_tc,
            "outliers": {}, "retry_signals": [], "session_efficiency": [],
            "error_cost": {}, "trends": {},
        }))
    payload = await get_cost_data()
    assert payload["token_composition"] == expected_tc


@pytest.mark.asyncio
async def test_cost_by_provider_uses_provider_day_mart(tmp_path, monkeypatch):
    store_db = tmp_path / "store.db"
    conn = _connect(store_db)
    _insert_project(conn, "claude", "alpha")
    _insert_provider_day(conn, day="2026-04-01", provider="claude",
        cost_usd=2.5, message_count=10, session_count=2, project_count=1)
    _insert_provider_day(conn, day="2026-04-15", provider="codex",
        cost_usd=1.0, message_count=5, session_count=1, project_count=1)
    conn.commit()
    conn.close()
    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)
    payload = await get_cost_by_provider(period="all")
    rows = payload["rows"]
    assert {r["provider"] for r in rows} == {"claude", "codex"}
    by_prov = {r["provider"]: r for r in rows}
    assert by_prov["claude"]["cost_usd"] == pytest.approx(2.5)
    assert by_prov["claude"]["message_count"] == 10


@pytest.mark.asyncio
async def test_cost_by_provider_filter_passes_through_to_mart(tmp_path, monkeypatch):
    store_db = tmp_path / "store.db"
    conn = _connect(store_db)
    _insert_provider_day(conn, day="2026-04-01", provider="claude", cost_usd=2.5, message_count=10)
    _insert_provider_day(conn, day="2026-04-01", provider="cursor", cost_usd=1.5, message_count=5)
    conn.commit()
    conn.close()
    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)
    payload = await get_cost_by_provider(period="all", provider=["cursor"])
    rows = payload["rows"]
    assert len(rows) == 1
    assert rows[0]["provider"] == "cursor"


@pytest.mark.asyncio
async def test_cost_by_provider_falls_back_to_messages_when_mart_empty(tmp_path, monkeypatch):
    store_db = tmp_path / "store.db"
    conn = _connect(store_db)
    pid = _insert_project(conn, "claude", "alpha")
    cur = conn.execute(
        "INSERT INTO sessions (project_id, session_id, first_ts, last_ts, message_count) "
        "VALUES (?, 's1', '2026-04-01T00:00:00Z', '2026-04-01T00:00:00Z', 1)", (pid,))
    sfk = cur.lastrowid
    conn.execute(
        "INSERT INTO messages "
        "(session_fk, seq, timestamp, role, model, "
        " input_tokens, output_tokens, cache_create_tokens, cache_read_tokens, "
        " content_text, tools_json, raw_json, is_sidechain, uuid, parent_uuid) "
        "VALUES (?, 0, '2026-04-01T00:00:00Z', 'assistant', 'claude-sonnet-4-5', "
        " 100, 50, 0, 0, '', '[]', '{}', 0, NULL, NULL)", (sfk,))
    conn.commit()
    conn.close()
    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)
    payload = await get_cost_by_provider(period="all")
    rows = payload["rows"]
    assert len(rows) == 1
    assert rows[0]["provider"] == "claude"
    assert rows[0]["message_count"] == 1


@pytest.mark.asyncio
async def test_cost_by_provider_under_100ms_with_100k_mart_rows(tmp_path, monkeypatch):
    store_db = tmp_path / "store.db"
    conn = _connect(store_db)
    rows = []
    for d in range(1000):
        day = f"20{((d // 365) + 24) % 100:02d}-{((d % 365 // 30) % 12) + 1:02d}-{(d % 28) + 1:02d}"
        for p in range(100):
            rows.append((day, f"provider-{p}", 0.01, 1, 1, 1))
    conn.executemany(
        "INSERT OR IGNORE INTO provider_day_mart "
        "(day, provider, cost_usd, message_count, session_count, project_count) "
        "VALUES (?, ?, ?, ?, ?, ?)", rows)
    conn.commit()
    conn.close()
    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)
    await get_cost_by_provider(period="all")
    t0 = time.perf_counter()
    payload = await get_cost_by_provider(period="all")
    elapsed_ms = (time.perf_counter() - t0) * 1000
    assert len(payload["rows"]) <= 100
    assert elapsed_ms < 100, f"slow: {elapsed_ms:.1f}ms"
