//! Codex (OpenAI) — port of `etl/normalize/codex.py`.
//!
//! The canonical mapping, per `docs/specs/etl-architecture.md`: subtract
//! `cached_input_tokens` from `input_tokens` so canonical `input` counts only
//! freshly-billed input; fold `reasoning_output_tokens` into `output`; map
//! `cached_input_tokens` to `cache_read`; leave `cache_create` at 0 (OpenAI
//! does not bill prompt-cache writes).
//!
//! Python does not re-implement that arithmetic — it **delegates** to
//! `OpenAIPricer.normalize_tokens`, deliberately, because two other call sites
//! (`adapters/codex.py` and `infra.costs.compute_cost`) depend on the pricer's
//! copy as a seam. The port keeps the delegation: [`Pricer::OpenAi`]'s
//! `normalize_tokens` is the same function the pricing engine calls, so the two
//! cannot drift here either.
//!
//! **Gate order is load-bearing.** The token guard runs BEFORE the model guard
//! (`codex.py:72` then `:76`), the reverse of claude's. A row with a model and
//! no tokens and a row with tokens and no model both yield nothing, so the
//! order is unobservable in the output — but it is observable in a raise: a
//! poison token column on a model-less row raises here and is silently swapped
//! for "drop" there. Ported in the written order.

use stax_core::queries::pyjson::Value as PyValue;

use super::base::{EventSpec, NormalizeContext, Normalizer, UsageEvent};
use super::row::{
    MsgRow, PyRaise, as_dict, int_or_zero, int_or_zero_value, safe_load_raw, str_or_empty,
};
use crate::pricing::{Pricer, RawTokens, Tokens};

/// Codex-specific fields copied verbatim into `raw_extras`.
const RAW_EXTRAS_FIELDS: [&str; 3] = ["service_tier", "model_provider", "originator"];

/// The `codex` normalizer.
#[derive(Debug, Clone, Copy, Default)]
pub struct CodexNormalizer;

impl Normalizer for CodexNormalizer {
    fn provider_name(&self) -> &'static str {
        "codex"
    }

    fn normalize(&self, ctx: &NormalizeContext, row: &MsgRow) -> Result<Vec<UsageEvent>, PyRaise> {
        if str_or_empty(row, "role") != "assistant" {
            return Ok(Vec::new());
        }

        let Some(canonical) = canonical_tokens(row)? else {
            return Ok(Vec::new()); // not billable
        };

        let model = str_or_empty(row, "model");
        if model.is_empty() {
            return Ok(Vec::new());
        }

        // Exact-id membership: the OpenAI pricer falls back to a default Codex
        // family for any unrecognised `gpt-*` id, so `get_model_pricing` always
        // returns a number and cannot answer "do we actually know this model".
        let cost_source = ctx.rate_card_or_unknown(&model);
        let raw_extras = extras_from_raw_json(row.get("raw_json"));

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
                // OpenAI bills reasoning as output, so `canonical.output` already
                // includes it and `cost_usd` is correct. The same count rides
                // along as an attribution-only subset — never priced.
                .reasoning(reasoning_tokens(row)?)
                .model(model)
                .raw_extras(raw_extras),
            ),
        ])
    }
}

/// Reasoning-output tokens for this turn, or 0.
///
/// Sourced from whichever raw shape [`raw_openai_shape`] surfaces. Zero when
/// the raw shape is absent: the adapter has already folded reasoning into the
/// `output_tokens` column, so there is no separable count left to recover.
fn reasoning_tokens(row: &MsgRow) -> Result<i64, PyRaise> {
    let Some(raw) = raw_openai_shape(row)? else {
        return Ok(0);
    };
    Ok(raw.reasoning_output_tokens.max(0))
}

