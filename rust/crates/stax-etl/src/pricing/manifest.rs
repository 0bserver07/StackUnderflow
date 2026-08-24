//! The data-driven model manifest — port of `python-legacy: infra/model_manifest.py`.
//!
//! Reads the *same* `stackunderflow/data/models.toml` the Python implementation
//! reads (§2.4: identity and pricing are data, not code — zero transcription).
//! The path is an argument, never a constant: `_MANIFEST_PATH` is module state in
//! Python and this crate forbids the `set_var` pattern that would let a test move
//! it, so injection is the law here (spec §5 / findings ledger #5).
//!
//! Everything below is a behaviour-identical port, including the parts that read
//! oddly:
//!
//! * entry order is load-bearing — `canonicalize` returns the *first* match, so
//!   more specific families must precede broader ones;
//! * malformed entries are dropped with a warning rather than failing the load,
//!   because a silent `$0` from a `KeyError` swallowed at ingest is worse than a
//!   visible skip;
//! * [`Manifest::select_price`] compares `at_ts` against the date bounds as whole
//!   strings, while the price-book's [`super::price_book`] equivalent compares
//!   only the `YYYY-MM-DD` prefix. That asymmetry is real and load-bearing —
//!   `"2026-04-26T00:00:00+00:00" < "2026-04-26"` is false, so a timestamped
//!   `at_ts` on the boundary day lands in the *new* rate window here.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::path::Path;

use super::toml_lite::{self, Table, Value};

/// The four `$/M`-token rates: input, output, cache-write, cache-read.
pub type Rates = (f64, f64, f64, f64);

/// The fields every price row must carry as a number, per `_REQUIRED_PRICE_FIELDS`.
const REQUIRED_PRICE_FIELDS: [&str; 4] = ["input", "output", "cache_write", "cache_read"];

/// One effective-dated rate row.
#[derive(Debug, Clone, PartialEq)]
pub struct PriceRow {
    /// Inclusive lower bound (`YYYY-MM-DD`), or `None` for "always applied".
    pub effective_from: Option<String>,
    /// Exclusive upper bound (`YYYY-MM-DD`), or `None` for "still current".
    pub effective_until: Option<String>,
    /// Input rate, `$/M` tokens.
    pub input: f64,
    /// Output rate, `$/M` tokens.
    pub output: f64,
    /// Cache-write rate, `$/M` tokens.
    pub cache_write: f64,
    /// Cache-read rate, `$/M` tokens.
    pub cache_read: f64,
}

/// One `[[model]]` entry.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelEntry {
    /// The stable canonical key `canonicalize` returns.
    pub family: String,
    /// The pricer that owns it, or `None` when the manifest omits it.
    pub provider: Option<String>,
    /// Token set; an id matches when every token is present.
    pub match_tokens: Vec<String>,
    /// Exact ids, checked before token matching.
    pub ids: Vec<String>,
    /// Whether this family is the provider's fallback.
    pub fallback: bool,
    /// The priority/fast-tier input+output multiplier, when the family has one.
    pub fast_multiplier: Option<f64>,
    /// Human label, carried for the pricing surfaces that render it.
    pub display_name: Option<String>,
    /// One or more effective-dated rate rows, in file order.
    pub price: Vec<PriceRow>,
}

/// The parsed manifest: valid model entries plus the `[canonical_ids]` groups.
#[derive(Debug, Clone, Default)]
pub struct Manifest {
    models: Vec<ModelEntry>,
    dropped: Vec<String>,
    canonical_id_groups: Vec<(String, Vec<String>)>,
    by_family: HashMap<(String, String), usize>,
    fallback_family: HashMap<String, String>,
}

/// A manifest load failure.
#[derive(Debug)]
pub enum ManifestError {
    /// The file could not be read.
    Io(std::io::Error),
    /// The file is not TOML this reader accepts.
    Toml(toml_lite::TomlError),
}

