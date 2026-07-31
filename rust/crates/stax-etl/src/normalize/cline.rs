//! Cline and its two forks — port of `etl/normalize/cline.py`,
//! `kilocode.py` and `roocode.py`.
//!
//! **One transform, three registry keys.** `KiloCodeNormalizer` and
//! `RooCodeNormalizer` are `class X(ClineNormalizer)` bodies that override
//! nothing but `provider_name`, so this module is one struct carrying the key
//! as a field rather than three copies of the parser. The key is not cosmetic:
//! it is the pricer key `_compute_cost_usd` routes through
//! (`provider=self.provider_name`), and it is the `provider` column's fallback.
//!
//! The token source of truth is the `api_req_started.text` blob — a
//! JSON-*stringified* `{tokensIn, tokensOut, cacheWrites, cacheReads, cost}`
//! nested inside `raw_json` — parsed here so the normalizer stays authoritative
//! even when an upgrade path left stale column values on the row. Cline's own
//! pre-computed `cost` is preserved in `raw_extras` for cross-reference and
//! deliberately **not** consumed: every provider re-prices through
//! `compute_cost` so the marts read one number computed one way.

use stax_core::queries::pyjson::Value as PyValue;

use super::base::{EventSpec, NormalizeContext, Normalizer, UsageEvent};
use super::row::{
    MsgRow, PyRaise, as_nonempty_str, dict_get, int_or_zero, safe_int, safe_load_raw,
    safe_parse_json, str_or_empty,
};

/// Keep aligned with the adapter's `_DEFAULT_MODEL`.
const DEFAULT_MODEL: &str = "cline-auto";

/// Cline embeds the upstream-computed cost on every `api_req_started` event;
/// preserved for debugging, never consumed.
const RAW_EXTRAS_FIELDS: [&str; 3] = ["cost", "request", "apiProtocol"];

/// The canonical four tokens as `_parse_api_req_tokens` returns them.
struct ClineTokens {
    input: i64,
    output: i64,
    cache_read: i64,
    cache_create: i64,
}

/// The Cline-family transform. One struct, three registered keys.
#[derive(Debug, Clone, Copy)]
pub struct ClineNormalizer {
    provider: &'static str,
}

impl ClineNormalizer {
    /// `provider_name = "cline"`.
    #[must_use]
    pub const fn cline() -> Self {
        Self { provider: "cline" }
    }

    /// `class KiloCodeNormalizer(ClineNormalizer)` — same transform, own key,
    /// own pricer route (the pricer map sends `kilocode` to Anthropic).
    #[must_use]
    pub const fn kilocode() -> Self {
        Self {
            provider: "kilocode",
        }
    }

    /// `class RooCodeNormalizer(ClineNormalizer)`.
    #[must_use]
    pub const fn roocode() -> Self {
        Self {
            provider: "roocode",
        }
    }
}

impl Normalizer for ClineNormalizer {
    fn provider_name(&self) -> &'static str {
        self.provider
    }

    fn normalize(&self, ctx: &NormalizeContext, row: &MsgRow) -> Result<Vec<UsageEvent>, PyRaise> {
        if str_or_empty(row, "role") != "assistant" {
            return Ok(Vec::new());
        }

        // `_parse_api_req_tokens` returns `None` only in the docstring: every
        // branch returns a dict, so the `if tokens is None: return` guard below
        // it is dead on arrival. Ported as written — the shape is what a future
        // edit would restore, and pretending it does not exist would hide that.
        let tokens = parse_api_req_tokens(row)?;

        if tokens.input == 0
            && tokens.output == 0
            && tokens.cache_read == 0
            && tokens.cache_create == 0
        {
            return Ok(Vec::new());
        }

        let model = match str_or_empty(row, "model") {
            empty if empty.is_empty() => DEFAULT_MODEL.to_string(),
            model => model,
        };
        let cost_source = ctx.rate_card_or_unknown(&model);
        let raw_extras = extras_from_raw_json(row.get("raw_json"));

        Ok(vec![
            self.build_event(
                ctx,
                row,
                EventSpec::new(
                    tokens.input,
                    tokens.output,
                    tokens.cache_read,
                    tokens.cache_create,
                    cost_source,
                )
                .model(model)
                .raw_extras(raw_extras),
            ),
        ])
    }
}

/// Canonical four tokens from the `api_req_started` event.
///
/// Resolution order: the event's stringified `text` payload (on-disk truth),
/// then a `text` field passed directly by a synthetic row, then the columns the
/// adapter already wrote.
fn parse_api_req_tokens(row: &MsgRow) -> Result<ClineTokens, PyRaise> {
    let parsed = extract_text_field(row).and_then(|text| stax_core::queries::pyjson::loads(&text));
    if let Some(parsed) = parsed.as_ref()
        && matches!(parsed, PyValue::Object(_))
    {
        return Ok(ClineTokens {
            input: safe_int(parsed.get("tokensIn"))?,
            output: safe_int(parsed.get("tokensOut"))?,
            cache_read: safe_int(parsed.get("cacheReads"))?,
            cache_create: safe_int(parsed.get("cacheWrites"))?,
        });
    }
    Ok(ClineTokens {
        input: int_or_zero(row, "input_tokens")?,
        output: int_or_zero(row, "output_tokens")?,
        cache_read: int_or_zero(row, "cache_read_tokens")?,
        cache_create: int_or_zero(row, "cache_create_tokens")?,
    })
}

/// The `api_req_started.text` JSON string, or `None`.
fn extract_text_field(row: &MsgRow) -> Option<String> {
    if let Some(direct) = as_nonempty_str(row.get("text")) {
        return Some(direct.to_string());
    }
    let payload = safe_load_raw(row.get("raw_json"))?;
    let text = dict_get(&payload, "text")?;
    as_nonempty_str(Some(text)).map(ToString::to_string)
}

