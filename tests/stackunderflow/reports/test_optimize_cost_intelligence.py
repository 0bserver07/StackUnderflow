"""Cost-intelligence tests for the optimize surface.

Two layers, both deterministic:

1. **$-denomination** — every token-bearing ``Finding`` now carries an
   ``estimated_waste_usd`` priced through ``compute_cost`` (black box). We
   assert the dollar figure is present, positive, and consistent with the
   token estimate at the representative model's rate.

2. **Cost anomaly detector** (``reports/anomaly.py``) — deterministic
   ``daily_mart`` / ``session_mart`` fixtures drive the MAD path, the stddev
   fallback, the "too short to baseline" skip, and the flat-series no-op.
"""

from __future__ import annotations

import tempfile
import unittest
from pathlib import Path

from stackunderflow.infra.costs import compute_cost
from stackunderflow.reports import anomaly
from stackunderflow.reports.optimize import (
    WASTE_PRICING_MODEL,
    _cache_overhead_finding,
    _junk_reads_finding,
    _low_read_edit_finding,
    _tokens_to_usd,
)
from stackunderflow.reports.scope import Scope
from stackunderflow.store import db, schema


# ── helpers ──────────────────────────────────────────────────────────────────


def _open_store() -> tuple[tempfile.TemporaryDirectory, object]:
    tmp = tempfile.TemporaryDirectory()
    conn = db.connect(Path(tmp.name) / "store.db")
    schema.apply(conn)
    conn.execute(
        "INSERT INTO projects (provider, slug, display_name, first_seen, last_modified) "
        "VALUES ('claude', 'p', 'p', 0, 0)"
    )
    conn.commit()
    return tmp, conn


def _seed_daily(conn, day: str, cost: float, *, project_id: int = 1) -> None:
    conn.execute(
        "INSERT INTO daily_mart "
        "(day, project_id, provider, model, speed, input_tokens, output_tokens, "
        " cache_read, cache_create, message_count, session_count, cost_usd) "
        "VALUES (?, ?, 'anthropic', 'claude-sonnet-4-6', 'standard', "
        " 0, 0, 0, 0, 1, 1, ?)",
        (day, project_id, cost),
    )


def _seed_session(conn, sid: str, cost: float, *, first_ts: str = "2026-04-10T10:00:00+00:00") -> None:
    conn.execute(
        "INSERT INTO session_mart "
        "(session_id, project_id, provider, primary_model, first_ts, last_ts, "
        " message_count, user_message_count, assistant_message_count, "
        " input_tokens, output_tokens, cache_read, cache_create, cost_usd, is_one_shot) "
        "VALUES (?, 1, 'anthropic', 'claude-sonnet-4-6', ?, ?, "
        " 4, 2, 2, 0, 0, 0, 0, ?, 0)",
        (sid, first_ts, first_ts, cost),
    )


# ── 1. $-denomination of findings ────────────────────────────────────────────