/// The canonical four keys, or `None` when the row is not billable.
fn canonical_tokens(row: &MsgRow) -> Result<Option<Tokens>, PyRaise> {
    let canonical = match raw_openai_shape(row)? {
        Some(raw) => Pricer::OpenAi.normalize_tokens(&RawTokens::openai_shape(
            raw.input_tokens,
            raw.output_tokens,
            raw.cached_input_tokens,
            raw.reasoning_output_tokens,
        )),
        None => Tokens {
            input: int_or_zero(row, "input_tokens")?,
            output: int_or_zero(row, "output_tokens")?,
            cache_read: int_or_zero(row, "cache_read_tokens")?,
            cache_creation: int_or_zero(row, "cache_create_tokens")?,
        },
    };
    if canonical.input == 0
        && canonical.output == 0
        && canonical.cache_read == 0
        && canonical.cache_creation == 0
    {
        return Ok(None);
    }
    Ok(Some(canonical))
}

/// OpenAI's raw token keys, from the row itself or from `raw_json`.
struct OpenAiShape {
    input_tokens: i64,
    output_tokens: i64,
    cached_input_tokens: i64,
    reasoning_output_tokens: i64,
}

/// Two locations matter: the row directly (a synthetic fixture may pass
/// `cached_input_tokens` / `reasoning_output_tokens`), and the rollout payload
/// at `payload.info.last_token_usage`.
///
/// **Presence, not truthiness** — `"cached_input_tokens" in msg_row` is what
/// selects this branch, so a column explicitly set to `0` still routes through
/// the OpenAI reshape rather than through the canonical columns.
fn raw_openai_shape(row: &MsgRow) -> Result<Option<OpenAiShape>, PyRaise> {
    if row.contains_key("cached_input_tokens") || row.contains_key("reasoning_output_tokens") {
        return Ok(Some(OpenAiShape {
            input_tokens: int_or_zero(row, "input_tokens")?,
            output_tokens: int_or_zero(row, "output_tokens")?,
            cached_input_tokens: int_or_zero(row, "cached_input_tokens")?,
            reasoning_output_tokens: int_or_zero(row, "reasoning_output_tokens")?,
        }));
    }

    let Some(payload) = safe_load_raw(row.get("raw_json")) else {
        return Ok(None);
    };
    if !matches!(payload, PyValue::Object(_)) {
        return Ok(None);
    }
    // `(payload.get("payload") or payload).get("info")` — truthiness, so an
    // empty inner payload falls back to the outer one. A truthy NON-dict there
    // has no `.get`, and Python raises `AttributeError` rather than missing:
    // reproduced, because the raise is what decides whether the row survives.
    let inner = match payload.get("payload") {
        Some(nested) if nested.is_truthy() => nested,
        _ => &payload,
    };
    if !matches!(inner, PyValue::Object(_)) {
        return Err(PyRaise {
            kind: "AttributeError",
            detail: "'payload' is not a dict and has no attribute 'get'".to_string(),
        });
    }
    let Some(info) = as_dict(inner.get("info")) else {
        return Ok(None);
    };
    let Some(last) = as_dict(info.get("last_token_usage")) else {
        return Ok(None);
    };
    let PyValue::Object(entries) = last else {
        return Ok(None);
    };
    let present = |key: &str| entries.iter().any(|(name, _)| name == key);
    if !present("cached_input_tokens") && !present("reasoning_output_tokens") {
        return Ok(None);
    }
    Ok(Some(OpenAiShape {
        input_tokens: int_or_zero_value(last.get("input_tokens"))?,
        output_tokens: int_or_zero_value(last.get("output_tokens"))?,
        cached_input_tokens: int_or_zero_value(last.get("cached_input_tokens"))?,
        reasoning_output_tokens: int_or_zero_value(last.get("reasoning_output_tokens"))?,
    }))
}