impl fmt::Display for ManifestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ManifestError::Io(e) => write!(f, "reading models.toml: {e}"),
            ManifestError::Toml(e) => write!(f, "parsing models.toml: {e}"),
        }
    }
}

impl std::error::Error for ManifestError {}

impl From<std::io::Error> for ManifestError {
    fn from(value: std::io::Error) -> Self {
        ManifestError::Io(value)
    }
}

impl From<toml_lite::TomlError> for ManifestError {
    fn from(value: toml_lite::TomlError) -> Self {
        ManifestError::Toml(value)
    }
}

impl Manifest {
    /// Load and validate the manifest at `path`.
    ///
    /// # Errors
    /// When the file cannot be read or is not parseable TOML — matching Python,
    /// where `tomllib.load` raising is *not* caught by `_models()`.
    pub fn load(path: &Path) -> Result<Self, ManifestError> {
        let src = std::fs::read_to_string(path)?;
        Ok(Self::from_str_manifest(&src)?)
    }

    /// Parse a manifest from a TOML string.
    ///
    /// # Errors
    /// When the text is not parseable TOML.
    pub fn from_str_manifest(src: &str) -> Result<Self, toml_lite::TomlError> {
        let doc = toml_lite::parse(src)?;
        let mut models = Vec::new();
        let mut dropped = Vec::new();
        for entry in doc.array("model").unwrap_or_default() {
            match validate_model(entry) {
                Some(model) => models.push(model),
                None => dropped.push(dropped_label(entry)),
            }
        }
        let canonical_id_groups = read_canonical_id_groups(&doc);
        let mut manifest = Manifest {
            models,
            dropped,
            canonical_id_groups,
            by_family: HashMap::new(),
            fallback_family: HashMap::new(),
        };
        manifest.index();
        Ok(manifest)
    }

    /// Build the two lookup indexes Python gets from `@lru_cache`d helpers.
    ///
    /// `_by_family` is a dict comprehension, so a duplicated `(provider, family)`
    /// pair resolves to the LAST entry; `_fallback_family` scans in order and
    /// takes the FIRST entry flagged `fallback`. Both are reproduced literally.
    fn index(&mut self) {
        for (i, model) in self.models.iter().enumerate() {
            let Some(provider) = model.provider.clone() else {
                continue;
            };
            self.by_family
                .insert((provider.clone(), model.family.clone()), i);
            if model.fallback {
                self.fallback_family
                    .entry(provider)
                    .or_insert_with(|| model.family.clone());
            }
        }
    }

    /// Every valid model entry, in file order.
    #[must_use]
    pub fn models(&self) -> &[ModelEntry] {
        &self.models
    }

    /// Labels of entries dropped by validation (Python logs a warning per entry).
    #[must_use]
    pub fn dropped(&self) -> &[String] {
        &self.dropped
    }

    /// The `[canonical_ids]` groups, keyed by PRICER KEY, in file order.
    ///
    /// Port of `canonical_id_groups()`. The contract the manifest states is that
    /// each group name IS the pricer the ids route to.
    #[must_use]
    pub fn canonical_id_groups(&self) -> &[(String, Vec<String>)] {
        &self.canonical_id_groups
    }

    /// Every concrete id the rate card recognises, groups in file order and ids
    /// in listed order. Port of `canonical_ids()`.
    #[must_use]
    pub fn canonical_ids(&self) -> Vec<String> {
        self.canonical_id_groups
            .iter()
            .flat_map(|(_, ids)| ids.iter().cloned())
            .collect()
    }

    fn for_provider<'a>(&'a self, provider: &'a str) -> impl Iterator<Item = &'a ModelEntry> {
        self.models
            .iter()
            .filter(move |m| m.provider.as_deref() == Some(provider))
    }

    /// The provider's `fallback` family, or `None`. Port of `_fallback_family`.
    #[must_use]
    pub fn fallback_family(&self, provider: &str) -> Option<&str> {
        self.fallback_family.get(provider).map(String::as_str)
    }

