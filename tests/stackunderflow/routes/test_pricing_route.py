"""Tests for ``GET /api/pricing/doctor`` — read-only pricing health surface.

Locks the response shape, the query-param passthrough (``stale_days`` /
``limit``), and that the endpoint is genuinely read-only — it returns a
valid payload against a brand-new store with no schema applied and never
writes the DB. The data contract (unpriced / unknown / freshness) is
covered by ``test_pricing_invariants.py``; this file owns the HTTP edge.
"""

from __future__ import annotations

import itertools

import pytest
from fastapi import FastAPI
from fastapi.testclient import TestClient

import stackunderflow.deps as deps
from stackunderflow.routes.pricing import router as pricing_router
from stackunderflow.store import db, schema
from tests.conftest import app_route_paths, set_home_env

_SEQ = itertools.count()


@pytest.fixture()
def app_client(tmp_path, monkeypatch):
    """Mount only the pricing router against a schema-applied store."""
    store_db = tmp_path / "store.db"
    conn = db.connect(store_db)
    schema.apply(conn)
    conn.close()

    set_home_env(monkeypatch, tmp_path / "home")
    monkeypatch.setattr(deps, "store_path", store_db)

    app = FastAPI()
    app.include_router(pricing_router)
    return TestClient(app), store_db


# ── seeding ───────────────────────────────────────────────────────────────────


def _insert_event(
    conn,
    *,
    project_id,
    session_fk,
    model,
    cost_usd,
    cost_source="rate_card",
    provider="claude",
    input_tokens=0,
    output_tokens=0,
):
    seq = next(_SEQ)
    ts = "2026-04-01T00:00:00Z"
    conn.execute(
        "INSERT INTO messages "
        "(session_fk, seq, timestamp, role, model, "
        " input_tokens, output_tokens, cache_create_tokens, cache_read_tokens, "
        " content_text, tools_json, raw_json, is_sidechain) "
        "VALUES (?, ?, ?, 'assistant', ?, ?, ?, 0, 0, '', '[]', '{}', 0)",
        (session_fk, seq, ts, model, input_tokens, output_tokens),
    )
    mid = int(
        conn.execute(
            "SELECT next_id - 1 FROM _messages_id_seq WHERE rowid_kind = 1"
        ).fetchone()[0]
    )
    conn.execute(
        "INSERT INTO usage_events "
        "(source_message_fk, provider, account, project_id, session_id, ts, day, "
        " model, speed, input_tokens, output_tokens, cache_read_tokens, "
        " cache_create_tokens, cost_usd, cost_source, role) "
        "VALUES (?, ?, 'default', ?, 's1', ?, '2026-04-01', ?, 'standard', "
        " ?, ?, 0, 0, ?, ?, 'assistant')",
        (mid, provider, project_id, ts, model, input_tokens, output_tokens, cost_usd, cost_source),
    )


def _seed(store_db, events):
    conn = db.connect(store_db)
    pid = int(
        conn.execute(
            "INSERT INTO projects (provider, slug, display_name, first_seen, last_modified) "
            "VALUES ('claude', '-a', '-a', 0.0, 0.0)"
        ).lastrowid
    )
    sfk = int(
        conn.execute(
            "INSERT INTO sessions (project_id, session_id, first_ts, last_ts, message_count) "
            "VALUES (?, 's1', '2026-04-01T00:00:00Z', '2026-04-01T00:00:00Z', 1)",
            (pid,),
        ).lastrowid
    )
    for ev in events:
        _insert_event(conn, project_id=pid, session_fk=sfk, **ev)
    conn.commit()
    conn.close()


# ── shape ──────────────────────────────────────────────────────────────────────


class TestShape:
    def test_route_is_registered(self, app_client):
        client, _ = app_client
        assert "/api/pricing/doctor" in app_route_paths(client.app)

    def test_empty_store_shape(self, app_client):
        client, _ = app_client
        resp = client.get("/api/pricing/doctor")
        assert resp.status_code == 200
        body = resp.json()
        assert set(body.keys()) == {
            "stale_days", "ok", "summary", "unpriced_models",
            "unknown_cost_source", "rate_freshness",
        }
        assert body["ok"] is True
        assert body["summary"]["total_events"] == 0
        assert body["unpriced_models"] == []

    def test_fresh_store_without_schema_is_read_only_safe(self, tmp_path, monkeypatch):
        """A brand-new DB with NO schema applied (no ``usage_events`` table)
        must still return a valid payload — the route never applies schema or
        writes, so it degrades to an empty report."""
        store_db = tmp_path / "fresh.db"
        set_home_env(monkeypatch, tmp_path / "home")
        monkeypatch.setattr(deps, "store_path", store_db)
        app = FastAPI()
        app.include_router(pricing_router)
        resp = TestClient(app).get("/api/pricing/doctor")
        assert resp.status_code == 200
        assert resp.json()["summary"]["total_events"] == 0
        # The route must not have created the usage_events table.
        conn = db.connect(store_db)
        exists = conn.execute(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name='usage_events'"
        ).fetchone()
        conn.close()
        assert exists is None


# ── data ───────────────────────────────────────────────────────────────────────


class TestData:
    def test_unpriced_and_unknown_surface(self, app_client):
        client, store_db = app_client
        _seed(
            store_db,
            [
                {"model": "claude-opus-4-8", "cost_usd": 0.5,
                 "input_tokens": 1000, "output_tokens": 500},
                {"model": "exotic-model-x", "cost_usd": 0.0, "cost_source": "unknown",
                 "input_tokens": 2000, "output_tokens": 1000},
            ],
        )
        body = client.get("/api/pricing/doctor").json()
        assert body["summary"]["total_events"] == 2
        assert body["summary"]["unpriced_model_count"] == 1
        assert body["summary"]["unknown_cost_source_model_count"] == 1
        assert body["unpriced_models"][0]["model"] == "exotic-model-x"
        # Estimable exposure for a claude-shape unknown id (fallback-priced).
        assert body["unpriced_models"][0]["estimated_delta_usd"] > 0
        assert body["ok"] is True  # unknown→$0 is expected, not a defect

    def test_billable_unpriced_flips_ok(self, app_client):
        client, store_db = app_client
        _seed(
            store_db,
            [{"model": "bogus-priced", "cost_usd": 2.0, "cost_source": "rate_card",
              "input_tokens": 1000}],
        )
        body = client.get("/api/pricing/doctor").json()
        assert body["ok"] is False
        assert body["summary"]["billable_unpriced_model_count"] == 1

    def test_query_params_passthrough(self, app_client):
        client, store_db = app_client
        body = client.get("/api/pricing/doctor?stale_days=30&limit=1").json()
        assert body["stale_days"] == 30
        assert body["rate_freshness"]["stale_days_threshold"] == 30

    def test_limit_truncates_lists(self, app_client):
        client, store_db = app_client
        _seed(
            store_db,
            [
                {"model": f"exotic-{i}", "cost_usd": 0.0, "cost_source": "unknown",
                 "input_tokens": 1000 * (i + 1)}
                for i in range(4)
            ],
        )
        body = client.get("/api/pricing/doctor?limit=2").json()
        # Full count in the summary, truncated list in the body.
        assert body["summary"]["unpriced_model_count"] == 4
        assert len(body["unpriced_models"]) == 2
