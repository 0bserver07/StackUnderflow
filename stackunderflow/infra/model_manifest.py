"""Data-driven model manifest: identity + effective-dated pricing.

The source of truth that replaces the hardcoded per-provider rate dicts and
the token-matching ``_identify`` ladders. Model facts — which id maps to
which family, what it costs, when that price was in effect — live in
``stackunderflow/data/models.toml``: data you edit, diff, and review, not
Python branches. Adding a model or correcting a price is a manifest edit.

Pricing is effective-dated. Each model carries one or more price rows with
optional ``effective_from`` / ``effective_until`` (ISO ``YYYY-MM-DD`` strings).
Pass ``at_ts`` to price a historical event at the rate in effect then; omit it
for the current rate. Rows with no dates are always-current.

The per-provider ``ProviderPricer`` classes keep their token-normalization
logic and delegate identity + rates here.
"""

from __future__ import annotations

import logging
import sqlite3
import time
import tomllib
from functools import lru_cache
from pathlib import Path

logger = logging.getLogger(__name__)

_MANIFEST_PATH = Path(__file__).resolve().parent.parent / "data" / "models.toml"

_REQUIRED_PRICE_FIELDS = ("input", "output", "cache_write", "cache_read")


def _valid_price_row(row: object) -> bool:
    if not isinstance(row, dict):
        return False
    return all(
        isinstance(row.get(f), int | float) and not isinstance(row.get(f), bool)
        for f in _REQUIRED_PRICE_FIELDS
    )


def _valid_model(entry: object) -> bool:
    """A usable model entry: a non-empty ``family`` plus a non-empty ``price``
    list whose every row carries numeric input/output/cache_write/cache_read.

    Used to drop malformed manifest entries at load time so they can never
    KeyError at lookup (where the error would be swallowed into a $0 cost).
    """
    if not isinstance(entry, dict):
        return False
    if not isinstance(entry.get("family"), str) or not entry.get("family"):
        return False
    prices = entry.get("price")
    if not isinstance(prices, list) or not prices:
        return False
    return all(_valid_price_row(p) for p in prices)


@lru_cache(maxsize=1)
def _models() -> list[dict]:
    """Parse, validate, and cache the manifest once.

    Order is preserved and load-bearing: ``canonicalize`` returns the first
    matching entry, so more-specific families must appear before broader ones.

    Malformed entries (missing ``family``, or a price row lacking numeric
    input/output/cache_write/cache_read) are DROPPED with a warning rather than
    left to KeyError at lookup time — a silent pricing failure (the ingest
    normalizer swallows exceptions → $0 cost) is worse than a loud, visible skip.
    """
    with open(_MANIFEST_PATH, "rb") as fh:
        data = tomllib.load(fh)
    valid: list[dict] = []
    for entry in data.get("model", []):
        if _valid_model(entry):
            valid.append(entry)
        else:
            fam = entry.get("family") if isinstance(entry, dict) else entry
            logger.warning("model_manifest: dropping malformed model entry %r", fam)
    return valid


def _for_provider(provider: str) -> list[dict]:
    return [m for m in _models() if m.get("provider") == provider]


def _by_family(provider: str) -> dict[str, dict]:
    return {m["family"]: m for m in _for_provider(provider)}


def _fallback_family(provider: str) -> str | None:
    for m in _for_provider(provider):
        if m.get("fallback"):
            return m["family"]
    return None


def canonicalize(model_id: str, provider: str = "anthropic") -> str | None:
    """Map a free-form model id to a manifest family key.

    Splits the id on ``-`` / ``.`` into a token set and returns the first
    entry (in manifest order) whose ``match`` tokens are all present. Falls
    back to the provider's ``fallback`` family when nothing matches.
    """
    fallback = _fallback_family(provider)
    if not model_id:
        return fallback
    parts = set(model_id.lower().replace(".", "-").split("-"))
    for entry in _for_provider(provider):
        match = entry.get("match") or []
        if match and set(match).issubset(parts):
            return entry["family"]
    return fallback


