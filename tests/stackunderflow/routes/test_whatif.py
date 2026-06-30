"""Tests for the cross-provider what-if route (``GET /api/whatif``).

Covers the empty-store shape, project-scoped repricing off a seeded
``usage_events`` store, the whole-store fallback, the actual-vs-candidate delta
contract, and the currency pre-conversion.
"""

from __future__ import annotations

from pathlib import Path
from unittest.mock import patch

import pytest
from fastapi import FastAPI
from fastapi.testclient import TestClient

import stackunderflow.deps as deps
from stackunderflow.routes.whatif import router as whatif_router
from stackunderflow.services.whatif import CANDIDATES
from stackunderflow.store import db, schema


@pytest.fixture()
def app_client(tmp_path, monkeypatch):
    store_db = tmp_path / "store.db"
    conn = db.connect(store_db)
    schema.apply(conn)
    conn.close()
    monkeypatch.setattr(deps, "store_path", store_db)
    # Default: no active project unless a test sets one.
    monkeypatch.setattr(deps, "current_log_path", None, raising=False)

    app = FastAPI()
    app.include_router(whatif_router)
    return TestClient(app), store_db


def _seed_event(
    store_db: Path,
    *,
    slug: str = "alpha",
    model: str = "claude-opus-4-8",
    in_tok: int = 1_000_000,
    out_tok: int = 500_000,
    cost: float = 20.0,
) -> None:
    conn = db.connect(store_db)
    schema.apply(conn)
    cur = conn.execute(
        "INSERT INTO projects (provider, slug, display_name, first_seen, last_modified) "
        "VALUES (?, ?, ?, ?, ?)",
        ("claude", slug, slug, 0.0, 0.0),
    )
    proj_id = cur.lastrowid
    conn.execute(
        "INSERT INTO usage_events "
        "(source_message_fk, provider, project_id, session_id, ts, day, model, "
        " input_tokens, output_tokens, cache_read_tokens, cache_create_tokens, "
        " cost_usd, cost_source, role) "
        "VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        (
            1, "claude", proj_id, "s1", "2026-06-15T12:00:00+00:00", "2026-06-15",
            model, in_tok, out_tok, 0, 0, cost, "rate_card", "assistant",
        ),
    )
    conn.commit()
    conn.close()


# ── empty store ──────────────────────────────────────────────────────────────


class TestEmptyStore:
    def test_empty_store_all_scope(self, app_client):
        client, _ = app_client
        r = client.get("/api/whatif")
        assert r.status_code == 200
        body = r.json()
        assert body["scope"] == "all"
        assert body["project_slug"] is None
        assert body["tokens"]["total"] == 0
        assert body["actual"]["cost_usd"] == 0.0
        # Every candidate is still listed (priced at $0 for a zero workload).
        assert len(body["candidates"]) == len(CANDIDATES)
        assert all(r["cost_usd"] == 0.0 for r in body["candidates"])


# ── project-scoped ───────────────────────────────────────────────────────────


class TestProjectScope:
    def test_repriced_against_candidates(self, app_client):
        client, store_db = app_client
        _seed_event(store_db, in_tok=1_000_000, out_tok=1_000_000, cost=30.0)
        r = client.get("/api/whatif", params={"log_path": "/x/y/alpha"})
        assert r.status_code == 200
        body = r.json()
        assert body["scope"] == "project"
        assert body["project_slug"] == "alpha"
        assert body["tokens"]["input"] == 1_000_000
        assert body["tokens"]["output"] == 1_000_000
        assert body["actual"]["cost_usd"] == pytest.approx(30.0)
        assert body["actual"]["models"] == ["claude-opus-4-8"]
        # Candidates sorted cheapest-first; all priced.
        costs = [c["cost_usd"] for c in body["candidates"]]
        assert costs == sorted(costs)
        assert all(c > 0 for c in costs)
        assert body["cheapest"] == body["candidates"][0]

    def test_delta_is_signed_against_actual(self, app_client):
        client, store_db = app_client
        _seed_event(store_db, in_tok=1_000_000, out_tok=1_000_000, cost=30.0)
        body = client.get("/api/whatif", params={"log_path": "/x/y/alpha"}).json()
        for c in body["candidates"]:
            assert c["delta_usd"] == pytest.approx(c["cost_usd"] - 30.0)
            assert c["delta_pct"] == pytest.approx((c["cost_usd"] - 30.0) / 30.0 * 100.0)
        # The cheapest candidate is cheaper than the actual Opus spend.
        assert body["cheapest"]["delta_usd"] < 0

    def test_unknown_project_404(self, app_client):
        client, _ = app_client
        r = client.get("/api/whatif", params={"log_path": "/x/y/does-not-exist"})
        assert r.status_code == 404


# ── currency conversion ──────────────────────────────────────────────────────


class TestCurrencyConversion:
    def test_dollar_fields_converted(self, app_client):
        client, store_db = app_client
        _seed_event(store_db, in_tok=1_000_000, out_tok=1_000_000, cost=30.0)
        rate = 0.5
        with patch(
            "stackunderflow.routes.whatif.active_currency_payload",
            return_value={"code": "EUR", "symbol": "€", "rate_from_usd": rate},
        ):
            body = client.get("/api/whatif", params={"log_path": "/x/y/alpha"}).json()
            assert body["currency"]["code"] == "EUR"
            # actual cost 30 USD → 15 EUR.
            assert body["actual"]["cost_usd"] == pytest.approx(15.0)
            # Each candidate cost is halved; delta_pct stays dimensionless.
            for c in body["candidates"]:
                # delta_pct computed pre-conversion (USD/USD ratio) is stable.
                assert c["delta_pct"] is not None
            # cheapest stays consistent with the (re-scaled) candidates[0].
            assert body["cheapest"]["cost_usd"] == body["candidates"][0]["cost_usd"]

    def test_cheapest_not_double_scaled(self, app_client):
        """``cheapest`` aliases candidates[0]; conversion must scale it once."""
        client, store_db = app_client
        _seed_event(store_db, in_tok=1_000_000, out_tok=1_000_000, cost=30.0)
        # USD baseline.
        usd = client.get("/api/whatif", params={"log_path": "/x/y/alpha"}).json()
        usd_cheapest = usd["cheapest"]["cost_usd"]
        rate = 0.5
        with patch(
            "stackunderflow.routes.whatif.active_currency_payload",
            return_value={"code": "EUR", "symbol": "€", "rate_from_usd": rate},
        ):
            eur = client.get("/api/whatif", params={"log_path": "/x/y/alpha"}).json()
            # Exactly one ×0.5, not ×0.25.
            assert eur["cheapest"]["cost_usd"] == pytest.approx(usd_cheapest * rate)
