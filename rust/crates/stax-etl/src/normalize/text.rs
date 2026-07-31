//! The two idioms nine provider modules each keep a private copy of.
//!
//! Python duplicates `_extras_from_raw_json` and the `len(text) // 4` recovery
//! across the package — nine byte-identical copies of the first and eleven of
//! the second. Duplicating them here too would mean nine chances to typo the
//! `val != ""` guard, so they live once with the differences (`codex` tests
//! truthiness, `cline` walks a nested blob, `pi` falls back to the outer
//! payload) kept in the modules that actually differ.

use stax_core::queries::pyjson::Value as PyValue;

use super::row::{as_dict, safe_load_raw};

/// `max(len(text) // 4, 0)` — the text-length token recovery.
///
/// `len()` counts *characters*, so a reply of emoji estimates the way Python
/// counts it and not the way its UTF-8 length would.
#[must_use]
pub fn estimate_from_text(text: &str) -> i64 {
    (text.chars().count() / 4) as i64
}

/// The shared `_extras_from_raw_json`: pull `fields` off the parsed payload,
/// skipping `None` and `""`, in declaration order.
///
/// Returns `None` for an empty result so the column stays `NULL` rather than
/// holding `"{}"`.
#[must_use]
pub fn extras_from_payload(raw_json: Option<&PyValue>, fields: &[&str]) -> Option<PyValue> {
    let payload = safe_load_raw(raw_json)?;
    if !matches!(payload, PyValue::Object(_)) {
        return None;
    }
    let out = collect_fields(&payload, fields);
    (!out.is_empty()).then_some(PyValue::Object(out))
}

/// The `inner = payload.get(key) if isinstance(…, dict) else payload` variant —
/// hermes, openclaw, opencode and pi unwrap one envelope before reading.
#[must_use]
pub fn unwrap_envelope<'a>(payload: &'a PyValue, key: &str) -> &'a PyValue {
    as_dict(payload.get(key)).unwrap_or(payload)
}

/// `for key in fields: val = source.get(key); if val is not None and val != "":`
#[must_use]
pub fn collect_fields(source: &PyValue, fields: &[&str]) -> Vec<(String, PyValue)> {
    let mut out = Vec::new();
    for key in fields {
        if let Some(value) = source.get(key)
            && keepsake_worthy(value)
        {
            out.push(((*key).to_string(), value.clone()));
        }
    }
    out
}

/// `val is not None and val != ""`.
#[must_use]
pub fn keepsake_worthy(value: &PyValue) -> bool {
    !matches!(value, PyValue::Null) && !matches!(value, PyValue::Str(text) if text.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn estimation_counts_characters_not_bytes() {
        assert_eq!(estimate_from_text(""), 0);
        assert_eq!(estimate_from_text("abc"), 0);
        assert_eq!(estimate_from_text("abcd"), 1);
        // Four emoji are four characters (sixteen bytes) — one token, not four.
        assert_eq!(estimate_from_text("🙂🙂🙂🙂"), 1);
    }

    #[test]
    fn extras_skip_none_and_empty_string_but_keep_zero_and_false() {
        let raw =
            PyValue::Str(r#"{"a": null, "b": "", "c": 0, "d": false, "e": "keep"}"#.to_string());
        let got = extras_from_payload(Some(&raw), &["a", "b", "c", "d", "e"]);
        assert_eq!(
            got.as_ref().map(stax_core::queries::pyjson::dumps_default),
            Some(r#"{"c": 0, "d": false, "e": "keep"}"#.to_string())
        );
    }

    #[test]
    fn nothing_worth_keeping_means_a_null_column() {
        let raw = PyValue::Str(r#"{"a": null}"#.to_string());
        assert_eq!(extras_from_payload(Some(&raw), &["a"]), None);
        assert_eq!(extras_from_payload(Some(&raw), &["missing"]), None);
        assert_eq!(extras_from_payload(None, &["a"]), None);
        // Not an object → nothing.
        assert_eq!(
            extras_from_payload(Some(&PyValue::Str("[1]".to_string())), &["a"]),
            None
        );
    }
}
