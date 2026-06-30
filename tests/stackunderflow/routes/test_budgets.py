"""Tests for the spend-budget routes (``GET/PUT/DELETE /api/budgets``).

Covers the no-budget CTA shape, set/clear round-trips through the route,
status banding off a seeded ``usage_events`` store, the independent-leg PUT
semantics, validation (a non-positive ceiling → 422), and the currency
pre-conversion contract.
"""

from __future__ import annotations

from datetime import UTC, datetime
from pathlib import Path
from unittest.mock import patch

import pytest
from fastapi import FastAPI
from fastapi.testclient import TestClient

import stackunderflow.deps as deps
from stackunderflow.routes.budgets import router as budgets_router
from stackunderflow.store import db, schema


def _patch_settings_dir(tmpdir: Path):
    app_dir = tmpdir / ".stackunderflow"
    app_dir.mkdir(exist_ok=True)
    cfg_file = app_dir / "config.json"
    return (
        patch("stackunderflow.settings._APP_DIR", app_dir),
        patch("stackunderflow.settings._CFG_FILE", cfg_file),
    )


@pytest.fixture()
def app_client(tmp_path, monkeypatch):
    store_db = tmp_path / "store.db"
    conn = db.connect(store_db)
    schema.apply(conn)
    conn.close()
    monkeypatch.setattr(deps, "store_path", store_db)

    app = FastAPI()
    app.include_router(budgets_router)
    return TestClient(app), store_db


def _seed_event(store_db: Path, *, ts: str, cost: float, model: str = "claude-opus-4-8") -> None:
    """Insert one ``usage_events`` row at ``ts`` with the given cost."""
    conn = db.connect(store_db)
    schema.apply(conn)
    cur = conn.execute(
        "INSERT INTO projects (provider, slug, display_name, first_seen, last_modified) "
        "VALUES (?, ?, ?, ?, ?)",
        ("claude", "alpha", "alpha", 0.0, 0.0),
    )
    proj_id = cur.lastrowid
    conn.execute(
        "INSERT INTO usage_events "
        "(source_message_fk, provider, project_id, session_id, ts, day, model, "
        " input_tokens, output_tokens, cache_read_tokens, cache_create_tokens, "
        " cost_usd, cost_source, role) "
        "VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        (
            1, "claude", proj_id, "s1", ts, ts[:10], model,
            1000, 500, 0, 0, cost, "rate_card", "assistant",
        ),
    )
    conn.commit()
    conn.close()


# ── no budget ────────────────────────────────────────────────────────────────


class TestNoBudget:
    def test_no_budget_returns_null_status(self, app_client, tmp_path):
        client, _ = app_client
        p1, p2 = _patch_settings_dir(tmp_path)
        with p1, p2:
            r = client.get("/api/budgets")
            assert r.status_code == 200
            body = r.json()
            assert body["budget"] == {"monthly_usd": None, "daily_usd": None}
            assert body["status"] is None
            assert body["currency"]["code"] == "USD"


# ── set / get round-trip ─────────────────────────────────────────────────────


class TestSetBudget:
    def test_put_monthly_then_get(self, app_client, tmp_path):
        client, _ = app_client
        p1, p2 = _patch_settings_dir(tmp_path)
        with p1, p2:
            r = client.put("/api/budgets", json={"monthly_usd": 150.0})
            assert r.status_code == 200
            body = r.json()
            assert body["budget"]["monthly_usd"] == 150.0
            assert body["budget"]["daily_usd"] is None
            assert body["status"] is not None
            assert body["status"]["monthly"]["budget"] == 150.0
            assert body["status"]["daily"] is None

            # Persisted across a fresh GET.
            g = client.get("/api/budgets").json()
            assert g["budget"]["monthly_usd"] == 150.0

    def test_put_both_legs(self, app_client, tmp_path):
        client, _ = app_client
        p1, p2 = _patch_settings_dir(tmp_path)
        with p1, p2:
            r = client.put("/api/budgets", json={"monthly_usd": 200.0, "daily_usd": 12.0})
            body = r.json()
            assert body["budget"]["monthly_usd"] == 200.0
            assert body["budget"]["daily_usd"] == 12.0
            assert body["status"]["daily"]["budget"] == 12.0

    def test_put_omitted_leg_preserved(self, app_client, tmp_path):
        """Omitting a leg in the PUT body leaves the persisted value intact."""
        client, _ = app_client
        p1, p2 = _patch_settings_dir(tmp_path)
        with p1, p2:
            client.put("/api/budgets", json={"monthly_usd": 200.0, "daily_usd": 12.0})
            # PUT only the daily leg — monthly must survive.
            r = client.put("/api/budgets", json={"daily_usd": 8.0})
            body = r.json()
            assert body["budget"]["monthly_usd"] == 200.0
            assert body["budget"]["daily_usd"] == 8.0

    def test_put_null_leg_clears_it(self, app_client, tmp_path):
        client, _ = app_client
        p1, p2 = _patch_settings_dir(tmp_path)
        with p1, p2:
            client.put("/api/budgets", json={"monthly_usd": 200.0, "daily_usd": 12.0})
            r = client.put("/api/budgets", json={"daily_usd": None})
            body = r.json()
            assert body["budget"]["monthly_usd"] == 200.0
            assert body["budget"]["daily_usd"] is None

    def test_non_positive_is_422(self, app_client, tmp_path):
        client, _ = app_client
        p1, p2 = _patch_settings_dir(tmp_path)
        with p1, p2:
            r = client.put("/api/budgets", json={"monthly_usd": 0.0})
            assert r.status_code == 422


