//! The `Normalizer` contract and the event builder every provider shares —
//! port of `python-legacy: etl/normalize/base.py`.
//!
//! Cost is computed **once**, here, so the marts read a single number that is
//! never recomputed. The three seams the Python base class reaches through
//! module state — the manifest, the alias map, the price book — are a
//! [`PricingEngine`] the caller injects (findings ledger #5: `set_var` is
//! `unsafe` in Rust 2024 and this workspace forbids `unsafe`).
//!
//! # DIV-016: the CLI/ETL path runs UNPRIMED
//!
//! `compute_cost` consults the store-backed price book only when
//! `model_manifest.use_price_book_store()` has wired one, and the ONLY caller
//! that wires it is `server.py:154` at startup. `stax etl backfill`
//! never does, so on the normalize path `_price_book_rates` returns `None` for
//! every row and the in-code manifest prices everything. That is the seam
//! DIV-016 names, and [`NormalizeContext::unprimed`] is its Rust spelling: an
//! engine with no book attached. [`tests::the_normalize_path_prices_unprimed`]
//! pins both sides — an unprimed engine and a book-primed one — so the day a
//! caller primes the book on this path, a test says so instead of the dollars
//! moving quietly.

use stax_core::queries::pyjson::Value as PyValue;

use super::row::{MsgRow, PyRaise, py_str, str_or, str_or_empty};
use crate::pricing::{PricingEngine, RawTokens};

/// `cost_source` — the spec's four-value enum (`docs/specs/session-schema-v1.md`).
///
/// Python validates the string against a frozenset and raises `ValueError` on a
/// miss; an enum makes that miss unrepresentable, which is the one place this
/// port is *deliberately* stricter than its original. No shipped normalizer can
/// reach the raise (every call site passes a module constant), so nothing
/// observable changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum CostSource {
    /// `"live"` — an upstream feed priced this row. No normalizer stamps it.
    Live,
    /// `"rate_card"` — the model has an exact `RATE_CARD` entry.
    RateCard,
    /// `"estimated"` — tokens were recovered from text length.
    Estimated,
    /// `"unknown"` — the model is not in the rate card; cost is forced to `0.0`.
    Unknown,
}

impl CostSource {
    /// The wire string stored in `usage_events.cost_source`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Live => "live",
            Self::RateCard => "rate_card",
            Self::Estimated => "estimated",
            Self::Unknown => "unknown",
        }
    }
}

impl std::fmt::Display for CostSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One `usage_events` row, in the key order `_build_event` returns.
#[derive(Debug, Clone, PartialEq)]
pub struct UsageEvent {
    /// `msg_row.get("id")`, passed through uncoerced as Python does.
    pub source_message_fk: PyValue,
    /// `str(msg_row["provider"] or self.provider_name)`.
    pub provider: String,
    /// `str(msg_row["account"] or "default")`.
    pub account: String,
    /// `msg_row.get("project_id")`, passed through uncoerced.
    pub project_id: PyValue,
    /// `str(msg_row["session_id"] or "")`.
    pub session_id: String,
    /// The event timestamp.
    pub ts: String,
    /// `YYYY-MM-DD` derived from [`UsageEvent::ts`].
    pub day: String,
    /// The model id the row was priced as.
    pub model: String,
    /// `"standard"` or `"fast"` (Anthropic's priority tier).
    pub speed: String,
    /// Fresh (uncached) input tokens.
    pub input_tokens: i64,
    /// Output tokens, reasoning folded in where the provider bundles it.
    pub output_tokens: i64,
    /// Cache-read tokens.
    pub cache_read_tokens: i64,
    /// Cache-write tokens.
    pub cache_create_tokens: i64,
    /// Attribution-only subset of `output_tokens`; never priced, clamped `>= 0`.
    pub reasoning_tokens: i64,
    /// Dollars, computed once here.
    pub cost_usd: f64,
    /// Where the dollars came from.
    pub cost_source: CostSource,
    /// `str(msg_row["role"] or "")`.
    pub role: String,
    /// `json.dumps(raw_extras)` when non-empty, else `NULL`.
    pub raw_extras: Option<String>,
}

