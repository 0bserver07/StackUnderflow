//! Python value semantics over `serde_json::Value`.
//!
//! The adapters are ported *bug-for-bug*, and several of their coercions are
//! Python semantics rather than JSON semantics: `str(x)` on a decoded JSON
//! value, `bool(x)` truthiness, `int(x or 0)` with its exception ladder, and
//! `os.path.abspath` + the slug rewrite. Reimplementing those inline in each
//! adapter is how two ports drift; they live here once, with the Python
//! expression each one mirrors quoted in its doc comment.

use serde_json::Value;

/// Python's `str(v)` for a value decoded from JSON.
///
/// | JSON | Python `str()` |
/// |---|---|
/// | `null` | `None` |
/// | `true` / `false` | `True` / `False` |
/// | number | `str(int)` / `repr(float)` |
/// | string | the string itself (no quotes) |
/// | array / object | `repr()` of the list/dict |
///
/// Used where the Python source calls `str()` on a field that *should* be a
/// string but need not be — `claude.py:278` (`str(obj.get("timestamp", ""))`)
/// and `codex.py:309/97/101` (`str(... or "")`).
#[must_use]
pub fn py_str(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        other => py_repr(other),
    }
}

/// Python's `repr(v)` for a value decoded from JSON.
///
/// Identical to [`py_str`] except that strings come back quoted and escaped,
/// which is what Python does for values *nested inside* a list or dict.
///
/// Non-ASCII characters are emitted verbatim (Python emits them verbatim when
/// `str.isprintable()`, and `\uXXXX`-escapes them when it does not — this port
/// does not carry a Unicode printability table, so unprintable non-ASCII is the
/// one repr difference; it can only be reached by a container in a field that
/// should have held a scalar).
#[must_use]
pub fn py_repr(value: &Value) -> String {
    match value {
        Value::Null => "None".to_string(),
        Value::Bool(true) => "True".to_string(),
        Value::Bool(false) => "False".to_string(),
        Value::Number(n) => py_number_str(n),
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

/// `repr()` of a Python `str`: single-quoted unless that would need escaping.
fn py_str_repr(s: &str) -> String {
    let quote = if s.contains('\'') && !s.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::with_capacity(s.len() + 2);
    out.push(quote);
    for ch in s.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c if (c as u32) < 0x20 || c as u32 == 0x7f => {
                out.push_str(&format!("\\x{:02x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push(quote);
    out
}

/// `str()` of a JSON number under Python's int/float split.
fn py_number_str(n: &serde_json::Number) -> String {
    if let Some(i) = n.as_i64() {
        return i.to_string();
    }
    if let Some(u) = n.as_u64() {
        return u.to_string();
    }
    n.as_f64().map_or_else(|| n.to_string(), py_float_str)
}

/// Python's `repr(float)`: shortest round-trip, `.0` on integral values, and
/// scientific notation outside `1e-4 <= |x| < 1e16`.
///
/// Rust's `{}` never switches to scientific notation (it would print `1e300` as
/// 301 characters) and drops the `.0`, so the digits come from `{:e}` — which is
/// also shortest-round-trip — and are re-laid-out here.
#[must_use]
pub fn py_float_str(x: f64) -> String {
    if x.is_nan() {
        return "nan".to_string();
    }
    if x.is_infinite() {
        return if x > 0.0 { "inf" } else { "-inf" }.to_string();
    }
    if x == 0.0 {
        return if x.is_sign_negative() { "-0.0" } else { "0.0" }.to_string();
    }
    // `{:e}` renders as `[-]d[.ddd]e[-]X` with the shortest round-tripping
    // mantissa; split it into sign / digits / decimal exponent.
    let sci = format!("{x:e}");
    let (mantissa, exponent) = sci.split_once('e').unwrap_or((sci.as_str(), "0"));
    let exponent: i32 = exponent.parse().unwrap_or(0);
    let (sign, mantissa) = match mantissa.strip_prefix('-') {
        Some(rest) => ("-", rest),
        None => ("", mantissa),
    };
    let digits: String = mantissa.chars().filter(char::is_ascii_digit).collect();
    let digits = digits.trim_end_matches('0');
    let digits = if digits.is_empty() { "0" } else { digits };

    // CPython's `float_repr_style` short repr: exponential when the decimal
    // exponent is < -4 or >= 16.
    if !(-4..16).contains(&exponent) {
        let head = &digits[..1];
        let tail = &digits[1..];
        let frac = if tail.is_empty() {
            String::new()
        } else {
            format!(".{tail}")
        };
        return format!(
            "{sign}{head}{frac}e{}{:02}",
            sign_of(exponent),
            exponent.abs()
        );
    }
    if exponent >= 0 {
        let point = usize::try_from(exponent).unwrap_or(0) + 1;
        if digits.len() <= point {
            let zeros = "0".repeat(point - digits.len());
            return format!("{sign}{digits}{zeros}.0");
        }
        return format!("{sign}{}.{}", &digits[..point], &digits[point..]);
    }
    let zeros = "0".repeat(usize::try_from(-exponent).unwrap_or(0) - 1);
    format!("{sign}0.{zeros}{digits}")
}

const fn sign_of(exponent: i32) -> char {
    if exponent < 0 { '-' } else { '+' }
}

/// Python's `bool(v)` for a value decoded from JSON.
///
/// Mirrors `bool(obj.get("isSidechain", False))` (`claude.py:288`) and the
/// `payload.get("cwd") or ""` / `str(... or "")` idioms in `codex.py`: empty
/// string, empty list, empty dict, `0`, `0.0`, `false` and `null` are falsy;
/// everything else is truthy.
#[must_use]
pub fn py_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(items) => !items.is_empty(),
        Value::Object(map) => !map.is_empty(),
    }
}

/// Python's `max(int(val or 0), 0)` with `(TypeError, ValueError, OverflowError)`
/// swallowed to `0` — `claude.py:_safe_int` and `openai.py:_safe_int`, the same
/// function in two places.
///
/// * missing key / falsy value → `0`
/// * `"5"` → `5`, `" 5 "` → `5` (Python's `int(str)` strips whitespace),
///   `"5.5"` / `"0x5"` → `0` (ValueError)
/// * `5.9` → `5` (truncates toward zero), `inf` / `nan` → `0` (OverflowError /
///   ValueError)
/// * `true` → `1`; list / dict → `0` (TypeError)
/// * negative → `0` (the `max(_, 0)`)
///
/// **Divergence:** Python integers are unbounded, so a JSON literal beyond
/// `i64` keeps its exact value there; here it saturates at [`i64::MAX`]. Every
/// consumer of that value in this crate (`epoch_ms_to_iso`, the token slots)
/// treats both as "absurdly large", so no observable behavior changes.
#[must_use]
pub fn safe_int(value: Option<&Value>) -> i64 {
    let Some(value) = value else { return 0 };
    let raw = match value {
        Value::Null => 0,
        Value::Bool(b) => i64::from(*b),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                i
            } else if let Some(u) = n.as_u64() {
                i64::try_from(u).unwrap_or(i64::MAX)
            } else {
                match n.as_f64() {
                    // int(inf) / int(nan) raise; int(f) truncates toward zero.
                    Some(f) if f.is_finite() => f.trunc() as i64,
                    _ => 0,
                }
            }
        }
        // int("  12  ") == 12; anything else in there is a ValueError.
        Value::String(s) => s.trim().parse::<i64>().unwrap_or(0),
        // int([]) / int({}) raise TypeError, but `[] or 0` / `{} or 0` is 0
        // before `int()` ever sees them — both paths land on 0.
        Value::Array(_) | Value::Object(_) => 0,
    };
    raw.max(0)
}

/// Epoch-millis → ISO 8601 UTC, or `""` when out of `datetime` range.
///
/// `claude.py:_epoch_ms_to_iso` — `datetime.fromtimestamp(ts/1000, tz=UTC).isoformat()`.
/// The output shape is `YYYY-MM-DDTHH:MM:SS[.ffffff]+00:00`: the fractional part
/// is omitted when the microsecond field is zero, exactly as `isoformat()` does.
/// Years outside `1..=9999` are the `ValueError` branch and return `""`.
#[must_use]
pub fn epoch_ms_to_iso(ts_ms: i64) -> String {
    let secs = ts_ms.div_euclid(1000);
    let micros = ts_ms.rem_euclid(1000) * 1000;
    let days = secs.div_euclid(86_400);
    let secs_of_day = secs.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    if !(1..=9999).contains(&year) {
        return String::new();
    }
    let hour = secs_of_day / 3600;
    let minute = (secs_of_day % 3600) / 60;
    let second = secs_of_day % 60;
    let frac = if micros == 0 {
        String::new()
    } else {
        format!(".{micros:06}")
    };
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}{frac}+00:00")
}

