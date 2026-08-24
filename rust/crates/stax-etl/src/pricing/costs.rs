//! The public cost API — port of `python-legacy: infra/costs.py`.
//!
//! `compute_cost` is the whole surface every other module calls. Its precedence
//! chain, in order, is:
//!
//! 1. the user alias map, so a proxy-rewritten id resolves to something the rate
//!    tables know;
//! 2. the upstream pricing overlay, if one is loaded — a single *current*
//!    snapshot with no history, which is why `at_ts` deliberately does not apply
//!    to it;
//! 3. the store-backed [`super::price_book`], if one is wired — same precedence
//!    slot the manifest occupies, so a fresh store prices identically to today;
//! 4. the provider's pricer, which is where the manifest, the effective dating
//!    and the priority/fast multiplier live.
//!
//! The July provider-resolution work (`8a83ccb`) sits alongside it in
//! [`PricingEngine::vendor_for_model`] and
//! [`PricingEngine::resolve_pricing_provider`]. The distinction between them is
//! the load-bearing part: `vendor_for_model` answers "did anything actually
//! match?" and may say `None`; `provider_for_model` answers "who should I ask?"
//! and always names someone. Collapsing the two would turn
//! `deepseek-v4-flash-free` matching nothing into `deepseek-v4-flash-free` being
//! an Anthropic model.

use std::collections::HashMap;
use std::path::Path;

use super::manifest::{Manifest, ManifestError, Rates};
use super::price_book::{PriceBook, PriceBookRow, SOURCE_RATE_CARD};
use super::providers::{Pricer, get_pricer, hint_routing};
use super::{CostBreakdown, MILLION, RawTokens, apply_rates};

/// Per-token dollar rates, the shape `get_model_pricing` returns.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ModelPricing {
    /// `$`/token for fresh input.
    pub input_cost_per_token: f64,
    /// `$`/token for output.
    pub output_cost_per_token: f64,
    /// `$`/token for cache writes.
    pub cache_creation_cost_per_token: f64,
    /// `$`/token for cache reads.
    pub cache_read_cost_per_token: f64,
}

/// Map `model_id` through a user-provided alias table.
///
/// Port of `resolve_model_alias`: single-step lookup only, so a self-alias
/// terminates trivially and a misconfigured chain `a → b → c` returns `b` rather
/// than iterating.
#[must_use]
pub fn resolve_model_alias(model_id: &str, aliases: &HashMap<String, String>) -> String {
    aliases
        .get(model_id)
        .cloned()
        .unwrap_or_else(|| model_id.to_string())
}

/// A configured pricing engine: manifest plus the four injected seams.
///
/// Constructing one with [`PricingEngine::from_manifest_path`] and nothing else
/// reproduces the default state of a freshly imported `stackunderflow`: no
/// aliases configured, the overlay empty, the price-book seam disabled.
#[derive(Debug, Clone)]
pub struct PricingEngine {
    manifest: Manifest,
    aliases: HashMap<String, String>,
    overlay: HashMap<String, Rates>,
    book: Option<PriceBook>,
    exact_id_routing: HashMap<String, String>,
}

impl PricingEngine {
    /// Load the manifest at `path` and build a default engine.
    ///
    /// # Errors
    /// When the manifest cannot be read or parsed.
    pub fn from_manifest_path(path: &Path) -> Result<Self, ManifestError> {
        Ok(Self::from_manifest(Manifest::load(path)?))
    }

    /// Build an engine around an already-parsed manifest.
    #[must_use]
    pub fn from_manifest(manifest: Manifest) -> Self {
        // `costs._exact_id_routing`: id → pricer key, where each `[canonical_ids]`
        // GROUP NAME is the pricer key. A dict comprehension, so a duplicated id
        // resolves to the last group that lists it.
        let mut exact_id_routing = HashMap::new();
        for (pricer, ids) in manifest.canonical_id_groups() {
            for id in ids {
                exact_id_routing.insert(id.clone(), pricer.clone());
            }
        }
        Self {
            manifest,
            aliases: HashMap::new(),
            overlay: HashMap::new(),
            book: None,
            exact_id_routing,
        }
    }

    /// Attach a user alias map (`settings.model_aliases`).
    #[must_use]
    pub fn with_aliases(mut self, aliases: HashMap<String, String>) -> Self {
        self.aliases = aliases;
        self
    }

    /// Attach an upstream pricing overlay, keyed by concrete model id, in `$/M`.
    #[must_use]
    pub fn with_overlay(mut self, overlay: HashMap<String, Rates>) -> Self {
        self.overlay = overlay;
        self
    }

    /// Wire the store-backed price book. Absent = the seam is disabled, which is
    /// the import-time default on the Python side.
    #[must_use]
    pub fn with_price_book(mut self, book: PriceBook) -> Self {
        self.book = Some(book);
        self
    }