class TestWasteDollarDenomination(unittest.TestCase):
    def test_tokens_to_usd_matches_compute_cost(self):
        # 200k input tokens priced at the representative model must equal a
        # direct compute_cost() call on the same shape (black-box parity).
        expected = compute_cost(
            {"input": 200_000, "output": 0, "cache_creation": 0, "cache_read": 0},
            WASTE_PRICING_MODEL,
        )["total_cost"]
        self.assertAlmostEqual(_tokens_to_usd(200_000), round(expected, 4), places=4)

    def test_tokens_to_usd_cache_creation_prices_higher_than_input(self):
        # Cache-write tokens bill above plain input on Anthropic, so the
        # cache_creation slot must produce a strictly larger dollar figure.
        same_n = 100_000
        self.assertGreater(
            _tokens_to_usd(same_n, kind="cache_creation"),
            _tokens_to_usd(same_n, kind="input"),
        )

    def test_tokens_to_usd_none_and_zero(self):
        self.assertIsNone(_tokens_to_usd(None))
        self.assertIsNone(_tokens_to_usd(0))
        self.assertIsNone(_tokens_to_usd(-5))

    def test_low_read_edit_finding_carries_usd(self):
        bad = [{"session_fk": 1, "reads": 30}, {"session_fk": 2, "reads": 25}]
        finding = _low_read_edit_finding(bad)[0]
        self.assertIsNotNone(finding.estimated_waste_usd)
        self.assertGreater(finding.estimated_waste_usd, 0.0)
        # est_waste = Σ reads × 2000 tokens, priced as input.
        expected = _tokens_to_usd((30 + 25) * 2_000)
        self.assertAlmostEqual(finding.estimated_waste_usd, expected, places=4)

    def test_junk_reads_finding_carries_usd(self):
        hits = [{"session_fk": 1, "files": [{"path": "/a", "reads": 6}]}]
        finding = _junk_reads_finding(hits)[0]
        # redundant = reads - 1 = 5; est_waste = 5 × 2000 input tokens.
        expected = _tokens_to_usd(5 * 2_000)
        self.assertAlmostEqual(finding.estimated_waste_usd, expected, places=4)

    def test_cache_overhead_finding_prices_as_cache_creation(self):
        bad = [
            {"session_fk": 1, "cache_create_tokens": 400_000, "input_tokens": 10, "ratio": 0.97},
        ]
        finding = _cache_overhead_finding(bad)[0]
        # est_waste = Σ cache_create // 2 = 200_000, priced on the cache slot.
        expected = _tokens_to_usd(200_000, kind="cache_creation")
        self.assertAlmostEqual(finding.estimated_waste_usd, expected, places=4)
        # And it must differ from pricing those same tokens as plain input.
        self.assertNotAlmostEqual(
            finding.estimated_waste_usd, _tokens_to_usd(200_000), places=4
        )

    def test_finding_to_dict_includes_usd_key(self):
        finding = _low_read_edit_finding([{"session_fk": 1, "reads": 30}])[0]
        d = finding.to_dict()
        self.assertIn("estimated_waste_usd", d)
        self.assertIn("estimated_waste_tokens", d)


# ── 2. cost anomaly detector ─────────────────────────────────────────────────


