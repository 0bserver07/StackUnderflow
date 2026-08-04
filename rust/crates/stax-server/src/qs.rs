//! Query strings, parsed the way starlette + pydantic parse them.
//!
//! Not `axum::extract::Query`, and not `serde_urlencoded`, for two reasons that
//! both show up in the ported signatures:
//!
//! * **Repeated keys are a list, not an overwrite.**
//!   `provider: Annotated[list[str] | None, Query()]` means
//!   `?provider=cursor&provider=cline` arrives as `["cursor", "cline"]`.
//!   `serde_urlencoded` into a struct keeps one.
//! * **Scalars take the *last* occurrence.** starlette builds
//!   `QueryParams._dict` with a comprehension over the item list, so
//!   `?offset=1&offset=2` resolves to `2`. Not the first, which is the
//!   intuitive guess and the one that would have shipped.
//!
//! Coercion follows pydantic v2's lax mode for the shapes the routes actually
//! declare: `bool`, `int`, `int | None`, `str`, `list[str]`. A value that will
//! not coerce is a `422`, and [`QueryError`] carries enough to build one.

use std::borrow::Cow;

/// A parsed query string: ordered `(key, value)` pairs, decoded.
#[derive(Debug, Clone, Default)]
pub struct Query {
    pairs: Vec<(String, String)>,
}

/// A query parameter that would not coerce — starlette answers `422`.
#[derive(Debug, Clone)]
pub struct QueryError {
    /// The parameter name, for the `loc` of a validation error.
    pub field: String,
    /// The raw value that failed.
    pub input: String,
    /// pydantic's error `type`, e.g. `int_parsing`.
    pub kind: &'static str,
}

impl Query {
    /// Parse a raw query string (no leading `?`).
    ///
    /// `+` decodes to a space and `%XX` to its byte, matching
    /// `urllib.parse.parse_qsl`. Invalid UTF-8 after decoding is replaced
    /// rather than rejected, which is `errors="replace"` — starlette's default.
    #[must_use]
    pub fn parse(raw: &str) -> Self {
        let mut pairs = Vec::new();
        for chunk in raw.split('&') {
            if chunk.is_empty() {
                continue;
            }
            let (key, value) = match chunk.split_once('=') {
                Some((k, v)) => (k, v),
                None => (chunk, ""),
            };
            pairs.push((decode(key).into_owned(), decode(value).into_owned()));
        }
        Self { pairs }
    }