/// The injected pricing seam a normalize run reads.
///
/// One value, shared by every normalizer in the run — the Python equivalent is
/// the process-wide module state `infra.costs` reads.
#[derive(Debug, Clone)]
pub struct NormalizeContext {
    engine: PricingEngine,
    rate_card: std::collections::HashSet<String>,
}

impl NormalizeContext {
    /// Wrap an engine.
    #[must_use]
    pub fn new(engine: PricingEngine) -> Self {
        // `RATE_CARD = {mid: get_model_pricing(mid) for mid in _CANONICAL_IDS}`
        // is built ONCE at import on the Python side and the normalizers only
        // ever test membership of it. Materialising the key set once here is
        // the same computation with the same answer; doing it per row would
        // allocate the whole canonical-id list 383,700 times.
        let rate_card = engine.manifest().canonical_ids().into_iter().collect();
        Self { engine, rate_card }
    }

    /// The state `stax etl backfill` actually runs in: manifest only,
    /// no alias map, no overlay, **no price book** (DIV-016).
    ///
    /// # Errors
    /// When `data/models.toml` cannot be read or parsed.
    pub fn unprimed(
        manifest_path: &std::path::Path,
    ) -> Result<Self, crate::pricing::manifest::ManifestError> {
        Ok(Self::new(PricingEngine::from_manifest_path(manifest_path)?))
    }

    /// The pricing engine backing this run.
    #[must_use]
    pub fn engine(&self) -> &PricingEngine {
        &self.engine
    }

    /// `model in RATE_CARD` — the membership test that decides `rate_card` vs
    /// `unknown` in fifteen of the twenty normalizers.
    ///
    /// Membership, not "a rate resolves": the pricers fall back to a default
    /// family for unrecognised ids, so `get_model_pricing` almost never says
    /// `None`, and exact membership is the only honest "we know this model".
    #[must_use]
    pub fn is_rate_card_model(&self, model: &str) -> bool {
        self.rate_card.contains(model)
    }

    /// `COST_SOURCE_RATE_CARD if model in RATE_CARD else COST_SOURCE_UNKNOWN`.
    #[must_use]
    pub fn rate_card_or_unknown(&self, model: &str) -> CostSource {
        if self.is_rate_card_model(model) {
            CostSource::RateCard
        } else {
            CostSource::Unknown
        }
    }
}

/// The token shape + stamps a normalizer hands [`Normalizer::build_event`].
///
/// A struct rather than a long argument list because Python's is a
/// keyword-only signature, and getting `cache_read` and `cache_create` the
/// wrong way round is exactly the mistake positional arguments invite.
#[derive(Debug, Clone)]
pub struct EventSpec {
    /// Canonical fresh input.
    pub input_tokens: i64,
    /// Canonical output.
    pub output_tokens: i64,
    /// Canonical cache reads.
    pub cache_read_tokens: i64,
    /// Canonical cache writes.
    pub cache_create_tokens: i64,
    /// The stamp.
    pub cost_source: CostSource,
    /// Attribution-only reasoning subset (default 0).
    pub reasoning_tokens: i64,
    /// Explicit model id; `None` falls back to the row's column.
    pub model: Option<String>,
    /// Explicit role; `None` falls back to the row's column.
    pub role: Option<String>,
    /// Explicit speed; `None` falls back to the row's column.
    pub speed: Option<String>,
    /// Explicit timestamp; `None` falls back to the row's column.
    pub ts: Option<String>,
    /// Provider keepsakes; serialised only when non-empty.
    pub raw_extras: Option<PyValue>,
}

impl EventSpec {
    /// The canonical four tokens plus a stamp — every other field defaulted the
    /// way Python's keyword defaults are.
    #[must_use]
    pub fn new(
        input_tokens: i64,
        output_tokens: i64,
        cache_read_tokens: i64,
        cache_create_tokens: i64,
        cost_source: CostSource,
    ) -> Self {
        Self {
            input_tokens,
            output_tokens,
            cache_read_tokens,
            cache_create_tokens,
            cost_source,
            reasoning_tokens: 0,
            model: None,
            role: None,
            speed: None,
            ts: None,
            raw_extras: None,
        }
    }