# ── delete ───────────────────────────────────────────────────────────────────


class TestDeleteBudget:
    def test_delete_clears_budget(self, app_client, tmp_path):
        client, _ = app_client
        p1, p2 = _patch_settings_dir(tmp_path)
        with p1, p2:
            client.put("/api/budgets", json={"monthly_usd": 200.0, "daily_usd": 12.0})
            r = client.delete("/api/budgets")
            body = r.json()
            assert body["budget"] == {"monthly_usd": None, "daily_usd": None}
            assert body["status"] is None


# ── status banding off a seeded store ────────────────────────────────────────


class TestStatusBanding:
    def test_under_with_low_spend(self, app_client, tmp_path):
        client, store_db = app_client
        p1, p2 = _patch_settings_dir(tmp_path)
        with p1, p2:
            now = datetime.now(UTC).isoformat()
            _seed_event(store_db, ts=now, cost=5.0)
            client.put("/api/budgets", json={"monthly_usd": 200.0})
            body = client.get("/api/budgets").json()
            m = body["status"]["monthly"]
            assert m["used"] == pytest.approx(5.0)
            assert m["status"] == "under"
            # The model that drove the spend is surfaced.
            assert "claude-opus-4-8" in body["status"]["models"]

    def test_over_when_spend_exceeds_ceiling(self, app_client, tmp_path):
        client, store_db = app_client
        p1, p2 = _patch_settings_dir(tmp_path)
        with p1, p2:
            now = datetime.now(UTC).isoformat()
            _seed_event(store_db, ts=now, cost=250.0)
            client.put("/api/budgets", json={"monthly_usd": 200.0})
            body = client.get("/api/budgets").json()
            m = body["status"]["monthly"]
            assert m["status"] == "over"
            assert m["remaining"] < 0

    def test_daily_today_spend(self, app_client, tmp_path):
        client, store_db = app_client
        p1, p2 = _patch_settings_dir(tmp_path)
        with p1, p2:
            now = datetime.now(UTC).isoformat()
            _seed_event(store_db, ts=now, cost=9.0)
            client.put("/api/budgets", json={"daily_usd": 10.0})
            body = client.get("/api/budgets").json()
            d = body["status"]["daily"]
            assert d["used"] == pytest.approx(9.0)
            assert d["status"] == "approaching"  # 90% of $10


# ── currency conversion ──────────────────────────────────────────────────────


class TestCurrencyConversion:
    def test_status_fields_converted(self, app_client, tmp_path):
        client, store_db = app_client
        p1, p2 = _patch_settings_dir(tmp_path)
        rate = 0.5
        with (
            p1,
            p2,
            patch(
                "stackunderflow.routes.budgets.active_currency_payload",
                return_value={"code": "EUR", "symbol": "€", "rate_from_usd": rate},
            ),
        ):
            now = datetime.now(UTC).isoformat()
            _seed_event(store_db, ts=now, cost=10.0)
            client.put("/api/budgets", json={"monthly_usd": 200.0})
            body = client.get("/api/budgets").json()
            assert body["currency"]["code"] == "EUR"
            m = body["status"]["monthly"]
            # 200 USD budget × 0.5 = 100 EUR; 10 USD spend × 0.5 = 5 EUR.
            assert m["budget"] == pytest.approx(100.0)
            assert m["used"] == pytest.approx(5.0)
            # pct is dimensionless (USD/USD) → 10/200 = 5%.
            assert m["pct"] == pytest.approx(5.0)
            assert m["status"] == "under"
