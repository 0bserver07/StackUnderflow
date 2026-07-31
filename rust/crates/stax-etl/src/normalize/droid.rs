//! Droid (Factory) — port of `etl/normalize/droid.py`.
//!
//! The adapter has already distributed the session-level `tokenUsage` block
//! across the session's assistant messages, so by the time a row arrives its
//! columns hold a per-message slice of an exact session total. That is why
//! per-row counts stamp `rate_card`, not `estimated`: the estimation here is
//! *attribution*, not counting.
//!
//! `thinkingTokens` is folded into `output` — Droid bills thinking as output,
//! so keeping it inside `output_tokens` is what makes `cost_usd` correct — and
//! the same count rides along as `reasoning_tokens`, an additive-metadata
//! subset that is never priced.

use super::base::{CostSource, EventSpec, NormalizeContext, Normalizer, UsageEvent};
use super::row::{
    MsgRow, PyRaise, as_dict, clamped_int_or_zero, int_or_zero, safe_load_raw, str_or_empty,
};
use super::text::{estimate_from_text, extras_from_payload};

const DEFAULT_MODEL: &str = "droid-auto";
const RAW_EXTRAS_FIELDS: [&str; 3] = ["sessionId", "tokenUsage", "factoryVersion"];

/// The `droid` normalizer.
#[derive(Debug, Clone, Copy, Default)]
pub struct DroidNormalizer;

impl Normalizer for DroidNormalizer {
    fn provider_name(&self) -> &'static str {
        "droid"
    }

    fn normalize(&self, ctx: &NormalizeContext, row: &MsgRow) -> Result<Vec<UsageEvent>, PyRaise> {
        if str_or_empty(row, "role") != "assistant" {
            return Ok(Vec::new());
        }

        let mut input_tokens = int_or_zero(row, "input_tokens")?;
        let mut output_tokens = int_or_zero(row, "output_tokens")?;
        let cache_read = int_or_zero(row, "cache_read_tokens")?;
        let cache_create = int_or_zero(row, "cache_create_tokens")?;

        let mut thinking = int_or_zero(row, "thinking_tokens")?;
        if thinking == 0 {
            let payload = safe_load_raw(row.get("raw_json"));
            if let Some(payload) = payload.as_ref()
                && let Some(usage) = as_dict(payload.get("tokenUsage"))
            {
                thinking = clamped_int_or_zero(usage.get("thinkingTokens"))?;
            }
        }
        if thinking > 0 {
            output_tokens += thinking;
        }

        let mut estimated = false;
        if input_tokens == 0 && output_tokens == 0 && cache_read == 0 && cache_create == 0 {
            let text = str_or_empty(row, "content_text");
            if text.is_empty() {
                return Ok(Vec::new());
            }
            input_tokens = estimate_from_text(&text);
            estimated = true;
        }

        if input_tokens == 0 && output_tokens == 0 && cache_read == 0 && cache_create == 0 {
            return Ok(Vec::new());
        }

        let model = match str_or_empty(row, "model") {
            empty if empty.is_empty() => DEFAULT_MODEL.to_string(),
            model => model,
        };
        let cost_source = if estimated {
            CostSource::Estimated
        } else {
            ctx.rate_card_or_unknown(&model)
        };

        Ok(vec![
            self.build_event(
                ctx,
                row,
                EventSpec::new(
                    input_tokens,
                    output_tokens,
                    cache_read,
                    cache_create,
                    cost_source,
                )
                // NOTE: the reasoning count is carried EVEN on the estimated path,
                // where `output_tokens` was reset to 0 and the thinking fold is no
                // longer inside it. Reproduced as written.
                .reasoning(thinking)
                .model(model)
                .raw_extras(extras_from_payload(row.get("raw_json"), &RAW_EXTRAS_FIELDS)),
            ),
        ])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::normalize::test_support::{assistant_row, ctx};
    use stax_core::queries::pyjson::Value as PyValue;

    fn droid_row() -> MsgRow {
        assistant_row("droid", "claude-sonnet-4-5-20250929")
    }

    #[test]
    fn thinking_folds_into_output_and_is_also_attributed() {
        let row = droid_row()
            .with("input_tokens", PyValue::Int(100))
            .with("output_tokens", PyValue::Int(50))
            .with("thinking_tokens", PyValue::Int(30));
        let events = DroidNormalizer.normalize(&ctx(), &row).unwrap();
        assert_eq!(events[0].output_tokens, 80);
        assert_eq!(events[0].reasoning_tokens, 30);
    }

    #[test]
    fn thinking_is_recovered_from_the_token_usage_block_when_no_column_carries_it() {
        let row = droid_row().with("input_tokens", PyValue::Int(10)).with(
            "raw_json",
            PyValue::Str(r#"{"tokenUsage": {"thinkingTokens": 7}}"#.into()),
        );
        let events = DroidNormalizer.normalize(&ctx(), &row).unwrap();
        assert_eq!(events[0].output_tokens, 7);
        assert_eq!(events[0].reasoning_tokens, 7);
    }

    #[test]
    fn per_row_slices_stamp_rate_card_because_the_session_sum_is_exact() {
        let row = droid_row().with("cache_read_tokens", PyValue::Int(5));
        let events = DroidNormalizer.normalize(&ctx(), &row).unwrap();
        assert_eq!(events[0].cost_source, CostSource::RateCard);
    }

    #[test]
    fn only_a_row_with_no_token_data_at_all_estimates() {
        let row = droid_row().with("content_text", PyValue::Str("x".repeat(60)));
        let events = DroidNormalizer.normalize(&ctx(), &row).unwrap();
        assert_eq!(events[0].input_tokens, 15);
        assert_eq!(events[0].cost_source, CostSource::Estimated);
    }

    #[test]
    fn no_tokens_and_no_text_is_a_drop() {
        assert!(
            DroidNormalizer
                .normalize(&ctx(), &droid_row())
                .unwrap()
                .is_empty()
        );
    }
}