    /// `model=...`.
    #[must_use]
    pub fn model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }

    /// `reasoning_tokens=...`.
    #[must_use]
    pub fn reasoning(mut self, reasoning_tokens: i64) -> Self {
        self.reasoning_tokens = reasoning_tokens;
        self
    }

    /// `raw_extras=...`.
    #[must_use]
    pub fn raw_extras(mut self, raw_extras: Option<PyValue>) -> Self {
        self.raw_extras = raw_extras;
        self
    }
}

/// The arguments `_compute_cost_usd` takes — grok overrides the method, so the
/// shape is public.
#[derive(Debug, Clone)]
pub struct CostArgs<'a> {
    /// Fresh input.
    pub input_tokens: i64,
    /// Output.
    pub output_tokens: i64,
    /// Cache reads.
    pub cache_read_tokens: i64,
    /// Cache writes.
    pub cache_create_tokens: i64,
    /// The resolved model id.
    pub model: &'a str,
    /// `"standard"` / `"fast"`.
    pub speed: &'a str,
    /// The stamp — `unknown` short-circuits to `$0`.
    pub cost_source: CostSource,
    /// The event timestamp, for effective-dated rates. Always a string (possibly
    /// empty), never absent: `_build_event` computes it before calling.
    pub at_ts: &'a str,
}

/// Per-provider transform: `messages.row → usage_events.row(s)`.
pub trait Normalizer {
    /// The registry key — and the pricer key `_compute_cost_usd` routes through.
    fn provider_name(&self) -> &'static str;

    /// Yield 0..N `usage_events` rows for one `messages` row.
    ///
    /// # Errors
    /// A Python exception escaping a coercion. `backfill` catches it, logs at
    /// DEBUG and drops the row; see [`super::row`].
    fn normalize(&self, ctx: &NormalizeContext, row: &MsgRow) -> Result<Vec<UsageEvent>, PyRaise>;

    /// `Normalizer._compute_cost_usd` — one-shot price lookup; never raises.
    ///
    /// `0.0` when the stamp is `unknown` (the caller already decided the model
    /// is not in the rate card) or the model is empty. Honouring the flag is
    /// required: an unrecognised id falls through `compute_cost` to the
    /// Anthropic family heuristic, which returns *non-zero* conservative rates,
    /// so without this guard an `unknown` row would accrue phantom dollars.
    fn compute_cost_usd(&self, ctx: &NormalizeContext, args: &CostArgs<'_>) -> f64 {
        if args.model.is_empty() || args.cost_source == CostSource::Unknown {
            return 0.0;
        }
        let tokens = RawTokens::canonical(
            args.input_tokens,
            args.output_tokens,
            args.cache_create_tokens,
            args.cache_read_tokens,
        );
        // Python wraps this in `except Exception: return 0.0` — "pricing must
        // never break ingest". Nothing in the Rust chain returns a Result or
        // panics on data, so the guard has no body to catch; the shape it
        // guarantees (a float, always) is what survives.
        ctx.engine()
            .compute_cost(
                &tokens,
                args.model,
                // `provider=self.provider_name` — the registry key, NOT the
                // row's provider column. It matters for the Pi/OMP alias: an
                // `omp` row is normalised by `PiNormalizer`, whose
                // `provider_name` is `"pi"`, so it prices through Pi's pricer
                // while the event's `provider` column still reads `"omp"`.
                self.provider_name(),
                args.speed,
                Some(args.at_ts),
            )
            .total_cost
    }

