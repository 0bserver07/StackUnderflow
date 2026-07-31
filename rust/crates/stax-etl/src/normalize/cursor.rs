//! Cursor IDE — port of `etl/normalize/cursor.py`.
//!
//! Cursor v3 stores chat bubbles in a vscdb with **no** per-message token
//! counts — every `tokenCount.{inputTokens,outputTokens}` is zero — so the
//! adapter falls back to `len(text) // 4` and stamps `estimated` on the raw
//! payload. This mirrors that policy on the normalizer side, and preserves the
//! adapter's own flag in `raw_extras` so a reader can tell adapter-level
//! estimation from normalizer-level estimation even though the two should
//! always agree.
//!
//! The model resolution ladder ends at `composer-1` — Cursor's modern default —
//! whenever the row still carries the adapter's `cursor-auto` placeholder.

use super::base::{CostSource, EventSpec, NormalizeContext, Normalizer, UsageEvent};
use super::row::{
    MsgRow, PyRaise, as_dict, as_nonempty_str, int_or_zero, int_or_zero_value, safe_load_raw,
    str_or_empty,
};
use super::text::{estimate_from_text, extras_from_payload};

/// The default when the adapter wrote the `cursor-auto` placeholder.
const DEFAULT_MODEL: &str = "composer-1";
const RAW_EXTRAS_FIELDS: [&str; 3] = ["conversationId", "composerData", "cost_source"];

/// The `cursor` normalizer.
#[derive(Debug, Clone, Copy, Default)]
pub struct CursorNormalizer;

impl Normalizer for CursorNormalizer {
    fn provider_name(&self) -> &'static str {
        "cursor"
    }

    fn normalize(&self, ctx: &NormalizeContext, row: &MsgRow) -> Result<Vec<UsageEvent>, PyRaise> {
        if str_or_empty(row, "role") != "assistant" {
            return Ok(Vec::new());
        }

        let (input_tokens, output_tokens, estimated) = resolve_tokens(row)?;

        // Neither real nor estimated tokens, and no text to estimate from: a
        // pure-empty assistant message is not billable.
        if input_tokens == 0 && output_tokens == 0 {
            return Ok(Vec::new());
        }

        let model = resolve_model(row);
        let cost_source = if estimated {
            CostSource::Estimated
        } else {
            ctx.rate_card_or_unknown(&model)
        };

        Ok(vec![
            self.build_event(
                ctx,
                row,
                EventSpec::new(
                    input_tokens,
                    output_tokens,
                    // Cursor's cache columns pass straight through — unlike
                    // copilot's, which are hard-zeroed.
                    int_or_zero(row, "cache_read_tokens")?,
                    int_or_zero(row, "cache_create_tokens")?,
                    cost_source,
                )
                .model(model)
                .raw_extras(extras_from_payload(row.get("raw_json"), &RAW_EXTRAS_FIELDS)),
            ),
        ])
    }
}

/// `(input, output, estimated)`.
///
/// An explicit `tokenCount` block wins, then the canonical columns when either
/// is positive, then `len(content_text) // 4` on the **input** side only —
/// Cursor v3 does not separate prompt from completion text on a single bubble.
fn resolve_tokens(row: &MsgRow) -> Result<(i64, i64, bool), PyRaise> {
    if let Some((input, output)) = explicit_token_count(row)? {
        return Ok((input, output, false));
    }
    let input_column = int_or_zero(row, "input_tokens")?;
    let output_column = int_or_zero(row, "output_tokens")?;
    if input_column > 0 || output_column > 0 {
        return Ok((input_column, output_column, false));
    }
    let text = str_or_empty(row, "content_text");
    Ok((estimate_from_text(&text), 0, true))
}

/// `(input, output)` from a `tokenCount` block, or `None` when both are zero.
fn explicit_token_count(row: &MsgRow) -> Result<Option<(i64, i64)>, PyRaise> {
    let payload = safe_load_raw(row.get("raw_json"));
    let block = match as_dict(row.get("tokenCount")) {
        Some(direct) => Some(direct.clone()),
        None => payload
            .as_ref()
            .and_then(|p| as_dict(p.get("tokenCount")))
            .cloned(),
    };
    let Some(block) = block else {
        return Ok(None);
    };
    // `int(tc.get("inputTokens", 0) or 0)` — the default and the `or` are
    // redundant with each other; both are ported.
    let input = int_or_zero_value(block.get("inputTokens"))?;
    let output = int_or_zero_value(block.get("outputTokens"))?;
    if input == 0 && output == 0 {
        return Ok(None);
    }
    Ok(Some((input.max(0), output.max(0))))
}