/// Epoch-*seconds* (a float) → ISO 8601 UTC, or `None` when out of range.
///
/// `datetime.fromtimestamp(t, tz=UTC).isoformat()` — the exact float path, as
/// opposed to [`epoch_ms_to_iso`]'s integer arithmetic. Three adapters need it
/// because their sources store a float, or a millisecond count Python divides by
/// `1000` *before* the conversion: `cline.py:_ts_to_iso` (`millis / 1000.0`),
/// `cursor.py:_normalize_timestamp` (`float(raw) / 1000.0`), and
/// `grok.py:_session_timestamp` (`ms / 1000` and the ref's `st_mtime`).
///
/// The microsecond field is CPython's, bit-for-bit: `modf` splits the double,
/// the fraction is scaled by 1e6 and rounded **half-to-even**, and a carry moves
/// a whole second (`pytime_double_to_denominator`, `_PyTime_RoundHalfEven`).
/// Rounding half-up here would land 1 µs away on inputs like `1.0000005`.
///
/// `None` is the `(OverflowError, OSError, ValueError)` branch every caller
/// already catches: NaN, an infinity, a second count past `time_t`, or a year
/// outside `1..=9999`.
#[must_use]
pub fn epoch_seconds_to_iso(seconds: f64) -> Option<String> {
    // datetime.fromtimestamp(nan) raises ValueError, (±inf) OverflowError.
    if !seconds.is_finite() {
        return None;
    }
    let mut whole = seconds.trunc();
    // `modf`'s fractional part keeps the sign, as `f64::fract` does.
    let mut micros = round_half_even(seconds.fract() * 1e6);
    if micros >= 1e6 {
        micros -= 1e6;
        whole += 1.0;
    } else if micros < 0.0 {
        micros += 1e6;
        whole -= 1.0;
    }
    // `pytime_double_to_time_t` overflows before the year check can run.
    if !(-9.2e18..=9.2e18).contains(&whole) {
        return None;
    }
    #[allow(
        clippy::cast_possible_truncation,
        reason = "both values are range-checked above; micros is 0..1e6"
    )]
    iso_from_epoch_parts(whole as i64, micros as i64)
}

