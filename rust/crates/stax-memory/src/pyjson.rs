//! CPython-compatible JSON serialization — the byte-parity substrate.
//!
//! The wire contracts (`stackunderflow.memory/1`, `stackunderflow.resume/1`) are
//! *byte* contracts: `cli_helpers/agent_output.py:render` promises "the same
//! envelope dict always renders to a byte-identical string", the golden fixtures
//! under `contracts/stackunderflow-memory-v1/fixtures/` are literal CLI stdout,
//! and an agent diffing two envelopes across implementations must see nothing.
//! `serde_json`'s own writer is *not* byte-compatible with `json.dumps` in three
//! independent ways, each of which shows up in the shipped goldens:
//!
//! 1. **Non-ASCII escaping.** `json.dumps` defaults to `ensure_ascii=True`, so
//!    every codepoint outside `0x20..=0x7E` becomes `\uXXXX` (lowercase hex,
//!    surrogate pairs above the BMP). `serde_json` emits UTF-8 verbatim. Real
//!    divergence today: `decisions.success.json` carries `…` (the ellipsis
//!    the snippet truncator inserts) and `worked.success.json` carries it too.
//! 2. **Float repr.** Python's `repr` switches to exponent form at
//!    `decpt <= -4 || decpt > 16` and writes the exponent with a sign and at
//!    least two digits (`1e+16`, `1e-05`); Rust's shortest-round-trip writer
//!    says `1e16` / `1e-5`. The *digits* agree — both emit the shortest string
//!    that round-trips — only the presentation differs.
//! 3. **DEL.** `0x7F` is outside CPython's `S_CHAR` range and is escaped;
//!    `serde_json` passes it through raw.
//! 4. **Float *parsing*.** Not a writer problem at all, and the one this
//!    campaign did not predict: `serde_json`'s default decimal parser lands one
//!    ULP off CPython's on long decimals, so `499.25254474999997` in
//!    `file.success.json` came back as a different `f64` that then printed
//!    correctly as `499.25254475`. Fixed by the `float_roundtrip` feature in
//!    this crate's manifest, guarded by a test here.
//!
//! Everything else already lines up: the two-space pretty layout matches
//! `indent=2` exactly (`{}` / `[]` stay collapsed when empty, `": "` between key
//! and value, `,\n` between items), and control characters below `0x20` use the
//! same five shortcuts plus lowercase `\u00xx`.
//!
//! This module is pure: `Value` in, `String` out, no I/O and no globals.

use std::fmt::Write as _;

use serde::Serialize;
use serde_json::Value;
use serde_json::ser::Formatter;

/// Serialize like `json.dumps(obj, indent=2, default=str)` — the `render()` of
/// `cli_helpers/agent_output.py` and of `cli.py`'s `resume --json`.
///
/// No trailing newline: `click.echo` adds that, which is why every golden file
/// ends `}\n` while this function stops at `}`.
///
/// # Panics
///
/// Never for a `T` whose `Serialize` cannot fail (every type in this crate, and
/// `serde_json::Value`). A map with non-string keys would panic; the envelopes
/// have none.
#[must_use]
pub fn dumps_pretty<T: Serialize + ?Sized>(value: &T) -> String {
    dump(value, Layout::Indent2)
}

/// Serialize like `json.dumps(obj, separators=(",", ":"), default=str)` — the
/// compact form `agent_output.estimate_tokens` measures.
///
/// # Panics
///
/// See [`dumps_pretty`].
#[must_use]
pub fn dumps_compact<T: Serialize + ?Sized>(value: &T) -> String {
    dump(value, Layout::Compact)
}

