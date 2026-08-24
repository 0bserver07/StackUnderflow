//! CPython-compatible JSON serialization — the byte-parity substrate.
//!
//! The wire contracts (`staxtrace.memory/1`, `staxtrace.resume/1`) are
//! *byte* contracts: `cli_helpers/agent_output.py:render` promises "the same
//! envelope dict always renders to a byte-identical string", the golden fixtures
//! under `contracts/staxtrace-memory-v1/fixtures/` are literal CLI stdout,
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

/// Serialize like starlette's `JSONResponse.render` — the **HTTP** body writer.
///
/// This is *not* [`dumps_compact`]. The two differ in exactly one flag and it is
/// load-bearing for wave 5:
///
/// ```text
/// CLI  (cli_helpers/agent_output.py)  json.dumps(obj, indent=2)                       → ensure_ascii=True
/// HTTP (starlette JSONResponse)       json.dumps(obj, ensure_ascii=False,
///                                                allow_nan=False, indent=None,
///                                                separators=(",", ":"))               → ensure_ascii=False
/// ```
///
/// So a response body carrying `…` ships the three raw UTF-8 bytes `E2 80 A6`,
/// where the same value on stdout ships the seven ASCII bytes `…`. Using the
/// CLI writer for HTTP would diverge on the first non-ASCII project name — and
/// project names on the maintainer's store are full of them.
///
/// `allow_nan=False` makes CPython *raise* on a non-finite float rather than
/// write `NaN`. Unreachable through [`serde_json::Value`] (`Number::from_f64`
/// rejects non-finite), so this writer keeps [`python_float_repr`]'s
/// `allow_nan=True` spelling for a hand-built `f64` instead of panicking: a
/// visible `NaN` in a diff beats a 500 nobody can attribute.
///
/// No trailing newline — starlette writes the body exactly as rendered.
///
/// # Panics
///
/// See [`dumps_pretty`].
#[must_use]
pub fn dumps_http<T: Serialize + ?Sized>(value: &T) -> String {
    dump_styled(value, Layout::Compact, EnsureAscii::No)
}

/// Serialize like `json.dumps(obj)` with **every default** — the *third* live
/// response writer.
///
/// `routes/webhooks.py` returns a bare `Response(content=json.dumps(result),
/// media_type="application/json")` on all three of its endpoints, and
/// `routes/live.py::_format_sse` builds every SSE frame the same way. A bare
/// `json.dumps` is `ensure_ascii=True` (the CLI's flag) with the `(", ", ": ")`
/// separators (neither writer's), so it is a layout of its own:
///
/// ```text
/// JSONResponse  (dumps_http)        {"status":"pong"}
/// agent_output  (dumps_pretty)      {\n  "status": "pong"\n}
/// bare dumps    (dumps_py_default)  {"status": "pong"}
/// ```
///
/// Empty containers collapse exactly as CPython's do (`{}`, `[]`): the
/// separator is only written *between* items.
///
/// # Panics
///
/// See [`dumps_pretty`].
#[must_use]
pub fn dumps_py_default<T: Serialize + ?Sized>(value: &T) -> String {
    dump(value, Layout::PyDefault)
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

/// The three separator/indent combinations CPython's `json.dumps` is called
/// with anywhere in the reference tree.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Layout {
    /// `separators=(",", ":")`, `indent=None`.
    Compact,
    /// `indent=2` — which also implies the `(",", ": ")` separators.
    Indent2,
    /// No arguments at all: `(", ", ": ")`, `indent=None`.
    PyDefault,
}

impl Layout {
    /// What goes *between* two items — CPython's `item_separator`.
    const fn item_separator(self) -> &'static [u8] {
        match self {
            // `indent` is not None, so CPython strips the trailing space from
            // the default item separator and the newline supplies the gap.
            Self::Compact | Self::Indent2 => b",",
            Self::PyDefault => b", ",
        }
    }

    /// What goes between a key and its value — CPython's `key_separator`.
    const fn key_separator(self) -> &'static [u8] {
        match self {
            Self::Compact => b":",
            Self::Indent2 | Self::PyDefault => b": ",
        }
    }
}

/// CPython's `ensure_ascii` flag, as a type rather than a bare `bool`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum EnsureAscii {
    /// `json.dumps` default: every codepoint outside `0x20..=0x7E` is `\uXXXX`.
    Yes,
    /// starlette's `JSONResponse`: raw UTF-8 through, DEL included.
    No,
}