/// C's `round()` (half away from zero) corrected to half-to-even, which is what
/// CPython's `_PyTime_RoundHalfEven` does with the same two lines.
fn round_half_even(x: f64) -> f64 {
    let rounded = x.round();
    if (x - rounded).abs() == 0.5 {
        2.0 * (x / 2.0).round()
    } else {
        rounded
    }
}

/// `(epoch seconds, microseconds)` → `datetime.isoformat()` in UTC.
///
/// The shared tail of [`epoch_ms_to_iso`] and [`epoch_seconds_to_iso`]: the
/// fractional part is omitted when the microsecond field is zero, exactly as
/// `isoformat()` does, and a year outside `1..=9999` is the `ValueError` branch.
fn iso_from_epoch_parts(seconds: i64, micros: i64) -> Option<String> {
    let days = seconds.div_euclid(86_400);
    let secs_of_day = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    if !(1..=9999).contains(&year) {
        return None;
    }
    let hour = secs_of_day / 3600;
    let minute = (secs_of_day % 3600) / 60;
    let second = secs_of_day % 60;
    let frac = if micros == 0 {
        String::new()
    } else {
        format!(".{micros:06}")
    };
    Some(format!(
        "{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}{frac}+00:00"
    ))
}

/// Days since the Unix epoch → `(year, month, day)`.
///
/// Howard Hinnant's `civil_from_days`, the same algorithm CPython's
/// `datetime` uses in C. Valid across the whole `i64` range this crate can
/// produce; the caller range-checks the year.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "m is 1..=12 and d is 1..=31 by construction"
    )]
    (year, m as u32, d as u32)
}