    /// The **last** value for `key`, or `None`.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&str> {
        self.pairs
            .iter()
            .rev()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    /// Every value for `key`, in order — the `list[str]` shape.
    #[must_use]
    pub fn get_all(&self, key: &str) -> Vec<&str> {
        self.pairs
            .iter()
            .filter(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
            .collect()
    }

    /// `bool = default`.
    ///
    /// pydantic v2 lax-mode string→bool: `1/true/t/yes/y/on` and
    /// `0/false/f/no/n/off`, case-insensitive. Anything else is `422`, which is
    /// worth knowing — `?include_stats=maybe` is not silently false.
    ///
    /// # Errors
    /// When the value is present and does not coerce.
    pub fn bool_or(&self, key: &str, default: bool) -> Result<bool, QueryError> {
        let Some(raw) = self.get(key) else {
            return Ok(default);
        };
        match raw.to_ascii_lowercase().as_str() {
            "1" | "true" | "t" | "yes" | "y" | "on" => Ok(true),
            "0" | "false" | "f" | "no" | "n" | "off" => Ok(false),
            _ => Err(QueryError {
                field: key.to_owned(),
                input: raw.to_owned(),
                kind: "bool_parsing",
            }),
        }
    }

    /// `int = default`.
    ///
    /// # Errors
    /// When the value is present and is not an integer.
    pub fn int_or(&self, key: &str, default: i64) -> Result<i64, QueryError> {
        Ok(self.opt_int(key)?.unwrap_or(default))
    }

    /// `int | None = None`, saturated to `i64` — see [`Query::opt_pyint`].
    ///
    /// # Errors
    /// When the value is present and is not an integer.
    pub fn opt_int(&self, key: &str) -> Result<Option<i64>, QueryError> {
        Ok(self.opt_pyint(key)?.map(|value| value.saturated))
    }

    /// `int | None = None`, at CPython's precision — DIV-107, closed.
    ///
    /// # What the old rule got wrong
    ///
    /// `raw.trim().parse::<i64>()` was DIV-107: three whole classes of input
    /// that pydantic accepts came back `422` here. Enumerated against the
    /// reference (`fastapi 0.141.1` / `pydantic 2.13.4`) on
    /// `/api/context-replay/zz?at=…`, reference first, port-before second:
    ///
    /// | `?at=` | reference | port, before |
    /// |---|---|---|
    /// | `3036.0` | `200`, `at_seq: 3036` | `422 int_parsing` |
    /// | `0.0` / `-0.0` / `-0.0000` | `200`, `0` | `422` |
    /// | `+3.0` (i.e. `%2B3.0`) | `200`, `3` | `422` |
    /// | `3.000000000000000000000` | `200`, `3` | `422` |
    /// | `5_0` | `200`, `50` | `422` |
    /// | `1_0.0` | `200`, `10` | `422` |
    /// | `9223372036854775808` | `200`, echoed exactly | `422` |
    /// | `-9223372036854775809` | `200`, echoed exactly | `422` |
    /// | `99999999999999999999999` | `200`, echoed exactly | `422` |
    /// | `99999999999999999999999.0` | `200`, `99999999999999999999999` | `422` |
    ///
    /// and the rejections, which already agreed and are now pinned so the fix
    /// cannot over-shoot: `3036.5`, `1.0000000000000000001`, `0.000000000000000001`,
    /// `5.`, `10.`, `.0`, `1.0.0`, `1.-0`, `1e3`, `1E3`, `1e+3`, `3.0e3`,
    /// `1.0e0`, `1e100`, `1e400`, `0x10`, `0b101`, `0o17`, `1,0`, `_5`, `5_`,
    /// `1__0`, `1.0_0`, `1_0.0_0`, `true`, `True`, `inf`, `nan`, `''`, `5\0`.
    ///
    /// # The grammar, as measured
    ///
    /// 1. **Trim.** Rust's `str::trim` — Unicode `White_Space`. `\u{a0}5` is 5
    ///    on the reference and `\u{200b}5` is a `422`, which is exactly that
    ///    property and not Python's `str.strip`.
    /// 2. **The integer form**: an optional `+`/`-`, then ASCII digits with
    ///    single underscores *between* digits. CPython's `int()` rules, which is
    ///    why `1_0_0` is 100 and `_5`, `5_`, `1__0` are not integers at all.
    /// 3. **Failing that, the integral-decimal form**: split at the **first**
    ///    `.`; the fraction must be non-empty and every character `0`; the whole
    ///    part then goes through rule 2. That single model accounts for every
    ///    row above — `1_0.0` passes because the underscore is in the whole
    ///    part, `1.0_0` fails because it is in the fraction, `10.` fails on the
    ///    empty fraction, `.0` fails because rule 2 rejects an empty whole part,
    ///    and `1.0.0` fails because `0.0` is not all zeros.
    /// 4. **No exponent, no radix prefix, no thousands separator.** All measured.
    ///
    /// Rule 3 is a *digit* test, not a float conversion:
    /// `1.0000000000000000001` is `1.0` in `f64` and a `422` on the reference.
    ///
    /// # Errors
    /// When the value is present and does not match the grammar above.
    pub fn opt_pyint(&self, key: &str) -> Result<Option<PyInt>, QueryError> {
        let Some(raw) = self.get(key) else {
            return Ok(None);
        };
        parse_py_int(raw).map(Some).ok_or_else(|| QueryError {
            field: key.to_owned(),
            input: raw.to_owned(),
            kind: "int_parsing",
        })
    }

    /// `str = default`.
    #[must_use]
    pub fn str_or<'a>(&'a self, key: &str, default: &'a str) -> &'a str {
        self.get(key).unwrap_or(default)
    }

    /// `list[str] | None = None` — `None` when the key never appears.
    ///
    /// FastAPI gives the handler `None`, not `[]`, for an absent repeated
    /// param, and several routes branch on exactly that (`if not provider:`
    /// treats both the same, but `provider is not None` does not).
    #[must_use]
    pub fn opt_list(&self, key: &str) -> Option<Vec<String>> {
        let values = self.get_all(key);
        if values.is_empty() {
            None
        } else {
            Some(values.into_iter().map(str::to_owned).collect())
        }
    }
}

/// A coerced `int` query parameter, at CPython's precision.
///
/// pydantic's `int` is unbounded and Rust's is not, so the two halves are kept
/// apart rather than one of them being quietly lost:
///
/// * [`PyInt::saturated`] is what every arithmetic consumer wants. Every bound
///   in the ported handlers (`limit`, `offset`, `page`, `per_page`, `days`, the
///   `[-720, 840]` timezone clamp) is far inside `i64`, so a saturated value
///   compares against them exactly as the exact one would.
/// * [`PyInt::text`] is `repr(int(value))` — what CPython would print, and
///   therefore what a handler that *echoes* the parameter has to serialise.
///   `/api/context-replay` is the one that does (`at_seq`), and
///   `99999999999999999999999` comes back verbatim there.
///
/// # The residue, stated rather than papered over
///
/// A value past `i64` is *accepted* now — which is the 422-vs-200 half of
/// DIV-107 and the half every consumer feels — but a handler that echoes it
/// still prints the clamp. `/api/context-replay` is the only one that echoes,
/// and closing that last inch means an arbitrary-precision
/// [`serde_json::Value`], i.e. the workspace-wide `arbitrary_precision` feature,
/// which changes how *every* number in the port is parsed and rendered. Wildly
/// out of proportion to one query parameter, so `!CR-at-bignum` keeps its `!`
/// and this paragraph is why. Same class as DIV-453 and the
/// maintainer-accepted `--limit` clamp; `text` is here so the day that ruling
/// changes, the exact digits are already carried and not thrown away at the
/// parse.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PyInt {
    /// The canonical decimal spelling — `repr(int(value))`: sign and digits, no
    /// leading zeros, and no `-0`.
    pub text: String,
    /// The value, saturated at the `i64` bounds.
    pub saturated: i64,
}

