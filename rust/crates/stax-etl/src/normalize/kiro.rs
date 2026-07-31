//! Kiro (Amazon Kiro Agent) — port of `etl/normalize/kiro.py`.
//!
//! `.chat` files record execution metadata and a chat array but no token counts
//! on any role, so the canonical recovery is `len(content_text) // 4` onto
//! **input** with `estimated` stamped unconditionally. Both role spellings are
//! accepted: the source format says `bot`, the adapter may normalise to
//! `assistant`, and the normalizer must not depend on which version wrote the
//! row.

use super::base::{CostSource, EventSpec, NormalizeContext, Normalizer, UsageEvent};
use super::row::{MsgRow, PyRaise, int_or_zero, str_or_empty};
use super::text::{estimate_from_text, extras_from_payload};

const DEFAULT_MODEL: &str = "kiro-auto";
const RAW_EXTRAS_FIELDS: [&str; 4] = ["executionId", "actionId", "workflowId", "metadata"];

/// The `kiro` normalizer.
#[derive(Debug, Clone, Copy, Default)]
pub struct KiroNormalizer;

impl Normalizer for KiroNormalizer {
    fn provider_name(&self) -> &'static str {
        "kiro"
    }

    fn normalize(&self, ctx: &NormalizeContext, row: &MsgRow) -> Result<Vec<UsageEvent>, PyRaise> {
        let role = str_or_empty(row, "role");
        if role != "assistant" && role != "bot" {
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

    fn kiro_row() -> MsgRow {
        assistant_row("kiro", "claude-3-5-sonnet")
    }

    #[test]
    fn estimation_lands_on_the_input_side_unlike_groks() {
        let row = kiro_row().with("content_text", PyValue::Str("x".repeat(400)));
        let events = KiroNormalizer.normalize(&ctx(), &row).unwrap();
        assert_eq!((events[0].input_tokens, events[0].output_tokens), (100, 0));
        assert_eq!(events[0].cost_source, CostSource::Estimated);
    }

    #[test]
    fn both_role_spellings_are_billable() {
        for role in ["assistant", "bot"] {
            let row = kiro_row()
                .with("role", PyValue::Str(role.into()))
                .with("content_text", PyValue::Str("x".repeat(40)));
            assert_eq!(KiroNormalizer.normalize(&ctx(), &row).unwrap().len(), 1);
        }
        let human = kiro_row()
            .with("role", PyValue::Str("human".into()))
            .with("content_text", PyValue::Str("x".repeat(40)));
        assert!(KiroNormalizer.normalize(&ctx(), &human).unwrap().is_empty());
    }

    #[test]
    fn a_text_less_turn_yields_nothing() {
        assert!(
            KiroNormalizer
                .normalize(&ctx(), &kiro_row())
                .unwrap()
                .is_empty()
        );
    }
}
