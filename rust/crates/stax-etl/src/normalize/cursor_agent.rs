//! Cursor Agent — port of `etl/normalize/cursor_agent.py`.
//!
//! Neither on-disk shape (legacy marker plaintext, per-turn JSONL bubbles)
//! carries token counts, so the policy is unconditional: assistant rows
//! estimate `len(content_text) // 4` onto **input**, output stays 0, and the
//! stamp is always `estimated` — even when an adapter pre-computed counts,
//! because Cursor Agent never reports billing-grade numbers.
//!
//! **The registry key is `"cursor-agent"`, with the hyphen** — it must equal
//! the adapter's provider string. The hand-written registry block that shipped
//! this under the wrong key for months is the reason the Python registry became
//! self-discovering; the key is pinned by a test here and by the registry's own
//! parity test rather than left to a comment.

use super::base::{CostSource, EventSpec, NormalizeContext, Normalizer, UsageEvent};
use super::row::{MsgRow, PyRaise, int_or_zero, str_or_empty};
use super::text::{estimate_from_text, extras_from_payload};

const DEFAULT_MODEL: &str = "cursor-agent-auto";
const RAW_EXTRAS_FIELDS: [&str; 3] = ["conversationId", "transcriptType", "toolCalls"];

/// The `cursor-agent` normalizer.
#[derive(Debug, Clone, Copy, Default)]
pub struct CursorAgentNormalizer;

impl Normalizer for CursorAgentNormalizer {
    fn provider_name(&self) -> &'static str {
        "cursor-agent"
    }

    fn normalize(&self, ctx: &NormalizeContext, row: &MsgRow) -> Result<Vec<UsageEvent>, PyRaise> {
        if str_or_empty(row, "role") != "assistant" {
            return Ok(Vec::new());
        }

        let text = str_or_empty(row, "content_text");
        let mut input_tokens = int_or_zero(row, "input_tokens")?;
        let output_tokens = int_or_zero(row, "output_tokens")?;
        if input_tokens == 0 && output_tokens == 0 {
            if text.is_empty() {
                return Ok(Vec::new());
            }
            input_tokens = estimate_from_text(&text);
        }

        if input_tokens == 0 && output_tokens == 0 {
            return Ok(Vec::new());
        }

        let model = match str_or_empty(row, "model") {
            empty if empty.is_empty() => DEFAULT_MODEL.to_string(),
            model => model,
        };

        Ok(vec![
            self.build_event(
                ctx,
                row,
                EventSpec::new(input_tokens, output_tokens, 0, 0, CostSource::Estimated)
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

    fn agent_row() -> MsgRow {
        assistant_row("cursor-agent", "claude-sonnet-4-5-20250929")
    }

    #[test]
    fn the_key_carries_the_hyphen_the_adapter_writes() {
        assert_eq!(CursorAgentNormalizer.provider_name(), "cursor-agent");
    }

    #[test]
    fn estimation_is_unconditional_even_for_a_rate_card_model() {
        let row = agent_row().with("content_text", PyValue::Str("x".repeat(44)));
        let events = CursorAgentNormalizer.normalize(&ctx(), &row).unwrap();
        assert_eq!((events[0].input_tokens, events[0].output_tokens), (11, 0));
        assert_eq!(events[0].cost_source, CostSource::Estimated);
    }

    #[test]
    fn pre_computed_counts_are_kept_but_still_stamped_estimated() {
        let row = agent_row()
            .with("input_tokens", PyValue::Int(300))
            .with("output_tokens", PyValue::Int(20))
            .with("content_text", PyValue::Str("ignored".into()));
        let events = CursorAgentNormalizer.normalize(&ctx(), &row).unwrap();
        assert_eq!((events[0].input_tokens, events[0].output_tokens), (300, 20));
        assert_eq!(events[0].cost_source, CostSource::Estimated);
    }

    #[test]
    fn an_empty_turn_yields_nothing() {
        assert!(
            CursorAgentNormalizer
                .normalize(&ctx(), &agent_row())
                .unwrap()
                .is_empty()
        );
        let tiny = agent_row().with("content_text", PyValue::Str("abc".into()));
        assert!(
            CursorAgentNormalizer
                .normalize(&ctx(), &tiny)
                .unwrap()
                .is_empty()
        );
    }
}