def _select_price(prices: list[dict], at_ts: str | None) -> dict | None:
    """Pick the price row effective at ``at_ts`` (ISO string), or the current
    one when ``at_ts`` is None. Rows without dates always apply."""
    if not prices:
        return None
    if at_ts is None:
        current = [p for p in prices if not p.get("effective_until")]
        return (current or prices)[-1]
    for p in prices:
        ef = p.get("effective_from")
        eu = p.get("effective_until")
        if (ef is None or at_ts >= ef) and (eu is None or at_ts < eu):
            return p
    return prices[-1]


def rates_for(
    canonical: str | None,
    provider: str = "anthropic",
    at_ts: str | None = None,
) -> tuple[float, float, float, float] | None:
    """Return ``(input, output, cache_write, cache_read)`` in $/M for a family.

    An unknown family resolves to the provider's fallback family, preserving
    the pre-manifest contract that the Anthropic pricer never returns None.
    ``at_ts`` selects the effective-dated row; omit for the current rate.
    """
    table = _by_family(provider)
    entry = table.get(canonical) if canonical else None
    if entry is None:
        fb = _fallback_family(provider)
        entry = table.get(fb) if fb else None
    if entry is None:
        return None
    price = _select_price(entry.get("price") or [], at_ts)
    if price is None:
        return None
    return (
        float(price["input"]),
        float(price["output"]),
        float(price["cache_write"]),
        float(price["cache_read"]),
    )


def fast_multiplier(canonical: str | None, provider: str = "anthropic") -> float | None:
    """Per-model input/output multiplier for the priority/fast tier (Opus
    bills ~6×). ``None`` when the model has no fast-tier premium."""
    entry = _by_family(provider).get(canonical) if canonical else None
    if entry is None:
        return None
    mult = entry.get("fast_multiplier")
    return float(mult) if mult else None


# ── unified price book (store-backed) ────────────────────────────────────────
#
# The ``price_book`` table (migration v024) is the single effective-dated home
# the manifest + RATE_CARD back-fill into and the LiteLLM overlay appends "live"
# snapshots into. ``compute_cost`` reads it through ``price_book_lookup`` with
# the same precedence as before (live > rate_card/manifest) and falls back to
# the in-code manifest when the book is empty (fresh store), so cost numbers are
# unchanged. Rates are stored in the manifest's $/M unit so a book hit is
# byte-for-byte the in-code value.

_SOURCE_MANIFEST = "manifest"
_SOURCE_RATE_CARD = "rate_card"
_SOURCE_LIVE = "live"

# Read precedence when several sources carry a row for the same model — the
# live overlay wins, mirroring ``costs.compute_cost``'s overlay-first behaviour.
_SOURCE_PRECEDENCE = {_SOURCE_LIVE: 0, _SOURCE_RATE_CARD: 1, _SOURCE_MANIFEST: 2}

# Default store location — same convention as ``services/pricing_service`` and
# ``deps.store_path``. Kept here (not imported from ``deps``) to avoid an
# infra→app import cycle.
_STORE_PATH = Path.home() / ".stackunderflow" / "store.db"


def manifest_price_book_rows() -> list[dict]:
    """Flatten the in-code manifest into ``price_book``-shaped rows.

    One row per (family, price-row): the manifest family is the ``model`` key
    and each effective-dated price row maps directly. Empty
    ``effective_from`` / ``effective_until`` sentinels stand in for the
    manifest's ``None`` (always-current) so they survive the table's NOT NULL
    UNIQUE key.
    """
    rows: list[dict] = []
    for m in _models():
        provider = m.get("provider")
        family = m.get("family")
        if not provider or not family:
            continue
        for price in m.get("price") or []:
            rows.append(
                {
                    "provider": provider,
                    "model": family,
                    "effective_from": price.get("effective_from") or "",
                    "effective_until": price.get("effective_until") or "",
                    "input": float(price["input"]),
                    "output": float(price["output"]),
                    "cache_write": float(price["cache_write"]),
                    "cache_read": float(price["cache_read"]),
                    "source": _SOURCE_MANIFEST,
                }
            )
    return rows


