//! The unified, store-backed price book — port of `model_manifest.py`'s
//! `price_book` half (migration v024).
//!
//! Python reaches the book through module-level state: `use_price_book_store()`
//! wires a path, `prime_price_book_cache()` reads the whole table into
//! `_book_cache`, and the connection-free `compute_cost` consults that cache.
//! This crate forbids `unsafe`, so `set_var`-shaped global wiring is out (findings
//! ledger #5) and the book is instead a value the caller owns and injects into
//! [`super::PricingEngine`]. Rows arrive as data; opening `store.db` is
//! `stax-core`'s job, not this module's.
//!
//! **Two date comparisons, deliberately different.** [`row_effective_at`]
//! truncates `at_ts` to its first ten characters before comparing, while the
//! in-code manifest's `select_price` compares the whole string. A row priced at
//! `"2026-04-26T00:00:00+00:00"` therefore lands in the *old* window here and the
//! *new* one there. Both are current behaviour and both are ported as they stand.
//!
//! **Ledger — §4f "undated rate_card outranks dated manifest" is CLOSED.** The
//! wave brief carries that edge as live behaviour to port bug-for-bug. It is not:
//! the CHANGELOG entry that flagged it was followed by "Price-book precedence
//! corrected to **live > dated manifest family > undated rate_card snapshot** in
//! both lookup paths, closing the flagged edge", and both
//! `model_manifest.price_book_lookup` and `store_price_book_lookup` implement the
//! corrected order today. This module ports the corrected order — which IS the
//! bug-for-bug port of the tree as it stands — and
//! [`tests::dated_manifest_rows_outrank_undated_rate_card_rows`] pins it so a
//! regression is loud.

use std::collections::HashMap;

use super::manifest::{Manifest, Rates};

/// `_SOURCE_MANIFEST` — the effective-dated family rows the manifest backfills.
pub const SOURCE_MANIFEST: &str = "manifest";
/// `_SOURCE_RATE_CARD` — undated current-rate snapshots keyed by concrete id.
pub const SOURCE_RATE_CARD: &str = "rate_card";
/// `_SOURCE_LIVE` — dated snapshots appended from the upstream pricing feed.
pub const SOURCE_LIVE: &str = "live";

/// One `price_book` row. `effective_from` / `effective_until` use the empty
/// string as the manifest's `None` sentinel, because the table's unique key is
/// NOT NULL.
#[derive(Debug, Clone, PartialEq)]
pub struct PriceBookRow {
    /// Pricer key the row is filed under.
    pub provider: String,
    /// Concrete model id (live / rate_card rows) or manifest family (manifest rows).
    pub model: String,
    /// Inclusive lower bound, `""` for open.
    pub effective_from: String,
    /// Exclusive upper bound, `""` for open.
    pub effective_until: String,
    /// Input rate, `$/M`.
    pub input: f64,
    /// Output rate, `$/M`.
    pub output: f64,
    /// Cache-write rate, `$/M`.
    pub cache_write: f64,
    /// Cache-read rate, `$/M`.
    pub cache_read: f64,
    /// One of [`SOURCE_LIVE`], [`SOURCE_RATE_CARD`], [`SOURCE_MANIFEST`].
    pub source: String,
}

/// The whole book, grouped exactly as `_build_book_cache` groups it.
#[derive(Debug, Clone, Default)]
pub struct PriceBook {
    grouped: HashMap<(String, String, String), Vec<PriceBookRow>>,
}

impl PriceBook {
    /// Group and sort `rows` the way the SQL path returns them.
    ///
    /// `_build_book_cache` issues `ORDER BY effective_from` over the whole table
    /// and appends in that order, so rows within a group keep their relative
    /// order for equal dates. A *stable* sort reproduces that; an unstable one
    /// would silently reorder the two same-date rows a live snapshot can create.
    #[must_use]
    pub fn from_rows(rows: Vec<PriceBookRow>) -> Self {
        let mut rows = rows;
        rows.sort_by(|a, b| a.effective_from.cmp(&b.effective_from));
        let mut grouped: HashMap<(String, String, String), Vec<PriceBookRow>> = HashMap::new();
        for row in rows {
            grouped
                .entry((row.provider.clone(), row.model.clone(), row.source.clone()))
                .or_default()
                .push(row);
        }
        Self { grouped }
    }