/// The `chars/4 + 1` token estimate of `agent_output.estimate_tokens`.
///
/// Python measures `len()` of the compact `json.dumps` string in *characters*;
/// with `ensure_ascii=True` that string is pure ASCII, so characters and bytes
/// are the same count and `String::len` is the identical measure.
#[must_use]
pub fn estimate_tokens<T: Serialize + ?Sized>(value: &T) -> u64 {
    (dumps_compact(value).len() as u64) / 4 + 1
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Layout {
    Compact,
    Indent2,
}

fn dump<T: Serialize + ?Sized>(value: &T, layout: Layout) -> String {
    let mut out = Vec::new();
    let mut ser = serde_json::Serializer::with_formatter(&mut out, PythonFormatter::new(layout));
    value
        .serialize(&mut ser)
        .expect("serializing a JSON-native value to a Vec cannot fail");
    String::from_utf8(out).expect("the formatter only ever writes ASCII-safe UTF-8")
}

/// A [`Formatter`] that writes what CPython's `json` module writes.
struct PythonFormatter {
    layout: Layout,
    depth: usize,
    has_value: bool,
}

impl PythonFormatter {
    fn new(layout: Layout) -> Self {
        Self {
            layout,
            depth: 0,
            has_value: false,
        }
    }

    fn newline_indent<W>(&self, writer: &mut W) -> std::io::Result<()>
    where
        W: ?Sized + std::io::Write,
    {
        if self.layout == Layout::Compact {
            return Ok(());
        }
        writer.write_all(b"\n")?;
        for _ in 0..self.depth {
            writer.write_all(b"  ")?;
        }
        Ok(())
    }
}

impl Formatter for PythonFormatter {
    // ── strings: ensure_ascii=True ──────────────────────────────────────────
    //
    // `serde_json` routes every byte that needs escaping through
    // `write_char_escape` (which already matches CPython: `\b \t \n \f \r \" \\`
    // plus lowercase `\u00xx` for the rest of C0) and hands the untouched runs
    // to `write_string_fragment`. Its escape table covers `0x00..=0x1F`, `"` and
    // `\` only — so DEL and everything non-ASCII arrive here, and here is where
    // `ensure_ascii` happens.
    fn write_string_fragment<W>(&mut self, writer: &mut W, fragment: &str) -> std::io::Result<()>
    where
        W: ?Sized + std::io::Write,
    {
        if fragment.is_ascii() && !fragment.as_bytes().contains(&0x7F) {
            return writer.write_all(fragment.as_bytes());
        }
        let mut buf = String::with_capacity(fragment.len());
        for ch in fragment.chars() {
            if ch.is_ascii() && ch != '\u{7f}' {
                buf.push(ch);
            } else {
                for unit in escape_units(ch) {
                    let _ = write!(buf, "\\u{unit:04x}");
                }
            }
        }
        writer.write_all(buf.as_bytes())
    }

    // ── numbers: Python's repr ──────────────────────────────────────────────
    fn write_f64<W>(&mut self, writer: &mut W, value: f64) -> std::io::Result<()>
    where
        W: ?Sized + std::io::Write,
    {
        writer.write_all(python_float_repr(value).as_bytes())
    }

    fn write_f32<W>(&mut self, writer: &mut W, value: f32) -> std::io::Result<()>
    where
        W: ?Sized + std::io::Write,
    {
        // Python has no f32; a widened f32 renders through the same repr so the
        // two implementations cannot disagree by accident.
        self.write_f64(writer, f64::from(value))
    }

    // ── layout: json.dumps(indent=2) / separators=(",", ":") ────────────────
    fn begin_array<W>(&mut self, writer: &mut W) -> std::io::Result<()>
    where
        W: ?Sized + std::io::Write,
    {
        self.depth += 1;
        self.has_value = false;
        writer.write_all(b"[")
    }

    fn end_array<W>(&mut self, writer: &mut W) -> std::io::Result<()>
    where
        W: ?Sized + std::io::Write,
    {
        self.depth -= 1;
        if self.has_value {
            self.newline_indent(writer)?;
        }
        self.has_value = true;
        writer.write_all(b"]")
    }

    fn begin_array_value<W>(&mut self, writer: &mut W, first: bool) -> std::io::Result<()>
    where
        W: ?Sized + std::io::Write,
    {
        if !first {
            writer.write_all(b",")?;
        }
        self.newline_indent(writer)
    }

    fn end_array_value<W>(&mut self, _writer: &mut W) -> std::io::Result<()>
    where
        W: ?Sized + std::io::Write,
    {
        self.has_value = true;
        Ok(())
    }

    fn begin_object<W>(&mut self, writer: &mut W) -> std::io::Result<()>
    where
        W: ?Sized + std::io::Write,
    {
        self.depth += 1;
        self.has_value = false;
        writer.write_all(b"{")
    }

    fn end_object<W>(&mut self, writer: &mut W) -> std::io::Result<()>
    where
        W: ?Sized + std::io::Write,
    {
        self.depth -= 1;
        if self.has_value {
            self.newline_indent(writer)?;
        }
        self.has_value = true;
        writer.write_all(b"}")
    }

    fn begin_object_key<W>(&mut self, writer: &mut W, first: bool) -> std::io::Result<()>
    where
        W: ?Sized + std::io::Write,
    {
        if !first {
            writer.write_all(b",")?;
        }
        self.newline_indent(writer)
    }

    fn begin_object_value<W>(&mut self, writer: &mut W) -> std::io::Result<()>
    where
        W: ?Sized + std::io::Write,
    {
        writer.write_all(match self.layout {
            Layout::Compact => b":",
            Layout::Indent2 => b": ",
        })
    }

    fn end_object_value<W>(&mut self, _writer: &mut W) -> std::io::Result<()>
    where
        W: ?Sized + std::io::Write,
    {
        self.has_value = true;
        Ok(())
    }
}

/// The UTF-16 code units `ensure_ascii` escapes a character into: one for the
/// BMP, a surrogate pair above it (CPython emits `🚀` for a rocket).
fn escape_units(ch: char) -> Vec<u32> {
    let cp = ch as u32;
    if cp < 0x1_0000 {
        vec![cp]
    } else {
        let v = cp - 0x1_0000;
        vec![0xD800 + (v >> 10), 0xDC00 + (v & 0x3FF)]
    }
}

/// CPython's `repr(float)` — the exact text `json.dumps` writes for a float.
///
/// Both runtimes compute the *shortest* digit string that round-trips, so the
/// digits are shared; only the presentation rules are CPython's, taken from
/// `Python/pystrtod.c:format_float_short` with `type='r'`:
///
/// * exponent form when `decpt <= -4 || decpt > 16` — "convert to exponential
///   format at 1e16", per the comment in that switch;
/// * the exponent always carries a sign and at least two digits (`1e+16`,
///   `1e-05`);
/// * `Py_DTSF_ADD_DOT_0` appends `.0` to a positional result with no fraction,
///   which is why `0.0` is not `0` — and why the exponent form has no `.0`.
///
/// Non-finite values follow `json.dumps`'s default `allow_nan=True`
/// (`Infinity` / `-Infinity` / `NaN`). They are unreachable through
/// `serde_json::Value` (`Number::from_f64` rejects them) but a hand-built
/// `f64` field would reach here, and silently writing `null` — `serde_json`'s
/// choice — would be a shape divergence nobody could see in a diff.
///
/// # Panics
///
/// Never: the two `expect`s assert `f64`'s `LowerExp` contract (a mantissa, the
/// letter `e`, then a decimal exponent), which `std` guarantees for every finite
/// value and which the non-finite branches above have already excluded.
#[must_use]
pub fn python_float_repr(value: f64) -> String {
    if value.is_nan() {
        return "NaN".to_owned();
    }
    if value.is_infinite() {
        return if value.is_sign_positive() {
            "Infinity"
        } else {
            "-Infinity"
        }
        .to_owned();
    }

    // `{:e}` is Rust's shortest-round-trip writer in scientific form:
    // "6.007909187500001e2", "8e-1", "-0e0". Digits + decimal exponent are all
    // the presentation rules need.
    let sci = format!("{value:e}");
    let (sign, sci) = match sci.strip_prefix('-') {
        Some(rest) => ("-", rest),
        None => ("", sci.as_str()),
    };
    let (mantissa, exp) = sci
        .split_once('e')
        .expect("Rust's LowerExp for f64 always writes an exponent");
    let exp: i32 = exp.parse().expect("LowerExp writes a decimal exponent");
    let digits: String = mantissa.chars().filter(|c| *c != '.').collect();
    // `decpt` is CPython's: the position of the decimal point relative to the
    // start of `digits`, i.e. one more than the scientific exponent.
    let decpt = exp + 1;

    let mut out = String::with_capacity(digits.len() + 8);
    out.push_str(sign);
    if decpt <= -4 || decpt > 16 {
        out.push_str(&digits[..1]);
        if digits.len() > 1 {
            out.push('.');
            out.push_str(&digits[1..]);
        }
        let e = decpt - 1;
        let _ = write!(out, "e{}{:02}", if e < 0 { '-' } else { '+' }, e.abs());
        return out;
    }
    if decpt <= 0 {
        // "0." then -decpt padding zeros, then every digit: 0.8, 0.0001.
        out.push_str("0.");
        for _ in 0..-decpt {
            out.push('0');
        }
        out.push_str(&digits);
        return out;
    }
    // Both guards passed, so `decpt` is in 1..=16 and the widening is lossless.
    let split = usize::try_from(decpt).expect("decpt is in 1..=16 here");
    if split >= digits.len() {
        out.push_str(&digits);
        for _ in 0..(split - digits.len()) {
            out.push('0');
        }
        out.push_str(".0");
    } else {
        out.push_str(&digits[..split]);
        out.push('.');
        out.push_str(&digits[split..]);
    }
    out
}

/// Parse JSON the way `json.loads` does for our purposes, keeping key order.
///
/// A thin alias over `serde_json::from_str::<Value>` that exists to make the
/// `preserve_order` requirement legible at the call site: without that feature
/// this function silently sorts every object and byte-parity is gone.
///
/// # Errors
///
/// Propagates `serde_json`'s parse error.
pub fn loads(text: &str) -> Result<Value, serde_json::Error> {
    serde_json::from_str(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Expectations produced by `../StackUnderflow/.venv/bin/python -c
    /// 'print(repr(x))'` — the reference implementation, not a guess.
    #[test]
    fn float_repr_matches_cpython() {
        let cases: &[(f64, &str)] = &[
            (0.0, "0.0"),
            (-0.0, "-0.0"),
            (1.0, "1.0"),
            (-1.5, "-1.5"),
            (0.8, "0.8"),
            (0.1, "0.1"),
            (2.632_821_25, "2.63282125"),
            (396.124_464_75, "396.12446475"),
            (600.790_918_750_000_1, "600.7909187500001"),
            (499.252_544_749_999_97, "499.25254474999997"),
            (224.549_254_5, "224.5492545"),
            (0.0001, "0.0001"),
            (1e-5, "1e-05"),
            (1e-7, "1e-07"),
            (1e15, "1000000000000000.0"),
            (1e16, "1e+16"),
            (1e100, "1e+100"),
            (-1e-5, "-1e-05"),
            (1.5e300, "1.5e+300"),
            (5e-324, "5e-324"),
            (f64::MAX, "1.7976931348623157e+308"),
            (123_456_789_012_345_680.0, "1.2345678901234568e+17"),
            (1_234_567_890_123_456.0, "1234567890123456.0"),
        ];
        for (value, want) in cases {
            assert_eq!(&python_float_repr(*value), want, "repr({value})");
        }
    }

    #[test]
    fn non_finite_follows_allow_nan() {
        assert_eq!(python_float_repr(f64::NAN), "NaN");
        assert_eq!(python_float_repr(f64::INFINITY), "Infinity");
        assert_eq!(python_float_repr(f64::NEG_INFINITY), "-Infinity");
    }

    /// Every expectation below is `json.dumps(s, separators=(",", ":"))`
    /// run on `../StackUnderflow/.venv/bin/python`, not a reading of the docs.
    #[test]
    fn ensure_ascii_escapes_everything_above_tilde() {
        // The ellipsis the snippet truncator inserts — present in two shipped
        // goldens, and the single most likely byte-parity break.
        assert_eq!(dumps_compact(&json!("a\u{2026}b")), "\"a\\u2026b\"");
        // DEL is outside CPython's S_CHAR range; serde_json passes it raw.
        assert_eq!(dumps_compact(&json!("\u{7f}")), "\"\\u007f\"");
        // Above the BMP: a surrogate pair, lowercase hex.
        assert_eq!(dumps_compact(&json!("\u{1f680}")), "\"\\ud83d\\ude80\"");
        // C0 keeps the five shortcuts; everything else is lowercase u00xx.
        assert_eq!(dumps_compact(&json!("\n\t\u{1}")), "\"\\n\\t\\u0001\"");
        // Plain ASCII is untouched; quotes and backslashes escape, `/` does not.
        assert_eq!(dumps_compact(&json!("a\"b\\c/d")), "\"a\\\"b\\\\c/d\"");
    }
    #[test]
    fn pretty_layout_matches_indent_two() {
        let value = json!({"a": [1, 2], "b": {}, "c": [], "d": {"e": null}});
        assert_eq!(
            dumps_pretty(&value),
            "{\n  \"a\": [\n    1,\n    2\n  ],\n  \"b\": {},\n  \"c\": [],\n  \
             \"d\": {\n    \"e\": null\n  }\n}"
        );
    }

    #[test]
    fn compact_layout_has_no_spaces() {
        assert_eq!(
            dumps_compact(&json!({"a": [1, {"b": 2}], "c": true})),
            r#"{"a":[1,{"b":2}],"c":true}"#
        );
    }

    #[test]
    fn preserve_order_is_on() {
        // Without the feature serde_json's Map is a BTreeMap and this parse
        // would come back alphabetised — the byte-parity goal dies here.
        let parsed = loads(r#"{"zeta":1,"alpha":2,"mid":3}"#).expect("valid JSON");
        let keys: Vec<&str> = parsed
            .as_object()
            .expect("object")
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(keys, ["zeta", "alpha", "mid"]);
        assert_eq!(dumps_compact(&parsed), r#"{"zeta":1,"alpha":2,"mid":3}"#);
    }

    #[test]
    fn estimate_tokens_is_chars_over_four_plus_one() {
        // `agent_output.estimate_tokens([]) == 1` — "[]" is 2 chars.
        assert_eq!(estimate_tokens(&json!([])), 1);
        // Escaped characters count as their ESCAPED length, because Python
        // measures the ensure_ascii output.
        let escaped = dumps_compact(&json!(["\u{2026}"]));
        assert_eq!(escaped, "[\"\\u2026\"]");
        assert_eq!(escaped.len(), 10);
        assert_eq!(estimate_tokens(&json!(["\u{2026}"])), 3);
    }

    /// The `float_roundtrip` guard. serde_json's default float parser is a fast
    /// path that lands one ULP off CPython's on long decimals; three goldens
    /// caught it. These three literals are real `cost_usd` values from the
    /// shipped pack, and their neighbours one ULP away — if the feature is ever
    /// dropped from Cargo.toml this fails here with a readable reason instead of
    /// as a wall of byte diffs in the fixture runner.
    #[test]
    fn long_decimals_parse_to_cpythons_exact_bits() {
        for text in [
            "499.25254474999997",
            "3.9881487499999997",
            "600.7909187500001",
            "396.12446475",
            "224.5492545",
        ] {
            let parsed = loads(text).expect("valid JSON");
            assert_eq!(
                dumps_compact(&parsed),
                text,
                "serde_json's `float_roundtrip` feature is off"
            );
        }
        // The neighbours are genuinely different values, not different spellings:
        // 0x407f340a6c5d206c vs …206d, one ULP apart, each correctly shortest-
        // printed by its own runtime. Compared on the bits, because that is the
        // claim — CPython and this port must land on the same 64.
        let a: f64 = "499.25254474999997".parse().expect("f64");
        let b: f64 = "499.25254475".parse().expect("f64");
        assert_eq!(a.to_bits(), 0x407f_340a_6c5d_206c);
        assert_eq!(b.to_bits(), 0x407f_340a_6c5d_206d);
        assert_eq!(python_float_repr(a), "499.25254474999997");
        assert_eq!(python_float_repr(b), "499.25254475");
    }

    #[test]
    fn integers_and_floats_keep_their_json_identity() {
        // Python distinguishes 0 from 0.0 and so must we: `cost_usd: 0.0`
        // appears in two shipped goldens.
        assert_eq!(
            dumps_compact(&loads("[0,0.0,-0.0]").expect("valid")),
            "[0,0.0,-0.0]"
        );
    }
}
