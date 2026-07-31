//! Continue (continue.dev) — port of `etl/normalize/continue_.py`.
//!
//! Token counts may or may not be present per row depending on the Continue
//! version and the underlying gateway, so the policy is: trust the canonical
//! columns when any of the four is non-zero; otherwise recover
//! `len(content_text) // 4` on the input side and stamp `estimated`. A row with
//! neither tokens nor text is dropped.
//!
//! Module named `continue_ext` because `continue` is a Rust keyword — the same
//! reason Python's file is `continue_.py`. The registry key is `"continue"`.

use super::base::{CostSource, EventSpec, NormalizeContext, Normalizer, UsageEvent};
use super::row::{MsgRow, PyRaise, int_or_zero, str_or_empty};
use super::text::estimate_from_text;

const DEFAULT_MODEL: &str = "continue-auto";
const RAW_EXTRAS_FIELDS: [&str; 3] = ["provider", "modelTitle", "completionOptions"];

/// The `continue` normalizer.
#[derive(Debug, Clone, Copy, Default)]
pub struct ContinueNormalizer;

impl Normalizer for ContinueNormalizer {
    fn provider_name(&self) -> &'static str {
        "continue"
    }

    fn normalize(&self, ctx: &NormalizeContext, row: &MsgRow) -> Result<Vec<UsageEvent>, PyRaise> {
        if str_or_empty(row, "role") != "assistant" {
            return Ok(Vec::new());
        }

        let mut input_tokens = int_or_zero(row, "input_tokens")?;
        let output_tokens = int_or_zero(row, "output_tokens")?;
        let cache_read = int_or_zero(row, "cache_read_tokens")?;
        let cache_create = int_or_zero(row, "cache_create_tokens")?;

        let mut estimated = false;
        if input_tokens == 0 && output_tokens == 0 && cache_read == 0 && cache_create == 0 {
            let text = str_or_empty(row, "content_text");
            if text.is_empty() {
                return Ok(Vec::new()); // nothing to estimate from — drop
            }
            input_tokens = estimate_from_text(&text);
            estimated = true;
        }

        // The second guard is not redundant: `len(text) // 4` is 0 for text
        // shorter than four characters, so a one-character reply lands here.
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
        let raw_extras = super::text::extras_from_payload(row.get("raw_json"), &RAW_EXTRAS_FIELDS);

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
                .model(model)
                .raw_extras(raw_extras),
            ),
        ])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::normalize::test_support::{assistant_row, ctx};
    use stax_core::queries::pyjson::Value as PyValue;

    fn continue_row() -> MsgRow {
        assistant_row("continue", "claude-sonnet-4-5-20250929")
    }

    #[test]
    fn explicit_columns_are_trusted_and_stamp_rate_card() {
        let row = continue_row()
            .with("input_tokens", PyValue::Int(120))
            .with("output_tokens", PyValue::Int(30));
        let events = ContinueNormalizer.normalize(&ctx(), &row).unwrap();
        assert_eq!(events[0].input_tokens, 120);
        assert_eq!(events[0].cost_source, CostSource::RateCard);
    }

    #[test]
    fn a_token_less_row_estimates_on_the_input_side_and_stamps_estimated() {
        let row = continue_row().with("content_text", PyValue::Str("x".repeat(41)));
        let events = ContinueNormalizer.normalize(&ctx(), &row).unwrap();
        assert_eq!(events[0].input_tokens, 10); // 41 // 4
        assert_eq!(events[0].output_tokens, 0);
        assert_eq!(events[0].cost_source, CostSource::Estimated);
    }

    #[test]
    fn neither_tokens_nor_text_is_a_drop_and_so_is_text_shorter_than_four() {
        assert!(
            ContinueNormalizer
                .normalize(&ctx(), &continue_row())
                .unwrap()
                .is_empty()
        );
        let tiny = continue_row().with("content_text", PyValue::Str("ab".into()));
        assert!(
            ContinueNormalizer
                .normalize(&ctx(), &tiny)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn an_estimated_row_stays_estimated_even_for_a_rate_card_model() {
        let row = continue_row().with("content_text", PyValue::Str("x".repeat(400)));
        let events = ContinueNormalizer.normalize(&ctx(), &row).unwrap();
        assert_eq!(events[0].cost_source, CostSource::Estimated);
        // Estimated is a provenance stamp, not a price suppressor: the model is
        // known, so the row still costs money.
        assert!(events[0].cost_usd > 0.0);
    }
}
