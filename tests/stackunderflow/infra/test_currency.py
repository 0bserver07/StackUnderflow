"""Tests for stackunderflow.infra.currency.

Covers the Frankfurter cache lifecycle, defensive bounds, symbol fallback,
and graceful degradation when the network or cache is unavailable.

Every test isolates the cache file under a tmp dir and patches the network
layer — none of these tests touch ``api.frankfurter.app``.
"""
from __future__ import annotations

import json
from datetime import UTC, datetime, timedelta
from unittest.mock import patch

import pytest

from stackunderflow.infra import currency


@pytest.fixture
def isolated_cache(tmp_path, monkeypatch):
    """Redirect the on-disk cache into ``tmp_path`` and patch ``Path.home``.

    Yields the resolved cache file path so tests can write fixtures into
    it directly.
    """
    monkeypatch.setattr(currency.Path, "home", classmethod(lambda cls: tmp_path))
    cache_file = tmp_path / ".stackunderflow" / "cache" / "exchange-rate.json"
    yield cache_file


def _write_cache(path, fetched_at: datetime, rates: dict[str, float]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps({
        "fetched_at": fetched_at.isoformat(),
        "rates": rates,
    }))


# ── basic API ────────────────────────────────────────────────────────────────

def test_usd_is_identity(isolated_cache):
    assert currency.get_rate("USD") == 1.0
    assert currency.get_rate("usd") == 1.0
    assert currency.convert_usd(10.0, "USD") == 10.0


def test_invalid_iso_code_falls_back_to_usd(isolated_cache):
    assert currency.get_rate("EU") == 1.0       # too short
    assert currency.get_rate("EURO") == 1.0     # too long
    assert currency.get_rate("123") == 1.0      # not letters
    assert currency.get_rate("") == 1.0


# ── symbol resolution ────────────────────────────────────────────────────────

def test_known_symbols():
    assert currency.get_symbol("USD") == "$"
    assert currency.get_symbol("EUR") == "€"
    assert currency.get_symbol("GBP") == "£"
    assert currency.get_symbol("JPY") == "¥"


def test_lowercase_symbol_lookup_normalises():
    assert currency.get_symbol("eur") == "€"


def test_unknown_currency_falls_back_to_iso_code():
    assert currency.get_symbol("XOF") == "XOF"  # West African CFA — no symbol in our map
    assert currency.get_symbol("ABC") == "ABC"


# ── cache hit ────────────────────────────────────────────────────────────────

def test_cache_hit_returns_cached_rate(isolated_cache):
    _write_cache(isolated_cache, datetime.now(UTC), {"GBP": 0.79})

    with patch.object(currency, "_fetch_from_frankfurter") as fetch:
        rate = currency.get_rate("GBP")

    assert rate == 0.79
    fetch.assert_not_called()


def test_stale_cache_triggers_refetch(isolated_cache):
    """Cache older than 24h must force a refetch — the previous value is
    stale by definition once the TTL expires."""
    _write_cache(isolated_cache, datetime.now(UTC) - timedelta(hours=25), {"EUR": 0.85})

    with patch.object(currency, "_fetch_from_frankfurter", return_value={"EUR": 0.93}) as fetch:
        rate = currency.get_rate("EUR")

    assert rate == 0.93
    fetch.assert_called_once()


def test_missing_cache_triggers_fetch_then_writes_cache(isolated_cache):
    assert not isolated_cache.exists()

    with patch.object(currency, "_fetch_from_frankfurter", return_value={"EUR": 0.93}):
        rate = currency.get_rate("EUR")

    assert rate == 0.93
    assert isolated_cache.exists()
    cached = json.loads(isolated_cache.read_text())
    assert cached["rates"]["EUR"] == 0.93
    assert "fetched_at" in cached


# ── offline fallback ─────────────────────────────────────────────────────────

def test_offline_with_no_cache_returns_one(isolated_cache):
    """No cache + network failure → 1.0 (the safest possible fallback)."""
    with patch.object(currency, "_fetch_from_frankfurter", return_value=None):
        rate = currency.get_rate("EUR")

    assert rate == 1.0