/// `posixpath.normpath` — lexical only, no filesystem access.
///
/// Collapses `//` (except a leading exactly-double slash, which POSIX keeps),
/// drops `.`, and resolves `..` without following symlinks.
#[must_use]
pub fn normpath(path: &str) -> String {
    if path.is_empty() {
        return ".".to_string();
    }
    let initial_slashes = if path.starts_with('/') {
        if path.starts_with("//") && !path.starts_with("///") {
            2
        } else {
            1
        }
    } else {
        0
    };
    let mut comps: Vec<&str> = Vec::new();
    for comp in path.split('/') {
        if comp.is_empty() || comp == "." {
            continue;
        }
        if comp != ".."
            || (initial_slashes == 0 && comps.is_empty())
            || comps.last().is_some_and(|last| *last == "..")
        {
            comps.push(comp);
        } else if !comps.is_empty() {
            comps.pop();
        }
    }
    let joined = comps.join("/");
    let out = format!("{}{joined}", "/".repeat(initial_slashes));
    if out.is_empty() { ".".to_string() } else { out }
}

/// `os.path.abspath` with the working directory injected.
///
/// `cwd` is a parameter rather than a `std::env::current_dir()` call so the slug
/// derivation is a pure function: every real input is already absolute, and a
/// relative one must be reproducible in a test without mutating process state.
#[must_use]
pub fn abspath(path: &str, cwd: &str) -> String {
    if path.starts_with('/') {
        normpath(path)
    } else if cwd.ends_with('/') {
        normpath(&format!("{cwd}{path}"))
    } else {
        normpath(&format!("{cwd}/{path}"))
    }
}