    /// `Normalizer._build_event` — assemble one row and price it once.
    fn build_event(&self, ctx: &NormalizeContext, row: &MsgRow, spec: EventSpec) -> UsageEvent {
        let ts_value = spec.ts.unwrap_or_else(|| str_or_empty(row, "timestamp"));
        // `model if model is not None else (msg_row.get("model") or "")`.
        // NOTE the missing `str()`: Python leaves a truthy non-string model
        // uncoerced here. Every shipped normalizer passes `model=` explicitly,
        // so the branch is unreachable; `py_str` is applied rather than
        // widening the column's type for a path nothing takes.
        let model_value = spec.model.unwrap_or_else(|| match row.get("model") {
            Some(value) if value.is_truthy() => py_str(value),
            _ => String::new(),
        });
        let role_value = spec.role.unwrap_or_else(|| str_or_empty(row, "role"));
        let speed_value = spec
            .speed
            .unwrap_or_else(|| str_or(row, "speed", "standard"));

        let cost_usd = self.compute_cost_usd(
            ctx,
            &CostArgs {
                input_tokens: spec.input_tokens,
                output_tokens: spec.output_tokens,
                cache_read_tokens: spec.cache_read_tokens,
                cache_create_tokens: spec.cache_create_tokens,
                model: &model_value,
                speed: &speed_value,
                cost_source: spec.cost_source,
                at_ts: &ts_value,
            },
        );

        UsageEvent {
            source_message_fk: row.get("id").cloned().unwrap_or(PyValue::Null),
            provider: str_or(row, "provider", self.provider_name()),
            account: str_or(row, "account", "default"),
            project_id: row.get("project_id").cloned().unwrap_or(PyValue::Null),
            session_id: str_or_empty(row, "session_id"),
            day: day_from_ts(&ts_value),
            ts: ts_value,
            model: model_value,
            speed: speed_value,
            input_tokens: spec.input_tokens,
            output_tokens: spec.output_tokens,
            cache_read_tokens: spec.cache_read_tokens,
            cache_create_tokens: spec.cache_create_tokens,
            reasoning_tokens: spec.reasoning_tokens.max(0),
            cost_usd,
            cost_source: spec.cost_source,
            role: role_value,
            raw_extras: spec.raw_extras.and_then(dumps_if_truthy),
        }
    }
}

/// `json.dumps(raw_extras) if raw_extras else None`.
///
/// The truthiness test is on the *dict*, so an empty one stays `NULL` and the
/// column never holds `"{}"`.
fn dumps_if_truthy(value: PyValue) -> Option<String> {
    value
        .is_truthy()
        .then(|| stax_core::queries::pyjson::dumps_default(&value))
}

/// Derive `YYYY-MM-DD` from an ISO 8601 timestamp. Port of `_day_from_ts`.
///
/// Anything that does not parse returns `""` — callers decide whether an
/// empty-day row is a hard error or a filter.
///
/// The cheap path is a *character* slice (`ts[4]`, `ts[7]`, `ts[:10]`), not a
/// byte one, because Python's is. On the maintainer's store all 383,700 rows
/// take it (measured: zero rows fail the `-` positions), so the
/// `datetime.fromisoformat` fallback below is reachable only from synthetic
/// data; it covers the forms `fromisoformat` accepts that a store row could
/// plausibly hold — the basic `YYYYMMDD` date, with or without a time — and
/// returns `""` for the rest. That is narrower than CPython 3.11+'s full
/// grammar (week dates, `±HH:MM:SS.ffffff` offsets), and the narrowing is
/// recorded rather than hidden.
#[must_use]
pub fn day_from_ts(ts: &str) -> String {
    if ts.is_empty() {
        return String::new();
    }
    let chars: Vec<char> = ts.chars().collect();
    if chars.len() >= 10 && chars[4] == '-' && chars[7] == '-' {
        return chars[..10].iter().collect();
    }
    basic_iso_date(&chars).unwrap_or_default()
}

/// The `YYYYMMDD…` basic form, validated the way `date(y, m, d)` would be.
fn basic_iso_date(chars: &[char]) -> Option<String> {
    if chars.len() < 8 || !chars[..8].iter().all(char::is_ascii_digit) {
        return None;
    }
    // A separator (or nothing) must follow the date part.
    if let Some(next) = chars.get(8)
        && !matches!(next, 'T' | 't' | ' ')
    {
        return None;
    }
    let digits: String = chars[..8].iter().collect();
    let year: u32 = digits[0..4].parse().ok()?;
    let month: u32 = digits[4..6].parse().ok()?;
    let day: u32 = digits[6..8].parse().ok()?;
    if !(1..=12).contains(&month) || day == 0 || day > days_in_month(year, month) {
        return None;
    }
    Some(format!("{year:04}-{month:02}-{day:02}"))
}

