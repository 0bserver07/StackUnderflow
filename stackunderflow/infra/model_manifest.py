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


@lru_cache(maxsize=1)
def canonical_id_groups() -> dict[str, tuple[str, ...]]:
    """``[canonical_ids]`` groups, keyed by PRICER KEY (the contract: each
    group name in the manifest is the pricer the ids route to). Identity
    only — never prices."""
    with open(_MANIFEST_PATH, "rb") as fh:
        data = tomllib.load(fh)
    groups = data.get("canonical_ids", {})
    out: dict[str, tuple[str, ...]] = {}
    if isinstance(groups, dict):
        for pricer, ids in groups.items():
            if isinstance(ids, list):
                out[str(pricer)] = tuple(str(i) for i in ids)
    return out


@lru_cache(maxsize=1)
def canonical_ids() -> tuple[str, ...]:
    """Concrete model ids the rate card recognises (``[canonical_ids]``).

    Identity only — never prices. Order is stable and load-bearing for
    display: groups in file order, ids in listed order (``RATE_CARD`` is
    built in this order). Unknown/missing section yields an empty tuple —
    loudly wrong (everything prices as unknown) rather than silently
    hardcoded in Python.
    """
    out: list[str] = []
    for ids in canonical_id_groups().values():
        out.extend(ids)
    return tuple(out)


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

    Exact ``ids`` entries win first (manifest order); otherwise splits the
    id on ``-`` / ``.`` into a token set and returns the first entry whose
    ``match`` tokens are all present. Falls back to the provider's
    ``fallback`` family when nothing matches.
    """
    fallback = _fallback_family(provider)
    if not model_id:
        return fallback
    lowered = model_id.lower()
    # Exact ids first: token sets collapse duplicates ("gpt-5.5" → {gpt,5}),
    # so dotted point-releases are only distinguishable by exact id.
    for entry in _for_provider(provider):
        if any(lowered == str(i).lower() for i in entry.get("ids") or []):
            return entry["family"]
    parts = set(lowered.replace(".", "-").split("-"))
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


def _row_effective_at(rows, at_ts: str | None):
    """Pick the ``price_book`` row effective at ``at_ts`` (date-prefix compared).

    Mirrors ``_select_price``: with no ``at_ts`` prefer the open-ended
    (``effective_until == ''``) row, else the last; with an ``at_ts`` pick the
    window that contains it. ``at_ts`` may be a full ISO timestamp — only its
    ``YYYY-MM-DD`` prefix is compared against the date-only bounds.

    ``rows`` is any sequence of mapping-style rows (``sqlite3.Row`` from the
    connection path, or the lightweight dicts the in-memory cache stores) — both
    support ``r["effective_from"]`` / ``r["effective_until"]`` subscripting.
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

    Precedence: live > dated manifest family > undated rate_card snapshot:

      1. ``source='live'`` keyed by the concrete model id (LiteLLM overlay).
      2. ``source='manifest'`` keyed by the canonical family (``canonicalize``)
         — effective-DATED rows, so historical corrections apply.
      3. ``source='rate_card'`` keyed by the concrete model id (undated
         current-rate snapshot; last so it can never shadow a dated row).

    Returns ``None`` on a clean miss (book empty / model absent) so the caller
    falls back to the in-code manifest — guaranteeing a fresh store prices
    identically to today.
    """
    if not model:
        return None
    hit = _lookup_by_model_source(conn, provider, model, _SOURCE_LIVE, at_ts)
    if hit is not None:
        return hit
    # Dated manifest family rows outrank the undated rate_card snapshots:
    # rate_card rows are effective-undated by construction (current rates),
    # so a dated rate correction must never be shadowed by them.
    family = canonicalize(model, provider)
    if family:
        hit = _lookup_by_model_source(conn, provider, family, _SOURCE_MANIFEST, at_ts)
        if hit is not None:
            return hit
    return _lookup_by_model_source(conn, provider, model, _SOURCE_RATE_CARD, at_ts)


# ── store wiring + in-memory cache for the connection-free ``compute_cost`` ───
#
# ``compute_cost`` is a pure module-level function with no DB handle; the lookup
# must therefore reach the book through a configured seam. When wired, the book
# is loaded ONCE into a module-level structure (``_book_cache``) and every lookup
# hits memory — there is NO per-call DB query. A clean miss returns ``None`` so
# ``compute_cost`` falls back to the in-code manifest, which keeps a fresh /
# empty store (and any CLI/ETL run before a backfill) pricing exactly as today.
#
# The default at import is DISABLED, so every unit test and any bare ``import
# stackunderflow`` prices from the in-code manifest, deterministically. The
# server turns the book on at startup (``prime_price_book_cache`` after a
# backfill from the SAME in-code manifest — see ``server._lifespan``), where the
# rows are gate-proven equal to in-code. The cache is also re-primed by
# ``refresh_price_book_cache`` after the PricingService appends a live snapshot.
# Changing the wired path (or disabling the seam) invalidates the cache so the
# next lookup re-primes from the new source.

_store_path_override: Path | None = None
_use_store: bool = False

# In-memory book: (provider, model, source) -> rows sorted by effective_from.
# Each row is a lightweight dict mirroring the columns the SQL lookup reads.
# ``None`` means "not primed yet"; an empty dict means "primed, book empty".
# Invalidation keys off ``_store_path_override`` (see ``use_price_book_store``),
# so no separate "primed from" path needs tracking.
_book_cache: dict[tuple[str, str, str], list[dict]] | None = None


def use_price_book_store(path: str | Path | None = None, *, enabled: bool = True) -> None:
    """Enable (or disable) reading rates from the on-disk ``price_book``.

    ``path`` overrides the default ``~/.stackunderflow/store.db``. With
    ``enabled=False`` the book is ignored and pricing reverts to the in-code
    manifest (the default state). Changing the path or the enabled flag
    invalidates the in-memory cache so the next lookup re-primes from the new
    source — there is no per-call DB I/O.
    """
    global _store_path_override, _use_store
    new_path = Path(path) if path is not None else None
    # Any change to the wired source must drop the cached rows so the next
    # lookup re-primes; otherwise a toggle would serve a stale book.
    if not enabled or new_path != _store_path_override:
        _invalidate_book_cache()
    _use_store = bool(enabled)
    _store_path_override = new_path


def _store_path() -> Path:
    return _store_path_override if _store_path_override is not None else _STORE_PATH


def _invalidate_book_cache() -> None:
    """Drop the in-memory book so the next lookup re-primes."""
    global _book_cache
    _book_cache = None


def _build_book_cache(conn: sqlite3.Connection) -> dict[tuple[str, str, str], list[dict]]:
    """Read the entire ``price_book`` into memory, grouped + effective-sorted.

    One pass over the table; every lookup thereafter is a dict access. Rows are
    sorted by ``effective_from`` to match the ``ORDER BY`` the connection path
    uses, so ``_row_effective_at`` selects the identical row from memory.
    """
    grouped: dict[tuple[str, str, str], list[dict]] = {}
    rows = conn.execute(
        "SELECT provider, model, source, effective_from, effective_until, "
        "input, output, cache_write, cache_read FROM price_book "
        "ORDER BY effective_from"
    ).fetchall()
    for r in rows:
        key = (r["provider"], r["model"], r["source"])
        grouped.setdefault(key, []).append(
            {
                "effective_from": r["effective_from"],
                "effective_until": r["effective_until"],
                "input": float(r["input"]),
                "output": float(r["output"]),
                "cache_write": float(r["cache_write"]),
                "cache_read": float(r["cache_read"]),
            }
        )
    return grouped


def prime_price_book_cache(conn: sqlite3.Connection | None = None) -> bool:
    """Load the whole ``price_book`` into the module-level cache.

    Pass a live ``conn`` (server startup, right after a backfill) to prime from
    it directly; with no ``conn`` the wired store path is opened once. Returns
    ``True`` when the cache was populated (table present), ``False`` otherwise
    (missing file / table / read error) — never raises, so a fresh store leaves
    the cache empty and lookups fall through to the in-code manifest.
    """
    global _book_cache
    if conn is not None:
        prev_factory = conn.row_factory
        try:
            conn.row_factory = sqlite3.Row
            _book_cache = _build_book_cache(conn)
        except sqlite3.Error:
            _book_cache = {}
            return False
        finally:
            conn.row_factory = prev_factory
        return True

    path = _store_path()
    if not path.exists():
        _book_cache = {}
        return False
    own: sqlite3.Connection | None = None
    try:
        own = sqlite3.connect(path)
        own.row_factory = sqlite3.Row
        _book_cache = _build_book_cache(own)
        return True
    except sqlite3.Error:
        _book_cache = {}
        return False
    finally:
        if own is not None:
            own.close()


def refresh_price_book_cache() -> None:
    """Re-prime the in-memory book from the wired store.

    Called by ``PricingService`` after it appends a ``source='live'`` snapshot
    so the new rates are visible to ``compute_cost`` without a process restart
    and without a per-call DB read. No-op-safe when the seam is disabled.
    """
    if not _use_store:
        _invalidate_book_cache()
        return
    prime_price_book_cache()


def _ensure_book_cache() -> dict[tuple[str, str, str], list[dict]] | None:
    """Return the primed cache, lazily priming from the wired path on first use.

    Returns ``None`` when the seam is disabled (→ in-code path). A primed-but-
    empty cache (fresh store) returns an empty dict, whose misses also fall
    through to in-code.
    """
    if not _use_store:
        return None
    if _book_cache is None:
        prime_price_book_cache()
    return _book_cache


def _cached_rows(provider: str, model: str, source: str) -> list[dict]:
    cache = _book_cache or {}
    return cache.get((provider, model, source), [])


def _cached_lookup_by_model_source(
    provider: str, model: str, source: str, at_ts: str | None
) -> tuple[float, float, float, float] | None:
    row = _row_effective_at(_cached_rows(provider, model, source), at_ts)
    if row is None:
        return None
    return (
        float(row["input"]), float(row["output"]),
        float(row["cache_write"]), float(row["cache_read"]),
    )


def store_price_book_lookup(
    model: str, provider: str = "anthropic", at_ts: str | None = None
) -> tuple[float, float, float, float] | None:
    """Book lookup for the connection-free path, served from the in-memory cache.

    Mirrors ``price_book_lookup``'s precedence (live > manifest > rate_card) but
    reads the module-level ``_book_cache`` — NO per-call DB query. Returns
    ``None`` (→ in-code fallback) when the seam is disabled, the store is
    missing/empty, or the model is absent. Never raises: pricing must not break
    just because the book is unavailable.
    """
    if not model:
        return None
    cache = _ensure_book_cache()
    if cache is None:  # seam disabled
        return None
    hit = _cached_lookup_by_model_source(provider, model, _SOURCE_LIVE, at_ts)
    if hit is not None:
        return hit
    # Dated manifest family rows outrank the undated rate_card snapshots:
    # rate_card rows are effective-undated by construction (current rates),
    # so a dated rate correction must never be shadowed by them.
    family = canonicalize(model, provider)
    if family:
        hit = _cached_lookup_by_model_source(provider, family, _SOURCE_MANIFEST, at_ts)
        if hit is not None:
            return hit
    return _cached_lookup_by_model_source(provider, model, _SOURCE_RATE_CARD, at_ts)