class TestCostAnomalyDetector(unittest.TestCase):
    def setUp(self):
        self.tmp, self.conn = _open_store()

    def tearDown(self):
        self.conn.close()
        self.tmp.cleanup()

    def test_clean_baseline_with_one_spike_flagged_via_mad(self):
        # Five quiet ~$1 days + one $20 spike → MAD path flags exactly the spike.
        for i, cost in enumerate([1.0, 1.1, 0.9, 1.05, 0.95]):
            _seed_daily(self.conn, f"2026-04-0{i + 1}", cost)
        _seed_daily(self.conn, "2026-04-09", 20.0)
        self.conn.commit()

        result = anomaly.find_cost_anomalies(self.conn, include_sessions=False)
        self.assertEqual(result["method"], "mad")
        flagged = [a for a in result["anomalies"] if a["kind"] == "day"]
        self.assertEqual(len(flagged), 1)
        spike = flagged[0]
        self.assertEqual(spike["key"], "2026-04-09")
        self.assertAlmostEqual(spike["cost_usd"], 20.0, places=2)
        self.assertGreater(spike["deviation_usd"], 0.0)
        self.assertGreater(spike["score"], anomaly.MAD_K)
        self.assertIn("median", spike["reason"])
        self.assertGreater(spike["ratio"], 5.0)

    def test_uniform_series_flags_nothing(self):
        # All days identical → MAD 0 and stddev 0 → no anomaly.
        for i in range(6):
            _seed_daily(self.conn, f"2026-04-0{i + 1}", 2.0)
        self.conn.commit()
        result = anomaly.find_cost_anomalies(self.conn, include_sessions=False)
        self.assertEqual(result["anomalies"], [])
        self.assertEqual(result["method"], "none")

    def test_series_too_short_skips(self):
        # Below MIN_POINTS days → no baseline, no flag even with a huge value.
        _seed_daily(self.conn, "2026-04-01", 1.0)
        _seed_daily(self.conn, "2026-04-02", 50.0)
        self.conn.commit()
        result = anomaly.find_cost_anomalies(self.conn, include_sessions=False)
        self.assertEqual(result["anomalies"], [])
        self.assertEqual(result["day_count"], 2)

    def test_stddev_fallback_when_mad_zero_but_not_flat(self):
        # Most days identical (MAD == 0) but one differs → stddev fallback.
        for i in range(5):
            _seed_daily(self.conn, f"2026-04-0{i + 1}", 1.0)
        _seed_daily(self.conn, "2026-04-09", 8.0)
        self.conn.commit()
        result = anomaly.find_cost_anomalies(
            self.conn, include_sessions=False, k=2.0,
        )
        flagged = [a for a in result["anomalies"] if a["kind"] == "day"]
        self.assertEqual(len(flagged), 1)
        self.assertEqual(flagged[0]["method"], "stddev")
        self.assertEqual(flagged[0]["key"], "2026-04-09")
        self.assertIn("mean", flagged[0]["reason"])

    def test_session_outlier_flagged_with_details(self):
        # Slightly-varied baseline so MAD > 0 (the realistic case); one big spike.
        for i, c in enumerate([0.5, 0.6, 0.4, 0.55, 0.45]):
            _seed_session(self.conn, f"s{i}", c)
        _seed_session(self.conn, "s-big", 15.0)
        self.conn.commit()
        result = anomaly.find_cost_anomalies(self.conn)
        sess = [a for a in result["anomalies"] if a["kind"] == "session"]
        self.assertEqual(len(sess), 1)
        self.assertEqual(sess[0]["key"], "s-big")
        self.assertEqual(sess[0]["details"]["model"], "claude-sonnet-4-6")
        self.assertEqual(sess[0]["details"]["message_count"], 4)
        self.assertEqual(result["session_count"], 6)

    def test_sub_floor_spike_not_flagged(self):
        # A "spike" of fractions of a cent is statistical noise, not waste.
        for i in range(5):
            _seed_daily(self.conn, f"2026-04-0{i + 1}", 0.0001)
        _seed_daily(self.conn, "2026-04-09", 0.01)
        self.conn.commit()
        result = anomaly.find_cost_anomalies(self.conn, include_sessions=False)
        # 0.01 is below the absolute floor, so nothing surfaces.
        self.assertEqual([a for a in result["anomalies"] if a["kind"] == "day"], [])

    def test_scope_bounds_the_day_window(self):
        # Days outside the scope window must not enter the baseline or flags.
        for i, c in enumerate([1.0, 1.1, 0.9, 1.05, 0.95]):
            _seed_daily(self.conn, f"2026-04-0{i + 1}", c)
        _seed_daily(self.conn, "2026-04-09", 20.0)
        _seed_daily(self.conn, "2026-03-01", 999.0)  # well before the window
        self.conn.commit()
        scope = Scope(
            since="2026-04-01T00:00:00+00:00",
            until="2026-04-30T23:59:59+00:00",
            label="window",
        )
        result = anomaly.find_cost_anomalies(self.conn, scope=scope, include_sessions=False)
        keys = {a["key"] for a in result["anomalies"]}
        self.assertIn("2026-04-09", keys)
        self.assertNotIn("2026-03-01", keys)
        self.assertEqual(result["day_count"], 6)

    def test_empty_store_returns_empty(self):
        result = anomaly.find_cost_anomalies(self.conn)
        self.assertEqual(result["anomalies"], [])
        self.assertEqual(result["method"], "none")
        self.assertEqual(result["day_count"], 0)


if __name__ == "__main__":
    unittest.main()