    fn entry(&self, provider: &str, family: &str) -> Option<&ModelEntry> {
        self.by_family
            .get(&(provider.to_string(), family.to_string()))
            .map(|i| &self.models[*i])
    }

    /// Map a free-form model id to a manifest family key. Port of `canonicalize`.
    ///
    /// Exact `ids` win first (manifest order); otherwise the id is split on `-`
    /// and `.` into a token SET and the first entry whose `match` tokens are all
    /// present wins. Falls back to the provider's `fallback` family.
    ///
    /// Note the two Python subtleties kept here: the id is lower-cased but NOT
    /// trimmed, and the token *set* collapses duplicates, which is exactly why
    /// `gpt-5.5` needs an exact `ids` entry to be distinguishable from `gpt-5`.
    #[must_use]
    pub fn canonicalize(&self, model_id: &str, provider: &str) -> Option<String> {
        let fallback = self.fallback_family(provider).map(str::to_string);
        if model_id.is_empty() {
            return fallback;
        }
        let lowered = model_id.to_lowercase();
        for entry in self.for_provider(provider) {
            if entry.ids.iter().any(|i| i.to_lowercase() == lowered) {
                return Some(entry.family.clone());
            }
        }
        // `set(lowered.replace(".", "-").split("-"))` — a SET, so duplicate
        // tokens collapse. That collapse is exactly why `gpt-5.5` needs an
        // exact `ids` entry to be distinguishable from `gpt-5`.
        let normalized = lowered.replace('.', "-");
        let parts: HashSet<&str> = normalized.split('-').collect();
        for entry in self.for_provider(provider) {
            if !entry.match_tokens.is_empty()
                && entry
                    .match_tokens
                    .iter()
                    .all(|t| parts.contains(t.as_str()))
            {
                return Some(entry.family.clone());
            }
        }
        fallback
    }

    /// The rate row effective at `at_ts`, or the current one when `at_ts` is
    /// `None`. Port of `_select_price`.
    ///
    /// Whole-string comparison against the date bounds, deliberately: see the
    /// module docs for why the boundary behaviour differs from the price book's.
    #[must_use]
    pub fn select_price<'a>(prices: &'a [PriceRow], at_ts: Option<&str>) -> Option<&'a PriceRow> {
        if prices.is_empty() {
            return None;
        }
        let Some(at_ts) = at_ts else {
            // `not p.get("effective_until")` — absent AND empty both count as open.
            let current: Vec<&PriceRow> = prices
                .iter()
                .filter(|p| p.effective_until.as_deref().unwrap_or("").is_empty())
                .collect();
            return if current.is_empty() {
                prices.last()
            } else {
                current.last().copied()
            };
        };
        for p in prices {
            let from_ok = p
                .effective_from
                .as_ref()
                .is_none_or(|ef| at_ts >= ef.as_str());
            let until_ok = p
                .effective_until
                .as_ref()
                .is_none_or(|eu| at_ts < eu.as_str());
            if from_ok && until_ok {
                return Some(p);
            }
        }
        prices.last()
    }

    /// `(input, output, cache_write, cache_read)` in `$/M` for a family.
    ///
    /// Port of `rates_for`. An unknown family resolves to the provider's
    /// fallback family, which is what makes the Anthropic pricer never return
    /// `None`; providers without a fallback (openai) return `None` and let the
    /// caller's in-code table answer.
    #[must_use]
    pub fn rates_for(
        &self,
        canonical: Option<&str>,
        provider: &str,
        at_ts: Option<&str>,
    ) -> Option<Rates> {
        let entry = match canonical.filter(|c| !c.is_empty()) {
            Some(family) => self.entry(provider, family),
            None => None,
        };
        let entry = match entry {
            Some(e) => Some(e),
            None => self
                .fallback_family(provider)
                .and_then(|fb| self.entry(provider, fb)),
        }?;
        let price = Self::select_price(&entry.price, at_ts)?;
        Some((
            price.input,
            price.output,
            price.cache_write,
            price.cache_read,
        ))
    }

    /// The family's priority/fast-tier multiplier, or `None`. Port of
    /// `fast_multiplier` — including `float(mult) if mult else None`, so a
    /// declared `0.0` reads as "no premium".
    #[must_use]
    pub fn fast_multiplier(&self, canonical: Option<&str>, provider: &str) -> Option<f64> {
        let entry = canonical
            .filter(|c| !c.is_empty())
            .and_then(|family| self.entry(provider, family))?;
        entry.fast_multiplier.filter(|m| *m != 0.0)
    }
}

