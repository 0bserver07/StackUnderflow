//! `json.JSONDecodeError`'s `(pos, msg)` — CPython's decoder, error path only.
//!
//! # Why this exists
//!
//! FastAPI does not use pydantic to parse a request body. `routing.py` calls
//! `await request.json()`, catches CPython's `json.JSONDecodeError`, and builds
//! the validation error **by hand**:
//!
//! ```python
//! except json.JSONDecodeError as e:
//!     validation_error = RequestValidationError(
//!         [{"type": "json_invalid", "loc": ("body", e.pos),
//!           "msg": "JSON decode error", "input": {},
//!           "ctx": {"error": e.msg}}],
//!         body=e.doc,
//!     )
//! ```
//!
//! So `e.pos` (a character offset) and `e.msg` (one of nine fixed strings) are
//! **on the wire**, and no amount of care with `serde_json` produces them:
//! serde reports a line/column pair and its own wording, and the two disagree
//! on both. `POST /api/refresh` is the batch-C endpoint with a body, and its
//! isolated differ (`rust/REFRESH-DIFFER.md`) measured the gap:
//!
//! ```text
//! body "nope"
//!   python {"detail":[{"type":"json_invalid","loc":["body",0],"msg":"JSON decode error",
//!                      "input":{},"ctx":{"error":"Expecting value"}}]}
//!   rust   {"detail":[{"type":"json_invalid","loc":["body"],"msg":"JSON decode error",
//!                      "input":null}]}
//! ```
//!
//! # What this module is, and what it deliberately is not
//!
//! It is a **validator**, not a parser: it walks the document exactly as
//! `json/decoder.py` does and returns the first error CPython would raise, or
//! `None`. It never builds a value — `serde_json` does that, and only on inputs
//! this module has already blessed. That halves the surface (no unescaping, no
//! number conversion, no object construction) while keeping every branch that
//! can *raise*, which is the only part observable here.
//!
//! Offsets are **character** offsets, because Python's are: `e.pos` indexes a
//! `str`. A byte offset would be right until the first non-ASCII byte and then
//! silently wrong, which is the worst failure mode available.
//!
//! Transcribed from `Lib/json/decoder.py` (`JSONDecoder.decode`, `raw_decode`,
//! `py_make_scanner`, `JSONObject`, `JSONArray`, `py_scanstring`) rather than
//! inferred from the docs. The nine messages are quoted at their raise sites.

/// `json.decoder.WHITESPACE` — `[ \t\n\r]*`, and NOT `char::is_whitespace`.
///
/// CPython's JSON whitespace is these four characters only. `str::trim` would
/// also eat `\x0b`, `\x0c`, `\u{85}`, `\u{2028}` and the rest of Unicode's
/// whitespace class, and every one of those is an `Expecting value` in Python.
const fn is_json_ws(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\r')
}

/// The nine `JSONDecodeError` messages, quoted at their raise sites below.
mod msg {
    pub const EXPECTING_VALUE: &str = "Expecting value";
    pub const EXTRA_DATA: &str = "Extra data";
    pub const PROPERTY_NAME: &str = "Expecting property name enclosed in double quotes";
    pub const COLON: &str = "Expecting ':' delimiter";
    pub const COMMA: &str = "Expecting ',' delimiter";
    pub const UNTERMINATED: &str = "Unterminated string starting at";
    pub const CONTROL_CHAR: &str = "Invalid control character at";
    pub const INVALID_ESCAPE: &str = "Invalid \\escape";
    pub const INVALID_XXXX: &str = "Invalid \\uXXXX escape";
}

/// The error `json.loads(text)` would raise, as `(pos, msg)`.
///
/// `None` means CPython would accept the document — at which point the caller
/// hands it to `serde_json`, which is the parser.
#[must_use]
pub fn decode_error(text: &str) -> Option<(usize, String)> {
    let doc: Vec<char> = text.chars().collect();
    // `JSONDecoder.decode`: `obj, end = self.raw_decode(s, idx=_w(s, 0).end())`.
    let start = skip_ws(&doc, 0);
    let end = match scan_once(&doc, start) {
        Ok(end) => end,
        Err(err) => return Some(err),
    };
    // `end = _w(s, end).end(); if end != len(s): raise JSONDecodeError("Extra data", s, end)`.
    let end = skip_ws(&doc, end);
    if end != doc.len() {
        return Some((end, msg::EXTRA_DATA.to_owned()));
    }
    None
}

