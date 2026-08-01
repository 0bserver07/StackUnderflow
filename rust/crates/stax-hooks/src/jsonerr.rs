//! `json.JSONDecodeError` — the message, the line, the column and the char.
//!
//! `hooks install` on a broken `settings.json` prints CPython's decoder error
//! *inside* its own sentence:
//!
//! ```text
//! Error: …/settings.json is not valid JSON (Expecting property name enclosed
//! in double quotes: line 1 column 2 (char 1)); fix or remove it before
//! installing hooks
//! ```
//!
//! so the port has to reproduce `str(exc)` exactly, not merely fail. The parity
//! row `T-hooks-inst-dry-bad` is what found this: the first attempt said
//! "Expecting value" for `{oops` where CPython says "Expecting property name
//! enclosed in double quotes: line 1 column 2 (char 1)". A user pasting that
//! message into a search box gets different answers for the two — which is the
//! whole definition of a divergence.
//!
//! This is a scanner, not a parser: it walks the document the way
//! `json/decoder.py`'s `scan_once` / `JSONObject` / `JSONArray` / `py_scanstring`
//! do and reports the **first** point at which they would raise, with the same
//! message string and the same index. `stax_core::queries::pyjson::loads` stays
//! the decoder; this only explains its `None`.
//!
//! Positions are **character** offsets, as CPython's are — a multi-byte
//! character before the error moves `column` by one, not by its UTF-8 width.

/// The four whitespace characters `json`'s `WHITESPACE` regex accepts.
const WS: [char; 4] = [' ', '\t', '\n', '\r'];

/// `str(json.JSONDecodeError)` for a document that does not decode.
///
/// `None` when the document *is* valid JSON — the caller has already decoded it
/// and this would be dead output.
#[must_use]
pub fn decode_error(text: &str) -> Option<String> {
    let doc: Vec<char> = text.chars().collect();
    let (msg, pos) = scan_document(&doc)?;
    let (line, column) = line_column(&doc, pos);
    Some(format!("{msg}: line {line} column {column} (char {pos})"))
}

/// `JSONDecodeError.__init__`'s `lineno` / `colno`.
fn line_column(doc: &[char], pos: usize) -> (usize, usize) {
    let line = doc[..pos.min(doc.len())]
        .iter()
        .filter(|c| **c == '\n')
        .count()
        + 1;
    let last_newline = doc[..pos.min(doc.len())]
        .iter()
        .rposition(|c| *c == '\n')
        .map_or(-1_i64, |index| index as i64);
    (line, (pos as i64 - last_newline) as usize)
}

/// `decoder.decode`: one value, then trailing whitespace, then nothing.
fn scan_document(doc: &[char]) -> Option<(&'static str, usize)> {
    let start = skip_ws(doc, 0);
    let end = match scan_once(doc, start) {
        Ok(end) => end,
        Err(err) => return Some(err),
    };
    let end = skip_ws(doc, end);
    if end != doc.len() {
        return Some(("Extra data", end));
    }
    None
}

fn skip_ws(doc: &[char], mut index: usize) -> usize {
    while index < doc.len() && WS.contains(&doc[index]) {
        index += 1;
    }
    index
}

