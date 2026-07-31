//! The cost engine a request prices with — and the price-book seam.
//!
//! `server.py`'s lifespan does three things to pricing before it serves a byte:
//!
//! ```python
//! backfill_price_book(conn)                       # idempotent UPSERT, manifest -> table
//! _mm.use_price_book_store(deps.store_path, True) # flip the seam to the store
//! _mm.prime_price_book_cache(conn)                # load the table into memory
//! ```
//!
//! So a *running server* prices from the `price_book` table while the CLI
//! prices from the in-code manifest. That seam is the wave-3 ledger's open item
//! (RS-3-082: "primed-vs-unprimed book seam resurrects $0s — pin both sides"),
//! and an endpoint differ that ignored it would be comparing two different rate
//! sources and calling the agreement luck.
//!
//! This module pins the Rust side to the same source: read `price_book`, hand it
//! to [`PricingEngine::with_price_book`], fall through to the manifest when the
//! table is absent or empty (a fresh store — exactly what an unprimed cache
//! does). The one thing it does **not** do is `backfill_price_book`: the port
//! never writes rates, it only reads whatever the reference wrote.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rusqlite::Connection;
use stax_etl::pricing::costs::PricingEngine;
use stax_etl::pricing::price_book::{PriceBook, PriceBookRow};

/// `stackunderflow/data/models.toml` relative to the package dir.
///
/// Injected rather than discovered: `AppState` is handed the package directory
/// and everything under it is derived, so a test can point at a fixture tree
/// without an environment variable (the pure-injection law).
#[must_use]
pub fn manifest_path(package_dir: &Path) -> PathBuf {
    package_dir.join("data").join("models.toml")
}

/// Build the engine a request prices with.
///
/// # Errors
/// When `models.toml` is missing or unparseable — the same hard failure the
/// Python import would raise, rather than a silently free price list.
pub fn engine(conn: &Connection, package_dir: &Path) -> Result<PricingEngine> {
    let path = manifest_path(package_dir);
    let engine = PricingEngine::from_manifest_path(&path)
        .map_err(|err| anyhow::anyhow!("{err}"))
        .with_context(|| format!("loading {}", path.display()))?;
    let book = load_price_book(conn)?;
    Ok(if book.is_empty() {
        engine
    } else {
        engine.with_price_book(book)
    })
}

/// `_build_book_cache` — `SELECT … FROM price_book ORDER BY effective_from`.
///
/// A store with no `price_book` table (pre-v0xx, or a fixture) is not an error:
/// it is an empty book, which is what an unprimed cache behaves like.
///
/// # Errors
/// Any SQLite error other than "no such table".
pub fn load_price_book(conn: &Connection) -> Result<PriceBook> {
    let exists: bool = conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name='price_book'",
            [],
            |row| row.get::<_, i64>(0),
        )
        .optional_row()?
        .is_some();
    if !exists {
        return Ok(PriceBook::default());
    }
    let mut stmt = conn.prepare(
        "SELECT provider, model, effective_from, effective_until, \
                input, output, cache_write, cache_read, source \
         FROM price_book ORDER BY effective_from",
    )?;
    let rows = stmt
        .query_map([], |row| {
            Ok(PriceBookRow {
                provider: row.get(0)?,
                model: row.get(1)?,
                effective_from: row.get(2)?,
                effective_until: row.get(3)?,
                input: row.get(4)?,
                output: row.get(5)?,
                cache_write: row.get(6)?,
                cache_read: row.get(7)?,
                source: row.get(8)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(PriceBook::from_rows(rows))
}

/// `query_row` with "no rows" folded into `None` instead of an error.
trait OptionalRow<T> {
    fn optional_row(self) -> rusqlite::Result<Option<T>>;
}

impl<T> OptionalRow<T> for rusqlite::Result<T> {
    fn optional_row(self) -> rusqlite::Result<Option<T>> {
        match self {
            Ok(value) => Ok(Some(value)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(err) => Err(err),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_store_without_the_table_yields_an_empty_book() {
        let conn = Connection::open_in_memory().expect("in-memory");
        let book = load_price_book(&conn).expect("no table is not an error");
        assert!(book.is_empty());
    }

    #[test]
    fn rows_round_trip_out_of_the_table() {
        let conn = Connection::open_in_memory().expect("in-memory");
        conn.execute_batch(
            "CREATE TABLE price_book (
                 id INTEGER PRIMARY KEY,
                 provider TEXT NOT NULL, model TEXT NOT NULL,
                 effective_from TEXT NOT NULL DEFAULT '',
                 effective_until TEXT NOT NULL DEFAULT '',
                 input REAL NOT NULL DEFAULT 0.0, output REAL NOT NULL DEFAULT 0.0,
                 cache_write REAL NOT NULL DEFAULT 0.0, cache_read REAL NOT NULL DEFAULT 0.0,
                 source TEXT NOT NULL DEFAULT 'manifest',
                 updated_at REAL NOT NULL DEFAULT 0.0);
             INSERT INTO price_book (provider, model, input, output, cache_write, cache_read, source)
             VALUES ('anthropic', 'OPUS', 15.0, 75.0, 18.75, 1.5, 'manifest');",
        )
        .expect("schema");
        let book = load_price_book(&conn).expect("loads");
        assert_eq!(book.len(), 1);
    }

    #[test]
    fn the_manifest_lives_under_the_package_dir() {
        let package = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../stackunderflow");
        let path = manifest_path(&package);
        assert!(path.is_file(), "missing {}", path.display());
    }
}
