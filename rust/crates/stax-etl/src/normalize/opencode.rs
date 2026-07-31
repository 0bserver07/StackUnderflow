//! OpenCode — port of `etl/normalize/opencode.py`.
//!
//! The per-turn payload lives in the `message.data` JSON column with tokens at
//! `data.tokens`: `{input, output, reasoning, cache: {read, write}}`. Reasoning
//! folds into output; the cache block splits into read/write. `cost` is
//! preserved as `embeddedCost` and not consumed.

use stax_core::queries::pyjson::Value as PyValue;

use super::base::{EventSpec, NormalizeContext, Normalizer, UsageEvent};
use super::row::{MsgRow, PyRaise, as_dict, int_or_zero, safe_int, safe_load_raw, str_or_empty};
use super::text::{collect_fields, unwrap_envelope};
use crate::pricing::Tokens;

const DEFAULT_MODEL: &str = "opencode-auto";
const RAW_EXTRAS_FIELDS: [&str; 3] = ["modelID", "providerID", "embeddedCost"];

/// The `opencode` normalizer.
#[derive(Debug, Clone, Copy, Default)]
pub struct OpenCodeNormalizer;

impl Normalizer for OpenCodeNormalizer {
    fn provider_name(&self) -> &'static str {
        "opencode"
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
                .raw_extras(extras_from_raw_json(row.get("raw_json"))),
            ),
        ])
    }
}

/// The canonical four with reasoning folded into output.
///
/// **`reasoning_tokens` stays 0 here** even though OpenCode reports a separable
/// count — unlike codex and droid, which surface theirs. That asymmetry is the
/// reference's, not a porting slip; it is recorded rather than corrected.
fn canonical_tokens(row: &MsgRow) -> Result<Tokens, PyRaise> {
    if let Some(raw) = raw_tokens(row) {
        let cache = as_dict(raw.get("cache"));
        return Ok(Tokens {
            input: safe_int(raw.get("input"))?,
            output: safe_int(raw.get("output"))? + safe_int(raw.get("reasoning"))?,
            cache_read: safe_int(cache.and_then(|c| c.get("read")))?,
            cache_creation: safe_int(cache.and_then(|c| c.get("write")))?,
        });
    }
    Ok(Tokens {
        input: int_or_zero(row, "input_tokens")?,
        output: int_or_zero(row, "output_tokens")?,
        cache_read: int_or_zero(row, "cache_read_tokens")?,
        cache_creation: int_or_zero(row, "cache_create_tokens")?,
    })
}

/// The tokens block: on the row, at the payload's top level, or one `data`
/// envelope down.
fn raw_tokens(row: &MsgRow) -> Option<PyValue> {
    if let Some(direct) = as_dict(row.get("tokens")) {
        return Some(direct.clone());
    }
    let payload = safe_load_raw(row.get("raw_json"))?;
    if !matches!(payload, PyValue::Object(_)) {
        return None;
    }
    if let Some(tokens) = as_dict(payload.get("tokens")) {
        return Some(tokens.clone());
    }
    let data = as_dict(payload.get("data"))?;
    as_dict(data.get("tokens")).cloned()
}

fn extras_from_raw_json(raw_json: Option<&PyValue>) -> Option<PyValue> {
    let payload = safe_load_raw(raw_json)?;
    if !matches!(payload, PyValue::Object(_)) {
        return None;
    }
    let inner = unwrap_envelope(&payload, "data");
    let mut out = collect_fields(inner, &RAW_EXTRAS_FIELDS);
    if let Some(cost) = inner.get("cost")
        && !matches!(cost, PyValue::Null)
        && !out.iter().any(|(name, _)| name == "embeddedCost")
    {
        out.push(("embeddedCost".to_string(), cost.clone()));
    }
    (!out.is_empty()).then_some(PyValue::Object(out))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::normalize::test_support::{assistant_row, ctx};

    fn opencode_row(data: &str) -> MsgRow {
        assistant_row("opencode", "claude-sonnet-4-5-20250929")
            .with("raw_json", PyValue::Str(data.to_string()))
    }

    #[test]
    fn reasoning_folds_into_output_and_the_cache_block_splits() {
        let row = opencode_row(
            r#"{"tokens": {"input": 100, "output": 20, "reasoning": 5,
                "cache": {"read": 7, "write": 9}}, "cost": 0.42,
                "modelID": "claude-sonnet-4-5-20250929"}"#,
        );
        let events = OpenCodeNormalizer.normalize(&ctx(), &row).unwrap();
        assert_eq!(events[0].input_tokens, 100);
        assert_eq!(events[0].output_tokens, 25);
        assert_eq!(events[0].cache_read_tokens, 7);
        assert_eq!(events[0].cache_create_tokens, 9);
        // Reported separably by the provider, deliberately not attributed here.
        assert_eq!(events[0].reasoning_tokens, 0);
        assert_eq!(
            events[0].raw_extras.as_deref(),
            Some(r#"{"modelID": "claude-sonnet-4-5-20250929", "embeddedCost": 0.42}"#)
        );
    }

    #[test]
    fn a_data_envelope_is_unwrapped_once() {
        let row = opencode_row(r#"{"data": {"tokens": {"input": 11}, "providerID": "anthropic"}}"#);
        let events = OpenCodeNormalizer.normalize(&ctx(), &row).unwrap();
        assert_eq!(events[0].input_tokens, 11);
        assert_eq!(
            events[0].raw_extras.as_deref(),
            Some(r#"{"providerID": "anthropic"}"#)
        );
    }

    #[test]
    fn a_missing_cache_block_is_zero_not_a_raise() {
        let row = opencode_row(r#"{"tokens": {"input": 3}}"#);
        let events = OpenCodeNormalizer.normalize(&ctx(), &row).unwrap();
        assert_eq!(events[0].cache_read_tokens, 0);
        assert_eq!(events[0].cache_create_tokens, 0);
    }
}
