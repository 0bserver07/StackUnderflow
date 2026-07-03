"""Tests for ``GET /api/benchmark`` + ``/api/benchmark/recommend``."""

from __future__ import annotations

import pytest
from fastapi import HTTPException

from stackunderflow.routes import benchmark as bench_route
from stackunderflow.store import db, schema
from tests.stackunderflow.reports.test_benchmark import (
    _seed_project,
    _seed_winner_fixture,
)


@pytest.fixture(autouse=True)
def _clear_bench_cache():
    bench_route._BENCH_CACHE.clear()
    yield
    bench_route._BENCH_CACHE.clear()


def _seed_store(store_db):
    conn = db.connect(store_db)
    schema.apply(conn)
    pid = _seed_project(conn)
    _seed_winner_fixture(conn, pid)
    conn.commit()
    conn.close()


@pytest.mark.asyncio
async def test_benchmark_route_returns_report_and_warning(tmp_path, monkeypatch):
    store_db = tmp_path / "store.db"
    _seed_store(store_db)
    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)
    monkeypatch.setattr("stackunderflow.deps.current_log_path", None)

    body = await bench_route.get_benchmark(period="all")

    assert body["period"] == "all"
    assert body["scope"] == "all time"
    assert body["warning"]
    assert "currency" in body
    report = body["report"]
    assert report["verdict"]["winning_model"] == "sonnet"
    assert report["rubric_version"] == 1
    assert report["weights"] == {"success": 0.45, "cost": 0.35, "effort": 0.20}


@pytest.mark.asyncio
async def test_benchmark_route_rejects_bad_period(tmp_path, monkeypatch):
    monkeypatch.setattr("stackunderflow.deps.store_path", tmp_path / "store.db")
    with pytest.raises(HTTPException) as exc:
        await bench_route.get_benchmark(period="decade")
    assert exc.value.status_code == 400


@pytest.mark.asyncio
async def test_benchmark_route_converts_currency(tmp_path, monkeypatch):
    store_db = tmp_path / "store.db"
    _seed_store(store_db)
    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)
    monkeypatch.setattr("stackunderflow.deps.current_log_path", None)

    base = await bench_route.get_benchmark(period="all")
    base_cpo = base["report"]["verdict"]["cost_per_outcome_usd"]
    base_cell_cost = base["report"]["strata"][0]["models"][0]["median_cost"]["point"]

    monkeypatch.setattr(
        "stackunderflow.routes.benchmark.active_currency_payload",
        lambda: {"code": "EUR", "symbol": "€", "rate_from_usd": 2.0, "warning": None},
    )
    conv = await bench_route.get_benchmark(period="all")
    assert conv["report"]["verdict"]["cost_per_outcome_usd"] == pytest.approx(base_cpo * 2.0)
    assert conv["report"]["strata"][0]["models"][0]["median_cost"]["point"] == pytest.approx(
        base_cell_cost * 2.0
    )


@pytest.mark.asyncio
async def test_benchmark_route_memoizes_and_invalidates(tmp_path, monkeypatch):
    store_db = tmp_path / "store.db"
    _seed_store(store_db)
    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)
    monkeypatch.setattr("stackunderflow.deps.current_log_path", None)

    calls = {"n": 0}
    real = bench_route.analyze_benchmark

    def counting(conn, **kw):
        calls["n"] += 1
        return real(conn, **kw)

    monkeypatch.setattr(bench_route, "analyze_benchmark", counting)

    await bench_route.get_benchmark(period="all")
    await bench_route.get_benchmark(period="all")
    assert calls["n"] == 1  # second served from cache

    conn = db.connect(store_db)
    pid = conn.execute("SELECT id FROM projects LIMIT 1").fetchone()["id"]
    conn.execute(
        "INSERT INTO sessions (project_id, session_id, message_count, last_ts) "
        "VALUES (?, 'sess-new', 2, '2027-01-01T00:00:00+00:00')",
        (pid,),
    )
    conn.commit()
    conn.close()

    await bench_route.get_benchmark(period="all")
    assert calls["n"] == 2  # signature moved → recomputed


@pytest.mark.asyncio
async def test_benchmark_recommend_route(tmp_path, monkeypatch):
    store_db = tmp_path / "store.db"
    _seed_store(store_db)
    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)
    monkeypatch.setattr("stackunderflow.deps.current_log_path", None)

    body = await bench_route.get_benchmark_recommend(intent="fix", size="small")
    assert body["recommendation"]["recommended_model"] == "sonnet"
    assert body["recommendation"]["basis"] == "stratum"


@pytest.mark.asyncio
async def test_benchmark_recommend_requires_intent(tmp_path, monkeypatch):
    monkeypatch.setattr("stackunderflow.deps.store_path", tmp_path / "store.db")
    monkeypatch.setattr("stackunderflow.deps.current_log_path", None)
    with pytest.raises(HTTPException) as exc:
        await bench_route.get_benchmark_recommend(intent="   ")
    assert exc.value.status_code == 400


def test_benchmark_routes_registered_on_app():
    from stackunderflow.server import app
    from tests.conftest import app_route_paths

    paths = app_route_paths(app)
    assert "/api/benchmark" in paths
    assert "/api/benchmark/recommend" in paths