def _upsert_price_rows(conn: sqlite3.Connection, rows: list[dict]) -> int:
    """Idempotently write ``price_book`` rows (UPSERT on the unique key).

    A re-run overwrites the same (provider, model, effective_from, source) row
    in place, so backfilling twice — or refreshing a live snapshot — is a no-op
    on identity and a value-refresh otherwise. Returns the number of rows
    written.
    """
    now = time.time()
    written = 0
    for r in rows:
        conn.execute(
            "INSERT INTO price_book "
            "(provider, model, effective_from, effective_until, "
            " input, output, cache_write, cache_read, source, updated_at) "
            "VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?) "
            "ON CONFLICT(provider, model, effective_from, source) DO UPDATE SET "
            "  effective_until = excluded.effective_until, "
            "  input = excluded.input, output = excluded.output, "
            "  cache_write = excluded.cache_write, cache_read = excluded.cache_read, "
            "  updated_at = excluded.updated_at",
            (
                r["provider"], r["model"], r.get("effective_from", ""),
                r.get("effective_until", ""), r["input"], r["output"],
                r["cache_write"], r["cache_read"], r.get("source", _SOURCE_MANIFEST),
                now,
            ),
        )
        written += 1
    return written


def backfill_price_book(
    conn: sqlite3.Connection, rate_card_rows: list[dict] | None = None
) -> int:
    """Populate ``price_book`` from the manifest (+ optional RATE_CARD rows).

    The manifest's dated rows map directly (``source='manifest'``, keyed by
    family). ``rate_card_rows`` — passed by ``costs.backfill_price_book`` so
    this module needn't import ``costs`` (cycle) — carries the concrete
    ``_CANONICAL_IDS`` priced at their current rate (``source='rate_card'``),
    which is what the per-id lookup tier hits for non-manifest providers
    (openai/qwen/…). Idempotent; returns the row count written.
    """
    written = _upsert_price_rows(conn, manifest_price_book_rows())
    if rate_card_rows:
        written += _upsert_price_rows(conn, rate_card_rows)
    return written


def append_live_snapshot(conn: sqlite3.Connection, rows: list[dict]) -> int:
    """Append LiteLLM-overlay rows as dated ``source='live'`` snapshots.

    Each row needs ``provider`` / ``model`` / the four $/M rates; an absent
    ``effective_from`` is stamped with today's date so the snapshot is
    effective-dated (the overlay JSON cache carries no history, so each
    refresh records "as of today"). The same-day snapshot for a model
    overwrites in place via the unique key.
    """
    today = time.strftime("%Y-%m-%d", time.gmtime())
    stamped = [
        {**r, "effective_from": r.get("effective_from") or today, "source": _SOURCE_LIVE}
        for r in rows
    ]
    return _upsert_price_rows(conn, stamped)


def _row_effective_at(rows: list[sqlite3.Row], at_ts: str | None) -> sqlite3.Row | None:
    """Pick the ``price_book`` row effective at ``at_ts`` (date-prefix compared).

    Mirrors ``_select_price``: with no ``at_ts`` prefer the open-ended
    (``effective_until == ''``) row, else the last; with an ``at_ts`` pick the
    window that contains it. ``at_ts`` may be a full ISO timestamp — only its
    ``YYYY-MM-DD`` prefix is compared against the date-only bounds.
    """
    if not rows:
        return None
    if at_ts is None:
        current = [r for r in rows if not r["effective_until"]]
        return (current or rows)[-1]
    day = at_ts[:10]
    for r in rows:
        ef = r["effective_from"]
        eu = r["effective_until"]
        if (not ef or day >= ef) and (not eu or day < eu):
            return r
    return rows[-1]


