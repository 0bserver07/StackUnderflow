"""Wave 3A — ``/api/dashboard-data`` reads from ``project_mart`` + ``daily_mart``."""

from __future__ import annotations

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
        "INSERT INTO projects (provider, slug, display_name, first_seen, last_modified) "
        "VALUES (?, ?, ?, ?, ?)", (provider, slug, slug, 0.0, 0.0))
    return int(cur.lastrowid)


def _insert_session(conn, project_id, *, session_id, last_ts, n):
    conn.execute(
        "INSERT INTO sessions (project_id, session_id, first_ts, last_ts, message_count) "
        "VALUES (?, ?, ?, ?, ?)", (project_id, session_id, last_ts, last_ts, n))


def _insert_project_mart(conn, *, project_id, provider, slug, **kw):
    conn.execute(
        "INSERT INTO project_mart "
        "(project_id, provider, slug, display_name, first_ts, last_ts, "
        " total_messages, total_sessions, total_input_tokens, total_output_tokens, "
        " total_cache_read, total_cache_create, total_cost_usd) "
        "VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        (project_id, provider, slug, slug, kw.get("first_ts"), kw.get("last_ts"),
         kw.get("total_messages", 0), kw.get("total_sessions", 0),
         kw.get("total_input_tokens", 0), kw.get("total_output_tokens", 0),
         kw.get("total_cache_read", 0), kw.get("total_cache_create", 0),
         kw.get("total_cost_usd", 0.0)))


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


@pytest.mark.asyncio
async def test_dashboard_data_overview_from_project_mart(tmp_path, monkeypatch):
    store_db = tmp_path / "store.db"
    slug = "-mart-proj"
    conn = _connect(store_db)
    pid = _insert_project(conn, "claude", slug)
    _insert_session(conn, pid, session_id="s1", last_ts="2026-04-25T00:00:00Z", n=3)
    _insert_project_mart(conn, project_id=pid, provider="claude", slug=slug,
        total_messages=42, total_sessions=3, total_input_tokens=1000, total_output_tokens=500,
        total_cache_read=100, total_cache_create=50, total_cost_usd=1.25,
        first_ts="2026-04-01T00:00:00Z", last_ts="2026-04-30T00:00:00Z")
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
    _insert_project_mart(conn, project_id=pid, provider="claude", slug=slug,
        total_messages=2, total_input_tokens=300, total_cost_usd=0.6)
    _insert_daily_mart(conn, project_id=pid, day="2026-04-01",
        input_tokens=100, output_tokens=50, message_count=1, cost_usd=0.2)
    _insert_daily_mart(conn, project_id=pid, day="2026-04-02",
        input_tokens=200, output_tokens=100, message_count=1, cost_usd=0.4)
    conn.commit()
    conn.close()
    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)
    monkeypatch.setattr("stackunderflow.deps.current_log_path", f"/fake/{slug}")
    data_route.invalidate_dashboard_cache()
    payload = await data_route.get_dashboard_data()
    daily = payload["statistics"]["daily_stats"]
    days = sorted(d["date"] for d in daily)
    assert days == ["2026-04-01", "2026-04-02"]
    bucket_d2 = next(d for d in daily if d["date"] == "2026-04-02")
    assert bucket_d2["cost"] == pytest.approx(0.4)


@pytest.mark.asyncio
async def test_dashboard_data_models_from_daily_mart(tmp_path, monkeypatch):
    store_db = tmp_path / "store.db"
    slug = "-models-proj"
    conn = _connect(store_db)
    pid = _insert_project(conn, "claude", slug)
    _insert_session(conn, pid, session_id="s1", last_ts="2026-04-01T00:00:00Z", n=1)
    _insert_project_mart(conn, project_id=pid, provider="claude", slug=slug, total_messages=3)
    _insert_daily_mart(conn, project_id=pid, day="2026-04-01",
        model="claude-sonnet-4-5", message_count=2, cost_usd=0.5)
    _insert_daily_mart(conn, project_id=pid, day="2026-04-01",
        model="gpt-5", message_count=1, cost_usd=0.3)
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
    _insert_project_mart(conn, project_id=pid, provider="claude", slug=slug,
        total_messages=100000, total_input_tokens=10_000_000, total_cost_usd=42.0)
    rows = []
    for d in range(1000):
        for m in range(100):
            day_str = f"2024-{((d // 30) % 12) + 1:02d}-{(d % 28) + 1:02d}"
            rows.append((day_str, pid, "claude", f"model-{m}", "standard",
                10, 5, 0, 0, 1, 1, 0.001))
    conn.executemany(
        "INSERT OR IGNORE INTO daily_mart "
        "(day, project_id, provider, model, speed, input_tokens, output_tokens, "
        " cache_read, cache_create, message_count, session_count, cost_usd) "
        "VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)", rows)
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
