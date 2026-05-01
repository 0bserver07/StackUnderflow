"""Tests for ``GET /api/export`` — the HTTP analogue of the CLI command.

Uses FastAPI's TestClient so we get the full request → response cycle
including the streamed body and Content-Disposition header.
"""

from __future__ import annotations

import json

import pytest
from fastapi import FastAPI
from fastapi.testclient import TestClient

import stackunderflow.deps as deps
from stackunderflow.routes.export import router as export_router
from stackunderflow.store import db, schema


@pytest.fixture()
def app_client(tmp_path, monkeypatch):
    """Build a minimal FastAPI app with only the export router mounted."""
    store_db = tmp_path / "store.db"
    conn = db.connect(store_db)
    schema.apply(conn)
    cur = conn.execute(
        "INSERT INTO projects (provider, slug, display_name, first_seen, last_modified) "
        "VALUES (?, ?, ?, ?, ?)",
        ("claude", "alpha", "alpha", 0.0, 0.0),
    )
    proj_id = cur.lastrowid
    cur = conn.execute(
        "INSERT INTO sessions (project_id, session_id, first_ts, last_ts, message_count) "
        "VALUES (?, ?, ?, ?, ?)",
        (proj_id, "s1", "2025-01-01T10:00:00Z", "2025-01-01T10:00:00Z", 1),
    )
    sess_fk = cur.lastrowid
    conn.execute(
        "INSERT INTO messages "
        "(session_fk, seq, timestamp, role, model, "
        " input_tokens, output_tokens, cache_create_tokens, cache_read_tokens, "
        " content_text, tools_json, raw_json, is_sidechain) "
        "VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        (
            sess_fk, 0, "2025-01-01T10:00:00Z", "assistant",
            "claude-sonnet-4-5-20250929",
            1000, 200, 30, 50, "", "[]", "{}", 0,
        ),
    )
    conn.commit()
    conn.close()

    monkeypatch.setattr(deps, "store_path", store_db)

    app = FastAPI()
    app.include_router(export_router)
    return TestClient(app)


class TestExportRoute:
    def test_csv_period_all(self, app_client):
        r = app_client.get("/api/export?format=csv&period=all")
        assert r.status_code == 200
        assert r.headers["content-type"].startswith("text/csv")
        assert "attachment" in r.headers["content-disposition"]
        assert ".csv" in r.headers["content-disposition"]
        assert "date,provider,project,cost_usd" in r.text
        assert "alpha" in r.text

    def test_json_period_all(self, app_client):
        r = app_client.get("/api/export?format=json&period=all")
        assert r.status_code == 200
        assert r.headers["content-type"].startswith("application/json")
        assert "attachment" in r.headers["content-disposition"]
        data = json.loads(r.text)
        assert "daily" in data
        assert "models" in data

    def test_default_is_multi_period(self, app_client):
        r = app_client.get("/api/export?format=json")
        assert r.status_code == 200
        data = json.loads(r.text)
        assert "today" in data
        assert "last_7d" in data
        assert "last_30d" in data

    def test_unknown_format_400s(self, app_client):
        r = app_client.get("/api/export?format=xml&period=all")
        assert r.status_code == 400

    def test_unknown_period_400s(self, app_client):
        r = app_client.get("/api/export?format=csv&period=yesterday")
        assert r.status_code == 400

    def test_provider_filter(self, app_client):
        r = app_client.get("/api/export?format=csv&period=all&provider=claude")
        assert r.status_code == 200
        assert "alpha" in r.text

        r2 = app_client.get("/api/export?format=csv&period=all&provider=does-not-exist")
        assert r2.status_code == 200
        assert "alpha" not in r2.text

    def test_project_include_repeatable(self, app_client):
        r = app_client.get("/api/export?format=csv&period=all&project=alpha")
        assert r.status_code == 200
        assert "alpha" in r.text

        r2 = app_client.get("/api/export?format=csv&period=all&project=other")
        assert r2.status_code == 200
        # alpha excluded by include filter that only allows 'other'
        rows = [ln for ln in r2.text.splitlines() if ln and not ln.startswith("#") and "alpha" in ln]
        assert rows == []

    def test_exclude_filter(self, app_client):
        r = app_client.get("/api/export?format=csv&period=all&exclude=alpha")
        assert r.status_code == 200
        rows = [ln for ln in r.text.splitlines() if ln and not ln.startswith("#") and "alpha" in ln]
        assert rows == []
