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
            body = r.json()
            assert body["plan"] is None
            assert body["usage"] is None
            # Currency block is always stamped — same contract as every other
            # cost-bearing endpoint. UI reads it once per fetch.
            assert body["currency"]["code"] == "USD"
            assert body["currency"]["rate_from_usd"] == 1.0

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


# ── currency conversion ────────────────────────────────────────────────────


class TestCurrencyConversion:
    """`usage.*` dollar fields must be pre-converted via the active currency.

    The plan's ``monthly_usd`` keeps the literal USD value (it's the user's
    contract amount), but ``used`` / ``budget`` / ``remaining`` / ``projected``
    inside ``usage`` track the active currency so a single ``formatCost`` callsite
    renders correctly. Status banding is computed against USD before conversion
    so the % thresholds stay stable across currencies.
    """

    def test_usage_fields_converted_when_non_usd(self, app_client, tmp_path):
        client, _ = app_client
        p1, p2 = _patch_settings_dir(tmp_path)
        rate = 0.5  # 1 USD = 0.5 EUR (purely synthetic for the test)
        with (
            p1,
            p2,
            patch(
                "stackunderflow.routes.plan.active_currency_payload",
                return_value={"code": "EUR", "symbol": "€", "rate_from_usd": rate},
            ),
            patch(
                "stackunderflow.routes.plan._spend_in_window",
                return_value=10.0,
            ),
        ):
            plans_mod.set_plan("claude-pro")  # $20 USD budget
            r = client.get("/api/plan")
            assert r.status_code == 200
            body = r.json()
            assert body["currency"]["code"] == "EUR"
            assert body["currency"]["rate_from_usd"] == rate
            usage = body["usage"]
            # Pre-converted: 10 USD spend × 0.5 = 5 EUR; 20 USD budget × 0.5 = 10 EUR.
            assert usage["used"] == pytest.approx(5.0)
            assert usage["budget"] == pytest.approx(10.0)
            assert usage["remaining"] == pytest.approx(5.0)
            # pct is dimensionless and computed pre-conversion: 10/20 = 50%.
            assert usage["pct"] == pytest.approx(50.0)
            assert usage["status"] == "ok"
            # Plan keeps the canonical USD amount under its key.
            assert body["plan"]["monthly_usd"] == 20.0


# ── burn projector v2 ──────────────────────────────────────────────────────


class TestProjectionBlock:
    """The ``projection`` block on ``/api/plan`` mirrors ``services.burn``.

    All these tests patch the spend rollups directly so the route's wiring
    is exercised without a populated store; the math itself is covered in
    ``test_burn.py``.
    """

    def test_no_plan_projection_is_null(self, app_client, tmp_path):
        client, _ = app_client
        p1, p2 = _patch_settings_dir(tmp_path)
        with p1, p2:
            r = client.get("/api/plan")
            assert r.status_code == 200
            assert r.json()["projection"] is None

    def test_projection_keys_present_with_plan(self, app_client, tmp_path):
        client, _ = app_client
        p1, p2 = _patch_settings_dir(tmp_path)
        with (
            p1,
            p2,
            patch("stackunderflow.routes.plan._spend_in_window", return_value=5.0),
            patch(
                "stackunderflow.routes.plan._spend_daily_window",
                return_value=[5.0],
            ),
        ):
            plans_mod.set_plan("claude-pro")  # $20 budget
            r = client.get("/api/plan")
            assert r.status_code == 200
            proj = r.json()["projection"]
            assert proj is not None
            for key in (
                "projected_month_end_usd",
                "projection_method",
                "daily_burn_usd",
                "days_to_limit",
                "thresholds",
                "crossed_threshold",
                "alert",
            ):
                assert key in proj
            assert proj["projection_method"] in ("linear", "weighted-7d")
            # Single non-zero day → linear (below the 3-sample threshold).
            assert proj["projection_method"] == "linear"
            assert proj["thresholds"] == [50, 75, 90]

    def test_weighted_method_with_three_days(self, app_client, tmp_path):
        client, _ = app_client
        p1, p2 = _patch_settings_dir(tmp_path)
        with (
            p1,
            p2,
            patch("stackunderflow.routes.plan._spend_in_window", return_value=6.0),
            patch(
                "stackunderflow.routes.plan._spend_daily_window",
                return_value=[1.0, 2.0, 3.0],
            ),
        ):
            plans_mod.set_plan("claude-max")  # $200 budget
            r = client.get("/api/plan")
            proj = r.json()["projection"]
            assert proj["projection_method"] == "weighted-7d"
            # daily_burn > 0 with 3 non-zero samples
            assert proj["daily_burn_usd"] > 0
            # Far from any threshold on a $200 plan ($6 used = 3%) → no alert
            assert proj["crossed_threshold"] is None
            assert proj["alert"] is None

    def test_alert_surfaces_when_threshold_crossed(self, app_client, tmp_path):
        client, _ = app_client
        p1, p2 = _patch_settings_dir(tmp_path)
        with (
            p1,
            p2,
            patch("stackunderflow.routes.plan._spend_in_window", return_value=11.0),
            patch(
                "stackunderflow.routes.plan._spend_daily_window",
                # Last day of the period — no projected overrun, just the
                # threshold-crossed banner.
                return_value=[11.0],
            ),
            patch(
                "stackunderflow.services.plans.compute_usage",
                wraps=plans_mod.compute_usage,
            ) as wrapped,
        ):
            plans_mod.set_plan("claude-pro")  # $20 budget → 11/20 = 55%
            r = client.get("/api/plan")
            proj = r.json()["projection"]
            # 55% > 50% threshold → 50 is the highest crossed.
            assert proj["crossed_threshold"] == 50
            # Alert text could be either "Crossed 50%" or the projected
            # overrun depending on days-left, but must be non-null.
            assert proj["alert"] is not None
            wrapped.assert_called()

    def test_custom_thresholds_propagate_via_settings(self, app_client, tmp_path):
        client, _ = app_client
        p1, p2 = _patch_settings_dir(tmp_path)
        with (
            p1,
            p2,
            patch("stackunderflow.routes.plan._spend_in_window", return_value=1.0),
            patch(
                "stackunderflow.routes.plan._spend_daily_window",
                return_value=[1.0],
            ),
        ):
            plans_mod.set_plan("claude-pro")
            from stackunderflow.settings import Settings
            Settings().persist("plan_alert_thresholds", [25, 60])

            r = client.get("/api/plan")
            assert r.json()["projection"]["thresholds"] == [25, 60]

    def test_projection_currency_converts(self, app_client, tmp_path):
        """`projected_month_end_usd` and `daily_burn_usd` must follow the rate."""
        client, _ = app_client
        p1, p2 = _patch_settings_dir(tmp_path)
        rate = 0.5
        with (
            p1,
            p2,
            patch(
                "stackunderflow.routes.plan.active_currency_payload",
                return_value={"code": "EUR", "symbol": "€", "rate_from_usd": rate},
            ),
            patch("stackunderflow.routes.plan._spend_in_window", return_value=10.0),
            patch(
                "stackunderflow.routes.plan._spend_daily_window",
                # Single observation, last day → linear → daily_burn = $10.
                return_value=[10.0],
            ),
        ):
            plans_mod.set_plan("claude-pro")  # $20 USD
            r = client.get("/api/plan")
            proj = r.json()["projection"]
            # daily_burn USD = 10 → EUR = 5.
            assert proj["daily_burn_usd"] == pytest.approx(5.0)
            # Dimensionless fields stay as-is.
            assert proj["thresholds"] == [50, 75, 90]
