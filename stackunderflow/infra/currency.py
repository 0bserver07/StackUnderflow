"""Currency conversion via Frankfurter (ECB FX data) with a 24h disk cache.

Used at the API boundary so cost figures can be rendered in the user's
preferred currency. Costs are still computed in USD internally — model
rate cards are USD-denominated — and converted only when serialised.

Resolution at runtime:
  1. ``Settings().currency`` picks the active ISO code (default ``USD``).
  2. ``get_rate(target)`` consults a 24h JSON cache at
     ``~/.stackunderflow/cache/exchange-rate.json``; on miss it fetches
     from ``api.frankfurter.app`` and writes the cache.
  3. Any failure (network, parse, bounds rejection) falls back to a
     ``rate=1.0`` USD response and emits a WARNING — never raises.

The cache file shape is:
  ``{"fetched_at": "<ISO-UTC>", "rates": {"EUR": 0.93, "GBP": 0.79, ...}}``
"""

from __future__ import annotations

import json
import logging
import re
import urllib.error
import urllib.request
from datetime import UTC, datetime, timedelta
from pathlib import Path
from typing import Any

logger = logging.getLogger(__name__)


# ── constants ────────────────────────────────────────────────────────────────

_FRANKFURTER_URL = "https://api.frankfurter.app/latest?from=USD"
_CACHE_TTL = timedelta(hours=24)
_FETCH_TIMEOUT_S = 10

# Defensive bounds on any FX rate. Anything outside is either a parse bug or
# a tampered response — refuse to multiply it into displayed costs.
_MIN_VALID_RATE = 0.0001
_MAX_VALID_RATE = 1_000_000.0

_ISO_CODE_RE = re.compile(r"^[A-Z]{3}$")


# Top ~30 currency symbols. Anything else falls back to the ISO code.
_SYMBOLS: dict[str, str] = {
    "USD": "$",
    "EUR": "€",
    "GBP": "£",
    "JPY": "¥",
    "CHF": "CHF",
    "CAD": "$",
    "AUD": "$",
    "CNY": "¥",
    "INR": "₹",
    "KRW": "₩",
    "MXN": "$",
    "BRL": "R$",
    "SEK": "kr",
    "NOK": "kr",
    "DKK": "kr",
    "PLN": "zł",
    "RUB": "₽",
    "TRY": "₺",
    "ZAR": "R",
    "AED": "د.إ",
    "SAR": "﷼",
    "SGD": "S$",
    "HKD": "HK$",
    "NZD": "NZ$",
    "TWD": "NT$",
    "THB": "฿",
    "IDR": "Rp",
    "MYR": "RM",
    "PHP": "₱",
    "ILS": "₪",
    "CZK": "Kč",
    "HUF": "Ft",
    "RON": "lei",
    "BGN": "лв",
    "ARS": "$",
}


# ── cache I/O ────────────────────────────────────────────────────────────────

def _cache_path() -> Path:
    return Path.home() / ".stackunderflow" / "cache" / "exchange-rate.json"


def _read_cache() -> dict[str, Any] | None:
    path = _cache_path()
    if not path.exists():
        return None
    try:
        return json.loads(path.read_text())
    except (OSError, json.JSONDecodeError) as e:
        logger.info("currency: cache unreadable (%s) — refetching", e)
        return None


def _write_cache(rates: dict[str, float]) -> None:
    path = _cache_path()
    try:
        path.parent.mkdir(parents=True, exist_ok=True)
        payload = {
            "fetched_at": datetime.now(UTC).isoformat(),
            "rates": rates,
        }
        path.write_text(json.dumps(payload))
    except OSError as e:
        logger.info("currency: failed to write cache: %s", e)


def _is_fresh(fetched_at: str | None) -> bool:
    if not fetched_at:
        return False
    try:
        ts = datetime.fromisoformat(fetched_at.replace("Z", "+00:00"))
    except (ValueError, AttributeError):
        return False
    return (datetime.now(UTC) - ts) < _CACHE_TTL


# ── network ──────────────────────────────────────────────────────────────────

