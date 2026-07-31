//! Grok (the xAI `grok` CLI) — port of `etl/normalize/grok.py`.
//!
//! The CLI records **no** token counts anywhere, so every row estimates from
//! `len(content_text) // 4` on the OUTPUT side (Kiro estimates onto input; the
//! difference is deliberate — these are the model's own words) and stamps
//! `estimated`.
//!
//! **The `$0` override is the point of this module.** `grok-build` has no xAI
//! rate-card entry, and an unrecognised id falls through `compute_cost` to the
//! Anthropic *fallback* pricer, which would bill a non-Anthropic model at
//! Sonnet 3.5 rates. `_compute_cost_usd` is therefore overridden to force `$0`
//! for any grok model not explicitly in the rate card; the day a real xAI rate
//! lands in `data/models.toml` the normal pricer takes over with no further
//! change here. `cost_source` stays `estimated` so the token provenance is
//! still visible — a `$0` grok row is not an `unknown` row.
//!
//! Live evidence: 57 grok events on the maintainer's store, `SUM(cost_usd)`
//! exactly `0.0`. The override is load-bearing, not theoretical.

use super::base::{CostArgs, CostSource, EventSpec, NormalizeContext, Normalizer, UsageEvent};
use super::row::{MsgRow, PyRaise, int_or_zero, str_or_empty};
use super::text::{estimate_from_text, extras_from_payload};

const DEFAULT_MODEL: &str = "grok-build";
const BILLABLE_ROLES: [&str; 2] = ["assistant", "reasoning"];
const RAW_EXTRAS_FIELDS: [&str; 6] = [
    "id",
    "model_id",
    "model_fingerprint",
    "status",
    "synthetic_reason",
    "tool_call_id",
];

/// The `grok` normalizer.
#[derive(Debug, Clone, Copy, Default)]
pub struct GrokNormalizer;

impl Normalizer for GrokNormalizer {
    fn provider_name(&self) -> &'static str {
        "grok"
    }

    fn normalize(&self, ctx: &NormalizeContext, row: &MsgRow) -> Result<Vec<UsageEvent>, PyRaise> {
        let role = str_or_empty(row, "role");
        // `reasoning` and `assistant` are the model's billable turns; `bot` is
        // accepted for parity with the Kiro source shape.
        if !BILLABLE_ROLES.contains(&role.as_str()) && role != "bot" {
            return Ok(Vec::new());
        }

        let text = str_or_empty(row, "content_text");
        let input_tokens = int_or_zero(row, "input_tokens")?;
        let mut output_tokens = int_or_zero(row, "output_tokens")?;
        if input_tokens == 0 && output_tokens == 0 {
            if text.is_empty() {
                // Encrypted reasoning / empty tool-call turn — nothing to bill.
                return Ok(Vec::new());
            }
            output_tokens = estimate_from_text(&text);
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
                // `reasoning_tokens` stays 0: Grok's chain-of-thought is stored
                // `encrypted_content` and never decrypted, so its length — and its
                // token count — is unmeasurable. Nothing to attribute, even though
                // the model plainly reasons.
                EventSpec::new(input_tokens, output_tokens, 0, 0, CostSource::Estimated)
                    .model(model)
                    .raw_extras(extras_from_payload(row.get("raw_json"), &RAW_EXTRAS_FIELDS)),
            ),
        ])
    }

    /// Force `$0` for grok models with no rate-card entry.
    fn compute_cost_usd(&self, ctx: &NormalizeContext, args: &CostArgs<'_>) -> f64 {
        if !args.model.is_empty() && ctx.is_rate_card_model(args.model) {
            // `super()._compute_cost_usd(...)` — the base implementation, which
            // still honours the `unknown` short-circuit.
            return default_compute_cost_usd(self, ctx, args);
        }
        0.0
    }
}

/// `Normalizer._compute_cost_usd`'s body, callable as `super()` is.
///
/// Rust has no `super::method()`, and a default trait method that an impl
/// overrides is not reachable from the override. Extracting the body is the
/// only way to call it — the alternative, duplicating the pricing call, is how
/// the `unknown` short-circuit would eventually get dropped from one of them.
fn default_compute_cost_usd<N: Normalizer + ?Sized>(
    normalizer: &N,
    ctx: &NormalizeContext,
    args: &CostArgs<'_>,
) -> f64 {
    if args.model.is_empty() || args.cost_source == CostSource::Unknown {
        return 0.0;
    }
    let tokens = crate::pricing::RawTokens::canonical(
        args.input_tokens,
        args.output_tokens,
        args.cache_create_tokens,
        args.cache_read_tokens,
    );
    ctx.engine()
        .compute_cost(
            &tokens,
            args.model,
            normalizer.provider_name(),
            args.speed,
            Some(args.at_ts),
        )
        .total_cost
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::normalize::test_support::{assistant_row, ctx};
    use stax_core::queries::pyjson::Value as PyValue;

    fn grok_row() -> MsgRow {
        assistant_row("grok", "grok-build").with("content_text", PyValue::Str("x".repeat(400)))
    }

    #[test]
    fn estimation_lands_on_the_output_side() {
        let events = GrokNormalizer.normalize(&ctx(), &grok_row()).unwrap();
        assert_eq!((events[0].input_tokens, events[0].output_tokens), (0, 100));
        assert_eq!(events[0].cost_source, CostSource::Estimated);
        assert_eq!(events[0].reasoning_tokens, 0);
    }

    #[test]
    fn an_unpriced_grok_model_costs_exactly_zero_not_anthropic_fallback_rates() {
        let events = GrokNormalizer.normalize(&ctx(), &grok_row()).unwrap();
        assert_eq!(events[0].cost_usd, 0.0);
        // Prove the override is doing work: the SAME tokens through the base
        // implementation would have accrued Anthropic fallback dollars.
        let leaked = default_compute_cost_usd(
            &GrokNormalizer,
            &ctx(),
            &CostArgs {
                input_tokens: 0,
                output_tokens: 100,
                cache_read_tokens: 0,
                cache_create_tokens: 0,
                model: "grok-build",
                speed: "standard",
                cost_source: CostSource::Estimated,
                at_ts: "2026-04-25T00:00:00+00:00",
            },
        );
        assert!(
            leaked > 0.0,
            "without the override an unpriced grok row bills as Sonnet"
        );
    }

    #[test]
    fn a_rate_card_model_prices_normally_the_day_one_lands() {
        let row = grok_row().with("model", PyValue::Str("claude-sonnet-4-5-20250929".into()));
        let events = GrokNormalizer.normalize(&ctx(), &row).unwrap();
        assert!(events[0].cost_usd > 0.0);
        assert_eq!(events[0].cost_source, CostSource::Estimated);
    }

    #[test]
    fn reasoning_and_bot_rows_are_billable_but_user_and_tool_are_not() {
        for role in ["assistant", "reasoning", "bot"] {
            let row = grok_row().with("role", PyValue::Str(role.into()));
            assert_eq!(GrokNormalizer.normalize(&ctx(), &row).unwrap().len(), 1);
        }
        for role in ["user", "tool", "system", ""] {
            let row = grok_row().with("role", PyValue::Str(role.into()));
            assert!(GrokNormalizer.normalize(&ctx(), &row).unwrap().is_empty());
        }
    }

    #[test]
    fn an_encrypted_reasoning_turn_has_no_text_and_yields_nothing() {
        let row = grok_row()
            .with("role", PyValue::Str("reasoning".into()))
            .with("content_text", PyValue::Str(String::new()));
        assert!(GrokNormalizer.normalize(&ctx(), &row).unwrap().is_empty());
    }
}
