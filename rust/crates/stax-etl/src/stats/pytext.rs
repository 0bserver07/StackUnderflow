//! Python `str` / value semantics the mart dims path depends on, in one place.
//!
//! The second pass of `project_mart` (`etl/marts/project.py::_refresh_message_dims`)
//! runs the *real* pipeline functions — `stats.classifier`, `stats.enricher`,
//! `stats.aggregator._command_analysis` — over `messages.raw_json`. Those
//! functions are ordinary Python, so their string handling is Python's, not
//! Rust's, and the differences are not cosmetic:
//!
//! * `str.lstrip()` strips CPython's `Py_UNICODE_ISSPACE` set, which includes
//!   `U+001C`..`U+001F` (the C0 separators). Rust's `char::is_whitespace` is the
//!   Unicode `White_Space` property and does *not*. `command_mart.command_name`
//!   is parsed off an `lstrip`ped prompt, so the difference is a mart key.
//! * `s[:64]` slices *code points*. `&s[..64]` slices bytes and panics
//!   mid-character. The 64-char prefix is the interaction identity used by
//!   `_command_analysis`'s lookup table.
//! * `if value:` is Python truthiness over a decoded JSON value, not
//!   `Option::is_some` — `0`, `0.0`, `""`, `[]`, `{}` and `false` are all falsy.
//!   `_usage_from` reads `usage.get(k, 0) or 0`, and the cache-read dim counts
//!   records whose `cache_read` token count is *truthy*.
//!
//! Nothing here is mart-specific; it is the Python runtime, transcribed.

use serde_json::Value;

/// CPython's `Py_UNICODE_ISSPACE` — the predicate behind `str.isspace()`,
/// `str.strip()`/`lstrip()` with no argument, and the regex `\s` class for
/// `str` patterns.
///
/// The ASCII half is `unicodeobject.c`'s `_Py_ascii_whitespace` table:
/// `0x09`..`0x0D`, `0x1C`..`0x1F`, `0x20`. The `0x1C`..`0x1F` run (FILE /
/// GROUP / RECORD / UNIT SEPARATOR) is the one that surprises — those four are
/// whitespace to Python and *not* whitespace to Unicode, so they are the exact
/// characters where `str.lstrip()` and `str::trim_start()` disagree.
#[must_use]
pub fn is_py_space(c: char) -> bool {
    matches!(c,
        '\u{09}'..='\u{0d}'
        | '\u{1c}'..='\u{1f}'
        | '\u{20}'
        | '\u{85}'
        | '\u{a0}'
        | '\u{1680}'
        | '\u{2000}'..='\u{200a}'
        | '\u{2028}'
        | '\u{2029}'
        | '\u{202f}'
        | '\u{205f}'
        | '\u{3000}'
    )
}

/// Python's `s.lstrip()` (no argument).
#[must_use]
pub fn py_lstrip(s: &str) -> &str {
    s.trim_start_matches(is_py_space)
}

/// Python's `s.strip()` (no argument).
#[must_use]
pub fn py_strip(s: &str) -> &str {
    s.trim_matches(is_py_space)
}

/// Python's `s[:n]` — the first `n` *code points*, not bytes.
///
/// Returns the whole string when it is shorter than `n` characters, exactly as
/// Python's slice does (no panic, no `None`).
#[must_use]
pub fn py_char_prefix(s: &str, n: usize) -> &str {
    match s.char_indices().nth(n) {
        Some((byte_idx, _)) => &s[..byte_idx],
        None => s,
    }
}

/// Python's `bool(v)` for a value decoded from JSON.
///
/// `None`, `False`, `0`, `0.0`, `""`, `[]` and `{}` are falsy; everything else
/// is truthy. Mirrors `stax_adapters::pyval::py_bool` — the duplication is
/// deliberate for now (this crate does not depend on the adapters), and is on
/// the same dedup list as the `pyjson` twins.
#[must_use]
pub fn py_truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// Python's `str(v)` for a value decoded from JSON.
///
/// Reached from exactly one place in the mart path — `classifier._detect_error`
/// does `_categorise(str(err_body))` where `err_body` came out of a
/// `tool_result` block and is *usually* a string or a list of text blocks but
/// need not be. Mirrors `stax_adapters::pyval::py_str`; see that module for the
/// full table and the one recorded repr difference (unprintable non-ASCII).
#[must_use]
pub fn py_str(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => py_repr(other),
    }
}

