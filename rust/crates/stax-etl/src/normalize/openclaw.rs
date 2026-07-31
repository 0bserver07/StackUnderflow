//! OpenClaw — port of `etl/normalize/openclaw.py`.
//!
//! Same explicit-`usage`-block shape as hermes and pi, with one addition: the
//! block may carry a provider-embedded `cost`, which is preserved in
//! `raw_extras` as `embeddedCost` for cross-reference and **not** consumed —
//! every row re-prices through `compute_cost` so the marts read one number
//! computed one way.

use stax_core::queries::pyjson::Value as PyValue;

use super::base::{EventSpec, NormalizeContext, Normalizer, UsageEvent};
use super::row::{MsgRow, PyRaise, as_dict, int_or_zero, safe_int, safe_load_raw, str_or_empty};
use super::text::{collect_fields, unwrap_envelope};
use crate::pricing::Tokens;

const DEFAULT_MODEL: &str = "openclaw-auto";
const RAW_EXTRAS_FIELDS: [&str; 3] = ["provider", "agentName", "embeddedCost"];

/// The `openclaw` normalizer.
#[derive(Debug, Clone, Copy, Default)]
pub struct OpenClawNormalizer;

impl Normalizer for OpenClawNormalizer {
    fn provider_name(&self) -> &'static str {
        "openclaw"
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
            input: safe_int(usage.get("input"))?,
            output: safe_int(usage.get("output"))?,
            cache_read: safe_int(usage.get("cacheRead"))?,
            cache_creation: safe_int(usage.get("cacheWrite"))?,
        });
    }
    Ok(Tokens {
        input: int_or_zero(row, "input_tokens")?,
        output: int_or_zero(row, "output_tokens")?,
        cache_read: int_or_zero(row, "cache_read_tokens")?,
        cache_creation: int_or_zero(row, "cache_create_tokens")?,
    })
}

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

/// The keepsake sweep plus the embedded cost.
///
/// The `usage` block is looked up on the *parsed payload* directly rather than
/// through [`raw_usage`], which expects a `msg_row` shape — Python leaves a
/// comment saying exactly that, because the two lookups are one keystroke apart
/// and the wrong one silently finds nothing.
fn extras_from_raw_json(raw_json: Option<&PyValue>) -> Option<PyValue> {
    let payload = safe_load_raw(raw_json)?;
    if !matches!(payload, PyValue::Object(_)) {
        return None;
    }
    let inner = unwrap_envelope(&payload, "message");
    let mut out = collect_fields(inner, &RAW_EXTRAS_FIELDS);

    let usage = as_dict(inner.get("usage")).or_else(|| as_dict(payload.get("usage")));
    if let Some(usage) = usage
        && let Some(cost) = usage.get("cost")
        && !matches!(cost, PyValue::Null)
    {
        // `out["embeddedCost"] = cost` — an assignment, so a key already
        // collected by the sweep is REPLACED in place rather than skipped.
        match out.iter_mut().find(|(name, _)| name == "embeddedCost") {
            Some(slot) => slot.1 = cost.clone(),
            None => out.push(("embeddedCost".to_string(), cost.clone())),
        }
    }
    (!out.is_empty()).then_some(PyValue::Object(out))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::normalize::base::CostSource;
    use crate::normalize::test_support::{assistant_row, ctx};

    fn openclaw_row(usage: &str) -> MsgRow {
        assistant_row("openclaw", "claude-sonnet-4-5-20250929").with(
            "raw_json",
            PyValue::Str(format!(
                r#"{{"message": {{"usage": {usage}, "agentName": "claw"}}}}"#
            )),
        )
    }

    #[test]
    fn the_embedded_cost_is_preserved_and_not_consumed() {
        let row = openclaw_row(
            r#"{"input": 1000, "output": 100, "cacheRead": 5, "cacheWrite": 6,
                "cost": {"total": 99.0}}"#,
        );
        let events = OpenClawNormalizer.normalize(&ctx(), &row).unwrap();
        assert_eq!(events[0].input_tokens, 1000);
        assert_eq!(events[0].cache_create_tokens, 6);
        assert_eq!(
            events[0].raw_extras.as_deref(),
            Some(r#"{"agentName": "claw", "embeddedCost": {"total": 99.0}}"#)
        );
        // Re-priced, not taken from the payload.
        assert!(events[0].cost_usd > 0.0);
        assert!(events[0].cost_usd < 1.0);
        assert_eq!(events[0].cost_source, CostSource::RateCard);
    }

    #[test]
    fn garbage_usage_values_degrade_to_zero_and_can_drop_the_row() {
        let row = openclaw_row(r#"{"input": "abc", "output": null}"#);
        assert!(
            OpenClawNormalizer
                .normalize(&ctx(), &row)
                .unwrap()
                .is_empty()
        );
    }
}
