//! `json.dumps(obj, default=str)` over a [`serde_json::Value`] — the exact
//! bytes the writer puts in `messages.raw_json` and `messages.tools_json`.
//!
//! # Why this is not `Value::to_string()`
//!
//! It is tempting, and it is wrong in three separate ways. `crates/stax-adapters/
//! src/dump.rs` uses `record.raw.to_string()` and is *correct there* — the parity
//! harness's Python reference dumps with compact separators on purpose, so the
//! two agree. The **store column** is a different contract:
//!
//! | | `serde_json::to_string` | `json.dumps(obj, default=str)` |
//! |---|---|---|
//! | separators | `,` / `:` | `", "` / `": "` |
//! | non-ASCII | emitted as UTF-8 | escaped `\uXXXX` (`ensure_ascii=True`) |
//! | floats | Rust shortest round-trip | CPython `repr` (`1e-05`, not `1e-5`) |
//!
//! Any one of the three makes every `raw_json` byte differ from Python's, which
//! would sink the wave-4 full-row diff on the first record. The float column of
//! that table is DIV-035 restated: a locally rewritten `repr(float)` already
//! manufactured 145 false divergences once in this campaign.
//!
//! # Why it walks `serde_json::Value` instead of converting to `pyjson::Value`
//!
//! Converting would deep-clone every `raw` blob per row, and — the real reason —
//! [`stax_core::queries::pyjson::Value::Int`] is an `i64`, so a JSON integer in
//! `(i64::MAX, u64::MAX]` could not survive the trip. `serde_json` parses that
//! range as `u64` and CPython keeps it exact as an `int`; walking the source
//! value directly and formatting `u64` with `{}` keeps both exact.
//!
//! The two formatters that *are* subtle — the `ensure_ascii` string escaper and
//! `repr(float)` — are **not** re-implemented here. They are called through
//! `pyjson`, which is the single source of truth for both.
//!
//! # `default=str`
//!
//! Unreachable, and that is a statement about types, not luck: `Record::raw` is
//! a `serde_json::Value`, so every node is already JSON-native. Python's
//! `default=str` fires for `datetime`/`Path`/`set` objects an adapter might have
//! stuffed into the dict; no Rust adapter can, because the field will not hold
//! one.

use serde_json::Value;
use stax_core::queries::pyjson;

/// `json.dumps(value, default=str)` — the `messages.raw_json` byte contract.
#[must_use]
pub fn dumps_default(value: &Value) -> String {
    let mut out = String::new();
    write(&mut out, value);
    out
}

/// `json.dumps(list(record.tools))` — the `messages.tools_json` byte contract.
///
/// A separate entry point because the writer's argument is a `Vec<String>`, not
/// a `Value`, and building an intermediate array to serialise it is pure
/// ceremony. `[]` for the empty case matches Python exactly (`json.dumps([])`).
#[must_use]
pub fn dumps_str_list(items: &[String]) -> String {
    let mut out = String::from("[");
    for (index, item) in items.iter().enumerate() {
        if index > 0 {
            out.push_str(", ");
        }
        out.push_str(&escape(item));
    }
    out.push(']');
    out
}

/// `json.encoder.py_encode_basestring_ascii` — borrowed from `pyjson` rather
/// than rewritten (see the module docs, and DIV-035).
fn escape(text: &str) -> String {
    pyjson::dumps_default(&pyjson::Value::Str(text.to_string()))
}

fn write(out: &mut String, value: &Value) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(true) => out.push_str("true"),
        Value::Bool(false) => out.push_str("false"),
        Value::Number(number) => write_number(out, number),
        Value::String(text) => out.push_str(&escape(text)),
        Value::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push_str(", ");
                }
                write(out, item);
            }
            out.push(']');
        }
        Value::Object(entries) => {
            out.push('{');
            for (index, (key, item)) in entries.iter().enumerate() {
                if index > 0 {
                    out.push_str(", ");
                }
                out.push_str(&escape(key));
                out.push_str(": ");
                write(out, item);
            }
            out.push('}');
        }
    }
}