type Scan = Result<usize, (&'static str, usize)>;

/// `scan_once(s, idx)` — returns the index just past the value.
fn scan_once(doc: &[char], index: usize) -> Scan {
    let Some(&next) = doc.get(index) else {
        // `IndexError` → `StopIteration(idx)` → "Expecting value".
        return Err(("Expecting value", index));
    };
    match next {
        '"' => scan_string(doc, index + 1),
        '{' => scan_object(doc, index + 1),
        '[' => scan_array(doc, index + 1),
        _ => {
            for (literal, len) in [("null", 4), ("true", 4), ("false", 5)] {
                if matches_literal(doc, index, literal) {
                    return Ok(index + len);
                }
            }
            if let Some(end) = match_number(doc, index) {
                return Ok(end);
            }
            for (literal, len) in [("NaN", 3), ("Infinity", 8), ("-Infinity", 9)] {
                if matches_literal(doc, index, literal) {
                    return Ok(index + len);
                }
            }
            Err(("Expecting value", index))
        }
    }
}

fn matches_literal(doc: &[char], index: usize, literal: &str) -> bool {
    literal
        .chars()
        .enumerate()
        .all(|(offset, expected)| doc.get(index + offset) == Some(&expected))
}

/// `NUMBER_RE` — `(-?(?:0|[1-9]\d*))(\.\d+)?([eE][-+]?\d+)?`, anchored.
fn match_number(doc: &[char], index: usize) -> Option<usize> {
    let mut cursor = index;
    if doc.get(cursor) == Some(&'-') {
        cursor += 1;
    }
    match doc.get(cursor) {
        Some('0') => cursor += 1,
        Some(c) if c.is_ascii_digit() => {
            while doc.get(cursor).is_some_and(char::is_ascii_digit) {
                cursor += 1;
            }
        }
        _ => return None,
    }
    // The fraction only counts when at least one digit follows the point.
    if doc.get(cursor) == Some(&'.') && doc.get(cursor + 1).is_some_and(char::is_ascii_digit) {
        cursor += 1;
        while doc.get(cursor).is_some_and(char::is_ascii_digit) {
            cursor += 1;
        }
    }
    if matches!(doc.get(cursor), Some('e' | 'E')) {
        let mut probe = cursor + 1;
        if matches!(doc.get(probe), Some('+' | '-')) {
            probe += 1;
        }
        if doc.get(probe).is_some_and(char::is_ascii_digit) {
            while doc.get(probe).is_some_and(char::is_ascii_digit) {
                probe += 1;
            }
            cursor = probe;
        }
    }
    Some(cursor)
}

/// `py_scanstring(s, end)` — `end` is the index just past the opening quote.
fn scan_string(doc: &[char], mut index: usize) -> Scan {
    let begin = index - 1;
    loop {
        let Some(&c) = doc.get(index) else {
            return Err(("Unterminated string starting at", begin));
        };
        match c {
            '"' => return Ok(index + 1),
            '\\' => {
                let Some(&escape) = doc.get(index + 1) else {
                    return Err(("Unterminated string starting at", begin));
                };
                if escape == 'u' {
                    // `\uXXXX`: four hex digits, else "Invalid \uXXXX escape".
                    let hex_ok = (2..6)
                        .all(|offset| doc.get(index + offset).is_some_and(char::is_ascii_hexdigit));
                    if !hex_ok {
                        // `index + 1`, the `u` itself — NOT the first hex slot.
                        // `_decode_uXXXX` is called with `pos` pointing at the
                        // `u` and raises at that `pos`, so `"\u12"` is char 2,
                        // not 3. Measured against CPython 3.12 rather than
                        // reasoned about; it was `+ 2` and was the only
                        // mismatch in a 645K-document differential fuzz of this
                        // scanner against `json.loads`.
                        return Err(("Invalid \\uXXXX escape", index + 1));
                    }
                    index += 6;
                } else if "\"\\/bfnrt".contains(escape) {
                    index += 2;
                } else {
                    return Err(("Invalid \\escape", index));
                }
            }
            // `strict=True`: a raw control character terminates the scan.
            c if (c as u32) < 0x20 => {
                return Err(("Invalid control character at", index));
            }
            _ => index += 1,
        }
    }
}

/// `JSONObject((s, end), …)` — `end` is the index just past the `{`.
fn scan_object(doc: &[char], mut index: usize) -> Scan {
    let mut next = doc.get(index).copied();
    if next != Some('"') {
        index = skip_ws(doc, index);
        next = doc.get(index).copied();
        if next == Some('}') {
            return Ok(index + 1);
        }
        if next != Some('"') {
            return Err(("Expecting property name enclosed in double quotes", index));
        }
    }
    index += 1;
    loop {
        index = scan_string(doc, index)?;
        index = skip_ws(doc, index);
        if doc.get(index) != Some(&':') {
            index = skip_ws(doc, index);
            if doc.get(index) != Some(&':') {
                return Err(("Expecting ':' delimiter", index));
            }
        }
        index = skip_ws(doc, index + 1);
        index = scan_once(doc, index)?;
        index = skip_ws(doc, index);
        let next = doc.get(index).copied();
        index += 1;
        match next {
            Some('}') => return Ok(index),
            Some(',') => {}
            _ => return Err(("Expecting ',' delimiter", index - 1)),
        }
        index = skip_ws(doc, index);
        let next = doc.get(index).copied();
        index += 1;
        if next != Some('"') {
            return Err((
                "Expecting property name enclosed in double quotes",
                index - 1,
            ));
        }
    }
}

/// `JSONArray((s, end), …)` — `end` is the index just past the `[`.
fn scan_array(doc: &[char], mut index: usize) -> Scan {
    index = skip_ws(doc, index);
    if doc.get(index) == Some(&']') {
        return Ok(index + 1);
    }
    loop {
        index = scan_once(doc, index)?;
        index = skip_ws(doc, index);
        let next = doc.get(index).copied();
        index += 1;
        match next {
            Some(']') => return Ok(index),
            Some(',') => {}
            _ => return Err(("Expecting ',' delimiter", index - 1)),
        }
        index = skip_ws(doc, index);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every expectation here was produced by CPython 3.12.13 itself:
    /// `json.loads(case)` in a subprocess, message + lineno + colno + pos. They
    /// are transcribed, not predicted.
    #[test]
    fn the_messages_are_cpythons() {
        let cases: [(&str, &str); 22] = [
            // `\uXXXX` — the position is the `u`, not the first hex slot. No
            // case crossed this constant until the differential fuzz did, and
            // the scanner was off by one the whole time (wave-6's law: a
            // constant a port copies needs a row that crosses it). All five
            // transcribed from CPython 3.12 like the rest.
            (
                r#""\u12""#,
                "Invalid \\uXXXX escape: line 1 column 3 (char 2)",
            ),
            (
                r#"{"a": "\uZZZZ"}"#,
                "Invalid \\uXXXX escape: line 1 column 9 (char 8)",
            ),
            (
                r#""\u""#,
                "Invalid \\uXXXX escape: line 1 column 3 (char 2)",
            ),
            (
                r#"{"k": "a\ud80"}"#,
                "Invalid \\uXXXX escape: line 1 column 10 (char 9)",
            ),
            (
                r#""\uABC""#,
                "Invalid \\uXXXX escape: line 1 column 3 (char 2)",
            ),
            (
                "{oops, not json\n",
                "Expecting property name enclosed in double quotes: line 1 column 2 (char 1)",
            ),
            ("", "Expecting value: line 1 column 1 (char 0)"),
            ("   ", "Expecting value: line 1 column 4 (char 3)"),
            (
                "{",
                "Expecting property name enclosed in double quotes: line 1 column 2 (char 1)",
            ),
            ("[1,2", "Expecting ',' delimiter: line 1 column 5 (char 4)"),
            (
                r#"{"a""#,
                "Expecting ':' delimiter: line 1 column 5 (char 4)",
            ),
            (r#"{"a":}"#, "Expecting value: line 1 column 6 (char 5)"),
            (
                r#"{"a":1,}"#,
                "Expecting property name enclosed in double quotes: line 1 column 8 (char 7)",
            ),
            (r#"{"a":1}x"#, "Extra data: line 1 column 8 (char 7)"),
            (
                "\"unterminated",
                "Unterminated string starting at: line 1 column 1 (char 0)",
            ),
            (
                r#"{"a" 1}"#,
                "Expecting ':' delimiter: line 1 column 6 (char 5)",
            ),
            ("nul", "Expecting value: line 1 column 1 (char 0)"),
            ("[1 2]", "Expecting ',' delimiter: line 1 column 4 (char 3)"),
            (
                "{\n  \"a\": 1,\n  b: 2\n}",
                "Expecting property name enclosed in double quotes: line 3 column 3 (char 14)",
            ),
            (
                r#"{"a": 01}"#,
                "Expecting ',' delimiter: line 1 column 8 (char 7)",
            ),
            (
                "{'a': 1}",
                "Expecting property name enclosed in double quotes: line 1 column 2 (char 1)",
            ),
            ("[,]", "Expecting value: line 1 column 2 (char 1)"),
        ];
        for (input, expected) in cases {
            assert_eq!(
                decode_error(input).as_deref(),
                Some(expected),
                "input {input:?}"
            );
        }
        assert_eq!(
            decode_error(r#"{"a": tru}"#).as_deref(),
            Some("Expecting value: line 1 column 7 (char 6)")
        );
    }

    #[test]
    fn valid_documents_have_no_error() {
        for ok in [
            "{}",
            "[]",
            " {\"a\": [1, 2.5e-3, null, true, false]} ",
            "\"text\"",
            "-0.5",
            r#"{"a": "bé\n"}"#,
        ] {
            assert_eq!(decode_error(ok), None, "input {ok:?}");
        }
    }

    #[test]
    fn the_column_counts_characters_not_bytes() {
        // `é` is two UTF-8 bytes and one character; CPython counts characters.
        assert_eq!(
            decode_error("[\"é\" 1]").as_deref(),
            Some("Expecting ',' delimiter: line 1 column 6 (char 5)")
        );
    }
}