def _fetch_from_frankfurter() -> dict[str, float] | None:
    """Pull the full USD-base rate table from Frankfurter.

    Returns the validated rates dict on success, ``None`` on any failure
    (network, parse, schema). All callers gracefully degrade to USD.
    """
    try:
        with urllib.request.urlopen(_FRANKFURTER_URL, timeout=_FETCH_TIMEOUT_S) as resp:
            data = json.loads(resp.read().decode("utf-8"))
    except (urllib.error.URLError, urllib.error.HTTPError, json.JSONDecodeError, TimeoutError, OSError) as e:
        logger.warning("currency: failed to fetch rates from %s: %s", _FRANKFURTER_URL, e)
        return None

    raw = data.get("rates")
    if not isinstance(raw, dict):
        logger.warning("currency: malformed Frankfurter response (no 'rates' field)")
        return None

    rates: dict[str, float] = {}
    for code, val in raw.items():
        if not isinstance(code, str) or not _ISO_CODE_RE.match(code):
            continue
        if not _is_valid_rate(val):
            continue
        rates[code] = float(val)
    return rates or None


def _is_valid_rate(value: Any) -> bool:
    if not isinstance(value, (int, float)):
        return False
    f = float(value)
    if f != f or f in (float("inf"), float("-inf")):  # NaN/Inf guard
        return False
    return _MIN_VALID_RATE <= f <= _MAX_VALID_RATE


# ── public API ───────────────────────────────────────────────────────────────

def get_rate(target_currency: str) -> float:
    """Return the USD → ``target_currency`` rate.

    USD is identity (1.0). Anything else hits the 24h cache or refetches.
    Any failure path (offline, parse error, missing code) returns 1.0
    so callers always have a usable number.
    """
    code = (target_currency or "USD").upper()
    if code == "USD":
        return 1.0
    if not _ISO_CODE_RE.match(code):
        return 1.0

    cached = _read_cache()
    if cached and _is_fresh(cached.get("fetched_at")):
        rate = cached.get("rates", {}).get(code)
        if _is_valid_rate(rate):
            return float(rate)

    fresh = _fetch_from_frankfurter()
    if fresh is not None:
        _write_cache(fresh)
        rate = fresh.get(code)
        if _is_valid_rate(rate):
            return float(rate)

    # Last resort: fall back to a stale cache if any rate is still in band.
    if cached:
        rate = cached.get("rates", {}).get(code)
        if _is_valid_rate(rate):
            logger.warning(
                "currency: serving stale rate for %s (Frankfurter unreachable)",
                code,
            )
            return float(rate)

    logger.warning("currency: no rate for %s — falling back to USD", code)
    return 1.0


def get_symbol(currency: str) -> str:
    """Return the conventional symbol for ``currency`` or its ISO code."""
    code = (currency or "USD").upper()
    return _SYMBOLS.get(code, code)


def convert_usd(usd: float, target: str) -> float:
    """Convert a USD amount into ``target`` using the cached rate."""
    return float(usd) * get_rate(target)


def list_supported() -> list[str]:
    """Return the list of ISO codes Frankfurter publishes (plus USD).

    Reads the cache only — does not trigger a network fetch. If no cache
    is present yet, returns ``["USD"]`` so callers can still render
    something sensible.
    """
    out = ["USD"]
    cached = _read_cache()
    if cached and isinstance(cached.get("rates"), dict):
        out.extend(sorted(c for c in cached["rates"] if _ISO_CODE_RE.match(c)))
    return out


def format_in_currency(usd: float, target: str | None = None) -> dict[str, Any]:
    """Return the canonical ``{code, symbol, rate_from_usd, amount}`` payload.

    If ``target`` is omitted, the active ``Settings().currency`` is used.
    Always succeeds — falls back to USD on any error.
    """
    if target is None:
        # Imported lazily to avoid a circular import via settings → infra.
        from stackunderflow.settings import Settings
        target = Settings().currency or "USD"

    code = (target or "USD").upper()
    if not _ISO_CODE_RE.match(code):
        code = "USD"
    rate = get_rate(code)
    return {
        "code": code,
        "symbol": get_symbol(code),
        "rate_from_usd": rate,
        "amount": float(usd) * rate,
    }


def active_currency_payload() -> dict[str, Any]:
    """The ``currency`` block stamped onto every API response.

    Shape: ``{"code", "symbol", "rate_from_usd"}``. Independent of any
    specific dollar amount — meant to live at the top level of the JSON
    response so the UI can pull it once per fetch.
    """
    payload = format_in_currency(0.0)
    payload.pop("amount", None)
    return payload


__all__ = [
    "active_currency_payload",
    "convert_usd",
    "format_in_currency",
    "get_rate",
    "get_symbol",
    "list_supported",
]