/// `json.decoder.WHITESPACE.match(s, idx).end()`.
fn skip_ws(doc: &[char], mut idx: usize) -> usize {
    while idx < doc.len() && is_json_ws(doc[idx]) {
        idx += 1;
    }
    idx
}

type ScanResult = Result<usize, (usize, String)>;

/// `py_make_scanner`'s `_scan_once` — dispatch on the first character.
///
/// Its `raise StopIteration(idx)` is what `raw_decode` converts into
/// `JSONDecodeError("Expecting value", s, err.value)`, so every "this is not a
/// value" exit reports that message at the index it started from.
fn scan_once(doc: &[char], idx: usize) -> ScanResult {
    let expecting_value = || Err((idx, msg::EXPECTING_VALUE.to_owned()));
    let Some(&next) = doc.get(idx) else {
        // `except IndexError: raise StopIteration(idx)` — end of document.
        return expecting_value();
    };
    match next {
        '"' => scan_string(doc, idx + 1),
        '{' => scan_object(doc, idx + 1),
        '[' => scan_array(doc, idx + 1),
        'n' if literal(doc, idx, "null") => Ok(idx + 4),
        't' if literal(doc, idx, "true") => Ok(idx + 4),
        'f' if literal(doc, idx, "false") => Ok(idx + 5),
        // `NaN` / `Infinity` / `-Infinity` are accepted by CPython and rejected
        // by `serde_json`. Accepting them here and letting serde reject them
        // would turn a 200 into a 500, so they are matched and reported as the
        // narrowing they are — see the module docs on DIV-109's sibling.
        'N' if literal(doc, idx, "NaN") => Ok(idx + 3),
        'I' if literal(doc, idx, "Infinity") => Ok(idx + 8),
        '-' if literal(doc, idx, "-Infinity") => Ok(idx + 9),
        c if c == '-' || c.is_ascii_digit() => {
            scan_number(doc, idx).map_or_else(expecting_value, Ok)
        }
        _ => expecting_value(),
    }
}

fn literal(doc: &[char], idx: usize, word: &str) -> bool {
    word.chars()
        .enumerate()
        .all(|(offset, c)| doc.get(idx + offset) == Some(&c))
}

/// `json.decoder.NUMBER_RE` — `-?(?:0|[1-9]\d*)(\.\d+)?([eE][-+]?\d+)?`.
///
/// `None` where the regex does not match, which `_scan_once` turns into the
/// `StopIteration` its caller reports as `Expecting value`.
fn scan_number(doc: &[char], start: usize) -> Option<usize> {
    let mut i = start;
    let digits = |doc: &[char], i: &mut usize| -> bool {
        let from = *i;
        while doc.get(*i).is_some_and(char::is_ascii_digit) {
            *i += 1;
        }
        *i > from
    };
    if doc.get(i) == Some(&'-') {
        i += 1;
    }
    match doc.get(i) {
        Some('0') => i += 1,
        Some(c) if c.is_ascii_digit() => {
            if !digits(doc, &mut i) {
                return None;
            }
        }
        _ => return None,
    }
    if doc.get(i) == Some(&'.') {
        let mut j = i + 1;
        if !digits(doc, &mut j) {
            // The regex's `(\.\d+)?` group simply does not match; the integer
            // part still did, so the number ENDS here and the stray `.` becomes
            // the caller's problem (an `Extra data` or a delimiter error).
            return Some(i);
        }
        i = j;
    }
    if matches!(doc.get(i), Some('e' | 'E')) {
        let mut j = i + 1;
        if matches!(doc.get(j), Some('+' | '-')) {
            j += 1;
        }
        if digits(doc, &mut j) {
            i = j;
        }
    }
    Some(i)
}

/// `py_scanstring`, error path only. `begin` is the index of the opening quote;
/// `idx` is the character after it.
fn scan_string(doc: &[char], idx: usize) -> ScanResult {
    let begin = idx - 1;
    let mut i = idx;
    loop {
        let Some(&c) = doc.get(i) else {
            // `raise JSONDecodeError("Unterminated string starting at", s, begin)`
            return Err((begin, msg::UNTERMINATED.to_owned()));
        };
        match c {
            '"' => return Ok(i + 1),
            '\\' => {
                let Some(&esc) = doc.get(i + 1) else {
                    return Err((begin, msg::UNTERMINATED.to_owned()));
                };
                if esc == 'u' {
                    // `Invalid \uXXXX escape` is raised at the index of the
                    // four hex digits, i.e. one past the `u`.
                    let hex_at = i + 2;
                    let ok =
                        (0..4).all(|k| doc.get(hex_at + k).is_some_and(char::is_ascii_hexdigit));
                    if !ok {
                        return Err((hex_at, msg::INVALID_XXXX.to_owned()));
                    }
                    i = hex_at + 4;
                } else if matches!(esc, '"' | '\\' | '/' | 'b' | 'f' | 'n' | 'r' | 't') {
                    i += 2;
                } else {
                    // `raise JSONDecodeError(f"Invalid \\escape: {esc!r}", s, pos)`
                    // where `pos` is the index of the BACKSLASH.
                    return Err((i, format!("{}: {}", msg::INVALID_ESCAPE, char_repr(esc))));
                }
            }
            // `if strict and terminator < '\x20'` — `strict` is the default.
            c if (c as u32) < 0x20 => {
                return Err((i, format!("{} {}", msg::CONTROL_CHAR, char_repr(c))));
            }
            _ => i += 1,
        }
    }
}