/// `cost` (from the nested `text` blob or the payload) plus `request` /
/// `apiProtocol`, in the order Python inserts them.
fn extras_from_raw_json(raw_json: Option<&PyValue>) -> Option<PyValue> {
    let payload = safe_load_raw(raw_json)?;
    if !matches!(payload, PyValue::Object(_)) {
        return None;
    }

    let mut out: Vec<(String, PyValue)> = Vec::new();
    let parsed_text = safe_parse_json(payload.get("text"));
    if let Some(parsed_text) = parsed_text.as_ref()
        && matches!(parsed_text, PyValue::Object(_))
    {
        if let Some(cost) = parsed_text.get("cost")
            && *cost != PyValue::Null
        {
            out.push(("cost".to_string(), cost.clone()));
        }
        for key in RAW_EXTRAS_FIELDS {
            if key == "cost" {
                continue;
            }
            // `if key in parsed_text` — presence, so an explicit `null` counts.
            if let Some(value) = parsed_text.get(key) {
                out.push((key.to_string(), value.clone()));
            }
        }
    }

    for key in RAW_EXTRAS_FIELDS {
        let Some(value) = payload.get(key) else {
            continue;
        };
        if *value == PyValue::Null || out.iter().any(|(name, _)| name == key) {
            continue;
        }
        out.push((key.to_string(), value.clone()));
    }

    (!out.is_empty()).then_some(PyValue::Object(out))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::normalize::base::CostSource;
    use crate::normalize::test_support::{assistant_row, ctx};

    fn cline_row(text: &str) -> MsgRow {
        assistant_row("cline", "claude-sonnet-4-5-20250929").with(
            "raw_json",
            PyValue::Str(format!(r#"{{"text": {}}}"#, json_string(text))),
        )
    }

    fn json_string(text: &str) -> String {
        stax_core::queries::pyjson::dumps_compact(&PyValue::Str(text.to_string()))
    }

    #[test]
    fn tokens_come_from_the_stringified_text_blob_not_the_columns() {
        let row = cline_row(
            r#"{"tokensIn": 111, "tokensOut": 22, "cacheReads": 3, "cacheWrites": 4, "cost": 0.5}"#,
        )
        // Stale columns the parser must ignore.
        .with("input_tokens", PyValue::Int(999_999));
        let events = ClineNormalizer::cline().normalize(&ctx(), &row).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].input_tokens, 111);
        assert_eq!(events[0].output_tokens, 22);
        assert_eq!(events[0].cache_read_tokens, 3);
        assert_eq!(events[0].cache_create_tokens, 4);
        // The upstream cost is preserved, not consumed.
        assert_eq!(events[0].raw_extras.as_deref(), Some(r#"{"cost": 0.5}"#));
        assert!(events[0].cost_usd > 0.0);
    }

    #[test]
    fn a_malformed_text_blob_falls_back_to_the_columns() {
        let row = cline_row("not json at all")
            .with("input_tokens", PyValue::Int(10))
            .with("output_tokens", PyValue::Int(20));
        let events = ClineNormalizer::cline().normalize(&ctx(), &row).unwrap();
        assert_eq!(events[0].input_tokens, 10);
        assert_eq!(events[0].output_tokens, 20);
    }

    #[test]
    fn negative_wire_counts_clamp_to_zero_and_can_zero_the_whole_row() {
        let row = cline_row(r#"{"tokensIn": -5, "tokensOut": -5}"#);
        assert!(
            ClineNormalizer::cline()
                .normalize(&ctx(), &row)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn the_default_model_stands_in_for_a_blank_column() {
        let row = cline_row(r#"{"tokensIn": 5}"#).with("model", PyValue::Null);
        let events = ClineNormalizer::cline().normalize(&ctx(), &row).unwrap();
        assert_eq!(events[0].model, DEFAULT_MODEL);
        // `cline-auto` is a first-party rate-card id, so this is not `unknown`.
        assert_eq!(events[0].cost_source, CostSource::RateCard);
    }

    #[test]
    fn the_three_keys_share_one_transform_and_differ_only_in_routing() {
        let row = cline_row(r#"{"tokensIn": 100, "tokensOut": 10}"#);
        let ctx = ctx();
        let cline = ClineNormalizer::cline().normalize(&ctx, &row).unwrap();
        let kilo = ClineNormalizer::kilocode().normalize(&ctx, &row).unwrap();
        let roo = ClineNormalizer::roocode().normalize(&ctx, &row).unwrap();
        for other in [&kilo, &roo] {
            assert_eq!(other[0].input_tokens, cline[0].input_tokens);
            assert_eq!(other[0].output_tokens, cline[0].output_tokens);
            // Cline-family extensions all run against the user's Anthropic key,
            // so the pricer map routes all three to the same rates.
            assert_eq!(other[0].cost_usd.to_bits(), cline[0].cost_usd.to_bits());
        }
        // The provider column comes off the ROW, so it agrees here; the
        // difference the keys make is the pricer route and the fallback.
        assert_eq!(
            ClineNormalizer::kilocode()
                .normalize(&ctx, &row.clone().with("provider", PyValue::Null))
                .unwrap()[0]
                .provider,
            "kilocode"
        );
    }

    #[test]
    fn non_assistant_rows_yield_nothing() {
        let row = cline_row(r#"{"tokensIn": 100}"#).with("role", PyValue::Str("user".into()));
        assert!(
            ClineNormalizer::cline()
                .normalize(&ctx(), &row)
                .unwrap()
                .is_empty()
        );
    }
}
