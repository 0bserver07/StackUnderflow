"""Unified price book — the audit #2 correctness gate.

Pricing used to live in three places with an implicit resolution order
(``infra/costs.RATE_CARD``, the ``data/models.toml`` manifest, the LiteLLM
overlay in ``services/pricing_service``). v024 adds a single effective-dated
``price_book`` table the first two back-fill into and the overlay appends
``live`` snapshots into; ``compute_cost`` reads it (when a store is wired)
ahead of the in-code manifest, with a clean-miss fallback to that manifest.

THE GATE (``TestPriceBookEqualsInCode``): this is a refactor of WHERE rates
live, not WHAT they are. ``compute_cost`` over a representative spread of
(model, provider, speed, tokens) MUST return the IDENTICAL number whether it
prices from the backfilled book or the in-code manifest. If a future edit
changes a number on one path only, this fails instead of silently shipping
wrong dollars.

Plus: lookup precedence (live > rate_card > manifest), effective-dating by
``at_ts``, the live-overlay append, backfill idempotency, and the safe-by-
default fallback (no store wired ⇒ in-code path, byte-for-byte unchanged).

ACTIVATION (the live default): the server primes the whole book into a
module-level in-memory cache (``prime_price_book_cache``) after a backfill, and
``compute_cost`` serves rates from memory. ``TestCachePrimedBookEqualsInCode``
re-proves the equality gate through that priming path, ``TestNoPerCallDbIO``
locks the no-per-call-DB-I/O contract, and ``TestLiveSnapshotRefreshesCache``
checks the post-append refresh hook.
"""

from __future__ import annotations

import sqlite3

import pytest

from stackunderflow.infra import model_manifest as mm
from stackunderflow.infra.costs import backfill_price_book, compute_cost
from stackunderflow.store import db, schema

# ── representative (model, provider, speed) spread ────────────────────────────
# Spans: current Opus (standard + fast-tier 6× multiplier), a dated/legacy
# Anthropic id, Haiku 3 (the as-published non-formula cache rates), Sonnet
# (no fast premium — fast must equal standard), two OpenAI families (cache-write
# 0×), the GLM Anthropic-proxy, and an unknown claude id (Anthropic fallback).
_CASES = [
    ("claude-opus-4-8", "anthropic", "standard"),
    ("claude-opus-4-8", "anthropic", "fast"),
    ("claude-opus-4-20250514", "anthropic", "standard"),
    ("claude-opus-4-20250514", "anthropic", "fast"),
    ("claude-3-haiku-20240307", "anthropic", "standard"),
    ("claude-sonnet-4-5-20250929", "anthropic", "standard"),
    ("claude-sonnet-4-5-20250929", "anthropic", "fast"),
    ("claude-fable-5", "anthropic", "standard"),
    ("gpt-5-codex", "openai", "standard"),
    ("gpt-4o", "openai", "standard"),
    ("glm-5", "anthropic", "standard"),
    ("glm-5.1", "anthropic", "standard"),
    ("claude-made-up-model-xyz", "anthropic", "standard"),  # fallback family
]

# A token shape that exercises every rate column (input/output/cache rw/r).
_TOKENS = {"input": 1000, "output": 500, "cache_read": 200, "cache_creation": 50}


@pytest.fixture
def backfilled_store(tmp_path):
    """A migrated store with ``price_book`` populated from manifest + RATE_CARD."""
    conn = db.connect(tmp_path / "store.db")
    schema.apply(conn)
    backfill_price_book(conn)
    conn.commit()
    conn.close()
    yield tmp_path / "store.db"


@pytest.fixture(autouse=True)
def _reset_book_seam():
    """Every test starts and ends with the store seam disabled (the default)."""
    mm.use_price_book_store(enabled=False)
    yield
    mm.use_price_book_store(enabled=False)


# ── THE GATE: book path == in-code path ───────────────────────────────────────