/// The project slug: absolute path, trailing separators stripped, `/` and `_`
/// both rewritten to `-`.
///
/// `claude.py:_slug_for` and `codex.py:_slug_for` are the same three lines; the
/// codex one carries a comment saying so ("Keeps a single project under both
/// adapters aligned"), which is why one function serves both here.
#[must_use]
pub fn slug_for(project_path: &str, cwd: &str) -> String {
    abspath(project_path, cwd)
        .trim_end_matches('/')
        .replace(['/', '_'], "-")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn py_str_matches_cpython_on_scalars() {
        assert_eq!(py_str(&json!(null)), "None");
        assert_eq!(py_str(&json!(true)), "True");
        assert_eq!(py_str(&json!(false)), "False");
        assert_eq!(py_str(&json!(5)), "5");
        assert_eq!(py_str(&json!(-5)), "-5");
        assert_eq!(py_str(&json!("hi")), "hi");
        assert_eq!(py_str(&json!(1.0)), "1.0");
        assert_eq!(py_str(&json!(1.5)), "1.5");
    }

    #[test]
    fn py_str_matches_cpython_on_containers() {
        assert_eq!(py_str(&json!([1, 2])), "[1, 2]");
        assert_eq!(py_str(&json!(["a", null])), "['a', None]");
        assert_eq!(py_str(&json!({"x": 1})), "{'x': 1}");
        assert_eq!(py_str(&json!({"x": "it's"})), "{'x': \"it's\"}");
    }

    #[test]
    fn py_float_str_switches_to_exponent_where_cpython_does() {
        // str(1e15) == '1000000000000000.0'; str(1e16) == '1e+16'
        assert_eq!(py_float_str(1e15), "1000000000000000.0");
        assert_eq!(py_float_str(1e16), "1e+16");
        assert_eq!(py_float_str(1.5e-7), "1.5e-07");
        assert_eq!(py_float_str(0.0001), "0.0001");
        assert_eq!(py_float_str(0.00001), "1e-05");
        assert_eq!(py_float_str(-2.25), "-2.25");
        assert_eq!(py_float_str(0.0), "0.0");
        assert_eq!(py_float_str(f64::INFINITY), "inf");
    }

    #[test]
    fn safe_int_swallows_every_garbage_shape() {
        assert_eq!(safe_int(None), 0);
        assert_eq!(safe_int(Some(&json!(null))), 0);
        assert_eq!(safe_int(Some(&json!(5))), 5);
        assert_eq!(safe_int(Some(&json!(-5))), 0);
        assert_eq!(safe_int(Some(&json!(5.9))), 5);
        assert_eq!(safe_int(Some(&json!("garbage"))), 0);
        assert_eq!(safe_int(Some(&json!(" 5 "))), 5);
        assert_eq!(safe_int(Some(&json!("5.5"))), 0);
        assert_eq!(safe_int(Some(&json!([1]))), 0);
        assert_eq!(safe_int(Some(&json!({"x": 1}))), 0);
        assert_eq!(safe_int(Some(&json!(true))), 1);
    }

    #[test]
    fn epoch_ms_to_iso_matches_the_python_fixture_values() {
        // tests/python-legacy: adapters/test_claude.py asserts this prefix.
        assert_eq!(
            epoch_ms_to_iso(1_704_067_200_000),
            "2024-01-01T00:00:00+00:00"
        );
        assert_eq!(
            epoch_ms_to_iso(1_704_067_260_000),
            "2024-01-01T00:01:00+00:00"
        );
        assert_eq!(
            epoch_ms_to_iso(1_704_067_200_123),
            "2024-01-01T00:00:00.123000+00:00"
        );
        assert_eq!(epoch_ms_to_iso(0), "1970-01-01T00:00:00+00:00");
        // Pre-epoch is negative-but-valid on POSIX.
        assert_eq!(epoch_ms_to_iso(-1000), "1969-12-31T23:59:59+00:00");
        // Out of datetime's year range → the ValueError branch.
        assert_eq!(epoch_ms_to_iso(i64::MAX), "");
    }

    #[test]
    fn epoch_seconds_to_iso_matches_cpythons_float_path() {
        // The three shapes the cline / cursor / grok adapters feed it.
        assert_eq!(
            epoch_seconds_to_iso(1_704_067_200.0).as_deref(),
            Some("2024-01-01T00:00:00+00:00")
        );
        assert_eq!(
            epoch_seconds_to_iso(1_745_596_800_000.0 / 1000.0).as_deref(),
            Some("2025-04-25T16:00:00+00:00")
        );
        assert_eq!(
            epoch_seconds_to_iso(1_704_067_200_123.0 / 1000.0).as_deref(),
            Some("2024-01-01T00:00:00.123000+00:00")
        );
        // Sub-microsecond input: half-to-even, not half-up.
        assert_eq!(
            epoch_seconds_to_iso(0.000_000_5).as_deref(),
            Some("1970-01-01T00:00:00+00:00")
        );
        assert_eq!(
            epoch_seconds_to_iso(0.000_001_5).as_deref(),
            Some("1970-01-01T00:00:00.000002+00:00")
        );
        // Negative fractions carry a whole second backwards.
        assert_eq!(
            epoch_seconds_to_iso(-0.5).as_deref(),
            Some("1969-12-31T23:59:59.500000+00:00")
        );
        // The (OverflowError, OSError, ValueError) branch.
        assert_eq!(epoch_seconds_to_iso(f64::NAN), None);
        assert_eq!(epoch_seconds_to_iso(f64::INFINITY), None);
        assert_eq!(epoch_seconds_to_iso(1e300), None);
        assert_eq!(epoch_seconds_to_iso(1e30), None);
    }

    #[test]
    fn normpath_matches_posixpath() {
        assert_eq!(normpath("/Users/me/legacy/"), "/Users/me/legacy");
        assert_eq!(normpath("//a//b/../c"), "//a/c");
        assert_eq!(normpath("///a//b"), "/a/b");
        assert_eq!(normpath(""), ".");
        assert_eq!(normpath("a/../.."), "..");
        assert_eq!(normpath("/.."), "/");
    }

    #[test]
    fn slug_for_matches_both_adapters() {
        assert_eq!(slug_for("/Users/me/legacy", "/cwd"), "-Users-me-legacy");
        assert_eq!(
            slug_for("/Users/test/dev/sample-project", "/cwd"),
            "-Users-test-dev-sample-project"
        );
        // `_` is rewritten to `-` too, and a trailing slash is stripped.
        assert_eq!(slug_for("/a/my_project/", "/cwd"), "-a-my-project");
        // Relative paths resolve against the injected cwd.
        assert_eq!(slug_for("rel", "/cwd"), "-cwd-rel");
    }
}
