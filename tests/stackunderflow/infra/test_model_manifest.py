"""Effective-dated pricing in the model manifest.

Proves the documented behaviour: a model with dated price rows prices an
event at the rate in effect at its timestamp, and `compute_cost` threads
`at_ts` through to the manifest. Uses a synthetic model (monkeypatched in)
so the test doesn't depend on real rates that change over time.
"""

from __future__ import annotations

from stackunderflow.infra import model_manifest
from stackunderflow.infra.costs import compute_cost

_DATED = [
    {"effective_until": "2026-01-15", "input": 15.0, "output": 75.0,
     "cache_write": 18.75, "cache_read": 1.5},
    {"effective_from": "2026-01-15", "input": 5.0, "output": 25.0,
     "cache_write": 6.25, "cache_read": 0.5},
]


def test_select_price_current_prefers_open_ended_row():
    # No at_ts → the open-ended (current) row, not the expired one.
    assert model_manifest._select_price(_DATED, None)["input"] == 5.0


def test_select_price_picks_window_for_timestamp():
    assert model_manifest._select_price(_DATED, "2026-01-01")["input"] == 15.0  # pre-drop
    assert model_manifest._select_price(_DATED, "2026-02-01")["input"] == 5.0   # post-drop


def _synthetic_model():
    return [{
        "family": "TESTM", "provider": "anthropic", "match": ["testm"],
        "fallback": True, "price": _DATED,
    }]


def test_rates_for_honors_at_ts(monkeypatch):
    monkeypatch.setattr(model_manifest, "_models", _synthetic_model)
    assert model_manifest.rates_for("TESTM", "anthropic", at_ts="2026-01-01")[0] == 15.0
    assert model_manifest.rates_for("TESTM", "anthropic", at_ts="2026-02-01")[0] == 5.0
    assert model_manifest.rates_for("TESTM", "anthropic")[0] == 5.0  # current


def test_compute_cost_threads_at_ts_to_manifest(monkeypatch):
    """End-to-end: compute_cost prices a non-overlay model by timestamp."""
    monkeypatch.setattr(model_manifest, "_models", _synthetic_model)
    # Force the manifest path (no overlay) so at_ts is consulted.
    monkeypatch.setattr("stackunderflow.infra.costs._overlay_rates", lambda m: None)
    tok = {"input": 1_000_000, "output": 0, "cache_read": 0, "cache_creation": 0}
    assert compute_cost(tok, "testm-1", at_ts="2026-01-01")["total_cost"] == 15.0
    assert compute_cost(tok, "testm-1", at_ts="2026-02-01")["total_cost"] == 5.0


# ── loader hardening (audit fix #15) ─────────────────────────────────────────

_GOOD_PRICE = {"input": 1.0, "output": 2.0, "cache_write": 3.0, "cache_read": 0.1}


def test_valid_model_accepts_well_formed_entry():
    assert model_manifest._valid_model(
        {"family": "X", "match": ["x"], "price": [_GOOD_PRICE]}
    )


def test_valid_model_rejects_malformed_entries():
    bad = [
        "not-a-dict",
        {},                                              # no family
        {"family": "", "price": [_GOOD_PRICE]},          # empty family
        {"family": "X"},                                 # no price
        {"family": "X", "price": []},                    # empty price list
        {"family": "X", "price": [{"input": 1.0}]},      # price row missing fields
        {"family": "X", "price": [{**_GOOD_PRICE, "input": "nan"}]},  # non-numeric
    ]
    for entry in bad:
        assert not model_manifest._valid_model(entry), entry


def test_real_manifest_entries_are_all_valid():
    """The shipped models.toml must contain only well-formed entries."""
    models = model_manifest._models()
    assert models, "manifest should not be empty"
    assert all(model_manifest._valid_model(m) for m in models)


def test_models_drops_malformed_entry_with_warning(monkeypatch, caplog):
    """A malformed entry is dropped (never KeyErrors at lookup) and warned."""
    import logging

    good = {"family": "GOOD", "provider": "anthropic", "match": ["good"],
            "fallback": True, "price": [_GOOD_PRICE]}
    bad = {"family": "BAD", "provider": "anthropic", "match": ["bad"],
           "price": [{"input": 1.0}]}  # missing output/cache fields
    monkeypatch.setattr(model_manifest.tomllib, "load", lambda fh: {"model": [good, bad]})
    model_manifest._models.cache_clear()
    try:
        with caplog.at_level(logging.WARNING):
            models = model_manifest._models()
        assert {m["family"] for m in models} == {"GOOD"}  # BAD dropped
        assert any("malformed" in r.getMessage() for r in caplog.records)
        # Looking up the dropped family must NOT raise — resolves to fallback.
        assert model_manifest.rates_for("BAD", "anthropic") is not None
    finally:
        model_manifest._models.cache_clear()  # restore the real manifest
