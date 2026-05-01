"""Tests for ``GET /api/plan`` — current plan + usage payload.

Mirrors the CLI ``plan show`` shape; we cover the no-plan-set case, the
happy path with a populated store, and the status banding so the
frontend's traffic-light contract is locked in.
"""

from __future__ import annotations

from datetime import UTC, datetime, timedelta
from pathlib import Path
from unittest.mock import patch

import pytest
from fastapi import FastAPI
from fastapi.testclient import TestClient

import stackunderflow.deps as deps
from stackunderflow.routes.plan import router as plan_router
from stackunderflow.services import plans as plans_mod
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
    """Mount only the plan router with an empty store."""
    store_db = tmp_path / "store.db"
    conn = db.connect(store_db)
    schema.apply(conn)
    conn.close()

    monkeypatch.setattr(deps, "store_path", store_db)

    app = FastAPI()
    app.include_router(plan_router)
    return TestClient(app), store_db


def _seed_message(store_db: Path, *, timestamp: str, in_tok: int, out_tok: int) -> None:
    """Insert one assistant message into the store at ``timestamp``."""
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
        (proj_id, "s1", timestamp, timestamp, 1),
    )
    sess_fk = cur.lastrowid
    conn.execute(
        "INSERT INTO messages "
        "(session_fk, seq, timestamp, role, model, "
        " input_tokens, output_tokens, cache_create_tokens, cache_read_tokens, "
        " content_text, tools_json, raw_json, is_sidechain) "
        "VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        (
            sess_fk, 0, timestamp, "assistant",
            "claude-sonnet-4-5-20250929",
            in_tok, out_tok, 0, 0, "", "[]", "{}", 0,
        ),
    )
    conn.commit()
    conn.close()


# ── happy path ──────────────────────────────────────────────────────────────

class TestPlanRoute:
    def test_no_plan_returns_nulls(self, app_client, tmp_path):
        client, _ = app_client
        p1, p2 = _patch_settings_dir(tmp_path)
        with p1, p2:
            r = client.get("/api/plan")
            assert r.status_code == 200
            assert r.json() == {"plan": None, "usage": None}

    def test_plan_set_but_no_messages(self, app_client, tmp_path):
        client, _ = app_client
        p1, p2 = _patch_settings_dir(tmp_path)
        with p1, p2:
            plans_mod.set_plan("claude-pro")

            r = client.get("/api/plan")
            assert r.status_code == 200
            data = r.json()
            assert data["plan"] == {
                "name": "claude-pro",
                "monthly_usd": 20.0,
                "reset_day": 1,
            }
            usage = data["usage"]
            assert usage["used"] == 0.0
            assert usage["budget"] == 20.0
            assert usage["remaining"] == 20.0
            assert usage["pct"] == 0.0
            assert usage["status"] == "ok"
            # Period bounds always present
            assert "period_start" in usage
            assert "period_end" in usage
            assert "projected" in usage

    def test_plan_with_messages_in_window(self, app_client, tmp_path):
        client, store_db = app_client
        p1, p2 = _patch_settings_dir(tmp_path)
        with p1, p2:
            # Seed a message dated today so it falls inside this period.
            now_ts = datetime.now(UTC).isoformat()
            _seed_message(store_db, timestamp=now_ts, in_tok=1000, out_tok=500)

            plans_mod.set_plan("claude-max")  # $200 budget — message cost is small

            r = client.get("/api/plan")
            assert r.status_code == 200
            data = r.json()
            assert data["plan"]["monthly_usd"] == 200.0
            usage = data["usage"]
            # Real cost from the rate card; just assert the shape and that
            # we got a non-negative number tracking through to the response.
            assert usage["used"] >= 0.0
            assert usage["budget"] == 200.0
            assert usage["remaining"] == usage["budget"] - usage["used"]
            assert usage["status"] in {"ok", "warn", "over"}

    def test_messages_outside_window_excluded(self, app_client, tmp_path):
        """A message from last year must not contribute to this period's spend."""
        client, store_db = app_client
        p1, p2 = _patch_settings_dir(tmp_path)
        with p1, p2:
            # 2 years ago — well outside any realistic billing period.
            old_ts = (datetime.now(UTC) - timedelta(days=730)).isoformat()
            _seed_message(store_db, timestamp=old_ts, in_tok=10_000_000, out_tok=10_000_000)

            plans_mod.set_plan("claude-pro")

            r = client.get("/api/plan")
            assert r.status_code == 200
            usage = r.json()["usage"]
            assert usage["used"] == 0.0
            assert usage["status"] == "ok"


# ── status banding ──────────────────────────────────────────────────────────

class TestStatusBanding:
    """Patch the spend rollup directly so we can pin every status branch."""

    def _patch_spend(self, total: float):
        return patch(
            "stackunderflow.routes.plan._spend_in_window",
            return_value=total,
        )

    def test_status_ok(self, app_client, tmp_path):
        client, _ = app_client
        p1, p2 = _patch_settings_dir(tmp_path)
        with p1, p2:
            plans_mod.set_plan("claude-pro")  # $20 budget
            with self._patch_spend(5.0):
                r = client.get("/api/plan")
                assert r.json()["usage"]["status"] == "ok"

    def test_status_warn_at_80_pct(self, app_client, tmp_path):
        client, _ = app_client
        p1, p2 = _patch_settings_dir(tmp_path)
        with p1, p2:
            plans_mod.set_plan("claude-pro")
            with self._patch_spend(16.0):
                r = client.get("/api/plan")
                assert r.json()["usage"]["status"] == "warn"

    def test_status_over_above_100_pct(self, app_client, tmp_path):
        client, _ = app_client
        p1, p2 = _patch_settings_dir(tmp_path)
        with p1, p2:
            plans_mod.set_plan("claude-pro")
            with self._patch_spend(25.0):
                r = client.get("/api/plan")
                data = r.json()
                assert data["usage"]["status"] == "over"
                assert data["usage"]["remaining"] < 0
