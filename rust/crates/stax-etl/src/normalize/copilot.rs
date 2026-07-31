//! GitHub Copilot — port of `etl/normalize/copilot.py`.
//!
//! Two on-disk shapes: legacy `events.jsonl` (`outputTokens` present,
//! `inputTokens` often not) and VS Code transcripts (both explicit). The
//! recovery ladder is the most convoluted in the package and is ported branch
//! for branch:
//!
//! * both columns zero → look for `raw_json.data.{inputTokens,outputTokens}`;
//! * `output_tokens == 0` → estimate. With text: `output = len // 4`, and when
//!   input is *also* zero, `input = output` — "use the same text rather than
//!   zero so the row prices to *something*", which is a comment in the original
//!   and a real doubling of a legacy row's billed input;
//! * `output_tokens == 0` and no text but a non-zero input → keep the input,
//!   leave output at 0, and still stamp `estimated`. That is the "weird
//!   half-shape" branch: nothing was estimated, but the stamp says it was.
//!
//! Cache columns stay 0 — Copilot's transcripts do not bill prompt caching.

use stax_core::queries::pyjson::Value as PyValue;

use super::base::{CostSource, EventSpec, NormalizeContext, Normalizer, UsageEvent};
use super::row::{
    MsgRow, PyRaise, as_dict, clamped_int_or_zero, int_or_zero, safe_load_raw, str_or_empty,
};
use super::text::{collect_fields, estimate_from_text, keepsake_worthy};

const DEFAULT_MODEL: &str = "copilot-auto";
const RAW_EXTRAS_FIELDS: [&str; 3] = ["toolCallId", "producer", "transcriptVersion"];

/// The `copilot` normalizer.
#[derive(Debug, Clone, Copy, Default)]
pub struct CopilotNormalizer;

impl Normalizer for CopilotNormalizer {
    fn provider_name(&self) -> &'static str {
        "copilot"
    }

    fn normalize(&self, ctx: &NormalizeContext, row: &MsgRow) -> Result<Vec<UsageEvent>, PyRaise> {
        if str_or_empty(row, "role") != "assistant" {
            return Ok(Vec::new());
        }

        let mut input_tokens = int_or_zero(row, "input_tokens")?;
        let mut output_tokens = int_or_zero(row, "output_tokens")?;

        // Newer transcripts nest the counts the adapter may not have flattened.
        if input_tokens == 0 && output_tokens == 0 {
            let payload = safe_load_raw(row.get("raw_json"));
            if let Some(payload) = payload.as_ref()
                && let Some(data) = as_dict(payload.get("data"))
            {
                input_tokens = clamped_int_or_zero(data.get("inputTokens"))?;
                output_tokens = clamped_int_or_zero(data.get("outputTokens"))?;
            }
        }

        let mut estimated = false;
        if output_tokens == 0 {
            let text = str_or_empty(row, "content_text");
            if input_tokens == 0 && text.is_empty() {
                return Ok(Vec::new());
            }
            if text.is_empty() {
                // Explicit input, no output, nothing to estimate from. Nothing
                // is actually estimated here — the stamp is still `estimated`.
                estimated = true;
            } else {
                output_tokens = estimate_from_text(&text);
                if input_tokens == 0 {
                    input_tokens = output_tokens;
                }
                estimated = true;
            }
        }

        if input_tokens == 0 && output_tokens == 0 {
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
                EventSpec::new(input_tokens, output_tokens, 0, 0, cost_source)
                    .model(model)
                    .raw_extras(extras_from_raw_json(row.get("raw_json"))),
            ),
        ])
    }
}

/// The shared keepsake sweep plus `data.producer` from VS Code transcripts.
fn extras_from_raw_json(raw_json: Option<&PyValue>) -> Option<PyValue> {
    let payload = safe_load_raw(raw_json)?;
    if !matches!(payload, PyValue::Object(_)) {
        return None;
    }
    let mut out = collect_fields(&payload, &RAW_EXTRAS_FIELDS);
    if let Some(data) = as_dict(payload.get("data"))
        && let Some(producer) = data.get("producer")
        // `if producer and "producer" not in out` — truthiness here, unlike the
        // `is not None and != ""` the sweep above uses.
        && producer.is_truthy()
        && !out.iter().any(|(name, _)| name == "producer")
    {
        out.push(("producer".to_string(), producer.clone()));
    }
    // Defensive: the sweep's guard admits values the truthiness guard would
    // not, so re-checking here would change the sweep. It does not — this is
    // only a reminder that the two guards differ by design.
    debug_assert!(out.iter().all(|(_, value)| keepsake_worthy(value)));
    (!out.is_empty()).then_some(PyValue::Object(out))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::normalize::test_support::{assistant_row, ctx};

    fn copilot_row() -> MsgRow {
        assistant_row("copilot", "claude-sonnet-4-5-20250929")
    }

    #[test]
    fn explicit_both_sides_are_trusted() {
        let row = copilot_row()
            .with("input_tokens", PyValue::Int(100))
            .with("output_tokens", PyValue::Int(40));
        let events = CopilotNormalizer.normalize(&ctx(), &row).unwrap();
        assert_eq!((events[0].input_tokens, events[0].output_tokens), (100, 40));
        assert_eq!(events[0].cost_source, CostSource::RateCard);
    }

    #[test]
    fn the_nested_data_block_is_picked_up_when_the_columns_are_empty() {
        let row = copilot_row().with(
            "raw_json",
            PyValue::Str(r#"{"data": {"inputTokens": 7, "outputTokens": 9}}"#.into()),
        );
        let events = CopilotNormalizer.normalize(&ctx(), &row).unwrap();
        assert_eq!((events[0].input_tokens, events[0].output_tokens), (7, 9));
        assert_eq!(events[0].cost_source, CostSource::RateCard);
    }

    #[test]
    fn a_legacy_row_estimates_output_and_copies_it_onto_input() {
        let row = copilot_row().with("content_text", PyValue::Str("x".repeat(40)));
        let events = CopilotNormalizer.normalize(&ctx(), &row).unwrap();
        assert_eq!(events[0].output_tokens, 10);
        assert_eq!(events[0].input_tokens, 10, "the doubling is the contract");
        assert_eq!(events[0].cost_source, CostSource::Estimated);
    }

    #[test]
    fn the_weird_half_shape_keeps_its_input_and_is_still_stamped_estimated() {
        let row = copilot_row().with("input_tokens", PyValue::Int(50));
        let events = CopilotNormalizer.normalize(&ctx(), &row).unwrap();
        assert_eq!((events[0].input_tokens, events[0].output_tokens), (50, 0));
        assert_eq!(events[0].cost_source, CostSource::Estimated);
    }

    #[test]
    fn no_tokens_and_no_text_is_a_drop() {
        assert!(
            CopilotNormalizer
                .normalize(&ctx(), &copilot_row())
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn producer_is_lifted_out_of_the_data_block() {
        let row = copilot_row()
            .with("input_tokens", PyValue::Int(5))
            .with("output_tokens", PyValue::Int(5))
            .with(
                "raw_json",
                PyValue::Str(r#"{"transcriptVersion": 2, "data": {"producer": "vscode"}}"#.into()),
            );
        let events = CopilotNormalizer.normalize(&ctx(), &row).unwrap();
        assert_eq!(
            events[0].raw_extras.as_deref(),
            Some(r#"{"transcriptVersion": 2, "producer": "vscode"}"#)
        );
    }
}
