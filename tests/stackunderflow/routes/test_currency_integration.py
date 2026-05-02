"""End-to-end currency wiring through the cost / dashboard / commands routes.

These tests exercise the full FX-conversion path: set ``currency=GBP``,
seed Frankfurter rate cache, hit the route, assert the response carries
the active-currency payload AND that dollar figures are scaled in place.
"""
from __future__ import annotations

import json
from datetime import UTC, datetime

import pytest

from stackunderflow.infra import currency
from stackunderflow.routes.cost import get_cost_data
from stackunderflow.store import db, schema


@pytest.fixture
def gbp_environment(tmp_path, monkeypatch):
    """Activate currency=GBP, seed a fresh rate cache at GBP=0.80, return tmp_path."""
    monkeypatch.setenv("STACKUNDERFLOW_CURRENCY", "GBP")
    monkeypatch.setattr(currency.Path, "home", classmethod(lambda cls: tmp_path))

    cache_dir = tmp_path / ".stackunderflow" / "cache"
    cache_dir.mkdir(parents=True, exist_ok=True)
    cache_file = cache_dir / "exchange-rate.json"
    cache_file.write_text(json.dumps({
        "fetched_at": datetime.now(UTC).isoformat(),
        "rates": {"GBP": 0.80, "EUR": 0.93},
    }))
    return tmp_path


def _seed_project(store_db, slug: str) -> None:
    conn = db.connect(store_db)
    schema.apply(conn)
    conn.execute(
        "INSERT INTO projects (provider, slug, display_name, first_seen, last_modified) "
        "VALUES (?, ?, ?, ?, ?)",
        ("claude", slug, slug, 0.0, 0.0),
    )
    conn.commit()
    conn.close()


def _stats_with_costs() -> dict:
    """Stats payload with USD-denominated cost figures across the §A3 keys."""
    return {
        "session_costs": [
            {"session_id": "s1", "cost": 10.00, "tokens": {"input": 100}},
            {"session_id": "s2", "cost": 5.00,  "tokens": {"input": 50}},
        ],
        "command_costs": [{"interaction_id": "i1", "cost": 1.00}],
        "tool_costs": {"Read": {"calls": 5, "cost": 0.50}},
        "token_composition": {"daily": {}, "totals": {}, "per_session": {}},
        "outliers": {"high_tool_commands": [], "high_step_commands": []},
        "retry_signals": [],
        "session_efficiency": [],
        "error_cost": {"estimated_retry_cost": 2.00, "top_error_commands": []},
        "trends": {"current": {}, "prior": {}},
    }


@pytest.mark.asyncio
async def test_cost_data_converts_dollar_figures_to_gbp(gbp_environment, tmp_path, monkeypatch):
    store_db = tmp_path / "store.db"
    slug = "-gbp-proj"
    _seed_project(store_db, slug)

    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)
    monkeypatch.setattr("stackunderflow.deps.current_log_path", f"/fake/{slug}")
    monkeypatch.setattr(
        "stackunderflow.routes.cost.queries.get_project_stats",
        lambda conn, *, project_id, tz_offset=0: ([], _stats_with_costs()),
    )

    payload = await get_cost_data()

    # Currency block at top level
    assert payload["currency"]["code"] == "GBP"
    assert payload["currency"]["symbol"] == "£"
    assert payload["currency"]["rate_from_usd"] == 0.80

    # Cost figures scaled by 0.80
    assert payload["session_costs"][0]["cost"] == pytest.approx(8.00)
    assert payload["session_costs"][1]["cost"] == pytest.approx(4.00)
    assert payload["command_costs"][0]["cost"] == pytest.approx(0.80)
    assert payload["tool_costs"]["Read"]["cost"] == pytest.approx(0.40)
    assert payload["error_cost"]["estimated_retry_cost"] == pytest.approx(1.60)

    # Token counts and other non-cost fields are NOT scaled
    assert payload["session_costs"][0]["tokens"]["input"] == 100
    assert payload["tool_costs"]["Read"]["calls"] == 5


