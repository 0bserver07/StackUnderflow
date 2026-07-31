//! Hermes — port of `etl/normalize/hermes.py`.
//!
//! Each assistant message carries an explicit
//! `message.usage.{input,output,cacheRead,cacheWrite}` block; the mapping is
//! direct. Hermes's `_safe_int` is the one that is an `isinstance` ladder
//! rather than a `try/except`, so it never raises — see
//! [`super::row::hermes_safe_int`].

use stax_core::queries::pyjson::Value as PyValue;

use super::base::{EventSpec, NormalizeContext, Normalizer, UsageEvent};
use super::row::{
    MsgRow, PyRaise, as_dict, hermes_safe_int, int_or_zero, safe_load_raw, str_or_empty,
};
use super::text::{collect_fields, unwrap_envelope};
use crate::pricing::Tokens;

const DEFAULT_MODEL: &str = "hermes-auto";
const RAW_EXTRAS_FIELDS: [&str; 2] = ["provider", "agentName"];

/// The `hermes` normalizer.
#[derive(Debug, Clone, Copy, Default)]
pub struct HermesNormalizer;

impl Normalizer for HermesNormalizer {
    fn provider_name(&self) -> &'static str {
        "hermes"
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

fn canonical_tokens(row: &MsgRow) -> Result<Tokens, PyRaise> {
    if let Some(usage) = raw_usage(row) {
        return Ok(Tokens {
            input: hermes_safe_int(usage.get("input")),
            output: hermes_safe_int(usage.get("output")),
            cache_read: hermes_safe_int(usage.get("cacheRead")),
            cache_creation: hermes_safe_int(usage.get("cacheWrite")),
        });
    }
    Ok(Tokens {
        input: int_or_zero(row, "input_tokens")?,
        output: int_or_zero(row, "output_tokens")?,
        cache_read: int_or_zero(row, "cache_read_tokens")?,
        cache_creation: int_or_zero(row, "cache_create_tokens")?,
    })
}

/// The `usage` block: passed directly on the row, nested under `message`, or at
/// the payload's top level once unwrapped.
fn raw_usage(row: &MsgRow) -> Option<PyValue> {
    if let Some(direct) = as_dict(row.get("usage")) {
        return Some(direct.clone());
    }
    let payload = safe_load_raw(row.get("raw_json"))?;
    if !matches!(payload, PyValue::Object(_)) {
        return None;
    }
    if let Some(message) = as_dict(payload.get("message"))
        && let Some(usage) = as_dict(message.get("usage"))
    {
        return Some(usage.clone());
    }
    as_dict(payload.get("usage")).cloned()
}

fn extras_from_raw_json(raw_json: Option<&PyValue>) -> Option<PyValue> {
    let payload = safe_load_raw(raw_json)?;
    if !matches!(payload, PyValue::Object(_)) {
        return None;
    }
    let inner = unwrap_envelope(&payload, "message");
    let out = collect_fields(inner, &RAW_EXTRAS_FIELDS);
    (!out.is_empty()).then_some(PyValue::Object(out))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::normalize::base::CostSource;
    use crate::normalize::test_support::{assistant_row, ctx};

    fn hermes_row(usage: &str) -> MsgRow {
        assistant_row("hermes", "claude-sonnet-4-5-20250929").with(
            "raw_json",
            PyValue::Str(format!(
                r#"{{"message": {{"usage": {usage}, "provider": "anthropic"}}}}"#
            )),
        )
    }

    #[test]
    fn the_usage_block_maps_directly_onto_the_canonical_four() {
        let row = hermes_row(r#"{"input": 10, "output": 20, "cacheRead": 30, "cacheWrite": 40}"#);
        let events = HermesNormalizer.normalize(&ctx(), &row).unwrap();
        assert_eq!(events[0].input_tokens, 10);
        assert_eq!(events[0].output_tokens, 20);
        assert_eq!(events[0].cache_read_tokens, 30);
        assert_eq!(events[0].cache_create_tokens, 40);
        assert_eq!(events[0].cost_source, CostSource::RateCard);
        assert_eq!(
            events[0].raw_extras.as_deref(),
            Some(r#"{"provider": "anthropic"}"#)
        );
    }

    #[test]
    fn hermes_never_raises_on_a_garbage_usage_value() {
        let row = hermes_row(r#"{"input": "not a number", "output": [1, 2], "cacheRead": "7"}"#);
        let events = HermesNormalizer.normalize(&ctx(), &row).unwrap();
        assert_eq!(events[0].input_tokens, 0);
        assert_eq!(events[0].output_tokens, 0);
        assert_eq!(events[0].cache_read_tokens, 7);
    }

    #[test]
    fn an_all_zero_usage_block_yields_nothing() {
        let row = hermes_row(r#"{"input": 0, "output": 0}"#);
        assert!(HermesNormalizer.normalize(&ctx(), &row).unwrap().is_empty());
    }
}
