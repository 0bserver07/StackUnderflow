//! The pricing brain — port of `infra/costs.py`, `infra/model_manifest.py` and
//! `infra/providers/`.
//!
//! `data/models.toml` is read, never transcribed (spec §2.4): the same file that
//! prices the Python implementation prices this one, effective dates and all.
//! The three rate tables that are still *code* on the Python side (OpenAI's
//! non-manifest families, Gemini, Qwen, Cursor) are code here too, in
//! [`providers`].
//!
//! # Shape of the port
//!
//! | Python | here |
//! |---|---|
//! | `infra/model_manifest.py` (manifest half) | [`manifest`] |
//! | `infra/model_manifest.py` (`price_book` half) | [`price_book`] |
//! | `infra/providers/*.py` | [`providers`] |
//! | `infra/costs.py` | [`costs`] |
//! | `tomllib` | [`toml_lite`] |
//!
//! # Injection, not module state
//!
//! Python threads the manifest path, the alias map, the upstream pricing overlay
//! and the price-book wiring through module-level globals (`_MANIFEST_PATH`,
//! `Settings()`, `_overlay_cache`, `_use_store`). Rust 2024 makes `set_var`
//! `unsafe` and this workspace forbids `unsafe`, so all four are constructor
//! arguments on [`costs::PricingEngine`] instead (findings ledger #5). A default
//! engine — no aliases, no overlay, no book — reproduces the default state of a
//! freshly imported `stackunderflow` process exactly, which is the state every
//! Python unit test and every CLI run before a backfill sees.

pub mod costs;
pub mod manifest;
pub mod price_book;
pub mod providers;
pub mod toml_lite;

pub use costs::{PricingEngine, format_dollars, resolve_model_alias};
pub use manifest::{Manifest, Rates};
pub use price_book::{PriceBook, PriceBookRow};
pub use providers::{Pricer, get_pricer};

/// `1_000_000.0` — rates are quoted per million tokens.
pub const MILLION: f64 = 1_000_000.0;

/// Raw provider-shaped token counts, before `normalize_tokens`.
///
/// Modelled as an ordered key/value list rather than a struct because the OpenAI
/// pricer branches on key *presence* (`"input_tokens" in raw or
/// "cached_input_tokens" in raw`), which a struct of `Option`s would blur.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RawTokens {
    entries: Vec<(String, i64)>,
}

impl RawTokens {
    /// The canonical four-key Anthropic shape.
    #[must_use]
    pub fn canonical(input: i64, output: i64, cache_creation: i64, cache_read: i64) -> Self {
        Self {
            entries: vec![
                ("input".to_string(), input),
                ("output".to_string(), output),
                ("cache_creation".to_string(), cache_creation),
                ("cache_read".to_string(), cache_read),
            ],
        }
    }

    /// The raw OpenAI wire shape: cached nested inside input, reasoning bundled
    /// into output.
    #[must_use]
    pub fn openai_shape(
        input_tokens: i64,
        output_tokens: i64,
        cached_input_tokens: i64,
        reasoning_output_tokens: i64,
    ) -> Self {
        Self {
            entries: vec![
                ("input_tokens".to_string(), input_tokens),
                ("output_tokens".to_string(), output_tokens),
                ("cached_input_tokens".to_string(), cached_input_tokens),
                (
                    "reasoning_output_tokens".to_string(),
                    reasoning_output_tokens,
                ),
            ],
        }
    }

    /// An empty map — every `get` misses, which Python treats as zero.
    #[must_use]
    pub fn empty() -> Self {
        Self::default()
    }

    /// Set a key, replacing any existing value.
    pub fn set(&mut self, key: &str, value: i64) {
        match self.entries.iter_mut().find(|(k, _)| k == key) {
            Some(entry) => entry.1 = value,
            None => self.entries.push((key.to_string(), value)),
        }
    }

    /// The value under `key`, or `None` when absent.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<i64> {
        self.entries.iter().find(|(k, _)| k == key).map(|(_, v)| *v)
    }

    /// Whether `key` is present — the test OpenAI's `normalize_tokens` branches on.
    #[must_use]
    pub fn contains(&self, key: &str) -> bool {
        self.entries.iter().any(|(k, _)| k == key)
    }
}

/// Token counts in the canonical four-bucket shape `compute` prices.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Tokens {
    /// Fresh (uncached) input tokens.
    pub input: i64,
    /// Output tokens, reasoning folded in where the provider bundles it.
    pub output: i64,
    /// Cache-write (creation) tokens.
    pub cache_creation: i64,
    /// Cache-read tokens.
    pub cache_read: i64,
}

