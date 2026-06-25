"""``GET /api/global-stats`` — currency conversion (RANK 22) + threadpool (RANK 16).

The route reads the cross-project stats from the store (mart-backed), converts
every USD cost figure into the active display currency, and stamps on the
``currency`` + ``config`` blocks. The blocking store read runs in a worker
thread so the event loop is never stalled.
"""

from __future__ import annotations

import json
import threading

import pytest

from stackunderflow.routes.projects import get_global_stats
from stackunderflow.store import db, schema


def _empty_store(tmp_path):
    store_db = tmp_path / "store.db"
    conn = db.connect(store_db)
    schema.apply(conn)
    conn.commit()
    conn.close()
    return store_db


def _fake_stats() -> dict:
    """A representative ``get_global_stats`` payload (USD), all cost shapes."""
    return {
        "first_use_date": "2026-05-01",
        "last_use_date": "2026-05-02",
        "daily_token_usage": [{"date": "2026-05-01", "input": 10, "output": 5}],
        "daily_costs": [
            {"date": "2026-05-01", "cost": 2.0, "by_model": {"claude-opus-4-6": 2.0}},
        ],
        "models": {"claude-opus-4-6": {"count": 1, "cost": 2.0}},
        "total_cache_read_tokens": 100,
        "total_cache_write_tokens": 50,
    }


def _patch_currency(monkeypatch, *, code: str, rate: float) -> None:
    monkeypatch.setattr(
        "stackunderflow.routes.projects.active_currency_payload",
        lambda: {"code": code, "symbol": "x", "rate_from_usd": rate, "warning": None},
    )


@pytest.mark.asyncio
async def test_global_stats_applies_currency_conversion(tmp_path, monkeypatch):
    """RANK 22: every cost leaf — ``models[*].cost``, ``daily_costs[*].cost`` and
    the nested ``by_model`` values — is scaled by the FX rate; tokens are not."""
    monkeypatch.setattr("stackunderflow.deps.store_path", _empty_store(tmp_path))
    monkeypatch.setattr("stackunderflow.store.queries.get_global_stats", lambda conn: _fake_stats())
    _patch_currency(monkeypatch, code="EUR", rate=0.5)

    response = await get_global_stats()
    body = json.loads(response.body.decode("utf-8"))

    assert body["models"]["claude-opus-4-6"]["cost"] == pytest.approx(1.0)
    assert body["daily_costs"][0]["cost"] == pytest.approx(1.0)
    assert body["daily_costs"][0]["by_model"]["claude-opus-4-6"] == pytest.approx(1.0)
    # Non-cost figures are left alone.
    assert body["models"]["claude-opus-4-6"]["count"] == 1
    assert body["daily_token_usage"][0]["input"] == 10
    assert body["total_cache_read_tokens"] == 100
    # Currency + config blocks present.
    assert body["currency"]["code"] == "EUR"
    assert "max_date_range_days" in body["config"]


@pytest.mark.asyncio
async def test_global_stats_no_conversion_at_unit_rate(tmp_path, monkeypatch):
    """USD (rate 1.0) leaves every figure untouched but still ships the block."""
    monkeypatch.setattr("stackunderflow.deps.store_path", _empty_store(tmp_path))
    monkeypatch.setattr("stackunderflow.store.queries.get_global_stats", lambda conn: _fake_stats())
    _patch_currency(monkeypatch, code="USD", rate=1.0)

    response = await get_global_stats()
    body = json.loads(response.body.decode("utf-8"))

    assert body["models"]["claude-opus-4-6"]["cost"] == pytest.approx(2.0)
    assert body["daily_costs"][0]["by_model"]["claude-opus-4-6"] == pytest.approx(2.0)
    assert body["currency"]["code"] == "USD"


@pytest.mark.asyncio
async def test_global_stats_runs_off_event_loop(tmp_path, monkeypatch):
    """RANK 16: the blocking store read runs in a worker thread, not the loop."""
    monkeypatch.setattr("stackunderflow.deps.store_path", _empty_store(tmp_path))
    _patch_currency(monkeypatch, code="USD", rate=1.0)

    captured: dict[str, int] = {}

    def spy(conn):
        captured["tid"] = threading.get_ident()
        return _fake_stats()

    monkeypatch.setattr("stackunderflow.store.queries.get_global_stats", spy)
    loop_tid = threading.get_ident()
    await get_global_stats()
    assert captured.get("tid") is not None, "blocking body never ran"
    assert captured["tid"] != loop_tid, "blocking work ran on the event-loop thread"
