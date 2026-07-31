//! Claude (Anthropic) — port of `etl/normalize/claude.py`.
//!
//! Anthropic's wire shape *is* the canonical four-token shape, and the adapter
//! already lifted it onto the `messages` columns, so the transform is a
//! forward. What is left is the gating, and the gating is the contract:
//!
//! 1. non-`assistant` rows are dropped;
//! 2. model-less rows are dropped — the adapter strips `"<synthetic>"` to
//!    `None`, and this honours that signal;
//! 3. all-zero-token assistant rows are dropped — an error stub or a
//!    tool-result attachment would otherwise become a `$0` row that inflates
//!    `message_count` downstream.
//!
//! Those three bare `return`s (`claude.py:44`, `:48`, `:59-70`) are the
//! substrate §6b divergence 2 sits on: whatever the classifier later does with
//! a turn, a turn that never became an event cannot be counted at all. They are
//! ported exactly, and [`tests`] pins each one, because "the Rust port emits
//! more rows than Python" is the shape a well-meaning fix takes.
//!
//! `raw_extras` stays `NULL` for Claude: `service_tier` is already encoded in
//! `speed`, and the `message.usage` block is captured verbatim in `raw_json`
//! upstream.

use super::base::{EventSpec, NormalizeContext, Normalizer, UsageEvent};
use super::row::{MsgRow, PyRaise, int_or_zero, str_or_empty, truthy};

/// The `claude` normalizer.
#[derive(Debug, Clone, Copy, Default)]
pub struct ClaudeNormalizer;

impl Normalizer for ClaudeNormalizer {
    fn provider_name(&self) -> &'static str {
        "claude"
    }

    fn normalize(&self, ctx: &NormalizeContext, row: &MsgRow) -> Result<Vec<UsageEvent>, PyRaise> {
        if str_or_empty(row, "role") != "assistant" {
            return Ok(Vec::new());
        }

        // `model = msg_row.get("model"); if not model: return` — truthiness on
        // the raw value, so `""` and `None` are both the synthetic signal.
        if !truthy(row.get("model")) {
            return Ok(Vec::new());
        }
        let model = super::row::py_str(row.get("model").expect("truthy implies present"));

        let input_tokens = int_or_zero(row, "input_tokens")?;
        let output_tokens = int_or_zero(row, "output_tokens")?;
        let cache_read = int_or_zero(row, "cache_read_tokens")?;
        let cache_create = int_or_zero(row, "cache_create_tokens")?;

        if input_tokens == 0 && output_tokens == 0 && cache_read == 0 && cache_create == 0 {
            return Ok(Vec::new());
        }

        // Exact rate-card membership, not "a rate resolves": the pricers fall
        // back to a default family, so `get_model_pricing` would never say
        // `None` and could not distinguish knowing from guessing.
        let cost_source = ctx.rate_card_or_unknown(&model);

        // `reasoning_tokens` stays 0. Anthropic thinking blocks ARE billed as
        // output and are already inside `output_tokens`, but `message.usage`
        // carries no separate count — only the (often redacted) thinking text.
        // Estimating from that text would be a fabricated number.
        Ok(vec![
            self.build_event(
                ctx,
                row,
                EventSpec::new(
                    input_tokens,
                    output_tokens,
                    cache_read,
                    cache_create,
                    cost_source,
                )
                .model(model),
            ),
        ])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::normalize::base::CostSource;
    use crate::normalize::test_support::{assistant_row, ctx};
    use stax_core::queries::pyjson::Value as PyValue;

    fn claude_row() -> MsgRow {
        assistant_row("claude", "claude-sonnet-4-5-20250929")
            .with("input_tokens", PyValue::Int(1_000))
            .with("output_tokens", PyValue::Int(500))
    }

    #[test]
    fn the_happy_path_forwards_four_tokens_and_stamps_rate_card() {
        let events = ClaudeNormalizer
            .normalize(&ctx(), &claude_row())
            .expect("no raise");
        assert_eq!(events.len(), 1);
        let ev = &events[0];
        assert_eq!(ev.input_tokens, 1_000);
        assert_eq!(ev.output_tokens, 500);
        assert_eq!(ev.cost_source, CostSource::RateCard);
        assert_eq!(ev.reasoning_tokens, 0);
        assert_eq!(ev.raw_extras, None);
        assert!(ev.cost_usd > 0.0);
    }

    #[test]
    fn claude_py_44_a_non_assistant_row_yields_nothing() {
        for role in ["user", "system", "tool", "summary", ""] {
            let row = claude_row().with("role", PyValue::Str(role.into()));
            assert!(
                ClaudeNormalizer.normalize(&ctx(), &row).unwrap().is_empty(),
                "role {role:?} must not produce an event"
            );
        }
    }

    #[test]
    fn claude_py_48_a_model_less_row_yields_nothing() {
        for model in [PyValue::Null, PyValue::Str(String::new())] {
            let row = claude_row().with("model", model.clone());
            assert!(
                ClaudeNormalizer.normalize(&ctx(), &row).unwrap().is_empty(),
                "model {model:?} is the adapter's <synthetic> signal"
            );
        }
    }

    #[test]
    fn claude_py_59_70_an_all_zero_assistant_row_yields_nothing() {
        let row = claude_row()
            .with("input_tokens", PyValue::Int(0))
            .with("output_tokens", PyValue::Int(0));
        assert!(ClaudeNormalizer.normalize(&ctx(), &row).unwrap().is_empty());
        // …but ONE non-zero bucket is enough, including a cache-only row.
        let cache_only = row.clone().with("cache_read_tokens", PyValue::Int(7));
        assert_eq!(
            ClaudeNormalizer
                .normalize(&ctx(), &cache_only)
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn an_unknown_model_stamps_unknown_and_costs_nothing() {
        let row = claude_row().with("model", PyValue::Str("claude-from-the-future".into()));
        let events = ClaudeNormalizer.normalize(&ctx(), &row).unwrap();
        assert_eq!(events[0].cost_source, CostSource::Unknown);
        assert_eq!(events[0].cost_usd, 0.0);
    }

    #[test]
    fn a_poison_token_column_raises_so_the_caller_can_drop_the_row() {
        let row = claude_row().with("input_tokens", PyValue::Str("not a number".into()));
        assert!(ClaudeNormalizer.normalize(&ctx(), &row).is_err());
    }
}