fn dump<T: Serialize + ?Sized>(value: &T, layout: Layout) -> String {
    dump_styled(value, layout, EnsureAscii::Yes)
}

fn dump_styled<T: Serialize + ?Sized>(
    value: &T,
    layout: Layout,
    ensure_ascii: EnsureAscii,
) -> String {
    let mut out = Vec::new();
    let mut ser = serde_json::Serializer::with_formatter(
        &mut out,
        PythonFormatter::new(layout, ensure_ascii),
    );
    value
        .serialize(&mut ser)
        .expect("serializing a JSON-native value to a Vec cannot fail");
    String::from_utf8(out).expect("the formatter only ever writes valid UTF-8")
}

/// A [`Formatter`] that writes what CPython's `json` module writes.
struct PythonFormatter {
    layout: Layout,
    ensure_ascii: EnsureAscii,
    depth: usize,
    has_value: bool,
}

impl PythonFormatter {
    fn new(layout: Layout, ensure_ascii: EnsureAscii) -> Self {
        Self {
            layout,
            ensure_ascii,
            depth: 0,
            has_value: false,
        }
    }

    fn newline_indent<W>(&self, writer: &mut W) -> std::io::Result<()>
    where
        W: ?Sized + std::io::Write,
    {
        if self.layout != Layout::Indent2 {
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
        // `ensure_ascii=False` (starlette's HTTP body writer): CPython's
        // `py_encode_basestring` escapes only `"`, `\` and C0 — all of which
        // serde_json has already routed through `write_char_escape` — so every
        // fragment that reaches here goes out verbatim, DEL included.
        if self.ensure_ascii == EnsureAscii::No {
            return writer.write_all(fragment.as_bytes());
        }
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
            writer.write_all(self.layout.item_separator())?;
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
            writer.write_all(self.layout.item_separator())?;
        }
        self.newline_indent(writer)
    }