/// Python's `repr(v)` for a value decoded from JSON.
#[must_use]
pub fn py_repr(v: &Value) -> String {
    match v {
        Value::Null => "None".to_string(),
        Value::Bool(true) => "True".to_string(),
        Value::Bool(false) => "False".to_string(),
        Value::Number(n) => py_number_repr(n),
        Value::String(s) => py_str_repr(s),
        Value::Array(items) => {
            let inner: Vec<String> = items.iter().map(py_repr).collect();
            format!("[{}]", inner.join(", "))
        }
        Value::Object(map) => {
            let inner: Vec<String> = map
                .iter()
                .map(|(k, v)| format!("{}: {}", py_str_repr(k), py_repr(v)))
                .collect();
            format!("{{{}}}", inner.join(", "))
        }
    }
}

/// `str(int)` / `repr(float)` for a JSON number.
fn py_number_repr(n: &serde_json::Number) -> String {
    if let Some(i) = n.as_i64() {
        return i.to_string();
    }
    if let Some(u) = n.as_u64() {
        return u.to_string();
    }
    match n.as_f64() {
        // Python's `repr(float)` is the shortest round-tripping form, and adds
        // a trailing `.0` to integral values where Rust's `Display` does not.
        Some(f) if f.fract() == 0.0 && f.is_finite() => format!("{f:?}"),
        Some(f) => {
            let s = format!("{f}");
            if s.contains(['.', 'e', 'E']) {
                s
            } else {
                format!("{s}.0")
            }
        }
        None => n.to_string(),
    }
}

/// Python's `repr(str)`: single quotes unless the value contains one and no
/// double quote, with the standard backslash escapes.
fn py_str_repr(s: &str) -> String {
    let quote = if s.contains('\'') && !s.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::with_capacity(s.len() + 2);
    out.push(quote);
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c => out.push(c),
        }
    }
    out.push(quote);
    out
}

/// `json.dumps(dict_of_str_to_int)` with CPython's *default* separators.
///
/// `json.dumps` defaults to `', '` and `': '` — not the compact form
/// `serde_json::to_string` emits — and `project_mart.errors_by_category` stores
/// the result verbatim, so the spaces are part of the column value the wave-3
/// gate diffs. Keys are emitted in insertion order (`Counter` is insertion
/// ordered and `dict(counter)` preserves it), and `ensure_ascii=True` is the
/// default; every category label in `classifier._TAXONOMY` plus the `"Other"`
/// fallback is ASCII, so the escape path here only has to cover what a label
/// could legally contain.
#[must_use]
pub fn py_json_dumps_counter(pairs: &[(String, i64)]) -> String {
    if pairs.is_empty() {
        return "{}".to_string();
    }
    let mut out = String::from("{");
    for (i, (k, v)) in pairs.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        out.push('"');
        for c in k.chars() {
            match c {
                '"' => out.push_str("\\\""),
                '\\' => out.push_str("\\\\"),
                '\n' => out.push_str("\\n"),
                '\r' => out.push_str("\\r"),
                '\t' => out.push_str("\\t"),
                c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
                c if c.is_ascii() => out.push(c),
                // ensure_ascii=True: non-ASCII becomes \uXXXX (surrogate pairs
                // above the BMP). Unreachable for the fixed taxonomy labels.
                c => {
                    let cp = c as u32;
                    if cp > 0xFFFF {
                        let v = cp - 0x1_0000;
                        out.push_str(&format!(
                            "\\u{:04x}\\u{:04x}",
                            0xD800 + (v >> 10),
                            0xDC00 + (v & 0x3FF)
                        ));
                    } else {
                        out.push_str(&format!("\\u{cp:04x}"));
                    }
                }
            }
        }
        out.push_str("\": ");
        out.push_str(&v.to_string());
    }
    out.push('}');
    out
}

// ── ASCII case-insensitive scanning ─────────────────────────────────────────
//
// Every literal in `classifier._TAXONOMY` — both the fast keyword screen and
// the confirming regex — is pure ASCII, and every regex carries `re.I`. An
// ASCII-folded byte scan is therefore the right primitive, with one recorded
// difference: Python's `re.I` on a `str` pattern is *Unicode* case-insensitive,
// so `U+212A KELVIN SIGN` matches `k` and `U+0130` lowercases to `i` + combining
// dot. Those fold non-ASCII code points onto ASCII ones; this scan does not.
// The divergence needs a Kelvin sign inside a tool-error body to be observable.
//
// Byte-level scanning is safe on UTF-8: every byte of a multi-byte sequence is
// >= 0x80, so an ASCII byte can never be part of one.

/// Whether `hay` contains `needle` (ASCII case-insensitively).
#[must_use]
pub fn contains_ci(hay: &str, needle: &str) -> bool {
    find_ci(hay, needle, 0).is_some()
}

