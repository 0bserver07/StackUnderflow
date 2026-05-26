"""Tests for ``GET /api/yield``."""

from __future__ import annotations

import pytest
from fastapi import HTTPException

from stackunderflow.routes.yield_route import get_yield
from stackunderflow.services.yield_tracker import YieldEntry

_ENTRIES = [
    YieldEntry(
        session_id="sess-1",
        project_slug="alpha",
        cwd="/repo/alpha",
        started_at="2026-04-01T10:00:00+00:00",
        cost_usd=4.50,
        classification="productive",
        follow_commit_sha="aaaaaaa1",
        follow_commit_msg="feat: ship",
        follow_commit_age_hours=2.0,
    ),
    YieldEntry(
        session_id="sess-2",
        project_slug="alpha",
        cwd="/repo/alpha",
        started_at="2026-04-02T10:00:00+00:00",
        cost_usd=1.00,
        classification="reverted",
    ),
    YieldEntry(
        session_id="sess-3",
        project_slug="beta",
        cwd="/repo/beta",
        started_at="2026-04-03T10:00:00+00:00",
        cost_usd=0.25,
        classification="abandoned",
    ),
]


@pytest.mark.asyncio
async def test_yield_route_returns_summary_and_sorted_entries(tmp_path, monkeypatch):
    from stackunderflow.store import db, schema
    db_conn = db.connect(tmp_path / "store.db")
    schema.apply(db_conn)
    db_conn.close()

    monkeypatch.setattr("stackunderflow.deps.store_path", tmp_path / "store.db")
    monkeypatch.setattr(
        "stackunderflow.routes.yield_route.compute_yield",
        lambda conn, period="month", project_filter=None: list(_ENTRIES),
    )

    body = await get_yield(period="month")
    assert body["period"] == "month"
    assert body["summary"]["productive"] == 1
    assert body["summary"]["reverted"] == 1
    assert body["summary"]["abandoned"] == 1
    assert body["summary"]["total"] == 3
    # Entries sorted by cost desc.
    assert [e["session_id"] for e in body["entries"]] == ["sess-1", "sess-2", "sess-3"]
    # Currency block + warning always present.
    assert "currency" in body
    assert "correlated by time" in body["warning"]


@pytest.mark.asyncio
async def test_yield_route_rejects_invalid_period(tmp_path, monkeypatch):
    monkeypatch.setattr("stackunderflow.deps.store_path", tmp_path / "store.db")
    with pytest.raises(HTTPException) as exc_info:
        await get_yield(period="bogus")
    assert exc_info.value.status_code == 400


@pytest.mark.asyncio
async def test_yield_route_passes_project_filter(tmp_path, monkeypatch):
    from stackunderflow.store import db, schema
    db_conn = db.connect(tmp_path / "store.db")
    schema.apply(db_conn)
    db_conn.close()

    monkeypatch.setattr("stackunderflow.deps.store_path", tmp_path / "store.db")
    captured: dict = {}

    def fake_compute(conn, period="month", project_filter=None):
        captured["period"] = period
        captured["project_filter"] = project_filter
        return []

    monkeypatch.setattr(
        "stackunderflow.routes.yield_route.compute_yield", fake_compute
    )
    await get_yield(period="week", project=["alpha", "beta"])
    assert captured["period"] == "week"
    assert captured["project_filter"] == ["alpha", "beta"]


@pytest.mark.asyncio
async def test_yield_route_converts_costs_to_active_currency(tmp_path, monkeypatch):
    """Cost figures get scaled by ``currency.rate_from_usd`` before send."""
    from stackunderflow.store import db, schema
    db_conn = db.connect(tmp_path / "store.db")
    schema.apply(db_conn)
    db_conn.close()

    monkeypatch.setattr("stackunderflow.deps.store_path", tmp_path / "store.db")
    monkeypatch.setattr(
        "stackunderflow.routes.yield_route.compute_yield",
        lambda conn, period="month", project_filter=None: list(_ENTRIES),
    )
    monkeypatch.setattr(
        "stackunderflow.routes.yield_route.active_currency_payload",
        lambda: {"code": "EUR", "symbol": "€", "rate_from_usd": 0.5},
    )

    body = await get_yield(period="month")
    # 4.50 USD → 2.25 EUR
    assert body["entries"][0]["cost_usd"] == pytest.approx(2.25)
    assert body["summary"]["productive_cost"] == pytest.approx(2.25)
    assert body["summary"]["total_cost"] == pytest.approx(4.50 * 0.5 + 1.00 * 0.5 + 0.25 * 0.5)
    assert body["currency"]["code"] == "EUR"