    /// Whether the book carries no rows (a fresh store — every lookup misses and
    /// the caller falls through to the in-code manifest).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.grouped.is_empty()
    }

    /// Number of rows held.
    #[must_use]
    pub fn len(&self) -> usize {
        self.grouped.values().map(Vec::len).sum()
    }

    fn rows(&self, provider: &str, model: &str, source: &str) -> &[PriceBookRow] {
        self.grouped
            .get(&(provider.to_string(), model.to_string(), source.to_string()))
            .map_or(&[][..], Vec::as_slice)
    }

    fn by_model_source(
        &self,
        provider: &str,
        model: &str,
        source: &str,
        at_ts: Option<&str>,
    ) -> Option<Rates> {
        let row = row_effective_at(self.rows(provider, model, source), at_ts)?;
        Some((row.input, row.output, row.cache_write, row.cache_read))
    }

    /// Resolve `(input, output, cache_write, cache_read)` in `$/M`, or `None` on
    /// a clean miss. Port of `store_price_book_lookup` / `price_book_lookup`.
    ///
    /// Precedence: `live` (by concrete id) > `manifest` (by canonical family,
    /// effective-DATED) > `rate_card` (by concrete id, undated). The middle tier
    /// is what stops an undated current-rate snapshot shadowing a dated
    /// historical correction.
    #[must_use]
    pub fn lookup(
        &self,
        manifest: &Manifest,
        model: &str,
        provider: &str,
        at_ts: Option<&str>,
    ) -> Option<Rates> {
        if model.is_empty() {
            return None;
        }
        if let Some(hit) = self.by_model_source(provider, model, SOURCE_LIVE, at_ts) {
            return Some(hit);
        }
        if let Some(family) = manifest.canonicalize(model, provider)
            && !family.is_empty()
            && let Some(hit) = self.by_model_source(provider, &family, SOURCE_MANIFEST, at_ts)
        {
            return Some(hit);
        }
        self.by_model_source(provider, model, SOURCE_RATE_CARD, at_ts)
    }
}