/// The measured pydantic grammar — see [`Query::opt_pyint`] for the table.
#[must_use]
pub fn parse_py_int(raw: &str) -> Option<PyInt> {
    let text = raw.trim();
    let digits = match parse_int_literal(text) {
        Some(digits) => digits,
        None => {
            // The integral-decimal form. `split_once` takes the FIRST `.`, so
            // `1.0.0` hands the fraction `0.0` to the all-zeros test and fails.
            let (whole, fraction) = text.split_once('.')?;
            if fraction.is_empty() || !fraction.bytes().all(|byte| byte == b'0') {
                return None;
            }
            parse_int_literal(whole)?
        }
    };
    Some(canonicalise(&digits))
}

/// CPython's `int(str)`: optional sign, ASCII digits, single underscores
/// *between* digits. Returns `(negative, digits-without-underscores)`.
fn parse_int_literal(text: &str) -> Option<(bool, String)> {
    let (negative, body) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text.strip_prefix('+').unwrap_or(text)),
    };
    let mut digits = String::with_capacity(body.len());
    // Seeded `true` so a LEADING underscore is rejected by the same test that
    // rejects a doubled one.
    let mut previous_was_underscore = true;
    for byte in body.bytes() {
        if byte == b'_' {
            if previous_was_underscore {
                return None;
            }
            previous_was_underscore = true;
            continue;
        }
        if !byte.is_ascii_digit() {
            return None;
        }
        previous_was_underscore = false;
        digits.push(byte as char);
    }
    // Empty, or a TRAILING underscore.
    if digits.is_empty() || previous_was_underscore {
        return None;
    }
    Some((negative, digits))
}

/// `repr(int(...))` plus the saturated `i64`.
fn canonicalise((negative, digits): &(bool, String)) -> PyInt {
    let trimmed = digits.trim_start_matches('0');
    let magnitude = if trimmed.is_empty() { "0" } else { trimmed };
    // `-0` is `0` in Python, and `"-0"` would not round-trip as JSON.
    let text = if *negative && magnitude != "0" {
        format!("-{magnitude}")
    } else {
        magnitude.to_owned()
    };
    let clamp = if *negative { i64::MIN } else { i64::MAX };
    let saturated = text.parse::<i64>().unwrap_or(clamp);
    PyInt { text, saturated }
}

/// `urllib.parse.unquote_plus` over percent-encoding.
fn decode(raw: &str) -> Cow<'_, str> {
    if !raw.contains('%') && !raw.contains('+') {
        return Cow::Borrowed(raw);
    }
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                match (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                    (Some(hi), Some(lo)) => {
                        out.push(hi << 4 | lo);
                        i += 3;
                    }
                    // A malformed escape is left literal, as `unquote` does.
                    _ => {
                        out.push(bytes[i]);
                        i += 1;
                    }
                }
            }
            byte => {
                out.push(byte);
                i += 1;
            }
        }
    }
    Cow::Owned(String::from_utf8_lossy(&out).into_owned())
}