fn days_in_month(year: u32, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if (year.is_multiple_of(4) && !year.is_multiple_of(100)) || year.is_multiple_of(400) => {
            29
        }
        2 => 28,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::normalize::claude::ClaudeNormalizer;
    use crate::pricing::price_book::{PriceBook, PriceBookRow, SOURCE_RATE_CARD};
    use crate::pricing::test_support::{manifest_path, sample_manifest};

    fn ctx() -> NormalizeContext {
        NormalizeContext::unprimed(&manifest_path()).expect("the checked-in models.toml parses")
    }

    fn claude_row() -> MsgRow {
        MsgRow::new()
            .with("id", PyValue::Int(1))
            .with("provider", PyValue::Str("claude".into()))
            .with("project_id", PyValue::Int(42))
            .with("session_id", PyValue::Str("s-1".into()))
            .with(
                "timestamp",
                PyValue::Str("2026-04-25T12:00:00+00:00".into()),
            )
            .with("role", PyValue::Str("assistant".into()))
            .with("model", PyValue::Str("claude-sonnet-4-5-20250929".into()))
            .with("input_tokens", PyValue::Int(1_000))
            .with("output_tokens", PyValue::Int(500))
            .with("cache_read_tokens", PyValue::Int(0))
            .with("cache_create_tokens", PyValue::Int(0))
            .with("speed", PyValue::Str("standard".into()))
    }

    #[test]
    fn day_takes_the_character_slice_and_falls_back_only_when_it_must() {
        assert_eq!(day_from_ts("2026-04-25T23:59:59+00:00"), "2026-04-25");
        assert_eq!(day_from_ts("2026-04-25"), "2026-04-25");
        assert_eq!(day_from_ts(""), "");
        assert_eq!(day_from_ts("nope"), "");
        // The fallback: the basic form has no dashes at 4 and 7.
        assert_eq!(day_from_ts("20260425T120000"), "2026-04-25");
        assert_eq!(day_from_ts("20260230"), ""); // 30 February
        // A leading multi-byte character must not shift the `-` test: Python
        // indexes characters, so this is a miss, not a panic and not a hit.
        assert_eq!(day_from_ts("é2026-04-25"), "");
    }

    #[test]
    fn raw_extras_is_null_when_empty_and_python_json_when_not() {
        let ctx = ctx();
        let n = ClaudeNormalizer;
        let empty = n.build_event(
            &ctx,
            &claude_row(),
            EventSpec::new(1, 0, 0, 0, CostSource::RateCard)
                .model("claude-sonnet-4-5-20250929")
                .raw_extras(Some(PyValue::Object(vec![]))),
        );
        assert_eq!(empty.raw_extras, None);
        let filled = n.build_event(
            &ctx,
            &claude_row(),
            EventSpec::new(1, 0, 0, 0, CostSource::RateCard)
                .model("claude-sonnet-4-5-20250929")
                .raw_extras(Some(PyValue::Object(vec![
                    ("cost".into(), PyValue::Float(0.5)),
                    ("note".into(), PyValue::Str("é".into())),
                ]))),
        );
        // `json.dumps` defaults: `", "` / `": "` separators, and
        // `ensure_ascii=True` — so the `é` is escaped, not passed through.
        assert_eq!(
            filled.raw_extras.as_deref(),
            Some(r#"{"cost": 0.5, "note": "\u00e9"}"#)
        );
    }

    #[test]
    fn an_unknown_stamp_forces_zero_dollars() {
        let ctx = ctx();
        let n = ClaudeNormalizer;
        let event = n.build_event(
            &ctx,
            &claude_row(),
            EventSpec::new(1_000, 500, 0, 0, CostSource::Unknown).model("not-a-real-model"),
        );
        assert_eq!(event.cost_usd, 0.0);
        // …and the same tokens under `rate_card` are NOT zero, which is what
        // makes the guard load-bearing rather than decorative.
        let priced = n.build_event(
            &ctx,
            &claude_row(),
            EventSpec::new(1_000, 500, 0, 0, CostSource::RateCard)
                .model("claude-sonnet-4-5-20250929"),
        );
        assert!(priced.cost_usd > 0.0);
    }

    #[test]
    fn the_normalize_path_prices_unprimed() {
        // DIV-016. `etl backfill` never calls `use_price_book_store`; only
        // `server.py` does. So the normalize pass sees no book, and a book that
        // disagrees with the manifest must NOT be able to change these dollars.
        let row = claude_row();
        let n = ClaudeNormalizer;
        let spec = || {
            EventSpec::new(1_000, 500, 0, 0, CostSource::RateCard)
                .model("claude-sonnet-4-5-20250929")
        };

        let unprimed = NormalizeContext::unprimed(&manifest_path()).expect("manifest");
        let from_manifest = n.build_event(&unprimed, &row, spec()).cost_usd;

        // A book that prices the same id at an absurd rate.
        let book = PriceBook::from_rows(vec![PriceBookRow {
            provider: "anthropic".to_string(),
            model: "claude-sonnet-4-5-20250929".to_string(),
            effective_from: String::new(),
            effective_until: String::new(),
            input: 999.0,
            output: 999.0,
            cache_write: 999.0,
            cache_read: 999.0,
            source: SOURCE_RATE_CARD.to_string(),
        }]);
        let primed = NormalizeContext::new(
            PricingEngine::from_manifest(sample_manifest()).with_price_book(book),
        );
        let from_book = n.build_event(&primed, &row, spec()).cost_usd;

        // Both sides pinned: the seam is real (the numbers differ) AND the
        // normalize path is on the unprimed side of it.
        assert!(
            (from_book - from_manifest).abs() > 0.1,
            "the fixture book must actually disagree, else this proves nothing"
        );
        let expected = 1_000.0_f64 * 3.0 / 1_000_000.0 + 500.0_f64 * 15.0 / 1_000_000.0;
        assert_eq!(
            from_manifest.to_bits(),
            expected.to_bits(),
            "the unprimed path must price from data/models.toml"
        );
    }

    #[test]
    fn passthrough_columns_keep_their_python_types() {
        let ctx = ctx();
        let n = ClaudeNormalizer;
        let event = n.build_event(
            &ctx,
            &claude_row(),
            EventSpec::new(1, 0, 0, 0, CostSource::RateCard).model("claude-sonnet-4-5-20250929"),
        );
        assert_eq!(event.source_message_fk, PyValue::Int(1));
        assert_eq!(event.project_id, PyValue::Int(42));
        // …and an absent one is None, not 0.
        let bare = n.build_event(
            &ctx,
            &MsgRow::new(),
            EventSpec::new(1, 0, 0, 0, CostSource::RateCard).model("m"),
        );
        assert_eq!(bare.source_message_fk, PyValue::Null);
        assert_eq!(bare.project_id, PyValue::Null);
        assert_eq!(bare.provider, "claude"); // falls back to provider_name
        assert_eq!(bare.account, "default");
        assert_eq!(bare.speed, "standard");
        assert_eq!(bare.day, "");
    }

    #[test]
    fn reasoning_tokens_are_clamped_and_never_priced() {
        let ctx = ctx();
        let n = ClaudeNormalizer;
        let plain = n.build_event(
            &ctx,
            &claude_row(),
            EventSpec::new(1_000, 500, 0, 0, CostSource::RateCard)
                .model("claude-sonnet-4-5-20250929"),
        );
        let attributed = n.build_event(
            &ctx,
            &claude_row(),
            EventSpec::new(1_000, 500, 0, 0, CostSource::RateCard)
                .model("claude-sonnet-4-5-20250929")
                .reasoning(400),
        );
        assert_eq!(attributed.reasoning_tokens, 400);
        assert_eq!(plain.cost_usd.to_bits(), attributed.cost_usd.to_bits());
        let negative = n.build_event(
            &ctx,
            &claude_row(),
            EventSpec::new(1, 0, 0, 0, CostSource::RateCard)
                .model("m")
                .reasoning(-9),
        );
        assert_eq!(negative.reasoning_tokens, 0);
    }
}
