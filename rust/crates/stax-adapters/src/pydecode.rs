//! `str(exc)` for the two CPython decoder exceptions this crate has to print.
//!
//! `custom_jsonl.py` interpolates decoder exceptions straight into user-facing
//! messages — three times, on three different rejection legs:
//!
//! ```text
//! Error: manifest /…/stackunderflow-history-plugin.json is not valid JSON:
//!        Expecting value: line 1 column 1 (char 0)
//! Error: line 4: not valid JSON: Expecting ',' delimiter: line 1 column 12 (char 11)
//! Error: stream is not valid UTF-8: 'utf-8' codec can't decode byte 0xff in
//!        position 3: invalid start byte
//! ```
//!
//! so the port has to reproduce `str(exc)` exactly, not merely fail. Both
//! functions are *explanations* of a failure the real decoder already returned;
//! neither is a decoder.
//!
//! # This is the THIRD copy of the JSON half, and that is a finding (DIV-451)
//!
//! `stax_hooks::jsonerr` and `stax_server::services::json_error` already carry
//! the same scanner, each pinned by its own transcribed-from-CPython table.
//! This crate cannot reach either: `stax-adapters` depends on `anyhow`,
//! `blake2b_simd`, `rusqlite` and `serde_json` and nothing else in the
//! workspace, and that independence is the charter's (`lib.rs`: "etl →
//! adapters → () stays acyclic"). Collapsing the three needs a crate-graph
//! decision that is the architect's, not a leg's — recorded in
//! `rust/TASKS-RS.md` with the close spelled out.
//!
//! What is NOT left to chance is drift: [`tests::the_messages_are_cpythons`]
//! is `stax_hooks::jsonerr`'s transcribed table, verbatim, plus this leg's own
//! rows. Two copies pinned by one corpus cannot disagree without a test going
//! red.
//!
//! Positions are **character** offsets in the JSON half, as CPython's are, and
//! **byte** offsets in the UTF-8 half, as CPython's are — the two exceptions
//! genuinely differ, because one is raised on a `str` and the other on `bytes`.

/// The four whitespace characters `json`'s `WHITESPACE` regex accepts.
const WS: [char; 4] = [' ', '\t', '\n', '\r'];