/// Flatten the manifest into `price_book`-shaped rows. Port of
/// `manifest_price_book_rows()` — one row per (family, price row), with empty
/// strings standing in for the absent dates so they survive the table's NOT NULL
/// unique key.
#[must_use]
pub fn manifest_price_book_rows(manifest: &Manifest) -> Vec<super::price_book::PriceBookRow> {
    let mut rows = Vec::new();
    for m in manifest.models() {
        let (Some(provider), family) = (m.provider.as_deref(), m.family.as_str()) else {
            continue;
        };
        if provider.is_empty() || family.is_empty() {
            continue;
        }
        for price in &m.price {
            rows.push(super::price_book::PriceBookRow {
                provider: provider.to_string(),
                model: family.to_string(),
                effective_from: price.effective_from.clone().unwrap_or_default(),
                effective_until: price.effective_until.clone().unwrap_or_default(),
                input: price.input,
                output: price.output,
                cache_write: price.cache_write,
                cache_read: price.cache_read,
                source: super::price_book::SOURCE_MANIFEST.to_string(),
            });
        }
    }
    rows
}

fn dropped_label(entry: &Table) -> String {
    entry
        .get("family")
        .and_then(Value::as_str)
        .unwrap_or("<no family>")
        .to_string()
}

/// Port of `_valid_model` + the field extraction `_models()` performs.
fn validate_model(entry: &Table) -> Option<ModelEntry> {
    let family = entry.get("family").and_then(Value::as_str)?;
    if family.is_empty() {
        return None;
    }
    let prices_raw = entry.array("price")?;
    if prices_raw.is_empty() {
        return None;
    }
    let mut price = Vec::with_capacity(prices_raw.len());
    for row in prices_raw {
        let mut values = [0.0_f64; 4];
        for (i, field) in REQUIRED_PRICE_FIELDS.iter().enumerate() {
            values[i] = row.get(field).and_then(Value::as_number)?;
        }
        price.push(PriceRow {
            effective_from: row
                .get("effective_from")
                .and_then(Value::as_str)
                .map(str::to_string),
            effective_until: row
                .get("effective_until")
                .and_then(Value::as_str)
                .map(str::to_string),
            input: values[0],
            output: values[1],
            cache_write: values[2],
            cache_read: values[3],
        });
    }
    Some(ModelEntry {
        family: family.to_string(),
        provider: entry
            .get("provider")
            .and_then(Value::as_str)
            .map(str::to_string),
        match_tokens: string_list(entry.get("match")),
        ids: string_list(entry.get("ids")),
        // Python truthiness: `m.get("fallback")` — any truthy value counts.
        fallback: entry.get("fallback").is_some_and(truthy),
        fast_multiplier: entry.get("fast_multiplier").and_then(Value::as_number),
        display_name: entry
            .get("display_name")
            .and_then(Value::as_str)
            .map(str::to_string),
        price,
    })
}

/// Python truthiness for the value shapes a manifest can hold.
fn truthy(value: &Value) -> bool {
    match value {
        Value::Boolean(b) => *b,
        Value::Integer(i) => *i != 0,
        Value::Float(f) => *f != 0.0,
        Value::String(s) => !s.is_empty(),
        Value::Array(items) => !items.is_empty(),
        Value::Other(s) => !s.is_empty(),
    }
}

