"""HTTP-route tests for /api/cfg/*.

The CLI already covers the underlying ``Settings.persist`` round-trip; here we
just verify the FastAPI handlers translate JSON in/out correctly and surface
validation errors as 400s.
"""

from __future__ import annotations

import json
from pathlib import Path
from unittest.mock import patch

import pytest
from fastapi.testclient import TestClient

from stackunderflow.routes import cfg as cfg_routes
from stackunderflow.server import app


@pytest.fixture
def client(tmp_path: Path):
    """A TestClient with settings I/O redirected to a tmp dir."""
    app_dir = tmp_path / ".stackunderflow"
    app_dir.mkdir(exist_ok=True)
    cfg_file = app_dir / "config.json"
    with (
        patch("stackunderflow.settings._APP_DIR", app_dir),
        patch("stackunderflow.settings._CFG_FILE", cfg_file),
    ):
        yield TestClient(app)


def test_get_cfg_returns_settings_and_currency(client):
    r = client.get("/api/cfg")
    assert r.status_code == 200
    body = r.json()
    assert "settings" in body
    assert "currency" in body
    assert body["currency"]["code"] in ("USD",) or len(body["currency"]["code"]) == 3
    assert body["settings"]["currency"]  # non-empty default


def test_get_currencies_includes_common_codes(client):
    r = client.get("/api/cfg/currencies")
    assert r.status_code == 200
    body = r.json()
    assert "USD" in body["common"]
    assert "EUR" in body["common"]
    assert "GBP" in body["common"]
    assert isinstance(body["supported"], list)
    assert body["current"]["code"]


def test_set_currency_persists_to_disk(client, tmp_path):
    cfg_file = tmp_path / ".stackunderflow" / "config.json"
    r = client.post("/api/cfg/currency", json={"code": "EUR"})
    assert r.status_code == 200, r.text
    body = r.json()
    assert body["currency"]["code"] == "EUR"
    on_disk = json.loads(cfg_file.read_text())
    assert on_disk["currency"] == "EUR"


def test_set_currency_uppercases(client):
    r = client.post("/api/cfg/currency", json={"code": "eur"})
    assert r.status_code == 200, r.text
    assert r.json()["currency"]["code"] == "EUR"


def test_set_currency_rejects_bad_code(client):
    r = client.post("/api/cfg/currency", json={"code": "EUROS"})
    assert r.status_code == 400


def test_set_currency_rejects_missing_body(client):
    r = client.post("/api/cfg/currency", json={})
    assert r.status_code == 400


def test_model_aliases_round_trip(client, tmp_path):
    cfg_file = tmp_path / ".stackunderflow" / "config.json"

    # Initially empty
    r = client.get("/api/cfg/model-aliases")
    assert r.status_code == 200
    assert r.json() == {"aliases": {}}

    # Add one
    r = client.post(
        "/api/cfg/model-aliases",
        json={"from": "openrouter/claude-opus", "to": "claude-opus-4-6"},
    )
    assert r.status_code == 200, r.text
    assert r.json()["aliases"] == {"openrouter/claude-opus": "claude-opus-4-6"}
    on_disk = json.loads(cfg_file.read_text())
    assert on_disk["model_aliases"] == {"openrouter/claude-opus": "claude-opus-4-6"}

    # Add a second one — ensure the first persists
    r = client.post(
        "/api/cfg/model-aliases",
        json={"from": "litellm/sonnet", "to": "claude-sonnet-4-6"},
    )
    assert r.status_code == 200
    assert r.json()["aliases"] == {
        "openrouter/claude-opus": "claude-opus-4-6",
        "litellm/sonnet": "claude-sonnet-4-6",
    }

    # Delete one — note the slash in the source id
    r = client.delete("/api/cfg/model-aliases", params={"from": "litellm/sonnet"})
    assert r.status_code == 200, r.text
    assert r.json()["aliases"] == {"openrouter/claude-opus": "claude-opus-4-6"}

    # Deleting a missing one returns 404
    r = client.delete("/api/cfg/model-aliases", params={"from": "does-not-exist"})
    assert r.status_code == 404

    # Missing ?from is a 400
    r = client.delete("/api/cfg/model-aliases")
    assert r.status_code == 400


def test_model_aliases_rejects_empty_fields(client):
    r = client.post("/api/cfg/model-aliases", json={"from": "", "to": "x"})
    assert r.status_code == 400
    r = client.post("/api/cfg/model-aliases", json={"from": "x", "to": ""})
    assert r.status_code == 400
    r = client.post("/api/cfg/model-aliases", json={"from": "x"})
    assert r.status_code == 400


def test_set_currency_invalidates_dashboard_cache(client, monkeypatch):
    """Changing the currency must drop any cached dashboard payloads.

    Otherwise the next /api/dashboard-data response would still serve a
    USD-scaled body for the old hot-cache key.
    """
    called: list[str | None] = []
    monkeypatch.setattr(
        cfg_routes,
        "invalidate_dashboard_cache",
        lambda slug=None: called.append(slug),
    )
    r = client.post("/api/cfg/currency", json={"code": "EUR"})
    assert r.status_code == 200
    assert called == [None]