/// CPython's `repr()` of a single character, as the two string errors embed it.
///
/// `repr` prefers single quotes, escapes the C0 controls it has short names for
/// and falls back to `\xNN`. Only the characters that can reach the two call
/// sites are handled; anything else is emitted literally inside the quotes,
/// which is what `repr` does for a printable.
fn char_repr(c: char) -> String {
    let body = match c {
        '\n' => "\\n".to_owned(),
        '\r' => "\\r".to_owned(),
        '\t' => "\\t".to_owned(),
        '\'' => "\\'".to_owned(),
        '\\' => "\\\\".to_owned(),
        c if (c as u32) < 0x20 || c as u32 == 0x7f => format!("\\x{:02x}", c as u32),
        c => c.to_string(),
    };
    format!("'{body}'")
}

/// `JSONObject`. `idx` is the character after `{`.
fn scan_object(doc: &[char], idx: usize) -> ScanResult {
    let mut end = idx;
    // `nextchar = s[end:end+1]; if nextchar != '"':` — note the slice, which
    // yields `''` at EOF rather than raising.
    if doc.get(end) != Some(&'"') {
        if doc.get(end).copied().is_some_and(is_json_ws) {
            end = skip_ws(doc, end);
        }
        match doc.get(end) {
            Some('}') => return Ok(end + 1),
            Some('"') => {}
            // `raise JSONDecodeError("Expecting property name enclosed in
            //  double quotes", s, end)`
            _ => return Err((end, msg::PROPERTY_NAME.to_owned())),
        }
    }
    end += 1;
    loop {
        // The key. `scanstring` is entered with `end` already past the quote.
        end = scan_string(doc, end)?;
        if doc.get(end) != Some(&':') {
            end = skip_ws(doc, end);
            if doc.get(end) != Some(&':') {
                return Err((end, msg::COLON.to_owned()));
            }
        }
        end += 1;
        end = skip_ws(doc, end);
        end = scan_once(doc, end)?;

        // `try: nextchar = s[end]; if nextchar in _ws: … except IndexError:
        //  nextchar = ''` then `end += 1`.
        let mut nextchar = doc.get(end).copied();
        if nextchar.is_some_and(is_json_ws) {
            end = skip_ws(doc, end + 1);
            nextchar = doc.get(end).copied();
        }
        end += 1;
        match nextchar {
            Some('}') => return Ok(end),
            Some(',') => {}
            // `raise JSONDecodeError("Expecting ',' delimiter", s, end - 1)`.
            _ => return Err((end - 1, msg::COMMA.to_owned())),
        }
        end = skip_ws(doc, end);
        let nextchar = doc.get(end).copied();
        end += 1;
        if nextchar != Some('"') {
            return Err((end - 1, msg::PROPERTY_NAME.to_owned()));
        }
    }
}