class TestPriceBookEqualsInCode:
    def test_compute_cost_identical_book_vs_incode(self, backfilled_store):
        """For every case, the book-priced total equals the in-code total."""
        # In-code path (book disabled — the default for all existing callers).
        mm.use_price_book_store(enabled=False)
        incode = {
            (m, s): compute_cost(_TOKENS, m, provider=p, speed=s)["total_cost"]
            for m, p, s in _CASES
        }
        # Book path (store wired).
        mm.use_price_book_store(backfilled_store, enabled=True)
        book = {
            (m, s): compute_cost(_TOKENS, m, provider=p, speed=s)["total_cost"]
            for m, p, s in _CASES
        }

        diffs = {
            key: (a, book[key])
            for key, a in incode.items()
            if abs(a - book[key]) > 1e-12
        }
        assert not diffs, f"book/in-code cost divergence: {diffs}"

    def test_full_breakdown_identical_not_just_total(self, backfilled_store):
        """Every cost component (input/output/cache) matches, not only the sum —
        catches a rate swapped between columns that happens to net the same."""
        keys = ("input_cost", "output_cost", "cache_creation_cost", "cache_read_cost", "total_cost")
        for model, provider, speed in _CASES:
            mm.use_price_book_store(enabled=False)
            a = compute_cost(_TOKENS, model, provider=provider, speed=speed)
            mm.use_price_book_store(backfilled_store, enabled=True)
            b = compute_cost(_TOKENS, model, provider=provider, speed=speed)
            for k in keys:
                assert a[k] == pytest.approx(b[k], abs=1e-12), f"{model}/{speed} {k} differs"

    def test_fast_tier_multiplier_preserved_on_book_path(self, backfilled_store):
        """The Opus 6× premium is a manifest concept the book path must re-apply:
        fast > standard for Opus, and equal for a model with no fast premium."""
        mm.use_price_book_store(backfilled_store, enabled=True)
        opus, son = "claude-opus-4-8", "claude-sonnet-4-5-20250929"
        opus_std = compute_cost(_TOKENS, opus, provider="anthropic", speed="standard")["total_cost"]
        opus_fast = compute_cost(_TOKENS, opus, provider="anthropic", speed="fast")["total_cost"]
        son_std = compute_cost(_TOKENS, son, provider="anthropic", speed="standard")["total_cost"]
        son_fast = compute_cost(_TOKENS, son, provider="anthropic", speed="fast")["total_cost"]
        assert opus_fast > opus_std  # premium applied
        assert son_fast == pytest.approx(son_std)  # no premium for sonnet


# ── ACTIVE default: in-memory cache primed from a backfilled store ─────────────
#
# The activation path the server takes: backfill the book, then prime the
# module-level in-memory cache from that store via ``prime_price_book_cache``.
# ``compute_cost`` then serves rates from memory. These tests assert the SAME
# number-preservation contract as the gate above, but through the priming +
# cached-lookup machinery (not the lazy per-path prime the gate exercises), AND
# that no DB connection is opened per ``compute_cost`` call.


class TestCachePrimedBookEqualsInCode:
    def _prime_from(self, store_path):
        """Wire + prime the in-memory cache from ``store_path`` (server path)."""
        conn = sqlite3.connect(store_path)
        try:
            mm.use_price_book_store(store_path, enabled=True)
            primed = mm.prime_price_book_cache(conn)
        finally:
            conn.close()
        assert primed is True  # a backfilled store always has rows

    def test_full_breakdown_identical_with_primed_cache(self, backfilled_store):
        """Every cost component matches in-code for the full spread, priced from
        the primed in-memory cache (the active server default)."""
        keys = ("input_cost", "output_cost", "cache_creation_cost", "cache_read_cost", "total_cost")
        # In-code baseline (seam off).
        mm.use_price_book_store(enabled=False)
        incode = {
            (m, s): compute_cost(_TOKENS, m, provider=p, speed=s)
            for m, p, s in _CASES
        }
        # Active book via primed cache.
        self._prime_from(backfilled_store)
        for m, p, s in _CASES:
            got = compute_cost(_TOKENS, m, provider=p, speed=s)
            for k in keys:
                assert got[k] == pytest.approx(incode[(m, s)][k], abs=1e-12), (
                    f"{m}/{s} {k} differs between primed-cache book and in-code"
                )

    def test_at_ts_effective_dating_matches_incode_on_primed_cache(self, backfilled_store):
        """A historical ``at_ts`` prices identically on the primed-cache book and
        in-code — the manifest's dated rows survive into the cache."""
        at = "2025-06-01T00:00:00Z"
        mm.use_price_book_store(enabled=False)
        incode = compute_cost(_TOKENS, "claude-opus-4-20250514", provider="anthropic", at_ts=at)
        self._prime_from(backfilled_store)
        book = compute_cost(_TOKENS, "claude-opus-4-20250514", provider="anthropic", at_ts=at)
        assert book["total_cost"] == pytest.approx(incode["total_cost"], abs=1e-12)


