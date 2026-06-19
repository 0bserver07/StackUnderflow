"""Tests for ``GET /api/cost-data/by-model`` — spend-by-model-over-time.

Powers the Cost tab's by-model time-series chart. The endpoint reads the
pre-aggregated ``model_day_mart`` and returns, per model, a daily cost +
message series plus a total, sorted by total cost descending, with cost
pre-converted into the active currency (parity with ``/api/cost-data/by-provider``).
"""

from __future__ import annotations

import pytest
from fastapi import HTTPException

from stackunderflow.routes.cost import get_cost_by_model
from stackunderflow.store import db, schema


def _seed_model_day(store_db, rows):
    """Insert rows directly into ``model_day_mart``.

    Each row: (day, model, speed, cost_usd, input_tokens, output_tokens,
    cache_read, cache_create, message_count, session_count). The endpoint
    reads the mart only, so seeding it directly tests the endpoint logic
    without running the full ETL backfill.
    """
    conn = db.connect(store_db)
    schema.apply(conn)
    conn.executemany(
        "INSERT INTO model_day_mart "
        "(day, model, speed, cost_usd, input_tokens, output_tokens, "
        " cache_read, cache_create, message_count, session_count) "
        "VALUES (?, ?, 'standard', ?, 0, 0, 0, 0, ?, 1)",
        rows,
    )
    conn.commit()
    conn.close()


@pytest.mark.asyncio
async def test_groups_models_with_daily_series_sorted_by_total(tmp_path, monkeypatch):
    store_db = tmp_path / "store.db"
    _seed_model_day(
        store_db,
        rows=[
            # (day, model, cost_usd, message_count)
            ("2026-04-01", "claude-fable-5", 700.0, 100),
            ("2026-04-01", "claude-opus-4-8", 30.0, 50),
            ("2026-04-02", "claude-fable-5", 400.0, 80),
            ("2026-04-02", "claude-opus-4-8", 20.0, 40),
        ],
    )
    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)
    payload = await get_cost_by_model(period="all")

    assert payload["period"] == "all"
    models = payload["models"]
    assert [m["model"] for m in models] == ["claude-fable-5", "claude-opus-4-8"]
    # Fable outspends Opus here; sort is descending by total (public contract).
    assert models[0]["total_cost"] > models[1]["total_cost"]

    # Each model carries a per-day series, ordered by day.
    fable_daily = models[0]["daily"]
    assert [d["date"] for d in fable_daily] == ["2026-04-01", "2026-04-02"]
    assert fable_daily[0]["message_count"] == 100
    # total == sum of the daily slices (modulo currency rate, applied to both).
    assert models[0]["total_cost"] == pytest.approx(
        sum(d["cost_usd"] for d in fable_daily)
    )

    assert "currency" in payload and "code" in payload["currency"]


@pytest.mark.asyncio
async def test_empty_store_returns_empty_models(tmp_path, monkeypatch):
    store_db = tmp_path / "store.db"
    conn = db.connect(store_db)
    schema.apply(conn)
    conn.close()
    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)
    payload = await get_cost_by_model(period="all")
    assert payload["models"] == []
    assert payload["period"] == "all"
    assert "currency" in payload


@pytest.mark.asyncio
async def test_invalid_period_400s(tmp_path, monkeypatch):
    store_db = tmp_path / "store.db"
    conn = db.connect(store_db)
    schema.apply(conn)
    conn.close()
    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)
    with pytest.raises(HTTPException) as exc:
        await get_cost_by_model(period="bogus")
    assert exc.value.status_code == 400
    assert "today" in exc.value.detail
    assert "all" in exc.value.detail
