//! CPython exception *messages*, for the two places `pull` interpolates one.
//!
//! `runner.pull` collects failures as `f"{remote_uuid}: manifest decrypt/parse
//! failed ({exc})"` and `f"{remote_uuid}/{shard_key}: decrypt/parse failed
//! ({exc})"`. Those strings are printed by `sync pull` and returned by
//! `sync pull --json`, so `{exc}` is a **byte contract**, not a debug aid — and
//! `{exc}` is whatever `json.loads` or `int()` raised.
//!
//! `serde_json`'s messages are nothing like CPython's ("expected value at line 1
//! column 1" vs "Expecting value: line 1 column 1 (char 0)"), so a port that
//! forwarded them would diverge on every malformed-blob case in the corpus. This
//! module translates the shapes the corpus actually crosses and is explicit
//! about the ones it does not — see DIV-214.

/// `json.loads(data)`, with CPython's `JSONDecodeError` message on failure.
///
/// # Errors
/// [`PyJsonError`], whose `Display` is `str(JSONDecodeError)`.
pub fn loads(data: &[u8]) -> Result<serde_json::Value, PyJsonError> {
    match serde_json::from_slice(data) {
        Ok(value) => Ok(value),
        Err(err) => Err(PyJsonError::from_serde(&err, data)),
    }
}

/// A decode failure carrying CPython's rendering.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PyJsonError {
    /// The `msg` half — `Expecting value`, `Expecting ',' delimiter`, …
    pub msg: String,
    /// 1-based line.
    pub lineno: usize,
    /// 1-based column.
    pub colno: usize,
    /// 0-based character offset.
    pub pos: usize,
    /// Whether [`Self::msg`] is one this module claims parity for (DIV-214).
    pub faithful: bool,
}

impl PyJsonError {
    fn from_serde(err: &serde_json::Error, data: &[u8]) -> Self {
        let line = err.line().max(1);
        // serde reports column 0 for an EOF at the very start; CPython's
        // columns are 1-based everywhere.
        let column = err.column().max(1);
        let text = err.to_string();
        let mut pos = char_offset(data, line, column);
        // Three serde families are CPython's one `Expecting value`:
        //
        //   `expected value`               — the byte begins no value at all
        //   `trailing comma`               — `[1,]`, where CPython also wants a value
        //   `expected ident` / `EOF while parsing a value`
        //                                  — a PARTIAL literal (`not json`, `tru`)
        //
        // The third family needs the position moved. serde reports where it gave
        // up mid-token; CPython's scanner checks `null`/`true`/`false` as a whole
        // at the token's FIRST character and raises there — `json.loads("not
        // json")` is `char 0`, where serde says column 2. Backing up over the
        // ASCII letters recovers the token start, which is the reference's answer.
        let partial_literal =
            text.starts_with("expected ident") || text.starts_with("EOF while parsing a value");
        if partial_literal {
            while pos > 0 && data.get(pos - 1).is_some_and(u8::is_ascii_alphabetic) {
                pos -= 1;
            }
        }
        let (msg, faithful) = if text.starts_with("expected value")
            || text.starts_with("trailing comma")
            || partial_literal
        {
            ("Expecting value".to_owned(), true)
        } else {
            // DIV-214: the delimiter families (`Expecting ',' delimiter`,
            // `Expecting ':' delimiter`, `Unterminated string starting at`,
            // `Invalid control character`, `Expecting property name enclosed in
            // double quotes`) are NOT translated. Reproducing them means
            // reproducing CPython's scanner state machine, and no corpus row
            // crosses one — which by this wave's own law means a translation
            // would be untested code that only looks right.
            (format!("Expecting value <unported: {text}>"), false)
        };
        let (lineno, colno) = line_column(data, pos);
        Self {
            msg,
            lineno,
            colno,
            pos,
            faithful,
        }
    }
}

impl std::fmt::Display for PyJsonError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `JSONDecodeError.__init__`: `f"{msg}: line {lineno} column {colno}
        // (char {pos})"`.
        write!(
            f,
            "{}: line {} column {} (char {})",
            self.msg, self.lineno, self.colno, self.pos
        )
    }
}

impl std::error::Error for PyJsonError {}

/// The 0-based offset of `(line, column)`.
///
/// CPython counts *characters*; serde counts bytes. They agree on ASCII, and
/// every malformed blob the corpus feeds is ASCII or raw ciphertext (which
/// fails at offset 0 either way). Documented rather than silently assumed.
fn char_offset(data: &[u8], line: usize, column: usize) -> usize {
    let mut offset = 0_usize;
    let mut current_line = 1_usize;
    for byte in data {
        if current_line == line {
            break;
        }
        offset += 1;
        if *byte == b'\n' {
            current_line += 1;
        }
    }
    offset + column.saturating_sub(1)
}

/// The inverse of [`char_offset`] — the 1-based `(line, column)` of an offset.
fn line_column(data: &[u8], pos: usize) -> (usize, usize) {
    let mut line = 1_usize;
    let mut column = 1_usize;
    for byte in data.iter().take(pos) {
        if *byte == b'\n' {
            line += 1;
            column = 1;
        } else {
            column += 1;
        }
    }
    (line, column)
}