@pytest.mark.asyncio
async def test_cost_data_passes_through_when_currency_is_usd(tmp_path, monkeypatch):
    """USD path is the no-op short-circuit — values must be unscaled."""
    monkeypatch.setenv("STACKUNDERFLOW_CURRENCY", "USD")
    monkeypatch.setattr(currency.Path, "home", classmethod(lambda cls: tmp_path))

    store_db = tmp_path / "store.db"
    slug = "-usd-proj"
    _seed_project(store_db, slug)

    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)
    monkeypatch.setattr("stackunderflow.deps.current_log_path", f"/fake/{slug}")
    monkeypatch.setattr(
        "stackunderflow.routes.cost.queries.get_project_stats",
        lambda conn, *, project_id, tz_offset=0: ([], _stats_with_costs()),
    )

    payload = await get_cost_data()

    assert payload["currency"]["code"] == "USD"
    assert payload["currency"]["rate_from_usd"] == 1.0
    # Pass-through, no conversion
    assert payload["session_costs"][0]["cost"] == 10.00
    assert payload["error_cost"]["estimated_retry_cost"] == 2.00


@pytest.mark.asyncio
async def test_cost_data_falls_back_to_usd_when_rate_unavailable(tmp_path, monkeypatch):
    """If currency=ZZZ resolves to a code that exists in no table, the
    response must NOT silently mislabel USD numbers with the wrong symbol.
    The new contract: code switches back to USD, rate stays 1.0, and a
    ``warning`` is populated so the UI can banner it."""
    monkeypatch.setenv("STACKUNDERFLOW_CURRENCY", "ZZZ")  # not in any table
    monkeypatch.setattr(currency.Path, "home", classmethod(lambda cls: tmp_path))
    # Simulate offline so the fetch fails — combined with an unknown code
    # this should bottom-out and switch back to USD with a warning.
    monkeypatch.setattr(currency, "_fetch_from_frankfurter", lambda: None)

    store_db = tmp_path / "store.db"
    slug = "-zzz-proj"
    _seed_project(store_db, slug)

    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)
    monkeypatch.setattr("stackunderflow.deps.current_log_path", f"/fake/{slug}")
    monkeypatch.setattr(
        "stackunderflow.routes.cost.queries.get_project_stats",
        lambda conn, *, project_id, tz_offset=0: ([], _stats_with_costs()),
    )

    payload = await get_cost_data()

    # The loud failure mode: code switches to USD, warning is set.
    assert payload["currency"]["code"] == "USD"
    assert payload["currency"]["symbol"] == "$"
    assert payload["currency"]["rate_from_usd"] == 1.0
    assert payload["currency"]["warning"] is not None
    assert "ZZZ" in payload["currency"]["warning"]
    # Amounts unchanged because we degraded to USD.
    assert payload["session_costs"][0]["cost"] == 10.00


@pytest.mark.asyncio
async def test_cost_data_uses_snapshot_when_frankfurter_403(tmp_path, monkeypatch):
    """End-to-end smoke for the snapshot fallback: GBP active, Frankfurter
    mocked to fail (the 403 case from the bug report), no cache. Response
    must keep ``code='GBP'``, scale costs by the snapshot rate, and carry
    a non-null ``warning`` for the banner."""
    monkeypatch.setenv("STACKUNDERFLOW_CURRENCY", "GBP")
    monkeypatch.setattr(currency.Path, "home", classmethod(lambda cls: tmp_path))
    monkeypatch.setattr(currency, "_fetch_from_frankfurter", lambda: None)

    store_db = tmp_path / "store.db"
    slug = "-gbp-403-proj"
    _seed_project(store_db, slug)

    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)
    monkeypatch.setattr("stackunderflow.deps.current_log_path", f"/fake/{slug}")
    monkeypatch.setattr(
        "stackunderflow.routes.cost.queries.get_project_stats",
        lambda conn, *, project_id, tz_offset=0: ([], _stats_with_costs()),
    )

    payload = await get_cost_data()

    # Code must NOT degrade to USD — that was the original bug.
    assert payload["currency"]["code"] == "GBP"
    assert payload["currency"]["symbol"] == "£"
    snap = currency.RATES_SNAPSHOT["GBP"]
    assert payload["currency"]["rate_from_usd"] == snap
    assert payload["currency"]["rate_from_usd"] != 1.0
    assert payload["currency"]["warning"] is not None
    # Costs scaled by the snapshot rate (USD * snap)
    assert payload["session_costs"][0]["cost"] == pytest.approx(10.0 * snap)
