"""Currency conversion via Frankfurter (ECB FX data) with a 24h disk cache.

Used at the API boundary so cost figures can be rendered in the user's
preferred currency. Costs are still computed in USD internally — model
rate cards are USD-denominated — and converted only when serialised.

Resolution chain at runtime (see ``get_rate``):

  1. ``Settings().currency`` picks the active ISO code (default ``USD``).
  2. ``get_rate(target)``:
       0. in-process memo (≤60s)        → use it, zero I/O (COST-7)
       a. cache fresh (≤24h)            → use it, no warning
       b. live fetch from Frankfurter   → use it, write cache, no warning
       c. cache stale (>24h, ≤30d)      → use it + warning "rate is N days old"
       d. ``RATES_SNAPSHOT[code]``      → use it + warning "using built-in
                                          snapshot from <RATES_SNAPSHOT_DATE>"
       e. unknown code                  → raise ``CurrencyError``

The cache file shape is:
  ``{"fetched_at": "<ISO-UTC>", "rates": {"EUR": 0.93, "GBP": 0.79, ...}}``

Step 0 and the negative fetch cache are the COST-7 fix: this module sits on
every cost-bearing API response, and uncached it charged each one a config.json
read, an FX-cache read, and — once the 24h cache lapsed with Frankfurter
unreachable — a fresh blocking 10s ``urlopen``, forever, because a failed fetch
persisted nothing. See ``clear_currency_memo`` for the reset hook.

The hardcoded ``RATES_SNAPSHOT`` is the last line of defence so non-USD
users never silently see USD numbers labelled with their currency symbol.
It is intentionally out-of-date by definition: refresh it whenever you
notice ``RATES_SNAPSHOT_DATE`` is more than ~6 months old. The ECB
cross-rates against USD typically drift 1-2% per month — that's well
within the precision users care about for cost dashboards.
"""

from __future__ import annotations

import json
import logging
import re
import threading
import time
import urllib.error
import urllib.request
from datetime import UTC, datetime, timedelta
from pathlib import Path
from typing import Any
from stackunderflow.settings import app_dir

logger = logging.getLogger(__name__)


# ── constants ────────────────────────────────────────────────────────────────

_FRANKFURTER_URL = "https://api.frankfurter.app/latest?from=USD"
_CACHE_TTL = timedelta(hours=24)
# How long a cache is allowed to live as a "stale-serve" fallback before we
# give up and fall through to the embedded snapshot. 30 days is generous —
# a user offline for a month still gets approximately-right numbers, with a
# loud warning telling them why.
_STALE_CACHE_MAX = timedelta(days=30)
_FETCH_TIMEOUT_S = 10

# Defensive bounds on any FX rate. Anything outside is either a parse bug or
# a tampered response — refuse to multiply it into displayed costs.
_MIN_VALID_RATE = 0.0001
_MAX_VALID_RATE = 1_000_000.0

_ISO_CODE_RE = re.compile(r"^[A-Z]{3}$")

# ── in-process memo (COST-7) ─────────────────────────────────────────────────
#
# Currency resolution sits on EVERY cost-bearing API response. Uncached it cost,
# per request: a ``Settings()`` construction that re-reads ``config.json`` from
# disk (even on the USD short-circuit), a ``_read_cache()`` JSON read of the FX
# cache file for non-USD, and — once the 24h cache lapsed — a blocking
# ``urlopen`` with a 10s timeout. Nothing was written on a FAILED fetch, so an
# unreachable Frankfurter meant every single request paid the full 10s again.
#
# Three memos fix that, all ~60s TTL on a monotonic clock:
#   * ``_RATE_MEMO``    — code → (expires_at, rate, warning). The resolved
#                         "asof" triple; a hit does zero disk I/O.
#   * ``_PAYLOAD_MEMO`` — the whole ``active_currency_payload`` dict, so the
#                         USD path skips the ``Settings()`` read too.
#   * ``_FETCH_BLOCKED_UNTIL`` — negative cache. A failed fetch parks the next
#                         attempt for ``_FETCH_NEGATIVE_TTL_S``; the resolution
#                         chain below it (stale cache → snapshot) is unchanged,
#                         so behaviour is identical minus the repeated stall.
#
# The TTL is short on purpose: FX rates are refreshed daily, so 60s of staleness
# is invisible, while any write that changes the ACTIVE code (POST
# /api/cfg/currency) calls ``clear_currency_memo()`` rather than waiting it out.
_MEMO_TTL_S = 60.0
_FETCH_NEGATIVE_TTL_S = 900.0  # 15 min