def _lookup_by_model_source(
    conn: sqlite3.Connection, provider: str, model: str, source: str, at_ts: str | None
) -> tuple[float, float, float, float] | None:
    rows = conn.execute(
        "SELECT effective_from, effective_until, input, output, cache_write, cache_read "
        "FROM price_book WHERE provider = ? AND model = ? AND source = ? "
        "ORDER BY effective_from",
        (provider, model, source),
    ).fetchall()
    row = _row_effective_at(rows, at_ts)
    if row is None:
        return None
    return (
        float(row["input"]), float(row["output"]),
        float(row["cache_write"]), float(row["cache_read"]),
    )


def price_book_lookup(
    conn: sqlite3.Connection,
    model: str,
    provider: str = "anthropic",
    at_ts: str | None = None,
) -> tuple[float, float, float, float] | None:
    """Resolve ``(input, output, cache_write, cache_read)`` $/M from the book.

    Precedence — same as ``costs.compute_cost`` (live > rate_card > manifest):

      1. ``source='live'`` keyed by the concrete model id (LiteLLM overlay).
      2. ``source='rate_card'`` keyed by the concrete model id.
      3. ``source='manifest'`` keyed by the canonical family (``canonicalize``).

    Returns ``None`` on a clean miss (book empty / model absent) so the caller
    falls back to the in-code manifest — guaranteeing a fresh store prices
    identically to today.
    """
    if not model:
        return None
    for source in (_SOURCE_LIVE, _SOURCE_RATE_CARD):
        hit = _lookup_by_model_source(conn, provider, model, source, at_ts)
        if hit is not None:
            return hit
    family = canonicalize(model, provider)
    if family:
        return _lookup_by_model_source(conn, provider, family, _SOURCE_MANIFEST, at_ts)
    return None


# ── opt-in store wiring for the connection-free ``compute_cost`` path ─────────
#
# ``compute_cost`` is a pure module-level function with no DB handle; the lookup
# must therefore reach a connection through a configured seam. When no store is
# wired (the default — every existing call site, every unit test), the lookup is
# skipped entirely and ``compute_cost`` prices from the in-code manifest exactly
# as before. Wiring a store in (``use_price_book_store``) makes the book the
# source while keeping the in-code manifest as the miss fallback.

_store_path_override: Path | None = None
_use_store: bool = False


def use_price_book_store(path: str | Path | None = None, *, enabled: bool = True) -> None:
    """Enable (or disable) reading rates from the on-disk ``price_book``.

    ``path`` overrides the default ``~/.stackunderflow/store.db``. With
    ``enabled=False`` the book is ignored and pricing reverts to the in-code
    manifest (the default state). Idempotent and cheap — opening the connection
    happens per-lookup and is guarded.
    """
    global _store_path_override, _use_store
    _use_store = bool(enabled)
    _store_path_override = Path(path) if path is not None else None


def _store_path() -> Path:
    return _store_path_override if _store_path_override is not None else _STORE_PATH


def store_price_book_lookup(
    model: str, provider: str = "anthropic", at_ts: str | None = None
) -> tuple[float, float, float, float] | None:
    """Book lookup for the connection-free path, behind the ``use_store`` seam.

    Returns ``None`` (→ in-code fallback) when the store isn't wired, the file
    is missing, the table doesn't exist yet, or any read error — pricing must
    never raise just because the book is unavailable.
    """
    if not _use_store:
        return None
    path = _store_path()
    if not path.exists():
        return None
    conn: sqlite3.Connection | None = None
    try:
        conn = sqlite3.connect(path)
        conn.row_factory = sqlite3.Row
        return price_book_lookup(conn, model, provider, at_ts)
    except sqlite3.Error:
        return None
    finally:
        if conn is not None:
            conn.close()