def test_offline_with_stale_cache_serves_stale_rate(isolated_cache):
    """If we can't refresh, falling back to a stale rate is still better
    than 1.0 — the user explicitly opted into a non-USD display."""
    _write_cache(isolated_cache, datetime.now(UTC) - timedelta(days=10), {"EUR": 0.90})

    with patch.object(currency, "_fetch_from_frankfurter", return_value=None):
        rate = currency.get_rate("EUR")

    assert rate == 0.90


def test_unknown_target_currency_returns_one(isolated_cache):
    """Frankfurter doesn't publish every ISO code (e.g. KPW). Asking for
    one that comes back missing must degrade to USD silently."""
    with patch.object(currency, "_fetch_from_frankfurter", return_value={"EUR": 0.93}):
        rate = currency.get_rate("KPW")

    assert rate == 1.0


# ── bounds rejection ─────────────────────────────────────────────────────────

def test_out_of_band_rate_in_response_is_rejected(isolated_cache):
    """A response with rate=0 (or absurdly large) must NOT be cached or
    served — we'd silently zero out every cost figure."""
    with patch.object(currency, "_fetch_from_frankfurter") as fetch:
        # Simulate a parser that already filtered the bad value out
        fetch.return_value = {"EUR": 0.93}  # GBP intentionally absent
        rate = currency.get_rate("GBP")

    assert rate == 1.0


def test_is_valid_rate_rejects_out_of_band():
    assert not currency._is_valid_rate(0)
    assert not currency._is_valid_rate(0.00001)         # below MIN
    assert not currency._is_valid_rate(2_000_000)        # above MAX
    assert not currency._is_valid_rate(float("nan"))
    assert not currency._is_valid_rate(float("inf"))
    assert not currency._is_valid_rate("0.93")           # wrong type
    assert currency._is_valid_rate(0.93)
    assert currency._is_valid_rate(150.0)


def test_corrupt_cache_does_not_crash(isolated_cache):
    """A tampered/invalid cache file must be ignored, not raise."""
    isolated_cache.parent.mkdir(parents=True, exist_ok=True)
    isolated_cache.write_text("{not json")

    with patch.object(currency, "_fetch_from_frankfurter", return_value={"EUR": 0.93}):
        rate = currency.get_rate("EUR")

    assert rate == 0.93


def test_cache_with_invalid_rate_is_skipped(isolated_cache):
    """Cache containing nonsense rates must be ignored, falling through
    to a refetch."""
    _write_cache(isolated_cache, datetime.now(UTC), {"EUR": -1.0})

    with patch.object(currency, "_fetch_from_frankfurter", return_value={"EUR": 0.93}):
        rate = currency.get_rate("EUR")

    assert rate == 0.93


# ── format_in_currency / active_currency_payload ─────────────────────────────

def test_format_in_currency_default_usd(isolated_cache, monkeypatch):
    monkeypatch.setenv("STACKUNDERFLOW_CURRENCY", "USD")
    out = currency.format_in_currency(12.50)
    assert out["code"] == "USD"
    assert out["symbol"] == "$"
    assert out["rate_from_usd"] == 1.0
    assert out["amount"] == 12.50


def test_format_in_currency_explicit_target(isolated_cache):
    _write_cache(isolated_cache, datetime.now(UTC), {"EUR": 0.93})

    out = currency.format_in_currency(10.0, "EUR")
    assert out["code"] == "EUR"
    assert out["symbol"] == "€"
    assert out["rate_from_usd"] == 0.93
    assert out["amount"] == pytest.approx(9.30)


def test_active_currency_payload_omits_amount(isolated_cache, monkeypatch):
    monkeypatch.setenv("STACKUNDERFLOW_CURRENCY", "USD")
    payload = currency.active_currency_payload()
    assert set(payload.keys()) == {"code", "symbol", "rate_from_usd"}
    assert payload["code"] == "USD"


# ── list_supported ───────────────────────────────────────────────────────────

def test_list_supported_returns_usd_only_when_no_cache(isolated_cache):
    assert currency.list_supported() == ["USD"]


def test_list_supported_includes_cached_codes(isolated_cache):
    _write_cache(isolated_cache, datetime.now(UTC), {"EUR": 0.93, "GBP": 0.79, "JPY": 150.0})

    supported = currency.list_supported()
    assert "USD" in supported
    assert "EUR" in supported
    assert "GBP" in supported
    assert "JPY" in supported