_RATE_MEMO: dict[str, tuple[float, float, str | None]] = {}
_PAYLOAD_MEMO: tuple[float, dict[str, Any]] | None = None
_FETCH_BLOCKED_UNTIL: float = 0.0
_MEMO_LOCK = threading.Lock()


def clear_currency_memo() -> None:
    """Drop every in-process currency memo (rates, payload, negative cache).

    Per the repo's resettable-cache policy (cf. ``infra.costs.clear_pricing_caches``,
    ``infra.model_manifest.clear_manifest_caches``). Called by the currency-write
    route so a settings change is visible immediately, and by tests that patch the
    cache file or the network layer between assertions.
    """
    global _PAYLOAD_MEMO, _FETCH_BLOCKED_UNTIL
    with _MEMO_LOCK:
        _RATE_MEMO.clear()
        _PAYLOAD_MEMO = None
        _FETCH_BLOCKED_UNTIL = 0.0


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


# ── hardcoded snapshot ───────────────────────────────────────────────────────
#
# Last-resort fallback when Frankfurter is unreachable AND we have no usable
# disk cache. Values are USD-base cross-rates (multiply a USD amount by the
# rate to get the target currency), modelled on ECB reference rates around
# the snapshot date. Refresh this table when it is more than ~6 months old.
#
# NOTE: these numbers will drift. The resolution chain only reaches this
# table when *both* the live API and a 30-day cache have failed, in which
# case the user is shown a banner explaining what happened.
RATES_SNAPSHOT_DATE = "2026-04-15"
RATES_SNAPSHOT: dict[str, float] = {
    # Anchor — USD is identity by definition.
    "USD": 1.0,
    # Major Western currencies
    "EUR": 0.92,
    "GBP": 0.79,
    "CHF": 0.88,
    "CAD": 1.36,
    "AUD": 1.52,
    "NZD": 1.66,
    # Asia-Pacific
    "JPY": 152.0,
    "CNY": 7.20,
    "INR": 83.5,
    "KRW": 1380.0,
    "SGD": 1.34,
    "HKD": 7.81,
    "TWD": 32.5,
    "THB": 36.5,
    "MYR": 4.70,
    "IDR": 16200.0,
    "PHP": 57.0,
    # Americas
    "MXN": 17.5,
    "BRL": 5.10,
    # Middle East / Africa
    "ILS": 3.70,
    "AED": 3.6725,   # USD peg
    "SAR": 3.7500,   # USD peg
    "TRY": 32.5,
    "ZAR": 18.5,
    # Europe (non-EUR)
    "NOK": 10.7,
    "SEK": 10.5,
    "DKK": 6.85,
    "PLN": 3.95,
    "RUB": 92.0,
    # Additional EU / EEA
    "CZK": 23.2,
    "HUF": 360.0,
    "RON": 4.55,
    "BGN": 1.80,
    # Latin America (extra)
    "ARS": 880.0,
}


# ── public exception ─────────────────────────────────────────────────────────


class CurrencyError(Exception):
    """Raised when no rate is resolvable for a non-USD ISO code.

    The resolution chain falls through every fallback (live, fresh cache,
    stale cache, snapshot) before raising. Callers at the API boundary
    catch this and degrade to an unconverted USD payload with a
    user-visible warning.
    """


