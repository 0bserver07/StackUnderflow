"""Route-level checks for the cost-intelligence additions to /api/optimize.

Asserts the payload now carries ``total_waste_usd`` (Σ priced waste across
patterns) and an ``anomalies`` block (cost-outlier detector output), and that
the dollar total agrees with the per-finding ``estimated_waste_usd`` figures.
"""

from __future__ import annotations

import pytest

from stackunderflow.reports.optimize import Finding
from stackunderflow.routes import optimize as optimize_route
from stackunderflow.store import db, schema


def _seed_store(store_db):
    conn = db.connect(store_db)
    schema.apply(conn)
    conn.execute(
        "INSERT INTO projects (provider, slug, display_name, first_seen, last_modified) "
        "VALUES ('claude', 'demo', 'demo', 0, 0)"
    )
    # A clean baseline of daily cost + one spike → the anomaly detector fires.
    for i, c in enumerate([1.0, 1.1, 0.9, 1.05, 0.95]):
        conn.execute(
            "INSERT INTO daily_mart "
            "(day, project_id, provider, model, speed, input_tokens, output_tokens, "
            " cache_read, cache_create, message_count, session_count, cost_usd) "
            "VALUES (?, 1, 'anthropic', 'claude-sonnet-4-6', 'standard', "
            " 0, 0, 0, 0, 1, 1, ?)",
            (f"2026-04-0{i + 1}", c),
        )
    conn.execute(
        "INSERT INTO daily_mart "
        "(day, project_id, provider, model, speed, input_tokens, output_tokens, "
        " cache_read, cache_create, message_count, session_count, cost_usd) "
        "VALUES ('2026-04-09', 1, 'anthropic', 'claude-sonnet-4-6', 'standard', "
        " 0, 0, 0, 0, 1, 1, 25.0)"
    )
    conn.commit()
    conn.close()


@pytest.mark.asyncio
async def test_payload_carries_total_waste_usd_and_anomalies(tmp_path, monkeypatch):
    store_db = tmp_path / "store.db"
    _seed_store(store_db)
    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)
    optimize_route.invalidate_optimize_cache()

    # Deterministic patterns: two priced findings + one unpriced.
    fake = [
        Finding(
            pattern_id="bloated_claude_md", severity="high", title="t1",
            description="d", affected_count=1, suggested_fix="f",
            estimated_waste_tokens=20_000, estimated_waste_usd=0.06,
        ),
        Finding(
            pattern_id="junk_reads", severity="medium", title="t2",
            description="d", affected_count=1, suggested_fix="f",
            estimated_waste_tokens=10_000, estimated_waste_usd=0.03,
        ),
        Finding(
            pattern_id="ghost_agents", severity="low", title="t3",
            description="d", affected_count=2, suggested_fix="f",
            estimated_waste_tokens=None, estimated_waste_usd=None,
        ),
    ]
    monkeypatch.setattr(
        "stackunderflow.routes.optimize.find_patterns", lambda conn, **kw: fake,
    )
    monkeypatch.setattr(
        "stackunderflow.routes.optimize.find_waste", lambda conn, **kw: [],
    )

    payload = await optimize_route.get_optimize_report(period="all", force=True)

    # 1. total_waste_usd sums the priced findings (None is treated as 0).
    assert payload["total_waste_usd"] == pytest.approx(0.09)

    # 2. Every pattern dict surfaces the new key.
    assert all("estimated_waste_usd" in p for p in payload["patterns"])

    # 3. Anomalies block present and well-formed; the $25 day is flagged.
    anomalies = payload["anomalies"]
    assert anomalies["method"] in {"mad", "stddev"}
    flagged_days = [a for a in anomalies["anomalies"] if a["kind"] == "day"]
    assert any(a["key"] == "2026-04-09" for a in flagged_days)
    top = next(a for a in flagged_days if a["key"] == "2026-04-09")
    assert top["cost_usd"] == pytest.approx(25.0)
    assert top["deviation_usd"] > 0
    assert top["reason"]
