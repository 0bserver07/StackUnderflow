//! `json.loads` for the mart path, with the same depth policy the adapters use.
//!
//! Three mart builders parse `messages.raw_json` host-side (`project`, `tool`
//! via `tools_json`, `message_tool`), and every one of them is *defensive by
//! contract*: Python catches `JSONDecodeError` / `TypeError` / `ValueError` and
//! skips the row, because "a poison row must never break the mart refresh".
//! [`loads`] returns `None` for the same set.
//!
//! The wrinkle is nesting. `serde_json::from_str` refuses past 128 nested
//! containers where CPython's `json` allows ~1000 (DIV-013, closed at
//! `dd97085`: 1024 is orjson's exact ceiling and the adapters bound their deep
//! path there). A `raw_json` blob deeper than 128 would parse in Python and
//! not here, which on the `project_mart` path is not a dropped record but a
//! *changed dimension* — the row would be skipped instead of classified. So the
//! same two-stage parse is used: try the cheap bounded parse, and on failure
//! retry with the recursion limit lifted on a stack sized to hold the ceiling.
//!
//! `stax_adapters::jsonl::parse_json` is the same function for the ingest side.
//! Kept separate rather than shared because `stax-etl` does not otherwise
//! depend on `stax-adapters`, and an ETL → adapters edge for one helper is the
//! wrong trade; the pair is on the dedup list with the `pyjson` twins.

use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::Value;

/// The depth ceiling, matching `stax_adapters::jsonl::MAX_JSON_DEPTH`.
///
/// 1024 is orjson's exact limit, which is what the Python ingest path enforces
/// before a blob ever reaches `raw_json`.
pub const MAX_JSON_DEPTH: usize = 1024;

/// Stack for the deep parse — see the adapters' measurement (~2.3 KB per level
/// unoptimised, so `MAX_JSON_DEPTH` ≈ 2.4 MB; 32 MB is an order of magnitude of
/// headroom, and a thread stack is reserved address space, not committed pages).
const DEEP_PARSE_STACK_BYTES: usize = 32 * 1024 * 1024;

/// How many blobs needed the deep path. Reported by the gate runner so "no deep
/// blobs on this store" is a measurement and not an assumption.
static DEEP_PARSES: AtomicU64 = AtomicU64::new(0);

/// How many blobs were skipped for exceeding [`MAX_JSON_DEPTH`].
static DEEP_SKIPS: AtomicU64 = AtomicU64::new(0);

/// `(deep_parses, deep_skips)` since process start.
#[must_use]
pub fn deep_counters() -> (u64, u64) {
    (
        DEEP_PARSES.load(Ordering::Relaxed),
        DEEP_SKIPS.load(Ordering::Relaxed),
    )
}

/// `json.loads(s)` with Python's error handling folded into the return type.
///
/// `None` covers what the Python `except (json.JSONDecodeError, TypeError,
/// ValueError)` covers, plus `None`/empty input (`json.loads(rj) if rj else {}`
/// is the guard at every call site — an empty string yields `{}` there, and
/// callers here treat `None` and "not an object" identically).
#[must_use]
pub fn loads(text: Option<&str>) -> Option<Value> {
    let text = text?;
    if text.is_empty() {
        return None;
    }
    match serde_json::from_str::<Value>(text) {
        Ok(v) => Some(v),
        Err(_) => loads_deep(text),
    }
}

fn loads_deep(text: &str) -> Option<Value> {
    if exceeds_depth(text.as_bytes(), MAX_JSON_DEPTH) {
        DEEP_SKIPS.fetch_add(1, Ordering::Relaxed);
        return None;
    }
    let parsed = std::thread::scope(|scope| {
        std::thread::Builder::new()
            .name("stax-etl-deep-json".to_string())
            .stack_size(DEEP_PARSE_STACK_BYTES)
            .spawn_scoped(scope, || parse_unbounded(text))
            .ok()
            .and_then(|worker| worker.join().ok())
            .flatten()
    });
    if parsed.is_some() {
        DEEP_PARSES.fetch_add(1, Ordering::Relaxed);
    }
    parsed
}

fn parse_unbounded(text: &str) -> Option<Value> {
    let mut de = serde_json::Deserializer::from_str(text);
    de.disable_recursion_limit();
    let mut values = de.into_iter::<Value>();
    let value = values.next()?.ok()?;
    // `from_str` rejects trailing content after the value, and so must this.
    let rest = text.as_bytes().get(values.byte_offset()..)?;
    rest.iter().all(u8::is_ascii_whitespace).then_some(value)
}

/// Whether `bytes` opens more than `limit` nested containers anywhere.
///
/// A byte scan, not a parse: brackets inside strings do not count, which is the
/// only subtlety. On malformed input the count can be wrong either way, which
/// is harmless — the parse that follows rejects the blob regardless.
fn exceeds_depth(bytes: &[u8], limit: usize) -> bool {
    let mut depth = 0_usize;
    let mut in_string = false;
    let mut escaped = false;
    for &b in bytes {
        if in_string {
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == b'"' {
                in_string = false;
            }
            continue;
        }
        match b {
            b'"' => in_string = true,
            b'{' | b'[' => {
                depth += 1;
                if depth > limit {
                    return true;
                }
            }
            b'}' | b']' => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn poison_rows_are_skipped_not_fatal() {
        assert!(loads(None).is_none());
        assert!(loads(Some("")).is_none());
        assert!(loads(Some("{not json")).is_none());
        assert!(loads(Some("{} trailing")).is_none());
        assert!(loads(Some("null")).is_some());
    }

    #[test]
    fn nesting_past_serdes_default_limit_still_parses() {
        // 300 levels: past serde_json's 128, well inside CPython's ~1000.
        let deep = format!("{}{}", "[".repeat(300), "]".repeat(300));
        assert!(
            serde_json::from_str::<serde_json::Value>(&deep).is_err(),
            "the bounded parser must be the one that fails"
        );
        assert!(
            loads(Some(&deep)).is_some(),
            "the deep path must recover it"
        );
    }

    #[test]
    fn nesting_past_the_ceiling_is_skipped() {
        let too_deep = format!(
            "{}{}",
            "[".repeat(MAX_JSON_DEPTH + 5),
            "]".repeat(MAX_JSON_DEPTH + 5)
        );
        assert!(loads(Some(&too_deep)).is_none());
    }

    #[test]
    fn brackets_inside_strings_do_not_count_as_depth() {
        let s = format!(r#"{{"k": "{}"}}"#, "[".repeat(2000));
        assert!(!exceeds_depth(s.as_bytes(), MAX_JSON_DEPTH));
        assert!(loads(Some(&s)).is_some());
    }
}