/// The most specific model id available.
fn resolve_model(row: &MsgRow) -> String {
    if let Some(direct) = as_nonempty_str(row.get("model"))
        && direct != "cursor-auto"
    {
        return direct.to_string();
    }
    if let Some(payload) = safe_load_raw(row.get("raw_json")) {
        if let Some(info) = as_dict(payload.get("modelInfo"))
            && let Some(name) = as_nonempty_str(info.get("modelName"))
        {
            return name.to_string();
        }
        if let Some(options) = as_dict(payload.get("providerOptions"))
            && let Some(cursor) = as_dict(options.get("cursor"))
            && let Some(name) = as_nonempty_str(cursor.get("modelName"))
        {
            return name.to_string();
        }
    }
    DEFAULT_MODEL.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::normalize::test_support::{assistant_row, ctx};
    use stax_core::queries::pyjson::Value as PyValue;

    fn cursor_row() -> MsgRow {
        assistant_row("cursor", "cursor-auto")
    }

    #[test]
    fn an_explicit_token_count_block_wins_and_is_not_estimated() {
        let row = cursor_row().with(
            "raw_json",
            PyValue::Str(r#"{"tokenCount": {"inputTokens": 90, "outputTokens": 10}}"#.into()),
        );
        let events = CursorNormalizer.normalize(&ctx(), &row).unwrap();
        assert_eq!((events[0].input_tokens, events[0].output_tokens), (90, 10));
        assert_ne!(events[0].cost_source, CostSource::Estimated);
    }

    #[test]
    fn an_all_zero_token_count_block_falls_through_to_estimation() {
        let row = cursor_row()
            .with(
                "raw_json",
                PyValue::Str(r#"{"tokenCount": {"inputTokens": 0, "outputTokens": 0}}"#.into()),
            )
            .with("content_text", PyValue::Str("x".repeat(80)));
        let events = CursorNormalizer.normalize(&ctx(), &row).unwrap();
        assert_eq!((events[0].input_tokens, events[0].output_tokens), (20, 0));
        assert_eq!(events[0].cost_source, CostSource::Estimated);
    }

    #[test]
    fn the_placeholder_model_resolves_to_composer_1() {
        let row = cursor_row().with("content_text", PyValue::Str("x".repeat(40)));
        let events = CursorNormalizer.normalize(&ctx(), &row).unwrap();
        assert_eq!(events[0].model, "composer-1");
    }

    #[test]
    fn the_model_ladder_reads_modelinfo_then_provideroptions() {
        let base = cursor_row().with("content_text", PyValue::Str("x".repeat(40)));
        let info = base.clone().with(
            "raw_json",
            PyValue::Str(r#"{"modelInfo": {"modelName": "claude-opus-4-7"}}"#.into()),
        );
        assert_eq!(
            CursorNormalizer.normalize(&ctx(), &info).unwrap()[0].model,
            "claude-opus-4-7"
        );
        let options = base.with(
            "raw_json",
            PyValue::Str(r#"{"providerOptions": {"cursor": {"modelName": "gpt-5"}}}"#.into()),
        );
        assert_eq!(
            CursorNormalizer.normalize(&ctx(), &options).unwrap()[0].model,
            "gpt-5"
        );
    }

    #[test]
    fn an_empty_bubble_is_not_billable() {
        assert!(
            CursorNormalizer
                .normalize(&ctx(), &cursor_row())
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn the_adapters_own_cost_source_flag_is_preserved_verbatim() {
        let row = cursor_row().with("input_tokens", PyValue::Int(10)).with(
            "raw_json",
            PyValue::Str(r#"{"conversationId": "c-1", "cost_source": "estimated"}"#.into()),
        );
        let events = CursorNormalizer.normalize(&ctx(), &row).unwrap();
        assert_eq!(
            events[0].raw_extras.as_deref(),
            Some(r#"{"conversationId": "c-1", "cost_source": "estimated"}"#)
        );
    }
}
