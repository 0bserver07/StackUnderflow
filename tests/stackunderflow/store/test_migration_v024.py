"""v024 migration: the unified effective-dated ``price_book`` table.

Audit #2 — pricing lived in three places (RATE_CARD dict, the data manifest,
the LiteLLM overlay). v024 adds the single persistent home they back-fill into.
The migration is **additive** (no existing table touched); these tests pin its
structural guarantees:

  1. The ``price_book`` table exists with the declared columns / types / defaults.
  2. ``schema.apply`` bumps ``PRAGMA user_version`` to the current head (>= 24).
  3. The UNIQUE (provider, model, effective_from, source) key rejects a dup.
  4. INSERTs that omit the dated/source columns land the DEFAULTs ('' / 'manifest').
  5. The loader is reentrant — running it on a DB where the table already
     exists (manual create / partial prior run) is a no-op and still bumps
     ``user_version`` via the ``_ADD_COLUMN_GUARDS`` entry.
  6. Idempotent re-apply does not duplicate the table.
"""

from __future__ import annotations

import sqlite3
from pathlib import Path

from stackunderflow.store import db, schema


def _column_info(conn, table: str, column: str) -> dict | None:
    for r in conn.execute(f"PRAGMA table_info({table})").fetchall():
        if r["name"] == column:
            return {"type": r["type"], "notnull": r["notnull"], "dflt_value": r["dflt_value"]}
    return None


def _table_exists(conn, name: str) -> bool:
    return (
        conn.execute(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name=?", (name,)
        ).fetchone()
        is not None
    )


_REAL_COLS = ("input", "output", "cache_write", "cache_read", "updated_at")
_TEXT_COLS = ("provider", "model", "effective_from", "effective_until", "source")


def test_v024_creates_price_book_table(tmp_path: Path) -> None:
    conn = db.connect(tmp_path / "store.db")
    try:
        schema.apply(conn)
        assert _table_exists(conn, "price_book")
        for col in _REAL_COLS:
            info = _column_info(conn, "price_book", col)
            assert info is not None, f"price_book.{col} missing"
            assert info["type"].upper() == "REAL"
            assert info["notnull"] == 1
        for col in _TEXT_COLS:
            info = _column_info(conn, "price_book", col)
            assert info is not None, f"price_book.{col} missing"
            assert info["type"].upper() == "TEXT"
            assert info["notnull"] == 1
    finally:
        conn.close()


def test_v024_user_version_bumped(tmp_path: Path) -> None:
    conn = db.connect(tmp_path / "store.db")
    try:
        schema.apply(conn)
        assert conn.execute("PRAGMA user_version").fetchone()[0] == schema.CURRENT_VERSION
        assert schema.CURRENT_VERSION >= 24
    finally:
        conn.close()


def test_v024_unique_key_rejects_duplicate(tmp_path: Path) -> None:
    conn = db.connect(tmp_path / "store.db")
    try:
        schema.apply(conn)
        conn.execute(
            "INSERT INTO price_book (provider, model, effective_from, source, "
            " input, output, cache_write, cache_read) "
            "VALUES ('anthropic', 'OPUS_48', '', 'manifest', 5, 25, 6.25, 0.5)"
        )
        try:
            conn.execute(
                "INSERT INTO price_book (provider, model, effective_from, source, "
                " input, output, cache_write, cache_read) "
                "VALUES ('anthropic', 'OPUS_48', '', 'manifest', 9, 9, 9, 9)"
            )
            raise AssertionError("expected UNIQUE violation on (provider, model, effective_from, source)")
        except sqlite3.IntegrityError:
            pass
        # Same model, different source IS allowed (live overlays a rate_card row).
        conn.execute(
            "INSERT INTO price_book (provider, model, effective_from, source, "
            " input, output, cache_write, cache_read) "
            "VALUES ('anthropic', 'OPUS_48', '', 'live', 5, 25, 6.25, 0.5)"
        )
        n = conn.execute("SELECT COUNT(*) FROM price_book").fetchone()[0]
        assert n == 2
    finally:
        conn.close()


def test_v024_insert_without_dated_columns_defaults(tmp_path: Path) -> None:
    """Rows that omit the dated/source columns read '' / 'manifest' defaults."""
    conn = db.connect(tmp_path / "store.db")
    try:
        schema.apply(conn)
        conn.execute(
            "INSERT INTO price_book (provider, model, input, output, cache_write, cache_read) "
            "VALUES ('openai', 'gpt-5-codex', 1.25, 10.0, 0.0, 0.125)"
        )
        row = conn.execute(
            "SELECT effective_from, effective_until, source, updated_at FROM price_book"
        ).fetchone()
        assert row["effective_from"] == ""
        assert row["effective_until"] == ""
        assert row["source"] == "manifest"
        assert row["updated_at"] == 0.0
    finally:
        conn.close()


def test_v024_reentrant_when_table_already_exists(tmp_path: Path) -> None:
    """Partial-application recovery: table present but ``user_version`` behind."""
    conn = db.connect(tmp_path / "store.db")
    try:
        schema.apply(conn)  # full chain → price_book exists
        conn.execute("PRAGMA user_version = 23")  # rewind so v024 looks pending
        conn.commit()
        schema.apply(conn)  # must not raise "table already exists"
        assert conn.execute("PRAGMA user_version").fetchone()[0] == schema.CURRENT_VERSION
        assert _table_exists(conn, "price_book")
    finally:
        conn.close()


def test_v024_idempotent_reapply(tmp_path: Path) -> None:
    conn = db.connect(tmp_path / "store.db")
    try:
        schema.apply(conn)
        schema.apply(conn)  # second call must not raise
        assert conn.execute("PRAGMA user_version").fetchone()[0] == schema.CURRENT_VERSION
        n = conn.execute(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='price_book'"
        ).fetchone()[0]
        assert n == 1
    finally:
        conn.close()