/// An `int` prints with `{}`; a `float` prints with CPython's `repr`.
///
/// The int/float split is the parse's, not ours: `serde_json` yields an integer
/// variant exactly where CPython's `json.loads` yields an `int` (no `.`, no
/// exponent, in range), so the two agree on which branch a literal takes before
/// either formatter runs.
fn write_number(out: &mut String, number: &serde_json::Number) {
    use std::fmt::Write as _;
    if let Some(value) = number.as_i64() {
        let _ = write!(out, "{value}");
    } else if let Some(value) = number.as_u64() {
        // Beyond `i64::MAX`. CPython's `int` is unbounded and prints the exact
        // digits; so does this. Routing through `pyjson::Value::Int` could not.
        let _ = write!(out, "{value}");
    } else if let Some(value) = number.as_f64() {
        out.push_str(&pyjson::repr_float(value));
    } else {
        // `serde_json::Number` has exactly the three representations above with
        // `arbitrary_precision` off (it is off — the workspace enables
        // `preserve_order` + `float_roundtrip` only). Unreachable; a literal
        // `0` is a less damaging landing than a panic inside a transaction.
        out.push('0');
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn separators_are_pythons_defaults_not_serde_jsons() {
        // The difference that would have made every raw_json byte diverge.
        let value = json!({"z": 1, "a": [1, 2]});
        assert_eq!(dumps_default(&value), r#"{"z": 1, "a": [1, 2]}"#);
        assert_eq!(
            value.to_string(),
            r#"{"z":1,"a":[1,2]}"#,
            "serde's is compact"
        );
    }

    #[test]
    fn key_order_is_the_sources_order() {
        // `preserve_order` earns its keep here: `json.dumps` never sorts.
        let value: Value = serde_json::from_str(r#"{"z":1,"a":2,"m":3}"#).unwrap();
        assert_eq!(dumps_default(&value), r#"{"z": 1, "a": 2, "m": 3}"#);
    }

    #[test]
    fn non_ascii_is_escaped_because_ensure_ascii_is_on() {
        // Written with doubled backslashes rather than a raw string so the
        // expectation is unmistakably the six ASCII bytes `\u00e9`, which is
        // what CPython emits and what the store column holds.
        assert_eq!(dumps_default(&json!("h\u{e9}llo")), "\"h\\u00e9llo\"");
        // Astral plane -> surrogate pair, exactly as CPython emits it.
        assert_eq!(dumps_default(&json!("\u{1f980}")), "\"\\ud83e\\udd80\"");
        // ...and as an object KEY, which the writer hits on tool-arg dicts.
        let value: Value = serde_json::from_str("{\"\u{e9}\":1}").unwrap();
        assert_eq!(dumps_default(&value), "{\"\\u00e9\": 1}");
    }

    #[test]
    fn floats_print_as_cpython_repr() {
        // Rust's Display says `0.00001625`; CPython says `1.625e-05`. DIV-035.
        assert_eq!(dumps_default(&json!(1.625e-5)), "1.625e-05");
        // Integral floats keep the `.0` CPython's repr leaves on.
        assert_eq!(dumps_default(&json!(3.0)), "3.0");
        // …and an integer stays an integer.
        assert_eq!(dumps_default(&json!(3)), "3");
    }

    #[test]
    fn an_integer_past_i64_max_keeps_its_exact_digits() {
        let value: Value = serde_json::from_str("18446744073709551615").unwrap();
        assert_eq!(dumps_default(&value), "18446744073709551615");
    }

    #[test]
    fn empty_containers_match_json_dumps() {
        assert_eq!(dumps_default(&json!({})), "{}");
        assert_eq!(dumps_default(&json!([])), "[]");
        assert_eq!(dumps_str_list(&[]), "[]");
    }

    #[test]
    fn tools_json_is_a_default_separator_list() {
        assert_eq!(
            dumps_str_list(&["Bash".to_string(), "Grep".to_string()]),
            r#"["Bash", "Grep"]"#
        );
        assert_eq!(dumps_str_list(&["a\"b".to_string()]), r#"["a\"b"]"#);
    }

    #[test]
    fn control_characters_take_the_short_escapes() {
        assert_eq!(dumps_default(&json!("a\nb\tc")), r#""a\nb\tc""#);
        assert_eq!(dumps_default(&json!("\u{1}")), "\"\\u0001\"");
    }
}