/// Pick the row effective at `at_ts`. Port of `_row_effective_at`.
///
/// With no `at_ts`, prefer the open-ended (`effective_until == ""`) row, else the
/// last. With an `at_ts`, compare only its `YYYY-MM-DD` prefix — taken by
/// CHARACTERS, as Python's `at_ts[:10]` is, not by bytes.
#[must_use]
pub fn row_effective_at<'a>(
    rows: &'a [PriceBookRow],
    at_ts: Option<&str>,
) -> Option<&'a PriceBookRow> {
    if rows.is_empty() {
        return None;
    }
    let Some(at_ts) = at_ts else {
        let current: Vec<&PriceBookRow> = rows
            .iter()
            .filter(|r| r.effective_until.is_empty())
            .collect();
        return if current.is_empty() {
            rows.last()
        } else {
            current.last().copied()
        };
    };
    let day: String = at_ts.chars().take(10).collect();
    for row in rows {
        let from_ok = row.effective_from.is_empty() || day.as_str() >= row.effective_from.as_str();
        let until_ok =
            row.effective_until.is_empty() || day.as_str() < row.effective_until.as_str();
        if from_ok && until_ok {
            return Some(row);
        }
    }
    rows.last()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pricing::manifest::manifest_price_book_rows;
    use crate::pricing::test_support::sample_manifest;

    fn row(model: &str, from: &str, until: &str, output: f64, source: &str) -> PriceBookRow {
        PriceBookRow {
            provider: "openai".to_string(),
            model: model.to_string(),
            effective_from: from.to_string(),
            effective_until: until.to_string(),
            input: 2.5,
            output,
            cache_write: 0.0,
            cache_read: 0.25,
            source: source.to_string(),
        }
    }

    #[test]
    fn dated_manifest_rows_outrank_undated_rate_card_rows() {
        // The exact shape §4f flagged: a `rate_card` snapshot at the CURRENT rate
        // sitting next to the two dated manifest rows for the same model.
        let manifest = sample_manifest();
        let book = PriceBook::from_rows(vec![
            row("GPT_54", "", "2026-04-26", 20.0, SOURCE_MANIFEST),
            row("GPT_54", "2026-04-26", "", 15.0, SOURCE_MANIFEST),
            row("gpt-5.4", "", "", 15.0, SOURCE_RATE_CARD),
        ]);
        // Pre-boundary lookup must find the $20 era, not the undated $15 snapshot.
        assert_eq!(
            book.lookup(&manifest, "gpt-5.4", "openai", Some("2026-01-15"))
                .map(|r| r.1),
            Some(20.0)
        );
        // Post-boundary and undated both resolve to the current era.
        assert_eq!(
            book.lookup(&manifest, "gpt-5.4", "openai", Some("2026-06-01"))
                .map(|r| r.1),
            Some(15.0)
        );
        assert_eq!(
            book.lookup(&manifest, "gpt-5.4", "openai", None)
                .map(|r| r.1),
            Some(15.0)
        );
    }

    #[test]
    fn live_rows_outrank_everything() {
        let manifest = sample_manifest();
        let book = PriceBook::from_rows(vec![
            row("GPT_54", "", "2026-04-26", 20.0, SOURCE_MANIFEST),
            row("gpt-5.4", "2026-01-01", "", 99.0, SOURCE_LIVE),
        ]);
        assert_eq!(
            book.lookup(&manifest, "gpt-5.4", "openai", Some("2026-01-15"))
                .map(|r| r.1),
            Some(99.0)
        );
    }

    #[test]
    fn a_miss_is_none_so_the_caller_falls_back_to_the_in_code_manifest() {
        let manifest = sample_manifest();
        let book = PriceBook::from_rows(vec![row("GPT_54", "", "", 15.0, SOURCE_MANIFEST)]);
        assert_eq!(book.lookup(&manifest, "gpt-4o", "openai", None), None);
        assert_eq!(book.lookup(&manifest, "", "openai", None), None);
        assert!(PriceBook::default().is_empty());
        assert_eq!(
            PriceBook::default().lookup(&manifest, "gpt-5.4", "openai", None),
            None
        );
    }

    #[test]
    fn the_book_truncates_at_ts_to_a_day_where_the_manifest_does_not() {
        let manifest = sample_manifest();
        let book = PriceBook::from_rows(vec![
            row("GPT_54", "", "2026-04-26", 20.0, SOURCE_MANIFEST),
            row("GPT_54", "2026-04-26", "", 15.0, SOURCE_MANIFEST),
        ]);
        // Book: "2026-04-25T23:59:59+00:00"[:10] == "2026-04-25" < "2026-04-26".
        assert_eq!(
            book.lookup(
                &manifest,
                "gpt-5.4",
                "openai",
                Some("2026-04-25T23:59:59+00:00")
            )
            .map(|r| r.1),
            Some(20.0)
        );
        // Both agree a day before the boundary.
        let gpt = manifest
            .models()
            .iter()
            .find(|m| m.family == "GPT_54")
            .expect("GPT_54 in the real manifest");
        assert_eq!(
            Manifest::select_price(&gpt.price, Some("2026-04-25T23:59:59+00:00")).map(|p| p.output),
            Some(20.0)
        );
        // …and disagree on the boundary day itself: the book truncates to
        // "2026-04-26" (>= from, so the new row) — same answer here, but by a
        // different comparison. The divergence bites only for a row whose bound
        // is a full timestamp, which the manifest never writes.
        assert_eq!(
            book.lookup(
                &manifest,
                "gpt-5.4",
                "openai",
                Some("2026-04-26T00:00:00+00:00")
            )
            .map(|r| r.1),
            Some(15.0)
        );
    }

    #[test]
    fn manifest_backfill_rows_round_trip_through_the_book() {
        let manifest = sample_manifest();
        let rows = manifest_price_book_rows(&manifest);
        assert!(!rows.is_empty());
        assert!(rows.iter().all(|r| r.source == SOURCE_MANIFEST));
        let book = PriceBook::from_rows(rows);
        assert_eq!(
            book.len(),
            manifest
                .models()
                .iter()
                .map(|m| m.price.len())
                .sum::<usize>()
        );
        // A book primed straight from the manifest prices identically to it.
        for model in manifest.models() {
            let Some(provider) = model.provider.as_deref() else {
                continue;
            };
            let expected = manifest.rates_for(Some(&model.family), provider, None);
            let got = book.by_model_source(provider, &model.family, SOURCE_MANIFEST, None);
            assert_eq!(got, expected, "family {}", model.family);
        }
    }

    #[test]
    fn no_at_ts_prefers_the_open_ended_row_then_the_last() {
        let rows = vec![
            row("X", "", "2026-01-01", 1.0, SOURCE_MANIFEST),
            row("X", "2026-01-01", "", 2.0, SOURCE_MANIFEST),
        ];
        assert_eq!(row_effective_at(&rows, None).map(|r| r.output), Some(2.0));
        // Every row closed → the last one wins anyway.
        let closed = vec![
            row("X", "", "2026-01-01", 1.0, SOURCE_MANIFEST),
            row("X", "2026-01-01", "2026-02-01", 2.0, SOURCE_MANIFEST),
        ];
        assert_eq!(row_effective_at(&closed, None).map(|r| r.output), Some(2.0));
        // An at_ts past every window → the last row, not None.
        assert_eq!(
            row_effective_at(&closed, Some("2030-01-01")).map(|r| r.output),
            Some(2.0)
        );
        assert_eq!(row_effective_at(&[], None), None);
    }
}