    /// The manifest this engine prices from.
    #[must_use]
    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }

    /// Return the cost breakdown for `tokens`. Port of `compute_cost`.
    ///
    /// `speed` threads Anthropic's priority/fast tier through to the pricer; only
    /// the Anthropic pricer interprets it, and only for families the manifest
    /// gives a `fast_multiplier`. `at_ts` prices the event at the manifest rate in
    /// effect then, and applies to manifest-priced models only — the overlay is a
    /// single current snapshot with no history.
    #[must_use]
    pub fn compute_cost(
        &self,
        tokens: &RawTokens,
        model: &str,
        provider: &str,
        speed: &str,
        at_ts: Option<&str>,
    ) -> CostBreakdown {
        let model = resolve_model_alias(model, &self.aliases);
        let pricer = get_pricer(provider);
        let normalized = pricer.normalize_tokens(tokens);

        if let Some(overlay) = self.overlay.get(&model) {
            // The overlay has no effective-dated history, so `at_ts` is ignored
            // here on purpose.
            return apply_rates(&normalized, Some(*overlay));
        }

        if let Some(book) = &self.book {
            // NOTE: the book is keyed by the PRICER-side provider — the key the
            // manifest backfilled the rows under — not by the `provider` argument.
            // `costs._price_book_rates` takes a `provider` parameter and then
            // ignores it in favour of `_provider_for_model(model)`; that is
            // reproduced, dead parameter and all.
            let pkey = self.provider_for_model(&model);
            if let Some(rates) = book.lookup(&self.manifest, &model, pkey, at_ts) {
                let rates = if speed == "fast" {
                    self.apply_fast_multiplier(rates, &model)
                } else {
                    rates
                };
                return apply_rates(&normalized, Some(rates));
            }
        }

        pricer.compute(&self.manifest, &normalized, &model, speed, at_ts)
    }

    /// `costs._apply_fast_multiplier` — fold the priority premium into book rates.
    ///
    /// The book stores standard rates; the fast premium is a manifest concept the
    /// in-code pricer applies after lookup, so a book hit for a `speed="fast"`
    /// Opus record must bill identically to the in-code path. Cache rates are
    /// never multiplied.
    fn apply_fast_multiplier(&self, rates: Rates, model: &str) -> Rates {
        let pkey = self.provider_for_model(model);
        let canonical = self.manifest.canonicalize(model, pkey);
        match self.manifest.fast_multiplier(canonical.as_deref(), pkey) {
            Some(mult) => (rates.0 * mult, rates.1 * mult, rates.2, rates.3),
            None => rates,
        }
    }

    /// The pricer key the model id itself *claims*, or `None` for no match.
    ///
    /// Port of `vendor_for_model`. The `None` is the point — see the module docs.
    #[must_use]
    pub fn vendor_for_model(&self, model: &str) -> Option<&str> {
        let lowered = model.trim().to_lowercase();
        if lowered.is_empty() {
            return None;
        }
        if let Some(exact) = self
            .exact_id_routing
            .get(&lowered)
            .filter(|key| !key.is_empty())
        {
            return Some(exact.as_str());
        }
        for (hint, key, is_prefix) in hint_routing() {
            let hit = if *is_prefix {
                lowered.starts_with(hint)
            } else {
                lowered.contains(hint)
            };
            if hit {
                return Some(key);
            }
        }
        None
    }

    /// Model id → pricer key, defaulting to `anthropic`. Port of
    /// `_provider_for_model`; the fallback matches `get_pricer`'s so an unrouted
    /// id still prices rather than raising.
    #[must_use]
    pub fn provider_for_model(&self, model: &str) -> &str {
        self.vendor_for_model(model).unwrap_or("anthropic")
    }

    /// Pricer key for a *stored* `(provider, model)` pair. Port of
    /// `resolve_pricing_provider` (July, `8a83ccb`).
    ///
    /// `projects.provider` records which TOOL wrote the transcript, not whose rate
    /// card applies, so a *definite* model→vendor match wins. Three guards keep
    /// the override from doing harm, and all three are inert by construction —
    /// each returns the recorded provider unchanged:
    ///
    /// 1. **no definite match** — an unrecognised id keeps its shell's verdict,
    ///    including a shell's honest `None` → `$0`, instead of being re-routed
    ///    into Anthropic's fallback and invented into existence;
    /// 2. **the vendor pricer can't price the id but the shell can** — never
    ///    trade a real number for "I don't know" (Cursor's dated Gemini previews);
    /// 3. **both agree on the rate** — the shell already delegates correctly, so
    ///    behaviour stays bit-for-bit unchanged.
    #[must_use]
    pub fn resolve_pricing_provider(&self, provider: Option<&str>, model: &str) -> String {
        let Some(vendor) = self.vendor_for_model(model) else {
            // Guard 1. `provider or "anthropic"` — None AND "" both fall through.
            return provider
                .filter(|p| !p.is_empty())
                .unwrap_or("anthropic")
                .to_string();
        };
        let Some(provider) = provider.filter(|p| !p.is_empty()) else {
            return vendor.to_string();
        };
        let shell = get_pricer(provider);
        let upstream = get_pricer(vendor);
        // `shell is upstream` — singleton identity, which for aliases (claude /
        // anthropic, codex / openai) is true and for subclasses (kilocode /
        // cline) is false.
        if shell == upstream {
            return provider.to_string();
        }
        let upstream_canonical = upstream.canonicalize(&self.manifest, model);
        let Some(upstream_rates) =
            upstream.rates_for(&self.manifest, upstream_canonical.as_deref())
        else {
            // Guard 2.
            return provider.to_string();
        };
        let shell_canonical = shell.canonicalize(&self.manifest, model);
        let shell_rates = shell.rates_for(&self.manifest, shell_canonical.as_deref());
        if shell_rates == Some(upstream_rates) {
            // Guard 3.
            return provider.to_string();
        }
        vendor.to_string()
    }

    /// Per-token rates for `model`, or `None` when nothing resolves. Port of
    /// `get_model_pricing`.
    #[must_use]
    pub fn get_model_pricing(&self, model: &str) -> Option<ModelPricing> {
        let model = resolve_model_alias(model, &self.aliases);
        let (i, o, cw, cr) = match self.overlay.get(&model) {
            Some(rates) => *rates,
            None => {
                let pricer = get_pricer(self.provider_for_model(&model));
                let canonical = pricer.canonicalize(&self.manifest, &model);
                pricer.rates_for(&self.manifest, canonical.as_deref())?
            }
        };
        Some(ModelPricing {
            input_cost_per_token: i / MILLION,
            output_cost_per_token: o / MILLION,
            cache_creation_cost_per_token: cw / MILLION,
            cache_read_cost_per_token: cr / MILLION,
        })
    }

    /// `RATE_CARD` — every canonical id with its resolved pricing, in manifest
    /// order. A `None` value means the id resolves to no rate; the KEY is still
    /// present, which is what [`PricingEngine::is_rate_card_model`] tests.
    #[must_use]
    pub fn rate_card(&self) -> Vec<(String, Option<ModelPricing>)> {
        self.manifest
            .canonical_ids()
            .into_iter()
            .map(|id| {
                let pricing = self.get_model_pricing(&id);
                (id, pricing)
            })
            .collect()
    }

    /// Whether `model` has an exact entry in the rate card. Port of
    /// `is_rate_card_model` — the same membership test every normalizer uses to
    /// stamp `cost_source` as `rate_card` vs `unknown`.
    ///
    /// It is membership, NOT "a rate resolves": the pricers fall back to a default
    /// family for unrecognised ids, so `get_model_pricing` would almost never say
    /// `None`, and exact membership is the only honest "we actually know this
    /// model" signal.
    #[must_use]
    pub fn is_rate_card_model(&self, model: &str) -> bool {
        !model.is_empty() && self.manifest.canonical_ids().iter().any(|id| id == model)
    }

    /// Best-effort would-be cost in USD. Port of `estimate_cost`: routes through
    /// `compute_cost` with the model-name provider heuristic so an unpriced row's
    /// dollar exposure can be quantified. Never raises; `0.0` when nothing resolves.
    #[must_use]
    pub fn estimate_cost(&self, tokens: &RawTokens, model: &str) -> f64 {
        let provider = self.provider_for_model(model).to_string();
        self.compute_cost(tokens, model, &provider, "standard", None)
            .total_cost
    }

    /// The `source='rate_card'` half of `costs.backfill_price_book`: every
    /// canonical id stamped at its current resolved rate, undated.
    ///
    /// The `source='manifest'` half is
    /// [`super::manifest::manifest_price_book_rows`]; a full backfill writes both,
    /// manifest rows first.
    #[must_use]
    pub fn rate_card_price_book_rows(&self) -> Vec<PriceBookRow> {
        self.rate_card()
            .into_iter()
            .filter_map(|(id, pricing)| {
                let pricing = pricing?;
                Some(PriceBookRow {
                    provider: self.provider_for_model(&id).to_string(),
                    model: id,
                    effective_from: String::new(),
                    effective_until: String::new(),
                    input: pricing.input_cost_per_token * MILLION,
                    output: pricing.output_cost_per_token * MILLION,
                    cache_write: pricing.cache_creation_cost_per_token * MILLION,
                    cache_read: pricing.cache_read_cost_per_token * MILLION,
                    source: SOURCE_RATE_CARD.to_string(),
                })
            })
            .collect()
    }

    /// The pricer a `(provider, model)` pair would be priced through, after the
    /// provider-resolution chain. Convenience for callers that need the pricer
    /// rather than its key.
    #[must_use]
    pub fn pricer_for(&self, provider: Option<&str>, model: &str) -> Pricer {
        get_pricer(&self.resolve_pricing_provider(provider, model))
    }
}