    fn begin_object_value<W>(&mut self, writer: &mut W) -> std::io::Result<()>
    where
        W: ?Sized + std::io::Write,
    {
        writer.write_all(self.layout.key_separator())
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
/// digits are shared except for the one case [`tie_break_to_even`] repairs
/// (DIV-008); the presentation rules are CPython's, taken from
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
    // DIV-008: the one place the two digit generators disagree.
    let digits = tie_break_to_even(&digits, decpt, value.abs()).unwrap_or(digits);

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

// ── DIV-008: the halfway decimal tie ────────────────────────────────────────
//
// Rust's shortest-round-trip writer and CPython's `_Py_dg_dtoa` agree on the
// *length* of the shortest digit string, and on the digits themselves whenever
// one candidate of that length is strictly closer to the double than its
// neighbour. They part company on the measure-zero case where the double's
// exact value sits *exactly* halfway between two candidates that both parse
// back to it: CPython selects the even final digit (`Python/dtoa.c`, mode 0 —
// the arm guarded by `word1(&u) & 1`), Rust rounds away from zero.
//
// `-1352070300110077.2` is the reference repro: that double is exactly
// …077.25, so …077.2 and …077.3 are equidistant and both parse to it. CPython
// prints the first, `{:e}` the second. Measured incidence before the fix:
// 65,839 of 5,158,262 adversarial doubles, and 0 of the 144,187 distinct
// `REAL`s in the live store — invisible on today's data, but `cost_usd` in the
// envelope is a SUM, not a stored value.
//
// The repair is one digit, so it is expressed as an exact integer question
// rather than a second digit generator: is the midpoint between the two
// candidates *equal* to the double? Both sides factor as (odd) × 2^a × 5^b,
// and `m < 2^53` bounds `b` far inside `u128`. No bignum, no new dependency.
//
// THIS BLOCK IS DUPLICATED, DELIBERATELY, IN:
//   * crates/stax-memory/src/pyjson.rs      (python_float_repr)
//   * crates/stax-core/src/queries.rs       (pyjson::repr_float)
// The two writers serve the same wire contract and the crates share no
// dependency edge (stax-memory is serde-only by design; stax-core drags in
// bundled SQLite). Keep them byte-identical — each crate's test module carries
// the same case table, so drift fails a test rather than a golden.

/// Re-break a halfway decimal tie the way CPython does: to the even digit.
///
/// `digits` and `decpt` are the shortest round-trip digits `{:e}` produced —
/// the value is `digits × 10^(decpt - digits.len())` — and `magnitude` is the
/// non-negative double they describe. Returns `Some(replacement)` only when the
/// double is *exactly* the midpoint between `digits` and an adjacent
/// same-length candidate that also round-trips, and that neighbour is the even
/// one; `None` leaves `{:e}`'s answer alone, which is the overwhelmingly
/// common path and costs one parity test.
fn tie_break_to_even(digits: &str, decpt: i32, magnitude: f64) -> Option<String> {
    // An even final digit is already CPython's answer: either there was no tie
    // at all, or `{:e}` rounded up *into* the even digit and both agree.
    if *digits.as_bytes().last()? % 2 == 0 {
        return None;
    }
    // 17 digits is `f64`'s shortest-repr ceiling, so `10 * d + 5` cannot
    // overflow `u64`; anything longer did not come from this writer.
    let value: u64 = digits.parse().ok()?;
    let scale = decpt - i32::try_from(digits.len()).ok()?;

    for (midpoint, neighbour) in [(10 * value - 5, value - 1), (10 * value + 5, value + 1)] {
        if !is_exact_decimal(midpoint, scale - 1, magnitude) {
            continue;
        }
        // A genuine tie is between two candidates of the same length — a
        // shorter neighbour would already have been the shortest repr — and
        // the neighbour has to round-trip. At a binade floor the gap below the
        // double is half the gap above it, so a lower candidate can be an
        // exact midpoint and still parse to the *previous* double: `2^-24` is
        // the live case, where CPython prints 5.960464477539063e-08 and the
        // even-looking 5.960464477539062e-08 is a different float.
        let replacement = neighbour.to_string();
        if replacement.len() != digits.len() {
            break;
        }
        match format!("{replacement}e{scale}").parse::<f64>() {
            Ok(parsed) if parsed.to_bits() == magnitude.to_bits() => return Some(replacement),
            _ => break,
        }
    }
    None
}

/// Is `n × 10^exp10` *exactly* the value of the finite, non-negative
/// `magnitude`?
///
/// `magnitude` is `m × 2^e` with `m < 2^53`. Splitting both sides into an odd
/// factor and a power of two reduces the question to two integer identities —
/// the odd parts must match and the 2-exponents must match. The odd parts carry
/// the `5^k` out of `10^k`, and `m < 2^53` caps the reachable `k` at roughly
/// ±27 before one side can no longer equal the other, so the running product in
/// [`times_pow5_eq`] stays well inside `u128`.
fn is_exact_decimal(n: u64, exp10: i32, magnitude: f64) -> bool {
    let bits = magnitude.to_bits();
    let raw_exponent = ((bits >> 52) & 0x7ff) as i32;
    let fraction = bits & ((1 << 52) - 1);
    // Subnormals carry no implicit leading bit and a fixed exponent.
    let (m, e) = if raw_exponent == 0 {
        (fraction, -1074)
    } else {
        (fraction | (1 << 52), raw_exponent - 1075)
    };
    if m == 0 {
        return n == 0;
    }
    let m_twos = m.trailing_zeros() as i32;
    let m_odd = m >> m_twos;
    let n_twos = n.trailing_zeros() as i32;
    let n_odd = n >> n_twos;
    if exp10 >= 0 {
        // n_odd·5^exp10 · 2^(n_twos + exp10)  ==  m_odd · 2^(m_twos + e)
        n_twos + exp10 == m_twos + e && times_pow5_eq(n_odd, exp10.unsigned_abs(), m_odd)
    } else {
        // n_odd · 2^n_twos  ==  m_odd·5^(-exp10) · 2^(m_twos + e - exp10)
        n_twos == m_twos + e - exp10 && times_pow5_eq(m_odd, exp10.unsigned_abs(), n_odd)
    }
}

/// `a × 5^k == b`, without overflow: the running product is checked against `b`
/// after every step, so it never climbs past `u64::MAX`.
fn times_pow5_eq(a: u64, k: u32, b: u64) -> bool {
    let mut product = u128::from(a);
    let limit = u128::from(b);
    for _ in 0..k {
        product *= 5;
        if product > limit {
            return false;
        }
    }
    product == limit
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

    /// DIV-008's case table — the drift alarm for the two copies of
    /// `tie_break_to_even`.
    ///
    /// `(bit pattern, CPython repr)`, every expectation produced by
    /// `../StackUnderflow/.venv/bin/python -c 'print(repr(x))'` on the reference
    /// interpreter. The same table lives in the other crate; if only one copy of
    /// the algorithm is edited, one of the two fails.
    ///
    /// Four groups, in order:
    ///
    /// 1. **Ties CPython breaks downward** — the divergence itself. Rust's
    ///    shortest writer rounds these up; the double is exactly `…25`, so both
    ///    spellings parse back to it and only a byte comparison can see it.
    /// 2. **Ties CPython breaks upward** — the same tie, where the even digit
    ///    happens to be the higher one and the two writers already agreed. Pins
    ///    that the fix did not simply invert the rounding.
    /// 3. **Binade floors** — an exact midpoint that must *not* be taken. At the
    ///    bottom of a binade the gap below the double is half the gap above, so
    ///    the lower candidate is equidistant in decimal yet parses to the previous
    ///    double. `2^-24` is the live case: CPython prints `…063`, and `…062` is a
    ///    different float.
    /// 4. **Odd last digit, no tie** — the fast path must not misfire on the
    ///    ordinary values that reach it constantly.
    const DIV008_CASES: &[(u64, &str)] = &[
        // 1. CPython rounds the tie down to the even digit; `{:e}` rounds up.
        (0xc313_36cd_97cc_33f5, "-1352070300110077.2"),
        (0x430e_1c6d_958d_7b72, "1059438285926254.2"),
        (0xc314_41f2_33f6_3165, "-1425502010969177.2"),
        (0x42b7_fa57_c450_e950, "26363981746409.312"),
        (0x4319_7469_53fb_86ad, "1791217536786859.2"),
        (0xc2d8_c0fe_9119_c088, "-108868734838530.12"),
        (0xc2bf_0dc9_05b0_7310, "-34144067629171.062"),
        // 2. CPython rounds the tie up to the even digit; both writers agreed already.
        (0xc310_c44c_b563_4c1f, "-1179858341778183.8"),
        (0xc2ed_1a52_01a8_fb4c, "-255991057565658.38"),
        (0x430d_bf9a_9de0_10e6, "1046680639898140.8"),
        (0xc31b_4e7d_6d57_492b, "-1921531245875786.8"),
        (0x4304_b413_2881_dc6e, "728436738898829.8"),
        (0xc31c_dda2_f0f2_6be3, "-2031247811189496.8"),
        // 3. Binade floors: the even-looking neighbour is a different double.
        (0x3e70_0000_0000_0000, "5.960464477539063e-08"),
        (0x3e60_0000_0000_0000, "2.9802322387695312e-08"),
        (0x3e10_0000_0000_0000, "9.313225746154785e-10"),
        (0x3ca0_0000_0000_0000, "1.1102230246251565e-16"),
        (0x0010_0000_0000_0000, "2.2250738585072014e-308"),
        (0x0000_0000_0000_0001, "5e-324"),
        (0x43e0_0000_0000_0000, "9.223372036854776e+18"),
        (0x4330_0000_0000_0000, "4503599627370496.0"),
        (0x4340_0000_0000_0000, "9007199254740992.0"),
        // 4. Odd final digit, no tie anywhere near it.
        (0x3fb9_9999_9999_999a, "0.1"),
        (0x4009_21fb_5444_2d18, "3.141592653589793"),
        (0x4005_bf0a_8b14_5769, "2.718281828459045"),
        (0xc02e_0000_0000_0000, "-15.0"),
        (0x3f84_7ae1_47ae_147b, "0.01"),
    ];

    /// DIV-008: halfway ties render exactly as CPython's `repr` does.
    #[test]
    fn halfway_ties_break_to_even_like_cpython() {
        for (bits, want) in DIV008_CASES {
            let value = f64::from_bits(*bits);
            assert_eq!(&python_float_repr(value), want, "repr({bits:#018x})");
        }
    }

    /// Both spellings of a tie parse to the same double, so a round-trip test
    /// cannot see the fix — only these bytes can. Guards against anyone
    /// "simplifying" the table away into a parse check.
    #[test]
    fn the_tie_repair_is_invisible_to_round_tripping() {
        for (bits, want) in DIV008_CASES {
            assert_eq!(
                want.parse::<f64>()
                    .expect("CPython prints a parseable float"),
                f64::from_bits(*bits),
                "{bits:#018x} must round-trip whatever the spelling"
            );
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

    // ── the bare-defaults writer (routes/webhooks.py, routes/live.py) ──────

    /// Every expectation is `json.dumps(obj)` — no keyword arguments — run on
    /// the reference interpreter, and each one is a shape batch D measured
    /// against the live Python server before this layout existed.
    #[test]
    fn py_default_layout_is_comma_space_colon_space() {
        assert_eq!(
            dumps_py_default(&json!({"status": "pong"})),
            r#"{"status": "pong"}"#
        );
        assert_eq!(
            dumps_py_default(&json!({"a": [1, {"b": 2}], "c": true})),
            r#"{"a": [1, {"b": 2}], "c": true}"#
        );
        // Empty containers collapse: the separator is written BETWEEN items.
        assert_eq!(dumps_py_default(&json!({})), "{}");
        assert_eq!(dumps_py_default(&json!([])), "[]");
        assert_eq!(
            dumps_py_default(&json!({"a": {}, "b": []})),
            r#"{"a": {}, "b": []}"#
        );
    }

    /// The flag that separates this layout from [`dumps_http`]: it is the CLI's
    /// `ensure_ascii=True`, not starlette's `False`, and the float presentation
    /// is CPython's in both.
    #[test]
    fn py_default_escapes_non_ascii_and_keeps_pythons_floats() {
        // The é goes out as the six ASCII bytes `é`, where `dumps_http`
        // would ship the two raw UTF-8 ones. Same value, different bytes.
        assert_eq!(
            dumps_py_default(&json!({"event": "café"})),
            "{\"event\": \"caf\\u00e9\"}"
        );
        assert_eq!(
            dumps_http(&json!({"event": "café"})),
            "{\"event\":\"café\"}"
        );
        assert_eq!(
            dumps_py_default(&json!({"n": 1e16, "m": 1e-5, "z": 0.0})),
            r#"{"n": 1e+16, "m": 1e-05, "z": 0.0}"#
        );
    }

    /// The three writers are three different strings for one value. If any two
    /// ever collapse into each other, a response body moved.
    #[test]
    fn the_three_writers_disagree_on_the_same_value() {
        let value = json!({"status": "pong", "n": 1});
        assert_eq!(dumps_http(&value), r#"{"status":"pong","n":1}"#);
        assert_eq!(dumps_compact(&value), r#"{"status":"pong","n":1}"#);
        assert_eq!(dumps_py_default(&value), r#"{"status": "pong", "n": 1}"#);
        assert_eq!(
            dumps_pretty(&value),
            "{\n  \"status\": \"pong\",\n  \"n\": 1\n}"
        );
    }

    // ── the HTTP writer (starlette JSONResponse.render) ────────────────────

    #[test]
    fn http_writer_does_not_escape_non_ascii() {
        // The divergence wave 5 exists to not ship: the SAME value renders
        // differently on stdout and on the wire.
        //   json.dumps({"n": "café…"})                      -> {"n": "caf\u00e9\u2026"}
        //   json.dumps({"n": "café…"}, ensure_ascii=False)  -> {"n": "café…"}
        let value = loads(r#"{"n":"caf\u00e9\u2026"}"#).expect("valid");
        assert_eq!(dumps_compact(&value), r#"{"n":"caf\u00e9\u2026"}"#);
        assert_eq!(dumps_http(&value), "{\"n\":\"café…\"}");
    }

    #[test]
    fn http_writer_passes_del_through_but_still_escapes_c0() {
        // CPython's `py_encode_basestring` (the ensure_ascii=False path) escapes
        // `"`, `\` and `0x00..=0x1F` only. DEL is NOT in that set — the ascii
        // encoder's `0x20 <= c <= 0x7E` window is what catches it.
        let value = loads(r#"["\u007f","\u0001","a\"b\\c","\n"]"#).expect("valid");
        assert_eq!(
            dumps_compact(&value),
            r#"["\u007f","\u0001","a\"b\\c","\n"]"#
        );
        assert_eq!(
            dumps_http(&value),
            "[\"\u{7f}\",\"\\u0001\",\"a\\\"b\\\\c\",\"\\n\"]"
        );
    }

    #[test]
    fn http_writer_keeps_compact_separators_and_python_floats() {
        // `separators=(",", ":")` — no spaces anywhere — and the float
        // presentation stays CPython's (`1e+16`, not ryu's `1e16`).
        let value = loads(r#"{"a":[1,2.5,1e16,1e-5],"b":{"c":null,"d":true}}"#).expect("valid");
        assert_eq!(
            dumps_http(&value),
            r#"{"a":[1,2.5,1e+16,1e-05],"b":{"c":null,"d":true}}"#
        );
    }

    #[test]
    fn http_writer_preserves_key_insertion_order() {
        // The whole byte-parity claim rests on this: the payload dicts the
        // routes build are ordered, and `preserve_order` keeps them that way.
        let value = loads(r#"{"z":1,"a":2,"m":3}"#).expect("valid");
        assert_eq!(dumps_http(&value), r#"{"z":1,"a":2,"m":3}"#);
    }
}