/// `int(value)` for a value that came out of `json.loads`.
///
/// `runner.pull` does `int(manifest.get("generation", 0))` without a guard, so
/// every branch here is reachable from a hostile manifest and each raises the
/// exception CPython raises.
///
/// # Errors
/// The `ValueError` / `TypeError` message, rendered as CPython renders it.
pub fn py_int(value: &serde_json::Value) -> Result<i64, String> {
    match value {
        // `int(True)` is 1 — `bool` is a subclass of `int`.
        serde_json::Value::Bool(flag) => Ok(i64::from(*flag)),
        serde_json::Value::Number(number) => number.as_i64().map_or_else(
            || {
                // `int(2.9)` truncates toward zero, and `int(float('inf'))`
                // raises `OverflowError` — unreachable through JSON, which has
                // no infinity literal.
                number.as_f64().map_or_else(
                    || Err(type_error("float")),
                    |float| Ok(float.trunc() as i64),
                )
            },
            Ok,
        ),
        serde_json::Value::String(text) => {
            // `int(" 12 ")` is 12: CPython strips ASCII whitespace, accepts a
            // leading sign, and rejects everything else.
            let trimmed = text.trim_matches(|c: char| c.is_ascii_whitespace());
            trimmed.parse::<i64>().map_err(|_| {
                format!(
                    "invalid literal for int() with base 10: {}",
                    stax_core::queries::paths::py_repr(text)
                )
            })
        }
        serde_json::Value::Null => Err(type_error("NoneType")),
        serde_json::Value::Array(_) => Err(type_error("list")),
        serde_json::Value::Object(_) => Err(type_error("dict")),
    }
}

fn type_error(kind: &str) -> String {
    format!("int() argument must be a string, a bytes-like object or a real number, not '{kind}'")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn garbage_reports_cpythons_expecting_value_at_char_zero() {
        let err = loads(b"not json").expect_err("garbage");
        assert!(err.faithful);
        assert_eq!(err.to_string(), "Expecting value: line 1 column 1 (char 0)");
    }

    #[test]
    fn an_empty_body_reports_the_same_shape() {
        let err = loads(b"").expect_err("empty");
        assert!(err.faithful);
        assert_eq!(err.to_string(), "Expecting value: line 1 column 1 (char 0)");
    }

    #[test]
    fn raw_ciphertext_fails_at_offset_zero_too() {
        // The realistic corruption: an age blob handed to `json.loads` because
        // the decryptor was a no-op, or a random byte string.
        let err = loads(&[0x00, 0x9f, 0x12, 0xff]).expect_err("binary");
        assert_eq!(err.msg, "Expecting value");
        assert_eq!(err.pos, 0);
    }

    #[test]
    fn a_partial_literal_reports_the_tokens_first_character() {
        // CPython: `json.loads("not json")` → char 0. serde gives up at column
        // 2 having consumed the `n`, so the port backs up to the token start.
        for (input, expected) in [
            ("not json", "Expecting value: line 1 column 1 (char 0)"),
            ("tru", "Expecting value: line 1 column 1 (char 0)"),
            ("xyz", "Expecting value: line 1 column 1 (char 0)"),
        ] {
            let err = loads(input.as_bytes()).expect_err(input);
            assert!(err.faithful, "{input}: {err}");
            assert_eq!(err.to_string(), expected, "{input}");
        }
    }

    #[test]
    fn a_trailing_comma_is_expecting_value_too() {
        // `json.loads("[1,]")` → `Expecting value: line 1 column 4 (char 3)`.
        let err = loads(b"[1,]").expect_err("trailing comma");
        assert!(err.faithful);
        assert_eq!(err.to_string(), "Expecting value: line 1 column 4 (char 3)");
    }

    #[test]
    fn a_later_line_offsets_by_the_lines_before_it() {
        let err = loads(b"{\n  \"a\": x\n}").expect_err("bad value");
        assert_eq!(err.lineno, 2);
        // Line 1 is `{\n` — two characters — so the offset is 2 + colno - 1.
        assert_eq!(err.pos, 2 + err.colno - 1);
    }

    #[test]
    fn valid_json_still_parses() {
        assert_eq!(
            loads(br#"{"schema":"stackunderflow.sync/1"}"#).expect("ok"),
            serde_json::json!({"schema": "stackunderflow.sync/1"})
        );
    }

    #[test]
    fn py_int_covers_every_json_type_the_way_cpython_does() {
        assert_eq!(py_int(&serde_json::json!(7)), Ok(7));
        assert_eq!(py_int(&serde_json::json!(2.9)), Ok(2));
        assert_eq!(py_int(&serde_json::json!(-2.9)), Ok(-2), "trunc, not floor");
        assert_eq!(py_int(&serde_json::json!(true)), Ok(1));
        assert_eq!(py_int(&serde_json::json!(" 12 ")), Ok(12));
        assert_eq!(
            py_int(&serde_json::json!("abc")),
            Err("invalid literal for int() with base 10: 'abc'".to_owned())
        );
        assert_eq!(
            py_int(&serde_json::Value::Null),
            Err("int() argument must be a string, a bytes-like object or a real number, not 'NoneType'".to_owned())
        );
        assert_eq!(
            py_int(&serde_json::json!([])),
            Err(
                "int() argument must be a string, a bytes-like object or a real number, not 'list'"
                    .to_owned()
            )
        );
        assert_eq!(
            py_int(&serde_json::json!({})),
            Err(
                "int() argument must be a string, a bytes-like object or a real number, not 'dict'"
                    .to_owned()
            )
        );
    }

    #[test]
    fn an_untranslated_family_says_so_rather_than_pretending() {
        // DIV-214 made visible: `{"a"` is `Expecting ':' delimiter` in CPython
        // and this module does not claim it. The marker in the message means a
        // differ row that crossed it would FAIL loudly instead of quietly
        // agreeing on the wrong string.
        let err = loads(br#"{"a""#).expect_err("truncated");
        assert!(!err.faithful, "{err}");
        assert!(err.to_string().contains("unported"), "{err}");
    }
}