/// The byte offset of the first ASCII-case-insensitive `needle` at or after
/// `from`, if any.
#[must_use]
pub fn find_ci(hay: &str, needle: &str, from: usize) -> Option<usize> {
    let h = hay.as_bytes();
    let n = needle.as_bytes();
    if n.is_empty() {
        return Some(from);
    }
    if h.len() < n.len() {
        return None;
    }
    let first = n[0].to_ascii_lowercase();
    let mut i = from;
    while i + n.len() <= h.len() {
        if h[i].to_ascii_lowercase() == first && starts_with_ci_bytes(&h[i..], n) {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// Whether `hay` begins with `needle` (ASCII case-insensitively).
#[must_use]
pub fn starts_with_ci(hay: &str, needle: &str) -> bool {
    starts_with_ci_bytes(hay.as_bytes(), needle.as_bytes())
}

fn starts_with_ci_bytes(hay: &[u8], needle: &[u8]) -> bool {
    hay.len() >= needle.len()
        && hay
            .iter()
            .zip(needle)
            .all(|(a, b)| a.eq_ignore_ascii_case(b))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn c0_separators_are_python_whitespace_but_not_rust_whitespace() {
        // The exact four characters where `str.lstrip()` and `trim_start()`
        // disagree; a prompt led by one of them parses to a slash command in
        // Python and to `freeform` under a naive port.
        for c in ['\u{1c}', '\u{1d}', '\u{1e}', '\u{1f}'] {
            assert!(is_py_space(c), "{c:?} is whitespace to CPython");
            assert!(!c.is_whitespace(), "{c:?} is not White_Space to Unicode");
        }
        assert_eq!(py_lstrip("\u{1c}\u{1f} /init"), "/init");
    }

    #[test]
    fn py_space_covers_the_unicode_half() {
        for c in [
            '\u{09}', '\u{0a}', '\u{0b}', '\u{0c}', '\u{0d}', '\u{20}', '\u{85}', '\u{a0}',
            '\u{1680}', '\u{2000}', '\u{200a}', '\u{2028}', '\u{2029}', '\u{202f}', '\u{205f}',
            '\u{3000}',
        ] {
            assert!(is_py_space(c), "{c:?}");
        }
        // Not whitespace to CPython, whatever it looks like.
        for c in ['\u{200b}', '\u{180e}', '\u{feff}', 'a'] {
            assert!(!is_py_space(c), "{c:?}");
        }
    }

    #[test]
    fn char_prefix_slices_code_points_not_bytes() {
        let s = "é".repeat(80);
        assert_eq!(py_char_prefix(&s, 64).chars().count(), 64);
        assert_eq!(py_char_prefix("abc", 64), "abc");
        assert_eq!(py_char_prefix("", 64), "");
    }

    #[test]
    fn truthiness_is_pythons_not_rusts() {
        assert!(!py_truthy(&json!(0)));
        assert!(!py_truthy(&json!(0.0)));
        assert!(!py_truthy(&json!("")));
        assert!(!py_truthy(&json!([])));
        assert!(!py_truthy(&json!({})));
        assert!(!py_truthy(&json!(null)));
        assert!(!py_truthy(&json!(false)));
        assert!(py_truthy(&json!(1)));
        assert!(py_truthy(&json!("0")));
        assert!(py_truthy(&json!([0])));
    }

    #[test]
    fn counter_dumps_uses_cpython_default_separators() {
        assert_eq!(py_json_dumps_counter(&[]), "{}");
        assert_eq!(
            py_json_dumps_counter(&[("Other".into(), 3), ("Syntax Error".into(), 1)]),
            r#"{"Other": 3, "Syntax Error": 1}"#
        );
    }

    #[test]
    fn ci_scanning_is_ascii_folded() {
        assert!(contains_ci(
            "TRACEBACK (most recent call last)",
            "traceback"
        ));
        assert!(starts_with_ci("Not Found", "not found"));
        assert_eq!(find_ci("aXbXc", "x", 2), Some(3));
        assert_eq!(find_ci("abc", "z", 0), None);
        // Multi-byte content must not be mis-indexed.
        assert_eq!(find_ci("héllo world", "world", 0), Some(7));
    }

    #[test]
    fn py_str_matches_cpython_on_the_shapes_detect_error_can_see() {
        assert_eq!(py_str(&json!("plain")), "plain");
        assert_eq!(py_str(&json!(null)), "None");
        assert_eq!(py_str(&json!(true)), "True");
        assert_eq!(py_str(&json!(12)), "12");
        assert_eq!(py_str(&json!({"a": 1})), "{'a': 1}");
        assert_eq!(py_str(&json!(["x", 2])), "['x', 2]");
    }
}