class TestNoPerCallDbIO:
    """The hard performance contract: a primed book serves lookups from memory,
    so ``compute_cost`` opens ZERO sqlite connections per call."""

    def test_compute_cost_opens_no_connection_when_cache_primed(
        self, backfilled_store, monkeypatch
    ):
        # Prime from the store, then forbid any further connection.
        conn = sqlite3.connect(backfilled_store)
        try:
            mm.use_price_book_store(backfilled_store, enabled=True)
            mm.prime_price_book_cache(conn)
        finally:
            conn.close()

        # Warm the (network/JSON-backed, NOT sqlite) overlay cache BEFORE we
        # start counting, so the assertion isolates the book lookup's I/O.
        compute_cost(_TOKENS, "claude-opus-4-8", provider="anthropic")

        calls = {"n": 0}
        real_connect = sqlite3.connect

        def _counting_connect(*a, **k):
            calls["n"] += 1
            return real_connect(*a, **k)

        monkeypatch.setattr(sqlite3, "connect", _counting_connect)
        # A spread of priced calls — every one must hit the in-memory cache.
        for m, p, s in _CASES:
            compute_cost(_TOKENS, m, provider=p, speed=s)
        assert calls["n"] == 0, f"compute_cost opened {calls['n']} DB connection(s) per-call"

    def test_lookups_still_correct_when_connect_is_poisoned(
        self, backfilled_store, monkeypatch
    ):
        """Belt-and-braces: with the cache primed, a ``sqlite3.connect`` that
        raises must not affect pricing — proving lookups never touch the DB."""
        conn = sqlite3.connect(backfilled_store)
        try:
            mm.use_price_book_store(backfilled_store, enabled=True)
            mm.prime_price_book_cache(conn)
        finally:
            conn.close()

        # Warm the overlay cache before poisoning ``connect`` (the overlay path
        # is JSON/network, not sqlite, but warming keeps the test honest).
        compute_cost(_TOKENS, "claude-opus-4-8", provider="anthropic")

        def _boom(*a, **k):
            raise AssertionError("compute_cost must not open a DB connection")

        monkeypatch.setattr(sqlite3, "connect", _boom)
        got = compute_cost(_TOKENS, "claude-opus-4-8", provider="anthropic")["total_cost"]
        assert got > 0.0


# ── live refresh: cache reflects an appended snapshot without a restart ────────


class TestLiveSnapshotRefreshesCache:
    def test_refresh_after_live_append_changes_priced_rate(self, tmp_path):
        """Appending a ``live`` snapshot + ``refresh_price_book_cache`` makes the
        new rate visible to ``compute_cost`` — no process restart, no per-call
        DB read. Uses an unrecognised id so no JSON overlay shadows the book."""
        store_path = tmp_path / "store.db"
        conn = db.connect(store_path)
        schema.apply(conn)
        conn.close()

        mm.use_price_book_store(store_path, enabled=True)
        mm.prime_price_book_cache()  # empty book → in-code fallback for the id
        model = "claude-not-a-real-id-zzz"
        before = compute_cost(_TOKENS, model, provider="anthropic")["total_cost"]

        # Append a distinguishable live rate for the concrete id and refresh.
        conn = sqlite3.connect(store_path)
        conn.row_factory = sqlite3.Row
        mm.append_live_snapshot(
            conn,
            [{"provider": "anthropic", "model": model,
              "input": 999.0, "output": 999.0, "cache_write": 999.0, "cache_read": 999.0}],
        )
        conn.commit()
        conn.close()
        mm.refresh_price_book_cache()

        after = compute_cost(_TOKENS, model, provider="anthropic")["total_cost"]
        assert after > before  # the live row now prices the id


# ── safe-by-default: no store wired ⇒ untouched in-code path ───────────────────