# ── cache I/O ────────────────────────────────────────────────────────────────

def _cache_path() -> Path:
    return app_dir() / "cache" / "exchange-rate.json"


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


def _cache_age(fetched_at: str | None) -> timedelta | None:
    """Return the age of a cache entry, or ``None`` if its timestamp is unparseable."""
    if not fetched_at:
        return None
    try:
        ts = datetime.fromisoformat(fetched_at.replace("Z", "+00:00"))
    except (ValueError, AttributeError):
        return None
    return datetime.now(UTC) - ts


def _is_fresh(fetched_at: str | None) -> bool:
    age = _cache_age(fetched_at)
    return age is not None and age < _CACHE_TTL


# ── network ──────────────────────────────────────────────────────────────────

def _note_fetch_failure() -> None:
    """Park the next live fetch for ``_FETCH_NEGATIVE_TTL_S`` (COST-7).

    Without this a hard-down Frankfurter cost every request a fresh 10s
    ``urlopen`` timeout: the failure path wrote nothing, so there was no record
    that the last attempt had just failed.
    """
    global _FETCH_BLOCKED_UNTIL
    with _MEMO_LOCK:
        _FETCH_BLOCKED_UNTIL = time.monotonic() + _FETCH_NEGATIVE_TTL_S


def _fetch_from_frankfurter() -> dict[str, float] | None:
    """Pull the full USD-base rate table from Frankfurter.

    Returns the validated rates dict on success, ``None`` on any failure
    (network, parse, schema). Callers fall through to the cache / snapshot
    chain in ``get_rate``.

    COST-7: a failure arms a negative cache — subsequent calls short-circuit to
    ``None`` for ``_FETCH_NEGATIVE_TTL_S`` instead of re-paying the 10s timeout.
    The caller's fallback chain is untouched, so a blocked fetch degrades
    exactly like a failed one (stale cache → snapshot). A success clears it.
    """
    global _FETCH_BLOCKED_UNTIL
    with _MEMO_LOCK:
        blocked = time.monotonic() < _FETCH_BLOCKED_UNTIL
    if blocked:
        logger.debug("currency: skipping fetch — a recent attempt failed (negative cache)")
        return None

    try:
        with urllib.request.urlopen(_FRANKFURTER_URL, timeout=_FETCH_TIMEOUT_S) as resp:
            data = json.loads(resp.read().decode("utf-8"))
    except (urllib.error.URLError, urllib.error.HTTPError, json.JSONDecodeError, TimeoutError, OSError) as e:
        logger.warning("currency: failed to fetch rates from %s: %s", _FRANKFURTER_URL, e)
        _note_fetch_failure()
        return None

    raw = data.get("rates")
    if not isinstance(raw, dict):
        logger.warning("currency: malformed Frankfurter response (no 'rates' field)")
        _note_fetch_failure()
        return None

    rates: dict[str, float] = {}
    for code, val in raw.items():
        if not isinstance(code, str) or not _ISO_CODE_RE.match(code):
            continue
        if not _is_valid_rate(val):
            continue
        rates[code] = float(val)
    if not rates:
        _note_fetch_failure()
        return None
    with _MEMO_LOCK:
        _FETCH_BLOCKED_UNTIL = 0.0
    return rates


def _is_valid_rate(value: Any) -> bool:
    if not isinstance(value, (int, float)):
        return False
    f = float(value)
    if f != f or f in (float("inf"), float("-inf")):  # NaN/Inf guard
        return False
    return _MIN_VALID_RATE <= f <= _MAX_VALID_RATE


# ── public API ───────────────────────────────────────────────────────────────