fn string_list(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// Port of `canonical_id_groups()`: only list-valued groups are kept, entries are
/// stringified, order is the file's.
fn read_canonical_id_groups(doc: &Table) -> Vec<(String, Vec<String>)> {
    let Some(groups) = doc.table("canonical_ids") else {
        return Vec::new();
    };
    groups
        .pairs()
        .iter()
        .filter_map(|(pricer, value)| {
            value.as_array().map(|items| {
                (
                    pricer.clone(),
                    items.iter().filter_map(stringify).collect::<Vec<String>>(),
                )
            })
        })
        .collect()
}

/// `str(i)` over the shapes a canonical-id list can hold.
fn stringify(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(s.clone()),
        Value::Integer(i) => Some(i.to_string()),
        Value::Boolean(b) => Some(if *b { "True" } else { "False" }.to_string()),
        Value::Other(s) => Some(s.clone()),
        Value::Float(_) | Value::Array(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
[[model]]
family = "OPUS_48"
provider = "anthropic"
match = ["opus", "4", "8"]
fast_multiplier = 6.0
  [[model.price]]
  input = 5.0
  output = 25.0
  cache_write = 6.25
  cache_read = 0.50

[[model]]
family = "SONNET_35"
provider = "anthropic"
match = ["sonnet", "3", "5"]
fallback = true
  [[model.price]]
  input = 3.0
  output = 15.0
  cache_write = 3.75
  cache_read = 0.30

[[model]]
family = "GPT_54"
provider = "openai"
ids = ["gpt-5.4"]
  [[model.price]]
  effective_until = "2026-04-26"
  input = 2.50
  output = 20.0
  cache_write = 0.0
  cache_read = 0.25
  [[model.price]]
  effective_from = "2026-04-26"
  input = 2.50
  output = 15.0
  cache_write = 0.0
  cache_read = 0.25

[canonical_ids]
anthropic = ["claude-opus-4-8"]
openai = ["gpt-5.4"]
"#;

    fn sample() -> Manifest {
        Manifest::from_str_manifest(SAMPLE).expect("parses")
    }

    #[test]
    fn canonicalize_matches_token_sets_then_falls_back() {
        let m = sample();
        assert_eq!(
            m.canonicalize("claude-opus-4-8", "anthropic").as_deref(),
            Some("OPUS_48")
        );
        // Unknown Claude id → the provider fallback, never None.
        assert_eq!(
            m.canonicalize("grok-4.5", "anthropic").as_deref(),
            Some("SONNET_35")
        );
        // Empty id → fallback.
        assert_eq!(
            m.canonicalize("", "anthropic").as_deref(),
            Some("SONNET_35")
        );
        // openai has no fallback family, so an unmatched id is None.
        assert_eq!(m.canonicalize("gpt-4o", "openai"), None);
        // Exact ids beat token matching.
        assert_eq!(
            m.canonicalize("GPT-5.4", "openai").as_deref(),
            Some("GPT_54")
        );
    }

    #[test]
    fn select_price_compares_whole_strings_not_day_prefixes() {
        let m = sample();
        let gpt = m
            .models()
            .iter()
            .find(|e| e.family == "GPT_54")
            .expect("GPT_54");
        // No at_ts → the open-ended row.
        assert_eq!(
            Manifest::select_price(&gpt.price, None).map(|p| p.output),
            Some(15.0)
        );
        // Before the boundary → the $20 era.
        assert_eq!(
            Manifest::select_price(&gpt.price, Some("2026-01-15")).map(|p| p.output),
            Some(20.0)
        );
        // On the boundary date → the new era (bound is exclusive).
        assert_eq!(
            Manifest::select_price(&gpt.price, Some("2026-04-26")).map(|p| p.output),
            Some(15.0)
        );
        // A timestamped at_ts on the boundary DAY: "2026-04-26T…" is NOT less
        // than "2026-04-26" as a whole string, so row 1 is skipped. This is the
        // asymmetry with the price book, which truncates to 10 chars first.
        assert_eq!(
            Manifest::select_price(&gpt.price, Some("2026-04-26T00:00:00+00:00")).map(|p| p.output),
            Some(15.0)
        );
        // …but the day BEFORE, timestamped, still lands in the old era.
        assert_eq!(
            Manifest::select_price(&gpt.price, Some("2026-04-25T23:59:59+00:00")).map(|p| p.output),
            Some(20.0)
        );
    }

    #[test]
    fn rates_and_fast_multiplier() {
        let m = sample();
        assert_eq!(
            m.rates_for(Some("OPUS_48"), "anthropic", None),
            Some((5.0, 25.0, 6.25, 0.5))
        );
        assert_eq!(m.fast_multiplier(Some("OPUS_48"), "anthropic"), Some(6.0));
        assert_eq!(m.fast_multiplier(Some("SONNET_35"), "anthropic"), None);
        // Unknown family → the fallback family's rates for anthropic…
        assert_eq!(
            m.rates_for(Some("NOPE"), "anthropic", None),
            Some((3.0, 15.0, 3.75, 0.3))
        );
        // …and None for a provider with no fallback.
        assert_eq!(m.rates_for(Some("NOPE"), "openai", None), None);
    }

    #[test]
    fn malformed_entries_are_dropped_not_fatal() {
        let src = r#"
[[model]]
family = "GOOD"
provider = "anthropic"
  [[model.price]]
  input = 1.0
  output = 2.0
  cache_write = 3.0
  cache_read = 4.0

[[model]]
family = "NO_PRICE"
provider = "anthropic"

[[model]]
family = "BAD_ROW"
provider = "anthropic"
  [[model.price]]
  input = "free"
  output = 2.0
  cache_write = 3.0
  cache_read = 4.0

[[model]]
provider = "anthropic"
  [[model.price]]
  input = 1.0
  output = 2.0
  cache_write = 3.0
  cache_read = 4.0
"#;
        let m = Manifest::from_str_manifest(src).expect("parses");
        assert_eq!(m.models().len(), 1);
        assert_eq!(m.models()[0].family, "GOOD");
        assert_eq!(m.dropped().len(), 3);
    }

    #[test]
    fn canonical_ids_keep_group_and_listed_order() {
        let m = sample();
        assert_eq!(
            m.canonical_id_groups()
                .iter()
                .map(|(k, _)| k.as_str())
                .collect::<Vec<_>>(),
            vec!["anthropic", "openai"]
        );
        assert_eq!(m.canonical_ids(), vec!["claude-opus-4-8", "gpt-5.4"]);
    }

    #[test]
    fn by_family_takes_the_last_duplicate_fallback_takes_the_first() {
        let src = r#"
[[model]]
family = "DUP"
provider = "anthropic"
fallback = true
  [[model.price]]
  input = 1.0
  output = 1.0
  cache_write = 1.0
  cache_read = 1.0

[[model]]
family = "OTHER"
provider = "anthropic"
fallback = true
  [[model.price]]
  input = 9.0
  output = 9.0
  cache_write = 9.0
  cache_read = 9.0

[[model]]
family = "DUP"
provider = "anthropic"
  [[model.price]]
  input = 2.0
  output = 2.0
  cache_write = 2.0
  cache_read = 2.0
"#;
        let m = Manifest::from_str_manifest(src).expect("parses");
        // dict comprehension → last wins
        assert_eq!(
            m.rates_for(Some("DUP"), "anthropic", None),
            Some((2.0, 2.0, 2.0, 2.0))
        );
        // linear scan → first flagged wins
        assert_eq!(m.fallback_family("anthropic"), Some("DUP"));
    }
}
