//! Codeium — port of `etl/normalize/codeium.py`. A registered no-op.
//!
//! The Codeium adapter only enumerates sessions: the client's on-disk format is
//! not parsed into individual messages, so no billable row for this provider
//! ever reaches the `messages` table. The normalizer exists so the registry has
//! an entry for every provider in the catalog and the lookup at the ingest seam
//! cannot `KeyError` when Codeium is enabled.
//!
//! Python writes this as `return` followed by an unreachable `yield`, which is
//! the idiom that makes the function a generator returning an empty iterator
//! rather than a function returning `None`. The distinction matters there
//! (`list(normalizer.normalize(row))` would raise on `None`) and disappears
//! here, where the return type already says "a list of events".

use super::base::{NormalizeContext, Normalizer, UsageEvent};
use super::row::{MsgRow, PyRaise};

/// The `codeium` normalizer — discovery-only, never yields.
#[derive(Debug, Clone, Copy, Default)]
pub struct CodeiumNormalizer;

impl Normalizer for CodeiumNormalizer {
    fn provider_name(&self) -> &'static str {
        "codeium"
    }

    fn normalize(
        &self,
        _ctx: &NormalizeContext,
        _row: &MsgRow,
    ) -> Result<Vec<UsageEvent>, PyRaise> {
        Ok(Vec::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::normalize::test_support::{assistant_row, ctx};
    use stax_core::queries::pyjson::Value as PyValue;

    #[test]
    fn nothing_is_billable_no_matter_how_billable_the_row_looks() {
        let row = assistant_row("codeium", "claude-sonnet-4-5-20250929")
            .with("input_tokens", PyValue::Int(10_000))
            .with("output_tokens", PyValue::Int(10_000));
        assert!(
            CodeiumNormalizer
                .normalize(&ctx(), &row)
                .unwrap()
                .is_empty()
        );
    }
}
