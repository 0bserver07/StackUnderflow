"""Tests for stackunderflow.infra.currency.

Covers the Frankfurter cache lifecycle, defensive bounds, symbol fallback,
the embedded snapshot fallback, and the warning surfaced in the currency
payload when a fallback is used.

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
    """Garbage ISO codes resolve to 1.0 via the swallowing path in ``get_rate``."""
    assert currency.get_rate("EU") == 1.0       # too short
    assert currency.get_rate("EURO") == 1.0     # too long
    assert currency.get_rate("123") == 1.0      # not letters
    assert currency.get_rate("") == 1.0


def test_resolve_rate_raises_for_invalid_iso_code(isolated_cache):
    """``resolve_rate`` is the strict variant — it raises so callers can warn."""
    with pytest.raises(currency.CurrencyError):
        currency.resolve_rate("EU")
    with pytest.raises(currency.CurrencyError):
        currency.resolve_rate("EURO")


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
        rate, warning = currency.resolve_rate("GBP")

    assert rate == 0.79
    assert warning is None
    fetch.assert_not_called()


def test_stale_cache_triggers_refetch(isolated_cache):
    """Cache older than 24h must force a refetch when the fetch succeeds."""
    _write_cache(isolated_cache, datetime.now(UTC) - timedelta(hours=25), {"EUR": 0.85})

    with patch.object(currency, "_fetch_from_frankfurter", return_value={"EUR": 0.93}) as fetch:
        rate, warning = currency.resolve_rate("EUR")

    assert rate == 0.93
    assert warning is None
    fetch.assert_called_once()


def test_missing_cache_triggers_fetch_then_writes_cache(isolated_cache):
    assert not isolated_cache.exists()

    with patch.object(currency, "_fetch_from_frankfurter", return_value={"EUR": 0.93}):
        rate, warning = currency.resolve_rate("EUR")

    assert rate == 0.93
    assert warning is None
    assert isolated_cache.exists()
    cached = json.loads(isolated_cache.read_text())
    assert cached["rates"]["EUR"] == 0.93
    assert "fetched_at" in cached


# ── snapshot fallback (the new behaviour) ────────────────────────────────────


def test_offline_with_no_cache_falls_back_to_snapshot(isolated_cache):
    """The snapshot is the last line of defence — a 403/offline situation
    with no usable cache must still produce a non-1.0 rate for any code in
    ``RATES_SNAPSHOT``, accompanied by a warning."""
    with patch.object(currency, "_fetch_from_frankfurter", return_value=None):
        rate, warning = currency.resolve_rate("GBP")

    assert rate == currency.RATES_SNAPSHOT["GBP"]
    assert rate != 1.0
    assert warning is not None
    assert "snapshot" in warning.lower()
    assert currency.RATES_SNAPSHOT_DATE in warning


def test_offline_with_no_cache_unknown_code_raises(isolated_cache):
    """Unknown ISO codes (not in cache, not in fetch, not in snapshot) raise."""
    with patch.object(currency, "_fetch_from_frankfurter", return_value=None):
        with pytest.raises(currency.CurrencyError):
            currency.resolve_rate("KPW")


def test_stale_cache_within_30d_serves_warning(isolated_cache):
    """Cache between 24h and 30d old + offline → use cache + stale warning."""
    _write_cache(isolated_cache, datetime.now(UTC) - timedelta(days=10), {"EUR": 0.90})

    with patch.object(currency, "_fetch_from_frankfurter", return_value=None):
        rate, warning = currency.resolve_rate("EUR")

    assert rate == 0.90
    assert warning is not None
    assert "old" in warning.lower() or "stale" in warning.lower()


def test_cache_older_than_30d_falls_through_to_snapshot(isolated_cache):
    """A cache older than 30 days is too stale to trust — fall through to snapshot."""
    _write_cache(isolated_cache, datetime.now(UTC) - timedelta(days=40), {"GBP": 0.50})

    with patch.object(currency, "_fetch_from_frankfurter", return_value=None):
        rate, warning = currency.resolve_rate("GBP")

    # Snapshot value, NOT the absurdly-old cache
    assert rate == currency.RATES_SNAPSHOT["GBP"]
    assert warning is not None
    assert "snapshot" in warning.lower()


def test_get_rate_swallows_currency_error_to_one(isolated_cache):
    """``get_rate`` is the legacy compat shim — never raises, returns 1.0."""
    with patch.object(currency, "_fetch_from_frankfurter", return_value=None):
        rate = currency.get_rate("KPW")
    assert rate == 1.0


def test_unknown_target_currency_uses_snapshot_when_present(isolated_cache):
    """A code Frankfurter doesn't publish but the snapshot does (e.g. ARS)
    must still resolve via the snapshot path."""
    with patch.object(currency, "_fetch_from_frankfurter", return_value={"EUR": 0.93}):
        rate, warning = currency.resolve_rate("ARS")

    assert rate == currency.RATES_SNAPSHOT["ARS"]
    assert warning is not None
    assert "snapshot" in warning.lower()


# ── bounds rejection ─────────────────────────────────────────────────────────

def test_out_of_band_rate_in_response_falls_back_to_snapshot(isolated_cache):
    """A response with a missing/out-of-band rate for the target must NOT
    silently emit 1.0 — it must fall through to the snapshot."""
    with patch.object(currency, "_fetch_from_frankfurter") as fetch:
        # Simulate a parser that already filtered the bad value out
        fetch.return_value = {"EUR": 0.93}  # GBP intentionally absent
        rate, warning = currency.resolve_rate("GBP")

    assert rate == currency.RATES_SNAPSHOT["GBP"]
    assert rate != 1.0
    assert warning is not None


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
        rate, warning = currency.resolve_rate("EUR")

    assert rate == 0.93
    assert warning is None


def test_cache_with_invalid_rate_is_skipped(isolated_cache):
    """Cache containing nonsense rates must be ignored, falling through
    to a refetch."""
    _write_cache(isolated_cache, datetime.now(UTC), {"EUR": -1.0})

    with patch.object(currency, "_fetch_from_frankfurter", return_value={"EUR": 0.93}):
        rate, warning = currency.resolve_rate("EUR")

    assert rate == 0.93
    assert warning is None


# ── format_in_currency / active_currency_payload ─────────────────────────────

def test_format_in_currency_default_usd(isolated_cache, monkeypatch):
    monkeypatch.setenv("STACKUNDERFLOW_CURRENCY", "USD")
    out = currency.format_in_currency(12.50)
    assert out["code"] == "USD"
    assert out["symbol"] == "$"
    assert out["rate_from_usd"] == 1.0
    assert out["amount"] == 12.50
    assert out["warning"] is None


def test_format_in_currency_explicit_target(isolated_cache):
    _write_cache(isolated_cache, datetime.now(UTC), {"EUR": 0.93})

    out = currency.format_in_currency(10.0, "EUR")
    assert out["code"] == "EUR"
    assert out["symbol"] == "€"
    assert out["rate_from_usd"] == 0.93
    assert out["amount"] == pytest.approx(9.30)
    assert out["warning"] is None


def test_format_in_currency_snapshot_warning(isolated_cache, monkeypatch):
    """Frankfurter mocked to fail + cold cache → snapshot path with warning."""
    monkeypatch.setenv("STACKUNDERFLOW_CURRENCY", "GBP")

    with patch.object(currency, "_fetch_from_frankfurter", return_value=None):
        out = currency.format_in_currency(10.0)

    assert out["code"] == "GBP"
    assert out["symbol"] == "£"
    assert out["rate_from_usd"] == currency.RATES_SNAPSHOT["GBP"]
    assert out["rate_from_usd"] != 1.0
    assert out["warning"] is not None
    assert "snapshot" in out["warning"].lower()


def test_format_in_currency_unresolvable_code_falls_back_to_usd(isolated_cache, monkeypatch):
    """The strict guarantee: never silently emit ``rate_from_usd=1.0`` for a
    non-USD code. If we have to fall through, we switch ``code`` to USD and
    populate a warning so the UI can banner it."""
    monkeypatch.setenv("STACKUNDERFLOW_CURRENCY", "KPW")

    with patch.object(currency, "_fetch_from_frankfurter", return_value=None):
        out = currency.format_in_currency(10.0)

    assert out["code"] == "USD"
    assert out["symbol"] == "$"
    assert out["rate_from_usd"] == 1.0
    assert out["amount"] == 10.0
    assert out["warning"] is not None
    assert "KPW" in out["warning"]


def test_active_currency_payload_includes_warning_field(isolated_cache, monkeypatch):
    monkeypatch.setenv("STACKUNDERFLOW_CURRENCY", "USD")
    payload = currency.active_currency_payload()
    assert set(payload.keys()) == {"code", "symbol", "rate_from_usd", "warning"}
    assert payload["code"] == "USD"
    assert payload["warning"] is None


def test_active_currency_payload_warning_on_403(isolated_cache, monkeypatch):
    """End-to-end: GBP active + Frankfurter unreachable + no cache → snapshot
    rate, GBP code (not USD!), warning populated."""
    monkeypatch.setenv("STACKUNDERFLOW_CURRENCY", "GBP")

    with patch.object(currency, "_fetch_from_frankfurter", return_value=None):
        payload = currency.active_currency_payload()

    assert payload["code"] == "GBP"
    assert payload["symbol"] == "£"
    assert payload["rate_from_usd"] == currency.RATES_SNAPSHOT["GBP"]
    assert payload["rate_from_usd"] != 1.0
    assert payload["warning"] is not None


# ── snapshot table sanity checks ─────────────────────────────────────────────


def test_snapshot_covers_top_30_currencies():
    """The snapshot must cover at least the top-30 codes the symbol map
    advertises so the dropdown never points at a missing entry."""
    required = {
        "USD", "EUR", "GBP", "JPY", "CHF", "CAD", "AUD", "CNY", "INR", "BRL",
        "MXN", "ZAR", "NOK", "SEK", "DKK", "NZD", "KRW", "SGD", "HKD", "TWD",
        "THB", "MYR", "IDR", "PHP", "ILS", "AED", "SAR", "TRY", "RUB", "PLN",
    }
    missing = required - set(currency.RATES_SNAPSHOT)
    assert not missing, f"snapshot missing required codes: {missing}"


def test_snapshot_rates_are_in_band():
    """Every snapshot rate must pass the same defensive bounds the parser
    enforces — otherwise it'd be skipped at use time."""
    for code, rate in currency.RATES_SNAPSHOT.items():
        assert currency._is_valid_rate(rate), f"{code}={rate} out of band"


def test_snapshot_date_format():
    """ISO-8601 calendar date so ``datetime.fromisoformat`` can parse it
    if a downstream consumer ever needs to reason about staleness."""
    parsed = datetime.fromisoformat(currency.RATES_SNAPSHOT_DATE)
    assert parsed.year >= 2025


# ── list_supported ───────────────────────────────────────────────────────────


def test_list_supported_includes_snapshot_codes_when_no_cache(isolated_cache):
    """The dropdown must be non-empty even on a brand-new install — every
    snapshot code is offered as a candidate so the user can pick one."""
    supported = currency.list_supported()
    assert supported[0] == "USD"
    assert "GBP" in supported
    assert "EUR" in supported
    # USD appears exactly once
    assert supported.count("USD") == 1


def test_list_supported_includes_cached_codes(isolated_cache):
    _write_cache(isolated_cache, datetime.now(UTC), {"EUR": 0.93, "GBP": 0.79, "JPY": 150.0})

    supported = currency.list_supported()
    assert "USD" in supported
    assert "EUR" in supported
    assert "GBP" in supported
    assert "JPY" in supported