/// Provider keepsakes, unwrapping a `payload` envelope when there is one.
///
/// `if val:` — truthiness, so an empty string or a `0` is dropped rather than
/// stored.
fn extras_from_raw_json(raw_json: Option<&PyValue>) -> Option<PyValue> {
    let payload = safe_load_raw(raw_json)?;
    if !matches!(payload, PyValue::Object(_)) {
        return None;
    }
    let inner = match as_dict(payload.get("payload")) {
        Some(nested) => nested,
        None => &payload,
    };
    let mut out: Vec<(String, PyValue)> = Vec::new();
    for key in RAW_EXTRAS_FIELDS {
        if let Some(value) = inner.get(key)
            && value.is_truthy()
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

    fn codex_row() -> MsgRow {
        assistant_row("codex", "gpt-5")
    }

    #[test]
    fn the_rollout_shape_subtracts_cached_input_and_folds_reasoning() {
        let row = codex_row().with(
            "raw_json",
            PyValue::Str(
                r#"{"payload": {"info": {"last_token_usage": {
                    "input_tokens": 1000, "cached_input_tokens": 400,
                    "output_tokens": 200, "reasoning_output_tokens": 50}}}}"#
                    .to_string(),
            ),
        );
        let events = CodexNormalizer.normalize(&ctx(), &row).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].input_tokens, 600); // 1000 - 400
        assert_eq!(events[0].output_tokens, 250); // 200 + 50
        assert_eq!(events[0].cache_read_tokens, 400);
        assert_eq!(events[0].cache_create_tokens, 0);
        assert_eq!(events[0].reasoning_tokens, 50);
    }

    #[test]
    fn the_columns_are_the_fallback_when_no_raw_shape_is_reachable() {
        let row = codex_row()
            .with("input_tokens", PyValue::Int(11))
            .with("output_tokens", PyValue::Int(22))
            .with("cache_read_tokens", PyValue::Int(33))
            .with("cache_create_tokens", PyValue::Int(44));
        let events = CodexNormalizer.normalize(&ctx(), &row).unwrap();
        assert_eq!(events[0].input_tokens, 11);
        assert_eq!(events[0].cache_create_tokens, 44);
        assert_eq!(events[0].reasoning_tokens, 0);
    }

    #[test]
    fn a_zero_valued_raw_key_still_selects_the_openai_reshape() {
        // Presence, not truthiness: this row has `cached_input_tokens = 0`, so
        // cache_create must come back 0 (OpenAI never bills writes) even though
        // the column says otherwise.
        let row = codex_row()
            .with("input_tokens", PyValue::Int(100))
            .with("cached_input_tokens", PyValue::Int(0))
            .with("cache_create_tokens", PyValue::Int(999));
        let events = CodexNormalizer.normalize(&ctx(), &row).unwrap();
        assert_eq!(events[0].input_tokens, 100);
        assert_eq!(events[0].cache_create_tokens, 0);
    }

    #[test]
    fn a_model_less_row_yields_nothing_even_with_tokens() {
        let row = codex_row()
            .with("model", PyValue::Null)
            .with("input_tokens", PyValue::Int(100));
        assert!(CodexNormalizer.normalize(&ctx(), &row).unwrap().is_empty());
    }

    #[test]
    fn an_all_zero_row_yields_nothing_even_with_a_model() {
        assert!(
            CodexNormalizer
                .normalize(&ctx(), &codex_row())
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn raw_extras_keeps_only_truthy_keepsakes() {
        let row = codex_row().with("input_tokens", PyValue::Int(10)).with(
            "raw_json",
            PyValue::Str(
                r#"{"payload": {"service_tier": "priority", "originator": "",
                        "model_provider": "openai"}}"#
                    .to_string(),
            ),
        );
        let events = CodexNormalizer.normalize(&ctx(), &row).unwrap();
        assert_eq!(
            events[0].raw_extras.as_deref(),
            Some(r#"{"service_tier": "priority", "model_provider": "openai"}"#)
        );
        assert_eq!(events[0].cost_source, CostSource::RateCard);
    }
}
