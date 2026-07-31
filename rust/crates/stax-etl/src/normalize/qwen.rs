//! Qwen — port of `etl/normalize/qwen.py`.
//!
//! Qwen logs a Gemini-shaped `usageMetadata` block, and the canonical mapping
//! is identical to Gemini's. Python keeps it as a *parallel implementation*
//! rather than reusing `GeminiNormalizer` — the provenance keys differ (Qwen
//! surfaces `functionCall`, Gemini `finishReason`) and the pricer routing
//! differs — and this port keeps the same split for the same reason. It is not
//! the cline/kilocode case: there the two classes are provably one transform
//! because one inherits the other.
//!
//! One real difference from gemini beyond the keys: Qwen has **no** `tokens`
//! fallback shape, so an older-format payload falls through to the columns
//! rather than being re-mapped.

use stax_core::queries::pyjson::Value as PyValue;

use super::base::{EventSpec, NormalizeContext, Normalizer, UsageEvent};
use super::row::{
    MsgRow, PyRaise, as_dict, clamped_int_or_zero, int_or_zero, safe_load_raw, str_or_empty,
};
use super::text::extras_from_payload;
use crate::pricing::Tokens;

const DEFAULT_MODEL: &str = "qwen-auto";
const RAW_EXTRAS_FIELDS: [&str; 3] = ["uuid", "sessionId", "functionCall"];
const USAGE_KEYS: [&str; 4] = [
    "promptTokenCount",
    "candidatesTokenCount",
    "cachedContentTokenCount",
    "thoughtsTokenCount",
];

/// The `qwen` normalizer.
#[derive(Debug, Clone, Copy, Default)]
pub struct QwenNormalizer;

impl Normalizer for QwenNormalizer {
    fn provider_name(&self) -> &'static str {
        "qwen"
    }

    fn normalize(&self, ctx: &NormalizeContext, row: &MsgRow) -> Result<Vec<UsageEvent>, PyRaise> {
        if str_or_empty(row, "role") != "assistant" {
            return Ok(Vec::new());
        }

        let canonical = canonical_tokens(row)?;
        if canonical.input == 0
            && canonical.output == 0
            && canonical.cache_read == 0
            && canonical.cache_creation == 0
        {
            return Ok(Vec::new());
        }

        let model = match str_or_empty(row, "model") {
            empty if empty.is_empty() => DEFAULT_MODEL.to_string(),
            model => model,
        };
        let cost_source = ctx.rate_card_or_unknown(&model);

        Ok(vec![
            self.build_event(
                ctx,
                row,
                EventSpec::new(
                    canonical.input,
                    canonical.output,
                    canonical.cache_read,
                    canonical.cache_creation,
                    cost_source,
                )
                .model(model)
                .raw_extras(extras_from_payload(row.get("raw_json"), &RAW_EXTRAS_FIELDS)),
            ),
        ])
    }
}

fn canonical_tokens(row: &MsgRow) -> Result<Tokens, PyRaise> {
    if let Some(raw) = raw_usage_metadata(row) {
        let prompt = clamped_int_or_zero(raw.get("promptTokenCount"))?;
        let cached = clamped_int_or_zero(raw.get("cachedContentTokenCount"))?;
        let candidates = clamped_int_or_zero(raw.get("candidatesTokenCount"))?;
        let thoughts = clamped_int_or_zero(raw.get("thoughtsTokenCount"))?;
        return Ok(Tokens {
            input: (prompt - cached).max(0),
            output: candidates + thoughts,
            cache_read: cached,
            cache_creation: 0,
        });
    }
    Ok(Tokens {
        input: int_or_zero(row, "input_tokens")?,
        output: int_or_zero(row, "output_tokens")?,
        cache_read: int_or_zero(row, "cache_read_tokens")?,
        cache_creation: int_or_zero(row, "cache_create_tokens")?,
    })
}

fn raw_usage_metadata(row: &MsgRow) -> Option<PyValue> {
    if USAGE_KEYS.iter().any(|key| row.contains_key(key)) {
        return Some(PyValue::Object(
            USAGE_KEYS
                .iter()
                .map(|key| {
                    (
                        (*key).to_string(),
                        row.get(key).cloned().unwrap_or(PyValue::Int(0)),
                    )
                })
                .collect(),
        ));
    }
    let payload = safe_load_raw(row.get("raw_json"))?;
    if !matches!(payload, PyValue::Object(_)) {
        return None;
    }
    as_dict(payload.get("usageMetadata")).cloned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::normalize::base::CostSource;
    use crate::normalize::test_support::{assistant_row, ctx};

    fn qwen_row() -> MsgRow {
        assistant_row("qwen", "qwen-coder-plus")
    }

    #[test]
    fn the_gemini_shaped_transform_applies() {
        let row = qwen_row().with(
            "raw_json",
            PyValue::Str(
                r#"{"usageMetadata": {"promptTokenCount": 800,
                    "cachedContentTokenCount": 200, "candidatesTokenCount": 60,
                    "thoughtsTokenCount": 10}, "sessionId": "s"}"#
                    .into(),
            ),
        );
        let events = QwenNormalizer.normalize(&ctx(), &row).unwrap();
        assert_eq!(events[0].input_tokens, 600);
        assert_eq!(events[0].output_tokens, 70);
        assert_eq!(events[0].cache_read_tokens, 200);
        assert_eq!(events[0].cache_create_tokens, 0);
        assert_eq!(events[0].cost_source, CostSource::RateCard);
        assert_eq!(
            events[0].raw_extras.as_deref(),
            Some(r#"{"sessionId": "s"}"#)
        );
    }

    #[test]
    fn there_is_no_friendly_tokens_fallback_so_it_falls_to_the_columns() {
        let row = qwen_row()
            .with(
                "raw_json",
                PyValue::Str(r#"{"tokens": {"input": 500, "output": 40}}"#.into()),
            )
            .with("input_tokens", PyValue::Int(3));
        let events = QwenNormalizer.normalize(&ctx(), &row).unwrap();
        assert_eq!(events[0].input_tokens, 3);
    }

    #[test]
    fn a_non_assistant_role_yields_nothing_gemini_s_extra_spelling_included() {
        for role in ["user", "gemini", "tool"] {
            let row = qwen_row()
                .with("role", PyValue::Str(role.into()))
                .with("input_tokens", PyValue::Int(10));
            assert!(QwenNormalizer.normalize(&ctx(), &row).unwrap().is_empty());
        }
    }
}