def resolve_rate(target_currency: str) -> tuple[float, str | None]:
    """Resolve the USD → ``target_currency`` rate plus an optional warning.

    Walks the resolution chain documented at the top of this module. The
    warning string is non-``None`` only when we had to fall back past the
    live fetch + fresh cache; the UI uses it to render a banner.

    Raises ``CurrencyError`` when every fallback fails (unknown code).

    COST-7: successful resolutions are memoized per code for ``_MEMO_TTL_S``, so
    a burst of API requests resolves once and the rest do zero disk I/O. Only
    successes are memoized — a raising code re-walks the chain, where the
    negative fetch cache (not a per-code memo) is what keeps it cheap.
    """
    code = (target_currency or "USD").upper()
    if code == "USD":
        return 1.0, None
    if not _ISO_CODE_RE.match(code):
        raise CurrencyError(f"invalid currency code: {target_currency!r}")

    with _MEMO_LOCK:
        hit = _RATE_MEMO.get(code)
        if hit is not None and time.monotonic() < hit[0]:
            return hit[1], hit[2]

    rate, warning = _resolve_rate_uncached(code)
    with _MEMO_LOCK:
        _RATE_MEMO[code] = (time.monotonic() + _MEMO_TTL_S, rate, warning)
    return rate, warning


def _resolve_rate_uncached(code: str) -> tuple[float, str | None]:
    """The resolution chain itself — ``code`` is already upper-cased + validated.

    Split out of ``resolve_rate`` so the memo wraps it without duplicating the
    fallback logic. Every step below is byte-for-byte the pre-memo behaviour.
    """
    cached = _read_cache()

    # (a) cache fresh — best case, no warning.
    if cached and _is_fresh(cached.get("fetched_at")):
        rate = cached.get("rates", {}).get(code)
        if _is_valid_rate(rate):
            return float(rate), None

    # (b) live fetch — refresh and use it, no warning.
    fresh = _fetch_from_frankfurter()
    if fresh is not None:
        _write_cache(fresh)
        rate = fresh.get(code)
        if _is_valid_rate(rate):
            return float(rate), None
        # Fetch succeeded but didn't include this code (e.g. KPW). Fall
        # through to the snapshot before raising — the snapshot may know it.

    # (c) stale cache — within 30 days, warn but use it.
    if cached:
        age = _cache_age(cached.get("fetched_at"))
        rate = cached.get("rates", {}).get(code)
        if age is not None and age <= _STALE_CACHE_MAX and _is_valid_rate(rate):
            days = max(1, age.days)
            warning = (
                f"FX rate for {code} is {days} day(s) old "
                f"(Frankfurter unreachable, cache stale)."
            )
            logger.warning("currency: %s", warning)
            return float(rate), warning

    # (d) hardcoded snapshot — last resort before raising.
    snap = RATES_SNAPSHOT.get(code)
    if _is_valid_rate(snap):
        warning = (
            f"Frankfurter unreachable; using built-in FX snapshot "
            f"from {RATES_SNAPSHOT_DATE} for {code}. "
            f"Numbers may be 1-2% off per month since the snapshot."
        )
        logger.warning("currency: %s", warning)
        return float(snap), warning

    # (e) nothing left.
    raise CurrencyError(f"no rate available for {code}")


def get_rate(target_currency: str) -> float:
    """Backwards-compatible rate lookup that swallows fallback warnings.

    Returns the rate from ``resolve_rate``, or ``1.0`` if every fallback
    fails. Prefer ``resolve_rate`` (or ``active_currency_payload``) when
    you want to surface the warning to the user.
    """
    try:
        rate, _ = resolve_rate(target_currency)
        return rate
    except CurrencyError:
        return 1.0


def get_symbol(currency: str) -> str:
    """Return the conventional symbol for ``currency`` or its ISO code."""
    code = (currency or "USD").upper()
    return _SYMBOLS.get(code, code)


def convert_usd(usd: float, target: str) -> float:
    """Convert a USD amount into ``target`` using the cached rate."""
    return float(usd) * get_rate(target)