/// Render a dollar amount the way the dashboards and reports do. Port of
/// `format_dollars`.
///
/// Four magnitude bands with different precisions, and thousands separators on
/// the top two. The `$` precedes the sign for negatives (`$-5.00`) because
/// Python's f-string does the same — this is a faithful port of a wart.
#[must_use]
pub fn format_dollars(amount: f64) -> String {
    // Python renders a NaN as `nan`; Rust's `{}` renders `NaN`. Every other
    // non-finite value formats identically in both.
    if amount.is_nan() {
        return "$nan".to_string();
    }
    let magnitude = amount.abs();
    if magnitude >= 100.0 {
        return format!("${}", group_thousands(&format!("{amount:.0}")));
    }
    if magnitude >= 1.0 {
        return format!("${}", group_thousands(&format!("{amount:.2}")));
    }
    if magnitude >= 0.01 {
        return format!("${amount:.3}");
    }
    format!("${amount:.4}")
}

/// Insert `,` every three digits of the integer part, as Python's `,` format spec
/// does. Handles a leading sign and a fractional tail.
fn group_thousands(rendered: &str) -> String {
    let (sign, rest) = match rendered.strip_prefix('-') {
        Some(rest) => ("-", rest),
        None => ("", rendered),
    };
    let (int_part, frac_part) = match rest.split_once('.') {
        Some((i, f)) => (i, Some(f)),
        None => (rest, None),
    };
    if !int_part.bytes().all(|b| b.is_ascii_digit()) {
        // `inf` and friends are never grouped.
        return rendered.to_string();
    }
    let mut grouped = String::with_capacity(int_part.len() + int_part.len() / 3);
    for (i, ch) in int_part.chars().enumerate() {
        if i > 0 && (int_part.len() - i).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(ch);
    }
    match frac_part {
        Some(frac) => format!("{sign}{grouped}.{frac}"),
        None => format!("{sign}{grouped}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pricing::price_book::{PriceBook, PriceBookRow, SOURCE_LIVE, SOURCE_MANIFEST};
    use crate::pricing::test_support::{manifest_path, sample_manifest};

    fn engine() -> PricingEngine {
        PricingEngine::from_manifest(sample_manifest())
    }

    fn tokens() -> RawTokens {
        RawTokens::canonical(1_000_000, 1_000_000, 1_000_000, 1_000_000)
    }

    #[test]
    fn loads_the_real_manifest_from_a_path_argument() {
        let engine = PricingEngine::from_manifest_path(&manifest_path()).expect("loads");
        assert!(engine.manifest().models().len() >= 18);
        assert!(
            engine.manifest().dropped().is_empty(),
            "no malformed entries"
        );
        assert!(engine.manifest().canonical_ids().len() >= 50);
    }

    #[test]
    fn vendor_for_model_is_definite_match_or_none() {
        let e = engine();
        // Exact canonical id → its group's pricer key.
        assert_eq!(e.vendor_for_model("claude-opus-4-8"), Some("anthropic"));
        assert_eq!(e.vendor_for_model("gpt-5.5"), Some("openai"));
        assert_eq!(e.vendor_for_model("composer-1"), Some("cursor"));
        assert_eq!(e.vendor_for_model("glm-5"), Some("anthropic"));
        // Hint routing for ids nobody lists.
        assert_eq!(e.vendor_for_model("claude-opus-5"), Some("anthropic"));
        assert_eq!(e.vendor_for_model("glm-5.2"), Some("anthropic"));
        assert_eq!(e.vendor_for_model("gemini-9-ultra"), Some("gemini"));
        assert_eq!(e.vendor_for_model("cursor-whatever"), Some("cursor"));
        // …and the whole point: a definite non-match.
        assert_eq!(e.vendor_for_model("deepseek-v4-flash-free"), None);
        assert_eq!(e.vendor_for_model("grok-4.5"), None);
        assert_eq!(e.vendor_for_model("big-pickle"), None);
        assert_eq!(e.vendor_for_model(""), None);
        assert_eq!(e.vendor_for_model("   "), None);
        // Trimming and case folding happen here (they do NOT in canonicalize).
        assert_eq!(e.vendor_for_model("  CLAUDE-OPUS-4-8 "), Some("anthropic"));
        // provider_for_model never says None.
        assert_eq!(e.provider_for_model("deepseek-v4-flash-free"), "anthropic");
    }

    #[test]
    fn resolve_pricing_provider_guard_1_keeps_the_shell_on_no_match() {
        let e = engine();
        // An id no pricer claims keeps the recorded provider — including a shell
        // whose honest answer is "no rate", which must stay $0 rather than being
        // invented into Anthropic's fallback.
        assert_eq!(
            e.resolve_pricing_provider(Some("opencode"), "deepseek-v4-flash-free"),
            "opencode"
        );
        assert_eq!(e.resolve_pricing_provider(Some("grok"), "grok-4.5"), "grok");
        assert_eq!(e.resolve_pricing_provider(None, "grok-4.5"), "anthropic");
        assert_eq!(
            e.resolve_pricing_provider(Some(""), "grok-4.5"),
            "anthropic"
        );
        // The shell's $0 verdict survives the resolution.
        let breakdown = e.compute_cost(
            &tokens(),
            "deepseek-v4-flash-free",
            &e.resolve_pricing_provider(Some("opencode"), "deepseek-v4-flash-free"),
            "standard",
            None,
        );
        assert_eq!(breakdown.total_cost, 0.0);
    }

    #[test]
    fn resolve_pricing_provider_guard_2_never_trades_a_number_for_unknown() {
        let e = engine();
        // Cursor's dated Gemini preview: the vendor pricer (gemini) has no row for
        // the dated id, but the cursor shell strips the suffix and finds one.
        assert_eq!(
            e.vendor_for_model("gemini-2.5-pro-preview-05-06"),
            Some("gemini")
        );
        assert_eq!(
            get_pricer("gemini").rates_for(
                e.manifest(),
                get_pricer("gemini")
                    .canonicalize(e.manifest(), "gemini-2.5-pro-preview-05-06")
                    .as_deref()
            ),
            None
        );
        assert_eq!(
            e.resolve_pricing_provider(Some("cursor"), "gemini-2.5-pro-preview-05-06"),
            "cursor"
        );
    }

    #[test]
    fn resolve_pricing_provider_guard_3_keeps_the_shell_when_rates_agree() {
        let e = engine();
        // openclaw delegates claude ids to the Anthropic pricer already, so both
        // sides quote the same rate and the recorded provider is kept.
        assert_eq!(
            e.resolve_pricing_provider(Some("openclaw"), "claude-sonnet-4-6"),
            "openclaw"
        );
        // Aliases are the same singleton — kept without even comparing rates.
        assert_eq!(
            e.resolve_pricing_provider(Some("claude"), "claude-opus-4-8"),
            "claude"
        );
        assert_eq!(
            e.resolve_pricing_provider(Some("codex"), "gpt-5.5"),
            "codex"
        );
    }

    #[test]
    fn resolve_pricing_provider_overrides_a_foreign_model_on_a_shell() {
        let e = engine();
        // The docstring's own example: a `pi` project logging claude-opus-4-7
        // would be billed at $1.25/$10 through PiPricer instead of $5/$25.
        assert_eq!(
            e.resolve_pricing_provider(Some("pi"), "claude-opus-4-7"),
            "anthropic"
        );
        let wrong = e.compute_cost(&tokens(), "claude-opus-4-7", "pi", "standard", None);
        let right = e.compute_cost(
            &tokens(),
            "claude-opus-4-7",
            &e.resolve_pricing_provider(Some("pi"), "claude-opus-4-7"),
            "standard",
            None,
        );
        assert_eq!(wrong.input_cost, 1.25);
        assert_eq!(right.input_cost, 5.0);
        assert_eq!(right.output_cost, 25.0);
    }

    #[test]
    fn get_pricer_falls_back_but_the_resolver_does_not() {
        let e = engine();
        // `get_pricer` invents an answer for an unknown provider …
        assert_eq!(get_pricer("totally-unknown"), Pricer::Anthropic);
        // … while the resolver keeps the recorded shell name for an unmatched id.
        assert_eq!(
            e.resolve_pricing_provider(Some("totally-unknown"), "unmatched-model"),
            "totally-unknown"
        );
    }

    #[test]
    fn speed_fast_multiplies_input_and_output_only_for_families_that_declare_it() {
        let e = engine();
        let standard = e.compute_cost(&tokens(), "claude-opus-4-8", "claude", "standard", None);
        let fast = e.compute_cost(&tokens(), "claude-opus-4-8", "claude", "fast", None);
        assert_eq!(fast.input_cost, standard.input_cost * 6.0);
        assert_eq!(fast.output_cost, standard.output_cost * 6.0);
        assert_eq!(fast.cache_creation_cost, standard.cache_creation_cost);
        assert_eq!(fast.cache_read_cost, standard.cache_read_cost);
        // Sonnet declares no fast_multiplier — a misclassified record is never
        // overcharged.
        let s_standard = e.compute_cost(&tokens(), "claude-sonnet-4-6", "claude", "standard", None);
        let s_fast = e.compute_cost(&tokens(), "claude-sonnet-4-6", "claude", "fast", None);
        assert_eq!(s_standard, s_fast);
        // Nothing outside the Anthropic pricer interprets `speed` at all.
        assert_eq!(
            e.compute_cost(&tokens(), "gpt-5.5", "codex", "standard", None),
            e.compute_cost(&tokens(), "gpt-5.5", "codex", "fast", None)
        );
    }

    #[test]
    fn at_ts_selects_the_effective_dated_row() {
        let e = engine();
        let before = e.compute_cost(
            &tokens(),
            "gpt-5.4",
            "codex",
            "standard",
            Some("2026-01-15"),
        );
        let after = e.compute_cost(
            &tokens(),
            "gpt-5.4",
            "codex",
            "standard",
            Some("2026-06-01"),
        );
        let now = e.compute_cost(&tokens(), "gpt-5.4", "codex", "standard", None);
        assert_eq!(before.output_cost, 20.0);
        assert_eq!(after.output_cost, 15.0);
        assert_eq!(now.output_cost, 15.0);
        // A model with a single undated row ignores at_ts entirely.
        assert_eq!(
            e.compute_cost(
                &tokens(),
                "claude-opus-4-8",
                "claude",
                "standard",
                Some("2020-01-01")
            ),
            e.compute_cost(&tokens(), "claude-opus-4-8", "claude", "standard", None)
        );
    }

    #[test]
    fn aliases_resolve_before_anything_else() {
        let mut aliases = HashMap::new();
        aliases.insert(
            "openrouter/claude-opus".to_string(),
            "claude-opus-4-8".to_string(),
        );
        aliases.insert("loop".to_string(), "loop".to_string());
        let e = engine().with_aliases(aliases);
        let aliased = e.compute_cost(
            &tokens(),
            "openrouter/claude-opus",
            "claude",
            "standard",
            None,
        );
        assert_eq!(aliased.input_cost, 5.0);
        // A self-alias terminates rather than looping.
        assert_eq!(resolve_model_alias("loop", &e.aliases), "loop");
    }

    #[test]
    fn the_overlay_wins_over_the_manifest_and_ignores_at_ts() {
        let mut overlay = HashMap::new();
        overlay.insert("gpt-5.4".to_string(), (1.0, 2.0, 3.0, 4.0));
        let e = engine().with_overlay(overlay);
        let historical = e.compute_cost(
            &tokens(),
            "gpt-5.4",
            "codex",
            "standard",
            Some("2026-01-15"),
        );
        assert_eq!(historical.input_cost, 1.0);
        assert_eq!(historical.output_cost, 2.0);
        assert_eq!(historical.total_cost, 10.0);
    }

    #[test]
    fn a_wired_book_takes_the_manifests_slot_and_a_miss_falls_through() {
        let e = engine();
        let book = PriceBook::from_rows(vec![PriceBookRow {
            provider: "anthropic".to_string(),
            model: "OPUS_48".to_string(),
            effective_from: String::new(),
            effective_until: String::new(),
            input: 111.0,
            output: 222.0,
            cache_write: 333.0,
            cache_read: 444.0,
            source: SOURCE_MANIFEST.to_string(),
        }]);
        let booked = engine().with_price_book(book);
        assert_eq!(
            booked
                .compute_cost(&tokens(), "claude-opus-4-8", "claude", "standard", None)
                .input_cost,
            111.0
        );
        // A model the book does not carry prices from the in-code manifest.
        assert_eq!(
            booked.compute_cost(&tokens(), "claude-sonnet-4-6", "claude", "standard", None),
            e.compute_cost(&tokens(), "claude-sonnet-4-6", "claude", "standard", None)
        );
    }

    #[test]
    fn a_book_hit_still_gets_the_fast_premium() {
        let book = PriceBook::from_rows(vec![PriceBookRow {
            provider: "anthropic".to_string(),
            model: "claude-opus-4-8".to_string(),
            effective_from: String::new(),
            effective_until: String::new(),
            input: 5.0,
            output: 25.0,
            cache_write: 6.25,
            cache_read: 0.50,
            source: SOURCE_LIVE.to_string(),
        }]);
        let booked = engine().with_price_book(book);
        let fast = booked.compute_cost(&tokens(), "claude-opus-4-8", "claude", "fast", None);
        assert_eq!(fast.input_cost, 30.0);
        assert_eq!(fast.output_cost, 150.0);
        assert_eq!(fast.cache_creation_cost, 6.25);
    }

    #[test]
    fn a_book_primed_from_the_manifest_prices_identically_to_it() {
        // The wave-3 price-book equality gate, in miniature — and note the
        // provider it sweeps with. See
        // `a_wired_book_reprices_rows_whose_provider_disagrees_with_their_model`
        // for why passing a fixed provider here would "fail" for the wrong reason.
        let e = engine();
        let mut rows = crate::pricing::manifest::manifest_price_book_rows(e.manifest());
        rows.extend(e.rate_card_price_book_rows());
        let booked = engine().with_price_book(PriceBook::from_rows(rows));
        for (id, _) in e.rate_card() {
            let provider = e.provider_for_model(&id).to_string();
            for speed in ["standard", "fast"] {
                for at_ts in [None, Some("2026-01-15"), Some("2026-06-01")] {
                    let plain = e.compute_cost(&tokens(), &id, &provider, speed, at_ts);
                    let from_book = booked.compute_cost(&tokens(), &id, &provider, speed, at_ts);
                    assert_eq!(plain, from_book, "{id} / {provider} / {speed} / {at_ts:?}");
                }
            }
        }
    }

    #[test]
    fn a_wired_book_reprices_rows_whose_provider_disagrees_with_their_model() {
        // FINDING (wave-3 mart gate): `compute_cost`'s two paths key on different
        // providers. The in-code path prices through `get_pricer(provider)` — the
        // provider the CALLER passed — while the book path looks the row up under
        // `_provider_for_model(model)`, the pricer the MODEL ID claims. When the
        // two disagree, wiring the book silently changes the price.
        //
        // Verified against the reference implementation on 2026-07-30 with a
        // backfilled `price_book` (73 rows):
        //     in-code, provider="claude", model="gpt-5-codex" -> 3.0/15.0/3.75/0.3
        //     book,    provider="claude", model="gpt-5-codex" -> 1.25/10.0/0.0/0.125
        // The server primes the book at startup; the CLI/ETL path before a
        // backfill does not. Any usage_event whose recorded provider disagrees
        // with its model's vendor therefore prices differently in the two paths —
        // which is DIV-001's mechanism seen from the read side.
        let e = engine();
        let mut rows = crate::pricing::manifest::manifest_price_book_rows(e.manifest());
        rows.extend(e.rate_card_price_book_rows());
        let booked = engine().with_price_book(PriceBook::from_rows(rows));

        let in_code = e.compute_cost(&tokens(), "gpt-5-codex", "claude", "standard", None);
        let from_book = booked.compute_cost(&tokens(), "gpt-5-codex", "claude", "standard", None);
        assert_eq!(in_code.input_cost, 3.0, "the Anthropic fallback family");
        assert_eq!(from_book.input_cost, 1.25, "the OpenAI rate_card row");
        assert_ne!(in_code, from_book);

        // …and it does not bite when the caller's provider agrees with the model,
        // which is what `resolve_pricing_provider` exists to arrange.
        let resolved = e.resolve_pricing_provider(Some("claude"), "gpt-5-codex");
        assert_eq!(resolved, "openai");
        assert_eq!(
            e.compute_cost(&tokens(), "gpt-5-codex", &resolved, "standard", None),
            booked.compute_cost(&tokens(), "gpt-5-codex", &resolved, "standard", None)
        );
    }

    #[test]
    fn a_wired_book_resurrects_every_deliberate_zero() {
        // FINDING (wave-3 mart gate, DIV-001's mechanism): with the price book
        // wired, a model the in-code path deliberately refuses to price ($0, "no
        // rate available") starts pricing at the Anthropic FALLBACK family. The
        // chain is: `_price_book_rates` looks the row up under
        // `_provider_for_model(model)`, which defaults to "anthropic" for an
        // unmatched id; `canonicalize` then resolves the unknown id to the
        // provider's fallback family (SONNET_35); and the manifest backfill wrote
        // a SONNET_35 row — so the lookup HITS instead of missing.
        //
        // Verified against the reference implementation on 2026-07-30 with a
        // backfilled `price_book`, tokens (1M, 1M, 1M, 1M):
        //     opencode / big-pickle             $0.0000 -> $22.0500
        //     opencode / deepseek-v4-flash-free $0.0000 -> $22.0500
        //     copilot  / copilot-auto           $0.0000 -> $22.0500
        //     kiro     / kiro-auto              $0.0000 -> $22.0500
        // The server primes the book at startup; the CLI/ETL path does not. Any
        // cent-exact comparison MUST pin the seam on both sides.
        let e = engine();
        let mut rows = crate::pricing::manifest::manifest_price_book_rows(e.manifest());
        rows.extend(e.rate_card_price_book_rows());
        let booked = engine().with_price_book(PriceBook::from_rows(rows));
        for (provider, model) in [
            ("opencode", "big-pickle"),
            ("opencode", "deepseek-v4-flash-free"),
            ("copilot", "copilot-auto"),
            ("kiro", "kiro-auto"),
        ] {
            assert_eq!(
                e.compute_cost(&tokens(), model, provider, "standard", None)
                    .total_cost,
                0.0,
                "in-code {provider}/{model}"
            );
            assert_eq!(
                booked
                    .compute_cost(&tokens(), model, provider, "standard", None)
                    .total_cost,
                22.05,
                "book {provider}/{model}"
            );
        }
    }

    #[test]
    fn rate_card_membership_is_identity_not_priceability() {
        let e = engine();
        assert!(e.is_rate_card_model("claude-opus-4-8"));
        assert!(e.is_rate_card_model("gpt-5.5"));
        // Priced (via the Anthropic fallback) but NOT a rate-card model. The
        // example has to be an id the card does NOT list: this test used
        // `claude-opus-5` until that model got a real entry, which is the
        // failure mode it is guarding against — membership is identity, and a
        // model gains it by being added to `[canonical_ids]`, not by being
        // priceable.
        assert!(!e.is_rate_card_model("claude-opus-4-9"));
        assert!(e.get_model_pricing("claude-opus-4-9").is_some());
        // The model that motivated the fix is now BOTH.
        assert!(e.is_rate_card_model("claude-opus-5"));
        assert!(e.get_model_pricing("claude-opus-5").is_some());
        assert!(!e.is_rate_card_model(""));
        // Every canonical id resolves to a price; membership and the key list agree.
        assert_eq!(e.rate_card().len(), e.manifest().canonical_ids().len());
    }

    #[test]
    fn get_model_pricing_is_compute_cost_over_a_million_tokens() {
        let e = engine();
        let pricing = e.get_model_pricing("claude-opus-4-8").expect("priced");
        let cost = e.compute_cost(&tokens(), "claude-opus-4-8", "claude", "standard", None);
        assert_eq!(pricing.input_cost_per_token * MILLION, cost.input_cost);
        assert_eq!(pricing.output_cost_per_token * MILLION, cost.output_cost);
    }

    #[test]
    fn estimate_cost_routes_through_the_model_name_heuristic() {
        let e = engine();
        // A model whose recorded provider we do not have: the heuristic still
        // finds Anthropic and quotes the exposure.
        let estimate = e.estimate_cost(&tokens(), "claude-opus-4-8");
        assert_eq!(estimate, 5.0 + 25.0 + 6.25 + 0.5);
        // Nothing resolves → the conservative fallback still prices, never panics.
        assert!(e.estimate_cost(&tokens(), "unheard-of-model") > 0.0);
    }

    #[test]
    fn format_dollars_bands() {
        assert_eq!(format_dollars(1234.5), "$1,234");
        assert_eq!(format_dollars(1_234_567.0), "$1,234,567");
        assert_eq!(format_dollars(100.0), "$100");
        assert_eq!(format_dollars(99.999), "$100.00");
        assert_eq!(format_dollars(1.0), "$1.00");
        assert_eq!(format_dollars(0.9999), "$1.000");
        assert_eq!(format_dollars(0.01), "$0.010");
        assert_eq!(format_dollars(0.009_99), "$0.0100");
        assert_eq!(format_dollars(0.0), "$0.0000");
        // The `$` precedes the sign, faithfully.
        assert_eq!(format_dollars(-5.0), "$-5.00");
        assert_eq!(format_dollars(-1234.0), "$-1,234");
    }

    #[test]
    fn openai_shaped_tokens_are_normalised_by_the_pricer_not_the_caller() {
        let e = engine();
        let raw = RawTokens::openai_shape(1_000_000, 1_000_000, 400_000, 0);
        let cost = e.compute_cost(&raw, "gpt-5.5", "codex", "standard", None);
        // 600K fresh input at $5/M + 1M output at $30/M + 400K cache-read at $0.50/M.
        assert_eq!(cost.input_cost, 600_000.0 * 5.0 / MILLION);
        assert_eq!(cost.output_cost, 30.0);
        assert_eq!(cost.cache_creation_cost, 0.0);
        assert_eq!(cost.cache_read_cost, 400_000.0 * 0.50 / MILLION);
        // The same raw dict through a non-OpenAI pricer sees none of those keys
        // and prices at zero tokens — the reshape is the pricer's job.
        let anthropic = e.compute_cost(&raw, "claude-opus-4-8", "claude", "standard", None);
        assert_eq!(anthropic.total_cost, 0.0);
    }

    #[test]
    fn grok_prices_through_the_anthropic_fallback_family() {
        // Not a defect to fix here: there is no Grok pricer, so `get_pricer`'s
        // conservative fallback prices it at the manifest's fallback family.
        // Recorded because the live store carries grok rows.
        let e = engine();
        let cost = e.compute_cost(&tokens(), "grok-4.5", "grok", "standard", None);
        assert_eq!(cost.input_cost, 3.0);
        assert_eq!(cost.output_cost, 15.0);
    }
}