/// The per-bucket cost breakdown `compute_cost` returns.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct CostBreakdown {
    /// Dollars attributable to fresh input.
    pub input_cost: f64,
    /// Dollars attributable to output.
    pub output_cost: f64,
    /// Dollars attributable to cache writes.
    pub cache_creation_cost: f64,
    /// Dollars attributable to cache reads.
    pub cache_read_cost: f64,
    /// The sum, in the same order Python adds it.
    pub total_cost: f64,
}

/// `ProviderPricer._apply_overlay_rates` — tokens × an explicit rate tuple.
///
/// The arithmetic order is load-bearing for bit-exact parity: each bucket is
/// `count * rate / 1e6` (multiply first, then divide) and the total is summed
/// input → output → cache-creation → cache-read. Reassociating any of it moves
/// the last bits, which is exactly what the wave-3 cent-exact mart gate measures.
/// `None` rates mean "no rate available" and cost zero, not "price at zero rates".
#[must_use]
pub fn apply_rates(tokens: &Tokens, rates: Option<Rates>) -> CostBreakdown {
    let Some((inp_r, out_r, cw_r, cr_r)) = rates else {
        return CostBreakdown::default();
    };
    let ic = tokens.input as f64 * inp_r / MILLION;
    let oc = tokens.output as f64 * out_r / MILLION;
    let cc = tokens.cache_creation as f64 * cw_r / MILLION;
    let rc = tokens.cache_read as f64 * cr_r / MILLION;
    CostBreakdown {
        input_cost: ic,
        output_cost: oc,
        cache_creation_cost: cc,
        cache_read_cost: rc,
        total_cost: ic + oc + cc + rc,
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    //! Shared fixtures. Tests load the REAL `data/models.toml` so an assertion
    //! about pricing is an assertion about what this machine actually bills —
    //! a hand-written fixture would drift the moment the manifest is edited.

    use std::path::PathBuf;

    use super::manifest::Manifest;

    /// `…/StackUnderflow-rust/stackunderflow/data/models.toml`, derived from the
    /// crate directory so the test works from any cwd.
    pub fn manifest_path() -> PathBuf {
        let crate_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        crate_dir
            .ancestors()
            .nth(3)
            .expect("crates/stax-etl sits three levels below the worktree root")
            .join("stackunderflow")
            .join("data")
            .join("models.toml")
    }

    /// The real manifest, parsed.
    pub fn sample_manifest() -> Manifest {
        Manifest::load(&manifest_path()).expect("the checked-in models.toml parses")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_rates_cost_zero_not_nothing() {
        let tokens = Tokens {
            input: 10,
            output: 20,
            cache_creation: 30,
            cache_read: 40,
        };
        assert_eq!(apply_rates(&tokens, None), CostBreakdown::default());
    }

    #[test]
    fn buckets_multiply_before_dividing_and_sum_in_order() {
        let tokens = Tokens {
            input: 1_234_567,
            output: 98_765,
            cache_creation: 4_321,
            cache_read: 7_654_321,
        };
        let rates = (5.0, 25.0, 6.25, 0.50);
        let got = apply_rates(&tokens, Some(rates));
        let ic = 1_234_567.0_f64 * 5.0 / MILLION;
        let oc = 98_765.0_f64 * 25.0 / MILLION;
        let cc = 4_321.0_f64 * 6.25 / MILLION;
        let rc = 7_654_321.0_f64 * 0.50 / MILLION;
        assert_eq!(got.input_cost.to_bits(), ic.to_bits());
        assert_eq!(got.output_cost.to_bits(), oc.to_bits());
        assert_eq!(got.cache_creation_cost.to_bits(), cc.to_bits());
        assert_eq!(got.cache_read_cost.to_bits(), rc.to_bits());
        assert_eq!(got.total_cost.to_bits(), (ic + oc + cc + rc).to_bits());
    }

    #[test]
    fn raw_tokens_distinguish_absent_from_zero() {
        let raw = RawTokens::canonical(1, 2, 3, 4);
        assert_eq!(raw.get("input"), Some(1));
        assert!(!raw.contains("input_tokens"));
        let mut openai = RawTokens::empty();
        openai.set("cached_input_tokens", 0);
        assert!(openai.contains("cached_input_tokens"));
        assert_eq!(openai.get("input_tokens"), None);
    }
}
