//! Pi (and OMP) — port of `etl/normalize/pi.py`.
//!
//! One transform, **two registry keys**: `provider_aliases = ("omp",)` on the
//! Python class registers `PiNormalizer` under `omp` as well, because Pi and
//! OMP differ only in their on-disk root — an adapter concern.
//!
//! The alias has a consequence worth stating: `provider_name` stays `"pi"` for
//! an OMP row, so the row prices through **Pi's** pricer while the event's
//! `provider` column still reads `"omp"` (it comes from the row, not the
//! class). Both halves are pinned by [`tests::an_omp_row_prices_as_pi_but_is_stamped_omp`].

use stax_core::queries::pyjson::Value as PyValue;

use super::base::{EventSpec, NormalizeContext, Normalizer, UsageEvent};
use super::row::{MsgRow, PyRaise, as_dict, int_or_zero, safe_int, safe_load_raw, str_or_empty};
use super::text::{keepsake_worthy, unwrap_envelope};
use crate::pricing::Tokens;

const DEFAULT_MODEL: &str = "gpt-5";
const RAW_EXTRAS_FIELDS: [&str; 3] = ["responseId", "sessionId", "cwd"];

/// The `pi` normalizer — also registered under `omp`.
#[derive(Debug, Clone, Copy, Default)]
pub struct PiNormalizer;

impl PiNormalizer {
    /// The extra provider strings this transform is registered under.
    pub const PROVIDER_ALIASES: [&'static str; 1] = ["omp"];
}

impl Normalizer for PiNormalizer {
    fn provider_name(&self) -> &'static str {
        "pi"
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

/// Pi's keepsake sweep is the only one that looks in **two** places per key:
/// the unwrapped `message` envelope first, then the outer payload.
fn extras_from_raw_json(raw_json: Option<&PyValue>) -> Option<PyValue> {
    let payload = safe_load_raw(raw_json)?;
    if !matches!(payload, PyValue::Object(_)) {
        return None;
    }
    let inner = unwrap_envelope(&payload, "message");
    let mut out: Vec<(String, PyValue)> = Vec::new();
    for key in RAW_EXTRAS_FIELDS {
        // `val = inner.get(key); if val is None: val = payload.get(key)` — the
        // fallback triggers on `None`, which a *present* null also satisfies.
        let value = match inner.get(key) {
            Some(value) if !matches!(value, PyValue::Null) => Some(value),
            _ => payload.get(key),
        };
        if let Some(value) = value
            && keepsake_worthy(value)
        {
            out.push((key.to_string(), value.clone()));
        }
    }
    (!out.is_empty()).then_some(PyValue::Object(out))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::normalize::base::CostSource;
    use crate::normalize::test_support::{assistant_row, ctx};

    fn pi_row(usage: &str) -> MsgRow {
        assistant_row("pi", "gpt-5").with(
            "raw_json",
            PyValue::Str(format!(
                r#"{{"sessionId": "outer", "message": {{"usage": {usage}, "responseId": "r-1"}}}}"#
            )),
        )
    }

    #[test]
    fn the_usage_block_maps_directly_and_extras_look_in_two_places() {
        let row = pi_row(r#"{"input": 90, "output": 10, "cacheRead": 1, "cacheWrite": 2}"#);
        let events = PiNormalizer.normalize(&ctx(), &row).unwrap();
        assert_eq!(events[0].input_tokens, 90);
        assert_eq!(events[0].cache_create_tokens, 2);
        assert_eq!(events[0].cost_source, CostSource::RateCard);
        // `responseId` off the envelope, `sessionId` off the outer payload.
        assert_eq!(
            events[0].raw_extras.as_deref(),
            Some(r#"{"responseId": "r-1", "sessionId": "outer"}"#)
        );
    }

    #[test]
    fn an_omp_row_prices_as_pi_but_is_stamped_omp() {
        let row = pi_row(r#"{"input": 1000, "output": 100}"#)
            .with("provider", PyValue::Str("omp".into()));
        let events = PiNormalizer.normalize(&ctx(), &row).unwrap();
        assert_eq!(events[0].provider, "omp", "the column comes off the row");
        assert_eq!(
            PiNormalizer.provider_name(),
            "pi",
            "the pricer key does not"
        );
        let as_pi = PiNormalizer
            .normalize(&ctx(), &pi_row(r#"{"input": 1000, "output": 100}"#))
            .unwrap();
        assert_eq!(
            events[0].cost_usd.to_bits(),
            as_pi[0].cost_usd.to_bits(),
            "an omp row must bill exactly as a pi row does"
        );
    }

    #[test]
    fn the_default_model_is_gpt_5_not_a_pi_specific_placeholder() {
        let row = pi_row(r#"{"input": 5}"#).with("model", PyValue::Null);
        let events = PiNormalizer.normalize(&ctx(), &row).unwrap();
        assert_eq!(events[0].model, "gpt-5");
    }
}
