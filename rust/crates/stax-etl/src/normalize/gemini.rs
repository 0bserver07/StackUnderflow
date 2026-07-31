//! Gemini (Google) — port of `etl/normalize/gemini.py`.
//!
//! `usageMetadata` carries `promptTokenCount` **including** cached input, so
//! the canonical mapping subtracts: `input = prompt - cached`,
//! `output = candidates + thoughts`, `cache_read = cached`, `cache_create = 0`
//! (Gemini does not bill prompt-cache writes the same way).
//!
//! Two role spellings are accepted — `assistant` and `gemini` — because
//! different adapter versions wrote different ones and the normalizer must not
//! depend on which.

use stax_core::queries::pyjson::Value as PyValue;

use super::base::{EventSpec, NormalizeContext, Normalizer, UsageEvent};
use super::row::{
    MsgRow, PyRaise, as_dict, clamped_int_or_zero, int_or_zero, safe_load_raw, str_or_empty,
};
use super::text::extras_from_payload;
use crate::pricing::Tokens;

const DEFAULT_MODEL: &str = "gemini-auto";
const RAW_EXTRAS_FIELDS: [&str; 3] = ["responseId", "finishReason", "safetyRatings"];

/// The four raw keys, whichever shape they were found in.
const USAGE_KEYS: [&str; 4] = [
    "promptTokenCount",
    "candidatesTokenCount",
    "cachedContentTokenCount",
    "thoughtsTokenCount",
];

/// The `gemini` normalizer.
#[derive(Debug, Clone, Copy, Default)]
pub struct GeminiNormalizer;

impl Normalizer for GeminiNormalizer {
    fn provider_name(&self) -> &'static str {
        "gemini"
    }

    fn normalize(&self, ctx: &NormalizeContext, row: &MsgRow) -> Result<Vec<UsageEvent>, PyRaise> {
        let role = str_or_empty(row, "role");
        if role != "assistant" && role != "gemini" {
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

/// The canonical four keys, applying Gemini's cached-subtract + thoughts-fold.
///
/// The transform runs whenever a raw usage block is reachable; otherwise the
/// adapter-written columns are trusted verbatim.
fn canonical_tokens(row: &MsgRow) -> Result<Tokens, PyRaise> {
    if let Some(raw) = raw_gemini_usage(row) {
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

/// Gemini's usage block, from the row directly (synthetic path), from
/// `raw_json.usageMetadata` (JSONL ≥0.39), or from `raw_json.tokens` (the
/// friendlier ≤0.38 names, where `input` already has cached folded in and so
/// maps onto `promptTokenCount`).
fn raw_gemini_usage(row: &MsgRow) -> Option<PyValue> {
    if USAGE_KEYS.iter().any(|key| row.contains_key(key)) {
        // `msg_row.get(k, 0)` — a *missing* key defaults to 0, a present `None`
        // stays `None` and reaches `int(None or 0)`, which is 0 as well.
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
    if let Some(metadata) = as_dict(payload.get("usageMetadata")) {
        return Some(metadata.clone());
    }
    let tokens = as_dict(payload.get("tokens"))?;
    let has_friendly = ["input", "output", "cached", "thoughts"]
        .iter()
        .any(|key| tokens.get(key).is_some());
    if !has_friendly {
        return None;
    }
    // `tokens.get(k, 0) or 0` — falsy becomes 0 before `int()`.
    let coalesce = |key: &str| match tokens.get(key) {
        Some(value) if value.is_truthy() => value.clone(),
        _ => PyValue::Int(0),
    };
    Some(PyValue::Object(vec![
        ("promptTokenCount".to_string(), coalesce("input")),
        ("candidatesTokenCount".to_string(), coalesce("output")),
        ("cachedContentTokenCount".to_string(), coalesce("cached")),
        ("thoughtsTokenCount".to_string(), coalesce("thoughts")),
    ]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::normalize::base::CostSource;
    use crate::normalize::test_support::{assistant_row, ctx};

    fn gemini_row() -> MsgRow {
        assistant_row("gemini", "gemini-1.5-pro")
    }

    #[test]
    fn cached_is_subtracted_from_prompt_and_thoughts_fold_into_output() {
        let row = gemini_row().with(
            "raw_json",
            PyValue::Str(
                r#"{"usageMetadata": {"promptTokenCount": 1000,
                    "cachedContentTokenCount": 300, "candidatesTokenCount": 200,
                    "thoughtsTokenCount": 60}}"#
                    .into(),
            ),
        );
        let events = GeminiNormalizer.normalize(&ctx(), &row).unwrap();
        assert_eq!(events[0].input_tokens, 700);
        assert_eq!(events[0].output_tokens, 260);
        assert_eq!(events[0].cache_read_tokens, 300);
        assert_eq!(events[0].cache_create_tokens, 0);
        assert_eq!(events[0].cost_source, CostSource::RateCard);
    }

    #[test]
    fn cached_larger_than_prompt_clamps_input_to_zero_rather_than_going_negative() {
        let row = gemini_row().with(
            "raw_json",
            PyValue::Str(
                r#"{"usageMetadata": {"promptTokenCount": 10,
                    "cachedContentTokenCount": 99}}"#
                    .into(),
            ),
        );
        let events = GeminiNormalizer.normalize(&ctx(), &row).unwrap();
        assert_eq!(events[0].input_tokens, 0);
        assert_eq!(events[0].cache_read_tokens, 99);
    }

    #[test]
    fn the_older_friendly_token_names_map_onto_the_same_transform() {
        let row = gemini_row().with(
            "raw_json",
            PyValue::Str(
                r#"{"tokens": {"input": 500, "output": 40, "cached": 100, "thoughts": 5}}"#.into(),
            ),
        );
        let events = GeminiNormalizer.normalize(&ctx(), &row).unwrap();
        assert_eq!(events[0].input_tokens, 400);
        assert_eq!(events[0].output_tokens, 45);
    }

    #[test]
    fn both_role_spellings_are_billable() {
        for role in ["assistant", "gemini"] {
            let row = gemini_row()
                .with("role", PyValue::Str(role.into()))
                .with("input_tokens", PyValue::Int(10));
            assert_eq!(GeminiNormalizer.normalize(&ctx(), &row).unwrap().len(), 1);
        }
        let user = gemini_row()
            .with("role", PyValue::Str("user".into()))
            .with("input_tokens", PyValue::Int(10));
        assert!(
            GeminiNormalizer
                .normalize(&ctx(), &user)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn a_direct_usage_key_on_the_row_selects_the_raw_path() {
        let row = gemini_row()
            .with("promptTokenCount", PyValue::Int(80))
            .with("cachedContentTokenCount", PyValue::Int(30))
            // Columns the raw path must shadow.
            .with("input_tokens", PyValue::Int(999));
        let events = GeminiNormalizer.normalize(&ctx(), &row).unwrap();
        assert_eq!(events[0].input_tokens, 50);
    }
}
