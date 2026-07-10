"""OpenAI pricer consults the data manifest first — gpt-5.5 regression.

gpt-5.5 is only distinguishable from gpt-5 by EXACT id (token sets collapse
duplicates: "gpt-5.5" → {gpt,5}), so its identity + rates live in
``data/models.toml`` (``ids`` + price rows) and must never regress into the
in-code ``_RATES`` ladder. Rates pinned to the official page fetched
2026-07-09: $5.00/M input, $0.50/M cached input, $30.00/M output, no
cache-write billing.
"""

from stackunderflow.infra.model_manifest import canonicalize as m_canon
from stackunderflow.infra.providers.openai import OpenAIPricer


def test_gpt55_identity_comes_from_manifest_exact_id():
    assert m_canon("gpt-5.5", provider="openai") == "GPT_55"
    assert OpenAIPricer().canonicalize("gpt-5.5") == "GPT_55"


def test_gpt55_does_not_collide_with_gpt5_family():
    p = OpenAIPricer()
    assert p.canonicalize("gpt-5") == "GPT_5"
    assert p.canonicalize("gpt-5-mini") == "GPT_5_MINI"
    assert p.canonicalize("gpt-5.4") == "GPT_54"
    assert p.canonicalize("gpt-5-codex") == "GPT_5_CODEX"


def test_gpt55_rates_match_published_pricing():
    assert OpenAIPricer().rates_for("GPT_55") == (5.0, 30.0, 0.0, 0.50)


def test_gpt55_compute_end_to_end():
    p = OpenAIPricer()
    tokens = {"input": 1_000_000, "output": 100_000, "cache_read": 200_000, "cache_creation": 0}
    breakdown = p.compute(tokens, "gpt-5.5")
    # 1M in @$5 + 100K out @$30/M + 200K cached @$0.50/M = 5 + 3 + 0.10
    assert round(breakdown["total_cost"], 4) == 8.10


def test_unknown_family_still_falls_back_to_code_table():
    assert OpenAIPricer().rates_for("NOT_A_FAMILY") is not None


# ── gpt-5.4 effective-dated rates (the $20 → $15 output cut) ────────────────


def test_gpt54_identity_via_manifest_exact_id():
    assert m_canon("gpt-5.4", provider="openai") == "GPT_54"


def test_gpt54_prices_by_era():
    """Historical events keep the rate actually billed at their timestamp;
    boundary 2026-04-26 (evidence trail in models.toml)."""
    p = OpenAIPricer()
    tokens = {"input": 0, "output": 1_000_000, "cache_read": 0, "cache_creation": 0}
    assert p.compute(tokens, "gpt-5.4", at_ts="2026-04-10T00:00:00Z")["total_cost"] == 20.0
    assert p.compute(tokens, "gpt-5.4", at_ts="2026-05-15T00:00:00Z")["total_cost"] == 15.0
    # Undated = current rate (feeds RATE_CARD / display surfaces).
    assert p.compute(tokens, "gpt-5.4")["total_cost"] == 15.0


def test_manifest_family_shadows_in_code_enum_rate():
    """GPT_54 exists in BOTH the manifest and the in-code enum — the
    manifest (data) must win, else rate corrections would require code."""
    assert OpenAIPricer().rates_for("GPT_54") == (2.50, 15.0, 0.0, 0.25)