def list_supported() -> list[str]:
    """Return the list of ISO codes the UI can offer in the dropdown.

    Includes USD, anything in the local cache, and every code in
    ``RATES_SNAPSHOT`` so the picker is non-empty even on a brand-new
    install with no successful fetch yet. Sorted alphabetically with USD
    pinned to the front.
    """
    seen: set[str] = {"USD"}
    seen.update(c for c in RATES_SNAPSHOT if _ISO_CODE_RE.match(c))
    cached = _read_cache()
    if cached and isinstance(cached.get("rates"), dict):
        seen.update(c for c in cached["rates"] if _ISO_CODE_RE.match(c))
    return ["USD"] + sorted(seen - {"USD"})


def format_in_currency(usd: float, target: str | None = None) -> dict[str, Any]:
    """Return the canonical ``{code, symbol, rate_from_usd, amount, warning}`` payload.

    If ``target`` is omitted, the active ``Settings().currency`` is used.

    On the happy path (live fetch or fresh cache) ``warning`` is ``None``.
    On a stale-cache or snapshot fallback ``warning`` carries a
    user-readable string explaining what happened — the UI shows it as a
    banner.

    If the chain bottoms out with an unknown code, this function returns
    a USD-denominated payload with a warning rather than raising — the
    correctness rule is "never silently emit ``rate_from_usd=1.0`` for a
    non-USD code", so we explicitly switch ``code`` to ``USD``.
    """
    if target is None:
        # Imported lazily to avoid a circular import via settings → infra.
        from stackunderflow.settings import Settings
        target = Settings().currency or "USD"

    requested = (target or "USD").upper()
    if not _ISO_CODE_RE.match(requested):
        requested = "USD"

    try:
        rate, warning = resolve_rate(requested)
        code = requested
    except CurrencyError as e:
        # Drop back to USD with a loud warning rather than mislabel a
        # USD number with the user's chosen symbol.
        logger.warning("currency: %s — falling back to USD", e)
        code = "USD"
        rate = 1.0
        warning = (
            f"Frankfurter unreachable and no offline rate available for "
            f"{requested}; showing USD instead. "
            f"Set CURRENCY=USD to silence this banner."
        )

    return {
        "code": code,
        "symbol": get_symbol(code),
        "rate_from_usd": rate,
        "amount": float(usd) * rate,
        "warning": warning,
    }


def active_currency_payload() -> dict[str, Any]:
    """The ``currency`` block stamped onto every API response.

    Shape: ``{"code", "symbol", "rate_from_usd", "warning"}``. Independent
    of any specific dollar amount — meant to live at the top level of the
    JSON response so the UI can pull it once per fetch.

    ``warning`` is ``None`` on the happy path and a human-readable string
    when a fallback was used. The UI surfaces it as a banner.

    COST-7: memoized for ``_MEMO_TTL_S``. This is the hottest currency call in
    the app — it runs on every cost-bearing response — and it read
    ``config.json`` off disk through ``Settings()`` on EVERY request, including
    the USD short-circuit that needs no rate at all. Callers get a fresh dict
    each time so nobody can mutate the memo. ``clear_currency_memo()`` drops it;
    the currency-write route calls that so a change is never masked by the TTL.
    """
    global _PAYLOAD_MEMO
    with _MEMO_LOCK:
        memo = _PAYLOAD_MEMO
        if memo is not None and time.monotonic() < memo[0]:
            return dict(memo[1])

    payload = format_in_currency(0.0)
    payload.pop("amount", None)
    with _MEMO_LOCK:
        _PAYLOAD_MEMO = (time.monotonic() + _MEMO_TTL_S, dict(payload))
    return payload


__all__ = [
    "CurrencyError",
    "RATES_SNAPSHOT",
    "RATES_SNAPSHOT_DATE",
    "active_currency_payload",
    "clear_currency_memo",
    "convert_usd",
    "format_in_currency",
    "get_rate",
    "get_symbol",
    "list_supported",
    "resolve_rate",
]
