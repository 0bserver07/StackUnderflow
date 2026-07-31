//! The reusable adapter conformance harness — the port of
//! `tests/stackunderflow/adapters/contract.py`.
//!
//! Python ships `AdapterContract` as a mixin every provider's test module
//! subclasses, so a new adapter inherits the invariants instead of restating
//! them. This is the same idea as a function: the 18 providers still to land
//! call [`assert_contract`] with a fixture-backed instance and get the whole set
//! for one line.
//!
//! It lives in the library rather than in `tests/` on purpose — integration test
//! binaries cannot import each other, and a contract that each provider's test
//! file has to copy is a contract that drifts.

use crate::base::SourceAdapter;

/// Assert every invariant an adapter must satisfy.
///
/// The invariants, and why each one exists:
///
/// 1. **A non-empty `name`** — it is a store column value and a
///    `capabilities.json` key.
/// 2. **Every ref claims this provider** — a ref that lies about its provider
///    writes rows under the wrong one.
/// 3. **`seq` strictly increases within a session** — it is the resume
///    watermark; a repeat or a decrease re-ingests or skips.
/// 4. **Token counts are non-negative** — negative tokens become negative cost.
/// 5. **Timestamps parse as ISO 8601** — the store orders and buckets on them
///    as text.
/// 6. **`since_offset` is storage-aware** — a read resumed from the midpoint
///    yields strictly fewer records, all of them past the watermark. This is the
///    one invariant that holds identically for byte offsets and rowids, which is
///    why the two share a field.
///
/// An adapter whose fixture yields nothing passes vacuously, exactly as the
/// Python mixin does — "empty fixture is acceptable for the contract".
///
/// # Panics
/// On the first violated invariant, naming the adapter and the value.
pub fn assert_contract(adapter: &dyn SourceAdapter) {
    let name = adapter.name();
    assert!(!name.is_empty(), "adapter name must not be empty");

    let refs = adapter.enumerate();
    for session in &refs {
        assert_eq!(
            session.provider, name,
            "{name}: enumerate() yielded a ref for provider {:?}",
            session.provider
        );
    }
    let Some(first) = refs.first() else { return };

    let full = adapter.read(first, 0);
    let mut previous = -1_i64;
    for record in &full {
        assert_eq!(
            record.provider, name,
            "{name}: read() yielded a record for provider {:?}",
            record.provider
        );
        assert!(
            record.seq > previous,
            "{name}: seq not strictly increasing: {previous} -> {}",
            record.seq
        );
        previous = record.seq;
        assert!(record.input_tokens >= 0, "{name}: negative input_tokens");
        assert!(record.output_tokens >= 0, "{name}: negative output_tokens");
        assert!(
            record.cache_create_tokens >= 0,
            "{name}: negative cache_create_tokens"
        );
        assert!(
            record.cache_read_tokens >= 0,
            "{name}: negative cache_read_tokens"
        );
        assert!(
            is_iso_8601(&record.timestamp),
            "{name}: timestamp {:?} is not ISO 8601",
            record.timestamp
        );
    }

    if full.len() < 2 {
        return;
    }
    let midpoint = full[full.len() / 2].seq;
    let resumed = adapter.read(first, midpoint);
    assert!(
        resumed.iter().all(|record| record.seq > midpoint),
        "{name}: resumed read returned a record at or before the watermark"
    );
    assert!(
        resumed.len() < full.len(),
        "{name}: resumed read returned {} records, full read {}",
        resumed.len(),
        full.len()
    );
}

/// Whether `value` parses the way `datetime.fromisoformat` would.
///
/// Structural, not calendrical: `YYYY-MM-DD`, an optional `T`/space plus
/// `HH:MM[:SS[.fff…]]`, and an optional `Z` or `±HH:MM[:SS]` offset. Python's
/// parser is stricter about impossible dates; the contract only needs "this is a
/// timestamp, not free text", and every adapter passes its source's string
/// through untouched.
#[must_use]
pub fn is_iso_8601(value: &str) -> bool {
    let value = value.strip_suffix('Z').unwrap_or(value);
    let (date, rest) = match value.split_once(['T', ' ']) {
        Some((date, rest)) => (date, Some(rest)),
        None => (value, None),
    };
    if !matches_mask(date, "0000-00-00") {
        return false;
    }
    let Some(rest) = rest else { return true };
    // Strip a trailing UTC offset before validating the time.
    let time = match rest.rfind(['+', '-']) {
        Some(index) => {
            let offset = &rest[index + 1..];
            if !(matches_mask(offset, "00:00")
                || matches_mask(offset, "0000")
                || matches_mask(offset, "00")
                || matches_mask(offset, "00:00:00"))
            {
                return false;
            }
            &rest[..index]
        }
        None => rest,
    };
    let (clock, fraction) = match time.split_once('.') {
        Some((clock, fraction)) => (clock, Some(fraction)),
        None => (time, None),
    };
    if let Some(fraction) = fraction
        && (fraction.is_empty() || !fraction.bytes().all(|byte| byte.is_ascii_digit()))
    {
        return false;
    }
    matches_mask(clock, "00:00:00") || matches_mask(clock, "00:00")
}

/// Match `value` against a mask where `0` means "one ASCII digit" and every
/// other character must appear literally.
fn matches_mask(value: &str, mask: &str) -> bool {
    value.len() == mask.len()
        && value.bytes().zip(mask.bytes()).all(|(byte, expected)| {
            if expected == b'0' {
                byte.is_ascii_digit()
            } else {
                byte == expected
            }
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso_shapes_the_adapters_actually_emit() {
        // Claude JSONL, Codex rollouts, and the legacy history conversion.
        assert!(is_iso_8601("2026-01-01T00:00:00Z"));
        assert!(is_iso_8601("2026-07-14T03:01:39.473Z"));
        assert!(is_iso_8601("2024-01-01T00:00:00+00:00"));
        assert!(is_iso_8601("2024-01-01T00:01:00.123000+00:00"));
        assert!(is_iso_8601("2026-01-01"));
        assert!(is_iso_8601("2026-01-01 00:00:00"));
    }

    #[test]
    fn free_text_is_not_a_timestamp() {
        assert!(!is_iso_8601(""));
        assert!(!is_iso_8601("None"));
        assert!(!is_iso_8601("2026-01-01T00:00:00+banana"));
        assert!(!is_iso_8601("26-01-01T00:00:00"));
        assert!(!is_iso_8601("2026-01-01T00:00:00."));
    }
}