class TestFallbackWhenBookUnavailable:
    def test_disabled_seam_prices_from_incode(self, backfilled_store):
        """With the seam off, a wired store is irrelevant — pricing is in-code."""
        mm.use_price_book_store(enabled=False)
        got = compute_cost(_TOKENS, "claude-opus-4-8", provider="anthropic")["total_cost"]
        assert got > 0.0

    def test_missing_store_file_falls_back(self, tmp_path):
        mm.use_price_book_store(tmp_path / "does-not-exist.db", enabled=True)
        # Must not raise and must still price (via the in-code fallback).
        got = compute_cost(_TOKENS, "claude-opus-4-8", provider="anthropic")["total_cost"]
        assert got > 0.0

    def test_empty_book_falls_back_to_incode(self, tmp_path):
        """A migrated-but-unfilled store (fresh install) prices identically to
        in-code — the book miss returns None and falls through."""
        conn = db.connect(tmp_path / "store.db")
        schema.apply(conn)  # price_book exists but is EMPTY (no backfill)
        conn.close()
        mm.use_price_book_store(enabled=False)
        incode = compute_cost(_TOKENS, "claude-opus-4-8", provider="anthropic")["total_cost"]
        mm.use_price_book_store(tmp_path / "store.db", enabled=True)
        book = compute_cost(_TOKENS, "claude-opus-4-8", provider="anthropic")["total_cost"]
        assert book == pytest.approx(incode)


# ── lookup precedence + effective-dating (connection-level) ───────────────────


class TestLookupPrecedenceAndDating:
    def _store(self, tmp_path):
        conn = db.connect(tmp_path / "store.db")
        schema.apply(conn)
        return conn

    def test_live_beats_rate_card_beats_manifest(self, tmp_path):
        conn = self._store(tmp_path)
        # Same concrete id under all three sources with distinguishable input rates.
        for src, inp in (("manifest", 1.0), ("rate_card", 2.0), ("live", 3.0)):
            conn.execute(
                "INSERT INTO price_book (provider, model, source, input, output, cache_write, cache_read) "
                "VALUES ('anthropic', 'claude-opus-4-8', ?, ?, 0, 0, 0)",
                (src, inp),
            )
        conn.commit()
        # live wins.
        assert mm.price_book_lookup(conn, "claude-opus-4-8", "anthropic")[0] == 3.0
        conn.execute("DELETE FROM price_book WHERE source = 'live'")
        # then rate_card.
        assert mm.price_book_lookup(conn, "claude-opus-4-8", "anthropic")[0] == 2.0
        conn.close()

    def test_manifest_family_resolves_when_no_concrete_row(self, tmp_path):
        """An id with no concrete (live/rate_card) row resolves via its
        canonical family's manifest row."""
        conn = self._store(tmp_path)
        # Only a manifest row keyed by the family, no concrete id row.
        conn.execute(
            "INSERT INTO price_book (provider, model, source, input, output, cache_write, cache_read) "
            "VALUES ('anthropic', 'OPUS_48', 'manifest', 5, 25, 6.25, 0.5)"
        )
        conn.commit()
        rates = mm.price_book_lookup(conn, "claude-opus-4-8", "anthropic")
        assert rates == (5.0, 25.0, 6.25, 0.5)
        conn.close()

    def test_effective_dating_picks_window_by_at_ts(self, tmp_path):
        conn = self._store(tmp_path)
        # Two dated rate_card rows: $15 until 2026-01-15, $5 after.
        conn.execute(
            "INSERT INTO price_book (provider, model, source, effective_from, effective_until, "
            " input, output, cache_write, cache_read) "
            "VALUES ('anthropic', 'claude-x', 'rate_card', '', '2026-01-15', 15, 75, 18.75, 1.5)"
        )
        conn.execute(
            "INSERT INTO price_book (provider, model, source, effective_from, effective_until, "
            " input, output, cache_write, cache_read) "
            "VALUES ('anthropic', 'claude-x', 'rate_card', '2026-01-15', '', 5, 25, 6.25, 0.5)"
        )
        conn.commit()
        assert mm.price_book_lookup(conn, "claude-x", "anthropic", at_ts="2026-01-01")[0] == 15.0
        assert mm.price_book_lookup(conn, "claude-x", "anthropic", at_ts="2026-02-01T08:00:00Z")[0] == 5.0
        # No at_ts → the open-ended (current) row.
        assert mm.price_book_lookup(conn, "claude-x", "anthropic")[0] == 5.0
        conn.close()

    def test_lookup_miss_returns_none(self, tmp_path):
        conn = self._store(tmp_path)
        # Empty book, and a provider/model with no manifest family either.
        assert mm.price_book_lookup(conn, "totally-unknown", "openai") is None
        assert mm.price_book_lookup(conn, "", "anthropic") is None
        conn.close()