fn hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repeated_keys_survive_as_a_list() {
        let q = Query::parse("provider=cursor&provider=cline");
        assert_eq!(
            q.opt_list("provider"),
            Some(vec!["cursor".into(), "cline".into()])
        );
    }

    #[test]
    fn a_scalar_takes_the_last_occurrence() {
        // starlette's `QueryParams._dict` is a comprehension over the pair
        // list, so the last write wins. Counter-intuitive and load-bearing.
        let q = Query::parse("offset=1&offset=2");
        assert_eq!(q.int_or("offset", 0).expect("int"), 2);
    }

    #[test]
    fn absent_repeated_param_is_none_not_empty() {
        let q = Query::parse("");
        assert!(q.opt_list("provider").is_none());
    }

    #[test]
    fn bools_take_pydantics_vocabulary() {
        for raw in ["1", "true", "TRUE", "t", "yes", "y", "on"] {
            let q = Query::parse(&format!("include_stats={raw}"));
            assert!(q.bool_or("include_stats", false).expect("bool"), "{raw}");
        }
        for raw in ["0", "false", "f", "no", "n", "off"] {
            let q = Query::parse(&format!("include_stats={raw}"));
            assert!(!q.bool_or("include_stats", true).expect("bool"), "{raw}");
        }
        let q = Query::parse("include_stats=maybe");
        assert_eq!(
            q.bool_or("include_stats", false).unwrap_err().kind,
            "bool_parsing"
        );
    }

    #[test]
    fn missing_values_take_the_declared_default() {
        let q = Query::parse("sort_by=name");
        assert_eq!(q.str_or("sort_by", "last_modified"), "name");
        assert_eq!(
            Query::parse("").str_or("sort_by", "last_modified"),
            "last_modified"
        );
        assert_eq!(Query::parse("").opt_int("limit").expect("none"), None);
    }

    #[test]
    fn percent_and_plus_decode_like_parse_qsl() {
        let q = Query::parse("q=a+b%20c&slug=%2Dhome%2Du");
        assert_eq!(q.get("q"), Some("a b c"));
        assert_eq!(q.get("slug"), Some("-home-u"));
    }

    #[test]
    fn a_bare_key_is_the_empty_string() {
        let q = Query::parse("details&limit=5");
        assert_eq!(q.get("details"), Some(""));
        assert_eq!(q.int_or("limit", 0).expect("int"), 5);
    }

    // ── DIV-107: the pydantic int grammar, transcribed from the probe ───────
    //
    // Every literal below was sent to the running reference as
    // `/api/context-replay/zz?at=<value>` and the status and echoed `at_seq`
    // recorded. `opt_pyint`'s doc comment carries the whole table; these are the
    // same values as assertions, so the table cannot rot silently.

    fn at(raw: &str) -> Option<PyInt> {
        parse_py_int(raw)
    }

    fn accepted(raw: &str) -> String {
        at(raw)
            .unwrap_or_else(|| panic!("the reference accepts {raw:?}"))
            .text
    }

    #[test]
    fn the_integer_form_is_cpythons_int() {
        assert_eq!(accepted("0"), "0");
        assert_eq!(accepted("5"), "5");
        assert_eq!(accepted("-5"), "-5");
        assert_eq!(accepted("+5"), "5");
        assert_eq!(accepted("03"), "3");
        assert_eq!(accepted("0000000000000000000000000000001"), "1");
        assert_eq!(accepted("-0"), "0", "Python has no negative zero int");
        assert_eq!(accepted("5_0"), "50");
        assert_eq!(accepted("1_0_0"), "100");
        // Whitespace is stripped first — including the tab and newline the
        // probe reached through `%09` / `%0A`.
        assert_eq!(accepted("  5  "), "5");
        assert_eq!(accepted("\t5"), "5");
        assert_eq!(accepted("5\n"), "5");
        // Rust's `trim` is Unicode `White_Space`: NBSP is, ZWSP is not, and the
        // reference agrees with both.
        assert_eq!(accepted("\u{a0}5"), "5");
        assert!(at("\u{200b}5").is_none());
    }

    #[test]
    fn the_integral_decimal_form_is_accepted_and_truncated() {
        // DIV-107's headline: `?at=3036.0` was a 200 there and a 422 here.
        assert_eq!(accepted("3036.0"), "3036");
        assert_eq!(accepted("0.0"), "0");
        assert_eq!(accepted("-0.0"), "0");
        assert_eq!(accepted("-0.0000"), "0");
        assert_eq!(accepted("+3.0"), "3");
        assert_eq!(accepted("-3.0"), "-3");
        assert_eq!(accepted("000.0"), "0");
        assert_eq!(accepted("3.000000000000000000000"), "3");
        assert_eq!(accepted("1.0000000000000000000000000000"), "1");
        // The underscore is allowed in the WHOLE part and not in the fraction.
        assert_eq!(accepted("1_0.0"), "10");
        assert!(at("1.0_0").is_none());
        assert!(at("1_0.0_0").is_none());
    }

    #[test]
    fn the_fraction_test_is_on_digits_not_on_a_float() {
        // `1.0000000000000000001` IS `1.0` as an `f64`, and the reference
        // answers 422. A `parse::<f64>()`-then-`fract()` implementation would
        // have been green on every other row in this file and wrong here.
        assert!(at("1.0000000000000000001").is_none());
        assert!(at("1.000000000000000000000000000000001").is_none());
        assert!(at("0.000000000000000001").is_none());
        assert!(at("3036.5").is_none());
    }

    #[test]
    fn everything_the_reference_refuses_is_still_refused() {
        for raw in [
            "", " ", "5.", "10.", ".0", "1.", "1.0.0", "1.-0", "1.+0", "1e3", "1E3", "1e+3",
            "3.0e3", "1.0e0", "1e100", "1e400", "1.5e3", "0x10", "0b101", "0o17", "1,0", "_5",
            "5_", "1__0", "+_5", "-_5", "--5", "true", "True", "inf", "nan", "abc", "5\u{0}",
        ] {
            assert!(at(raw).is_none(), "{raw:?} must be int_parsing");
        }
    }

    #[test]
    fn arbitrary_precision_survives_as_text_and_saturates_as_a_number() {
        // The second half of DIV-107. pydantic's int is unbounded; `i64` is not.
        let value = at("9223372036854775808").expect("accepted");
        assert_eq!(value.text, "9223372036854775808");
        assert_eq!(value.saturated, i64::MAX);

        let value = at("-9223372036854775809").expect("accepted");
        assert_eq!(value.text, "-9223372036854775809");
        assert_eq!(value.saturated, i64::MIN);

        let value = at("99999999999999999999999.0").expect("accepted");
        assert_eq!(value.text, "99999999999999999999999");
        assert_eq!(value.saturated, i64::MAX);

        // The boundary itself is exact, not a clamp that happens to agree.
        let value = at("9223372036854775807").expect("accepted");
        assert_eq!(value.text, "9223372036854775807");
        assert_eq!(value.saturated, i64::MAX);
    }

    #[test]
    fn the_empty_and_zero_class_is_pinned() {
        // Thrice-proven elsewhere (`--project ''`, `backup verify --name ''`,
        // `--history-source ''`): the empty string and `"0"` are DIFFERENT
        // inputs and only one of them is falsy. Measured on the reference:
        // `?at=` is a 422 and `?at=0` is a 200 with `at_seq: 0`.
        let q = Query::parse("at=");
        assert_eq!(q.opt_int("at").unwrap_err().kind, "int_parsing");
        let q = Query::parse("at=0");
        assert_eq!(q.opt_int("at").expect("zero"), Some(0));
        // …and ABSENT is a third thing again: `None`, not `0`, not an error.
        let q = Query::parse("");
        assert_eq!(q.opt_int("at").expect("absent"), None);
        // The same three through the defaulted spelling, where absent takes the
        // default and `"0"` overrides it with a falsy value.
        assert_eq!(Query::parse("").int_or("days", 30).expect("default"), 30);
        assert_eq!(Query::parse("days=0").int_or("days", 30).expect("zero"), 0);
        assert_eq!(
            Query::parse("days=").int_or("days", 30).unwrap_err().kind,
            "int_parsing"
        );
    }

    #[test]
    fn the_error_input_is_the_untrimmed_raw_value() {
        // FastAPI echoes what arrived, not what the parser looked at.
        let q = Query::parse("at=5%00");
        let err = q.opt_int("at").unwrap_err();
        assert_eq!(err.input, "5\u{0}");
        assert_eq!(err.field, "at");
    }
}