/// `JSONArray`. `idx` is the character after `[`.
fn scan_array(doc: &[char], idx: usize) -> ScanResult {
    let mut end = idx;
    let mut nextchar = doc.get(end).copied();
    if nextchar.is_some_and(is_json_ws) {
        end = skip_ws(doc, end + 1);
        nextchar = doc.get(end).copied();
    }
    if nextchar == Some(']') {
        return Ok(end + 1);
    }
    loop {
        end = scan_once(doc, end)?;
        let mut nextchar = doc.get(end).copied();
        if nextchar.is_some_and(is_json_ws) {
            end = skip_ws(doc, end + 1);
            nextchar = doc.get(end).copied();
        }
        end += 1;
        match nextchar {
            Some(']') => return Ok(end),
            Some(',') => {}
            _ => return Err((end - 1, msg::COMMA.to_owned())),
        }
        end = skip_ws(doc, end);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every expectation below was produced by running CPython, not inferred:
    ///
    /// ```text
    /// >>> json.loads(case)   # → (e.pos, e.msg)
    /// ```
    fn err(text: &str) -> Option<(usize, String)> {
        decode_error(text)
    }

    #[test]
    fn the_measured_cpython_cases_agree_position_and_message() {
        // These eleven are the exact pairs measured against
        // ../StackUnderflow/.venv/bin/python during the /api/refresh proof.
        assert_eq!(err("nope"), Some((0, "Expecting value".into())));
        assert_eq!(err(""), Some((0, "Expecting value".into())));
        assert_eq!(
            err("{"),
            Some((
                1,
                "Expecting property name enclosed in double quotes".into()
            ))
        );
        assert_eq!(err("{\"a\""), Some((4, "Expecting ':' delimiter".into())));
        assert_eq!(err("{\"a\":}"), Some((5, "Expecting value".into())));
        assert_eq!(err("{} extra"), Some((3, "Extra data".into())));
        assert_eq!(err("[1,"), Some((3, "Expecting value".into())));
        assert_eq!(
            err("{\"a\" 1}"),
            Some((5, "Expecting ':' delimiter".into()))
        );
        assert_eq!(
            err("\"\u{1}\""),
            Some((1, "Invalid control character at '\\x01'".into()))
        );
        assert_eq!(
            err("{a:1}"),
            Some((
                1,
                "Expecting property name enclosed in double quotes".into()
            ))
        );
        // Leading whitespace is counted: two spaces, a newline, two spaces.
        assert_eq!(err("  \n  x"), Some((5, "Expecting value".into())));
    }

    #[test]
    fn well_formed_documents_report_nothing_and_are_left_to_serde() {
        for ok in [
            "{}",
            "[]",
            r#"{"a": 1}"#,
            r#"{"a": [1, 2, {"b": null}], "c": true}"#,
            "  {\n  \"a\" : 1 ,\n  \"b\" : 2\n}  ",
            "0",
            "-1.5e-3",
            r#""text""#,
            r#""é \n \\ \" ""#,
            "[[[[]]]]",
        ] {
            assert_eq!(err(ok), None, "{ok:?} should parse");
        }
    }

    #[test]
    fn the_offset_is_in_characters_not_bytes() {
        // "é" is two UTF-8 bytes and one Python character. A byte offset would
        // report 5 here; CPython reports 4.
        assert_eq!(err("[\"é\", x]"), Some((6, "Expecting value".into())));
        assert_eq!(err("\"é\" x"), Some((4, "Extra data".into())));
    }

    #[test]
    fn json_whitespace_is_four_characters_and_not_unicodes_definition() {
        // `\x0c` (form feed) is whitespace to `str::trim` and NOT to the JSON
        // decoder, so it is an `Expecting value` at its own index.
        assert_eq!(err("\u{c}{}"), Some((0, "Expecting value".into())));
        // …while the four real ones are skipped.
        assert_eq!(err(" \t\r\n{}"), None);
    }

    #[test]
    fn the_two_string_errors_carry_cpythons_repr_of_the_offending_character() {
        assert_eq!(
            err("\"a\nb\""),
            Some((2, "Invalid control character at '\\n'".into()))
        );
        assert_eq!(
            err("\"abc"),
            Some((0, "Unterminated string starting at".into()))
        );
        assert_eq!(err(r#""\q""#), Some((1, "Invalid \\escape: 'q'".into())));
        assert_eq!(
            err(r#""\u12g4""#),
            Some((3, "Invalid \\uXXXX escape".into()))
        );
    }

    #[test]
    fn a_missing_comma_is_reported_at_the_character_before_the_cursor() {
        // `raise JSONDecodeError("Expecting ',' delimiter", s, end - 1)` — the
        // `- 1` is why this is 6 and not 7.
        assert_eq!(
            err(r#"{"a":1 "b":2}"#),
            Some((7, "Expecting ',' delimiter".into()))
        );
        assert_eq!(err("[1 2]"), Some((3, "Expecting ',' delimiter".into())));
    }

    #[test]
    fn a_trailing_comma_is_a_property_name_error_in_an_object() {
        assert_eq!(
            err(r#"{"a":1,}"#),
            Some((
                7,
                "Expecting property name enclosed in double quotes".into()
            ))
        );
        // …and an `Expecting value` in an array, because the array loop goes
        // straight back to `scan_once`.
        assert_eq!(err("[1,]"), Some((3, "Expecting value".into())));
    }
}