# ── backfill + live append ────────────────────────────────────────────────────


class TestBackfillAndLiveAppend:
    def test_backfill_populates_manifest_and_rate_card(self, tmp_path):
        conn = db.connect(tmp_path / "store.db")
        schema.apply(conn)
        n = backfill_price_book(conn)
        conn.commit()
        assert n > 0
        by_source = dict(
            conn.execute("SELECT source, COUNT(*) FROM price_book GROUP BY source").fetchall()
        )
        assert by_source.get("manifest", 0) > 0
        assert by_source.get("rate_card", 0) > 0
        # Every manifest family row matches the in-code manifest rate exactly.
        for row in conn.execute(
            "SELECT model, input, output, cache_write, cache_read FROM price_book WHERE source='manifest'"
        ):
            rates = mm.rates_for(row["model"], "anthropic")
            assert rates == (row["input"], row["output"], row["cache_write"], row["cache_read"])
        conn.close()

    def test_backfill_is_idempotent(self, tmp_path):
        conn = db.connect(tmp_path / "store.db")
        schema.apply(conn)
        backfill_price_book(conn)
        conn.commit()
        first = conn.execute("SELECT COUNT(*) FROM price_book").fetchone()[0]
        backfill_price_book(conn)  # re-run
        conn.commit()
        second = conn.execute("SELECT COUNT(*) FROM price_book").fetchone()[0]
        assert first == second  # UPSERT, no duplicates
        conn.close()

    def test_append_live_snapshot_stamps_today_and_source(self, tmp_path):
        conn = db.connect(tmp_path / "store.db")
        schema.apply(conn)
        mm.append_live_snapshot(
            conn,
            [{"provider": "anthropic", "model": "claude-opus-4-8",
              "input": 5.0, "output": 25.0, "cache_write": 6.25, "cache_read": 0.5}],
        )
        conn.commit()
        row = conn.execute(
            "SELECT source, effective_from, input FROM price_book WHERE model='claude-opus-4-8'"
        ).fetchone()
        assert row["source"] == "live"
        assert row["effective_from"] != ""  # stamped with a date
        assert row["input"] == 5.0
        conn.close()

    def test_pricing_service_save_appends_live_rows(self, tmp_path, monkeypatch):
        """``PricingService._save_to_cache`` mirrors the overlay into the book
        as ``source='live'`` rows (per-token → $/M scaling)."""
        from tests.conftest import set_home_env

        set_home_env(monkeypatch, tmp_path / "home")
        # Build the store at the home-relative path the service writes to.
        store_path = tmp_path / "home" / ".stackunderflow" / "store.db"
        conn = db.connect(store_path)
        schema.apply(conn)
        conn.close()

        from stackunderflow.services.pricing_service import PricingService

        svc = PricingService()
        svc._save_to_cache(
            {"claude-opus-4-8": {
                "input_cost_per_token": 5e-6, "output_cost_per_token": 2.5e-5,
                "cache_creation_cost_per_token": 6.25e-6, "cache_read_cost_per_token": 5e-7,
            }}
        )
        conn = sqlite3.connect(store_path)
        conn.row_factory = sqlite3.Row
        row = conn.execute(
            "SELECT source, input, output FROM price_book WHERE model='claude-opus-4-8' AND source='live'"
        ).fetchone()
        assert row is not None
        assert row["input"] == pytest.approx(5.0)   # 5e-6 * 1e6
        assert row["output"] == pytest.approx(25.0)
        conn.close()

    def test_pricing_service_save_without_store_is_noop(self, tmp_path, monkeypatch):
        """No store file ⇒ the live-append is a silent no-op (refresh still works)."""
        from tests.conftest import set_home_env

        set_home_env(monkeypatch, tmp_path / "home")
        from stackunderflow.services.pricing_service import PricingService

        # Must not raise even though no store.db exists.
        PricingService()._save_to_cache(
            {"claude-opus-4-8": {"input_cost_per_token": 5e-6, "output_cost_per_token": 2.5e-5}}
        )