/// `str(json.JSONDecodeError)` for a document that does not decode.
///
/// `None` when the document *is* valid JSON — the caller has already decoded it
/// and this would be dead output.
#[must_use]
pub fn json_decode_error(text: &str) -> Option<String> {
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
                    // `\uXXXX`: four hex digits, else "Invalid \uXXXX escape",
                    // raised at the `u` itself and not at the first hex slot.
                    let hex_ok = (2..6)
                        .all(|offset| doc.get(index + offset).is_some_and(char::is_ascii_hexdigit));
                    if !hex_ok {
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

// ── UnicodeDecodeError ───────────────────────────────────────────────────────

/// The lead bytes CPython's UTF-8 decoder is willing to *start* a sequence
/// with. `0xc0`/`0xc1` are overlong two-byte leads and `0xf5`..`0xff` are past
/// U+10FFFF, so both are refused before any continuation byte is read — which
/// is exactly the split between the decoder's two single-byte messages.
fn is_lead_byte(byte: u8) -> bool {
    (0xc2..=0xf4).contains(&byte)
}

/// `str(UnicodeDecodeError)` for bytes that are not UTF-8.
///
/// `None` when `bytes` decodes cleanly. The classification is
/// [`std::str::Utf8Error`]'s, mapped onto CPython's three `errmsg` strings:
///
/// | `error_len()` | CPython |
/// |---|---|
/// | `None` (truncated at the end) | `unexpected end of data`, spanning to the end of the buffer |
/// | `Some(1)` on a byte that cannot lead | `invalid start byte` |
/// | `Some(1)` on a byte that can lead | `invalid continuation byte` |
/// | `Some(n > 1)` | `invalid continuation byte`, spanning `n` bytes |
///
/// The singular/plural split is CPython's own: `raise_decode_error` prints
/// `byte 0x%02x in position %zd` when the bad run is one byte long and
/// `bytes in position %zd-%zd` otherwise. Every row of
/// [`tests::the_utf8_messages_are_cpythons`] was produced by running
/// `bytes.decode("utf-8")` under the campaign's own interpreter.
#[must_use]
pub fn utf8_decode_error(bytes: &[u8]) -> Option<String> {
    let error = std::str::from_utf8(bytes).err()?;
    let start = error.valid_up_to();
    let (end, reason) = match error.error_len() {
        None => (bytes.len(), "unexpected end of data"),
        Some(1) if !bytes.get(start).copied().is_some_and(is_lead_byte) => {
            (start + 1, "invalid start byte")
        }
        Some(len) => (start + len, "invalid continuation byte"),
    };
    let where_ = if end == start + 1 {
        format!(
            "byte 0x{:02x} in position {start}",
            bytes.get(start).copied().unwrap_or(0)
        )
    } else {
        format!("bytes in position {start}-{}", end - 1)
    };
    Some(format!("'utf-8' codec can't decode {where_}: {reason}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every expectation here was produced by CPython itself: `json.loads(case)`
    /// in a subprocess, message + lineno + colno + pos. They are transcribed,
    /// not predicted — and the table is `stax_hooks::jsonerr`'s, copied whole,
    /// so the two implementations of this scanner are pinned by ONE corpus.
    #[test]
    fn the_messages_are_cpythons() {
        let cases: [(&str, &str); 22] = [
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
                json_decode_error(input).as_deref(),
                Some(expected),
                "input {input:?}"
            );
        }
        assert_eq!(
            json_decode_error(r#"{"a": tru}"#).as_deref(),
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
            assert_eq!(json_decode_error(ok), None, "input {ok:?}");
        }
    }

    #[test]
    fn the_column_counts_characters_not_bytes() {
        // `é` is two UTF-8 bytes and one character; CPython counts characters.
        assert_eq!(
            json_decode_error("[\"é\" 1]").as_deref(),
            Some("Expecting ',' delimiter: line 1 column 6 (char 5)")
        );
    }

    /// Transcribed the same way: `bytes.decode("utf-8")` under the campaign's
    /// interpreter, `str(exc)` captured verbatim. The four rows that matter are
    /// the four the classification could get wrong — a non-lead byte
    /// (`invalid start byte`), a lead with a bad continuation (`invalid
    /// continuation byte`, singular), a lead with TWO good bytes and a bad
    /// third (plural), and a truncation at the buffer's end.
    #[test]
    fn the_utf8_messages_are_cpythons() {
        let cases: [(&[u8], &str); 15] = [
            (
                b"\xff",
                "'utf-8' codec can't decode byte 0xff in position 0: invalid start byte",
            ),
            (
                b"a\xffb",
                "'utf-8' codec can't decode byte 0xff in position 1: invalid start byte",
            ),
            (
                b"\x80abc",
                "'utf-8' codec can't decode byte 0x80 in position 0: invalid start byte",
            ),
            (
                b"\xc0\x80",
                "'utf-8' codec can't decode byte 0xc0 in position 0: invalid start byte",
            ),
            (
                b"\xc1\xbf",
                "'utf-8' codec can't decode byte 0xc1 in position 0: invalid start byte",
            ),
            (
                b"\xfe\xff",
                "'utf-8' codec can't decode byte 0xfe in position 0: invalid start byte",
            ),
            (
                b"\xf5\x80\x80\x80",
                "'utf-8' codec can't decode byte 0xf5 in position 0: invalid start byte",
            ),
            (
                b"\xc3(",
                "'utf-8' codec can't decode byte 0xc3 in position 0: invalid continuation byte",
            ),
            (
                b"\xe2(\xa1",
                "'utf-8' codec can't decode byte 0xe2 in position 0: invalid continuation byte",
            ),
            (
                b"\xe0\x80\x80",
                "'utf-8' codec can't decode byte 0xe0 in position 0: invalid continuation byte",
            ),
            (
                b"ok\xed\xa0\x80",
                "'utf-8' codec can't decode byte 0xed in position 2: invalid continuation byte",
            ),
            (
                b"\xe2\x82(",
                "'utf-8' codec can't decode bytes in position 0-1: invalid continuation byte",
            ),
            (
                b"\xc3",
                "'utf-8' codec can't decode byte 0xc3 in position 0: unexpected end of data",
            ),
            (
                b"\xe0\xa0",
                "'utf-8' codec can't decode bytes in position 0-1: unexpected end of data",
            ),
            (
                b"x\xf0\x9f\x92",
                "'utf-8' codec can't decode bytes in position 1-3: unexpected end of data",
            ),
        ];
        for (input, expected) in cases {
            assert_eq!(
                utf8_decode_error(input).as_deref(),
                Some(expected),
                "input {input:?}"
            );
        }
        // Clean bytes have no error — the `None` the caller reads as "decoded".
        assert_eq!(utf8_decode_error("héllo ok".as_bytes()), None);
        assert_eq!(utf8_decode_error(b""), None);
    }
}
