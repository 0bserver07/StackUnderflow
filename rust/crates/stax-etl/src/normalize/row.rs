//! `msg_row` — the joined `messages → sessions → projects` dict, and the
//! Python coercions every normalizer applies to it.
//!
//! `etl/backfill.py::_run_normalizers` hands each normalizer a plain `dict`
//! built from a `sqlite3.Row`, so its values carry Python's type distinctions:
//! INTEGER arrives as `int`, REAL as `float`, TEXT as `str`, NULL as `None`.
//! The normalizers then apply Python idioms whose behaviour *depends* on that
//! type — `int(x or 0)` is 0 for `None`/`""`/`0`, truncates a `float`, parses a
//! `str`, and **raises** for a list. So the row is modelled over
//! [`stax_core::queries::pyjson::Value`], which keeps the int/float split, not
//! over `serde_json::Value`, which does not.
//!
//! # Raising is a contract, not an accident
//!
//! `backfill._normalize_and_insert` wraps `list(normalizer.normalize(msg_row))`
//! in a bare `except Exception` that logs at DEBUG and returns `(0, 0)` — a
//! poison row is *silently dropped*, and that is current behaviour on the
//! maintainer's store. The coercions here therefore return
//! [`Result<_, PyRaise>`] rather than papering over the failure, `normalize`
//! propagates with `?`, and the pass driver turns `Err` into "this row produced
//! no events" exactly where Python's `except` sits. Swallowing the error inside
//! the coercion would change *which* rows survive.

use stax_core::queries::pyjson::Value as PyValue;

/// A Python exception escaping a coercion — `ValueError`, `TypeError` or
/// `OverflowError` from an `int()` / arithmetic call inside a normalizer.
///
/// The kind is carried for diagnostics only: `backfill` catches bare
/// `Exception`, so every kind has the same effect (the row yields no events).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PyRaise {
    /// The Python exception class name, e.g. `"ValueError"`.
    pub kind: &'static str,
    /// The `repr`-ish detail, for logs.
    pub detail: String,
}

impl PyRaise {
    fn new(kind: &'static str, detail: impl Into<String>) -> Self {
        Self {
            kind,
            detail: detail.into(),
        }
    }
}

impl std::fmt::Display for PyRaise {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.kind, self.detail)
    }
}

impl std::error::Error for PyRaise {}

/// The joined row a normalizer receives — an insertion-ordered `dict`.
///
/// Ordered rather than hashed because Python's is: nothing in the normalizers
/// iterates it today, but a `dict` that reorders on the way through is the kind
/// of silent difference that only shows up once something does.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MsgRow {
    entries: Vec<(String, PyValue)>,
}

impl MsgRow {
    /// An empty row — every `get` misses, which is what a synthetic test dict
    /// omitting a field looks like.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Build from `(key, value)` pairs, last-wins on a duplicate key.
    #[must_use]
    pub fn from_pairs(pairs: impl IntoIterator<Item = (String, PyValue)>) -> Self {
        let mut row = Self::new();
        for (key, value) in pairs {
            row.insert(key, value);
        }
        row
    }

    /// `msg_row[key] = value`, keeping the original position on a re-set.
    pub fn insert(&mut self, key: impl Into<String>, value: PyValue) {
        let key = key.into();
        match self.entries.iter_mut().find(|(name, _)| *name == key) {
            Some(slot) => slot.1 = value,
            None => self.entries.push((key, value)),
        }
    }

    /// Builder form of [`MsgRow::insert`].
    #[must_use]
    pub fn with(mut self, key: impl Into<String>, value: PyValue) -> Self {
        self.insert(key, value);
        self
    }

    /// `msg_row.get(key)` — `None` when the key is absent.
    ///
    /// A present-but-`None` value comes back as `Some(&PyValue::Null)`, which is
    /// the distinction `"cached_input_tokens" in msg_row` turns on.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&PyValue> {
        self.entries
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value)
    }

    /// `key in msg_row` — presence, regardless of the value.
    #[must_use]
    pub fn contains_key(&self, key: &str) -> bool {
        self.entries.iter().any(|(name, _)| name == key)
    }

    /// The pairs, in insertion order.
    #[must_use]
    pub fn entries(&self) -> &[(String, PyValue)] {
        &self.entries
    }
}

// ── Python coercions ─────────────────────────────────────────────────────────

/// Python's `str(v)` for a `pyjson::Value`.
///
/// Strings come back bare; everything else is `repr`-formatted, which is what
/// `str()` does for the non-`str` types a row can hold.
#[must_use]
pub fn py_str(value: &PyValue) -> String {
    match value {
        PyValue::Str(text) => text.clone(),
        other => py_repr(other),
    }
}

/// Python's `repr(v)` — the form nested values take inside a container.
#[must_use]
pub fn py_repr(value: &PyValue) -> String {
    match value {
        PyValue::Null => "None".to_string(),
        PyValue::Bool(true) => "True".to_string(),
        PyValue::Bool(false) => "False".to_string(),
        PyValue::Int(n) => n.to_string(),
        PyValue::Float(x) => py_float_repr(*x),
        PyValue::Str(text) => py_str_repr(text),
        PyValue::Array(items) => {
            let inner: Vec<String> = items.iter().map(py_repr).collect();
            format!("[{}]", inner.join(", "))
        }
        PyValue::Object(entries) => {
            let inner: Vec<String> = entries
                .iter()
                .map(|(key, value)| format!("{}: {}", py_str_repr(key), py_repr(value)))
                .collect();
            format!("{{{}}}", inner.join(", "))
        }
    }
}

/// `repr()` of a Python `str`: single-quoted unless that would need escaping.
fn py_str_repr(text: &str) -> String {
    let quote = if text.contains('\'') && !text.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::with_capacity(text.len() + 2);
    out.push(quote);
    for ch in text.chars() {
        match ch {
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

/// Python's `repr(float)`.
///
/// The finite case is `stax_core::queries::pyjson::repr_float` — CPython's
/// shortest round-trip with the `decpt <= -4 || decpt > 16` exponent switch,
/// the half-to-even tie repair (DIV-008) and the `-0.0` sign (DIV-024) already
/// settled there. Only the non-finite spellings differ: that function renders
/// the JSON literals (`NaN`, `Infinity`), and `repr()` renders the Python ones.
fn py_float_repr(x: f64) -> String {
    if x.is_nan() {
        return "nan".to_string();
    }
    if x.is_infinite() {
        return if x > 0.0 { "inf" } else { "-inf" }.to_string();
    }
    stax_core::queries::pyjson::repr_float(x)
}

/// Python truthiness of an *optional* value — a missing key is falsy, which is
/// what `msg_row.get(k) or default` relies on.
#[must_use]
pub fn truthy(value: Option<&PyValue>) -> bool {
    value.is_some_and(PyValue::is_truthy)
}

/// `str(msg_row.get(key) or fallback)`.
///
/// The falsy branch takes the fallback *before* `str()`, so a `0` column and a
/// missing column are the same thing here.
#[must_use]
pub fn str_or(row: &MsgRow, key: &str, fallback: &str) -> String {
    match row.get(key) {
        Some(value) if value.is_truthy() => py_str(value),
        _ => fallback.to_string(),
    }
}

/// `str(msg_row.get(key) or "")`.
#[must_use]
pub fn str_or_empty(row: &MsgRow, key: &str) -> String {
    str_or(row, key, "")
}

/// Python's `int(value)` — the exception ladder included.
///
/// # Errors
/// `TypeError` for a container or `None`, `ValueError` for an unparseable
/// string, `OverflowError` for a non-finite float.
pub fn py_int(value: &PyValue) -> Result<i64, PyRaise> {
    match value {
        PyValue::Int(n) => Ok(*n),
        PyValue::Bool(b) => Ok(i64::from(*b)),
        PyValue::Float(x) => {
            if x.is_nan() {
                return Err(PyRaise::new(
                    "ValueError",
                    "cannot convert float NaN to integer",
                ));
            }
            if x.is_infinite() {
                return Err(PyRaise::new(
                    "OverflowError",
                    "cannot convert float infinity to integer",
                ));
            }
            // Python truncates toward zero.
            Ok(x.trunc() as i64)
        }
        PyValue::Str(text) => parse_py_int(text),
        PyValue::Null => Err(PyRaise::new(
            "TypeError",
            "int() argument must be a string, a bytes-like object or a real number, not 'NoneType'",
        )),
        PyValue::Array(_) => Err(PyRaise::new(
            "TypeError",
            "int() argument must be a string, a bytes-like object or a real number, not 'list'",
        )),
        PyValue::Object(_) => Err(PyRaise::new(
            "TypeError",
            "int() argument must be a string, a bytes-like object or a real number, not 'dict'",
        )),
    }
}

/// `int(str)` — surrounding whitespace and `_` digit separators allowed, a
/// decimal point is not.
fn parse_py_int(text: &str) -> Result<i64, PyRaise> {
    let trimmed = text.trim_matches(|c: char| c.is_whitespace());
    let (sign, digits) = match trimmed.strip_prefix('-') {
        Some(rest) => (-1i64, rest),
        None => (1i64, trimmed.strip_prefix('+').unwrap_or(trimmed)),
    };
    // Python rejects a leading/trailing/doubled underscore.
    if digits.is_empty()
        || digits.starts_with('_')
        || digits.ends_with('_')
        || digits.contains("__")
        || !digits.chars().all(|c| c.is_ascii_digit() || c == '_')
    {
        return Err(PyRaise::new(
            "ValueError",
            format!(
                "invalid literal for int() with base 10: {}",
                py_str_repr(text)
            ),
        ));
    }
    let cleaned: String = digits.chars().filter(|c| *c != '_').collect();
    match cleaned.parse::<i64>() {
        Ok(n) => Ok(sign * n),
        // Python's int is arbitrary-precision, so this is a *port limit*, not a
        // Python behaviour: a token count wider than i64 has no home in the
        // INTEGER column it is bound to either. Recorded as such.
        Err(_) => Err(PyRaise::new(
            "OverflowError",
            format!("int literal wider than i64: {}", py_str_repr(text)),
        )),
    }
}

/// `int(msg_row.get(key) or 0)` — the token-column idiom.
///
/// # Errors
/// Propagates whatever `int()` would raise on a truthy non-numeric value.
pub fn int_or_zero(row: &MsgRow, key: &str) -> Result<i64, PyRaise> {
    match row.get(key) {
        Some(value) if value.is_truthy() => py_int(value),
        _ => Ok(0),
    }
}

/// `int(value or 0)` for a value already in hand.
///
/// # Errors
/// See [`py_int`].
pub fn int_or_zero_value(value: Option<&PyValue>) -> Result<i64, PyRaise> {
    match value {
        Some(value) if value.is_truthy() => py_int(value),
        _ => Ok(0),
    }
}

/// `max(int(value or 0), 0)` — the clamping form gemini / qwen / droid /
/// copilot use inline. Raises exactly where `int()` does; there is no `try`.
///
/// # Errors
/// See [`py_int`].
pub fn clamped_int_or_zero(value: Option<&PyValue>) -> Result<i64, PyRaise> {
    Ok(int_or_zero_value(value)?.max(0))
}

/// The shared `_safe_int` of cline / openclaw / opencode / pi:
/// `max(int(value or 0), 0)` wrapped in `except (TypeError, ValueError): 0`.
///
/// `OverflowError` is deliberately NOT caught by that tuple, so a `float('inf')`
/// still escapes — reproduced rather than smoothed over.
///
/// # Errors
/// `OverflowError` only.
pub fn safe_int(value: Option<&PyValue>) -> Result<i64, PyRaise> {
    match int_or_zero_value(value) {
        Ok(n) => Ok(n.max(0)),
        Err(raise) if raise.kind == "OverflowError" => Err(raise),
        Err(_) => Ok(0),
    }
}

/// Hermes's own `_safe_int` — an `isinstance` ladder rather than a `try`.
///
/// Numbers clamp, strings parse (a failure is 0, and `int("nan")` is a
/// `ValueError` so it is 0 too), everything else — including `None`, lists and
/// dicts — is 0. Nothing escapes: `int(float('inf'))` would raise
/// `OverflowError`, but a JSON document cannot carry an infinity for
/// `pyjson::loads` to hand back, so the branch is unreachable by construction
/// and is written as 0 rather than as a `Result` the caller cannot trigger.
#[must_use]
pub fn hermes_safe_int(value: Option<&PyValue>) -> i64 {
    match value {
        Some(PyValue::Bool(b)) => i64::from(*b).max(0),
        Some(PyValue::Int(n)) => (*n).max(0),
        Some(PyValue::Float(x)) => {
            if x.is_finite() {
                (x.trunc() as i64).max(0)
            } else {
                0
            }
        }
        Some(PyValue::Str(text)) => parse_py_int(text).map_or(0, |n| n.max(0)),
        _ => 0,
    }
}

/// `json.loads(raw_json)` when the column holds a string, or the value itself
/// when a synthetic row passed a dict — port of the `_safe_load_raw` every
/// provider module carries a copy of.
///
/// Returns `None` for anything that is not a dict-or-parseable-string, which is
/// what the `isinstance(payload, dict)` guard at every call site tests next.
/// A `bytes` column would be UTF-8 decoded with `errors="replace"` first;
/// `pyjson::Value` has no bytes variant and the store has no BLOB in
/// `raw_json` (measured: 383,700 rows, all `typeof() = 'text'`), so the branch
/// is not modelled.
#[must_use]
pub fn safe_load_raw(value: Option<&PyValue>) -> Option<PyValue> {
    match value? {
        object @ PyValue::Object(_) => Some(object.clone()),
        PyValue::Str(text) => stax_core::queries::pyjson::loads(text),
        _ => None,
    }
}

/// `json.loads(text)` for a value that must be a non-empty `str` first — the
/// `_safe_parse_json` of `cline.py`.
#[must_use]
pub fn safe_parse_json(value: Option<&PyValue>) -> Option<PyValue> {
    match value? {
        PyValue::Str(text) if !text.is_empty() => stax_core::queries::pyjson::loads(text),
        _ => None,
    }
}

/// `payload.get(key)` where `payload` must be a dict.
#[must_use]
pub fn dict_get<'a>(value: &'a PyValue, key: &str) -> Option<&'a PyValue> {
    match value {
        PyValue::Object(_) => value.get(key),
        _ => None,
    }
}

/// `isinstance(value, dict)`.
#[must_use]
pub fn as_dict(value: Option<&PyValue>) -> Option<&PyValue> {
    value.filter(|v| matches!(v, PyValue::Object(_)))
}

/// `isinstance(value, str) and value` — a non-empty string, else `None`.
#[must_use]
pub fn as_nonempty_str(value: Option<&PyValue>) -> Option<&str> {
    match value? {
        PyValue::Str(text) if !text.is_empty() => Some(text),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn int_of_a_missing_or_falsy_column_is_zero_not_a_raise() {
        let row = MsgRow::new()
            .with("zero", PyValue::Int(0))
            .with("empty", PyValue::Str(String::new()))
            .with("null", PyValue::Null);
        assert_eq!(int_or_zero(&row, "absent"), Ok(0));
        assert_eq!(int_or_zero(&row, "zero"), Ok(0));
        assert_eq!(int_or_zero(&row, "empty"), Ok(0));
        // `None or 0` is 0 — the `or` runs before `int()`, so no TypeError.
        assert_eq!(int_or_zero(&row, "null"), Ok(0));
    }

    #[test]
    fn int_truncates_floats_toward_zero_and_parses_strings() {
        assert_eq!(py_int(&PyValue::Float(3.9)), Ok(3));
        assert_eq!(py_int(&PyValue::Float(-3.9)), Ok(-3));
        assert_eq!(py_int(&PyValue::Str(" 12 ".into())), Ok(12));
        assert_eq!(py_int(&PyValue::Str("1_000".into())), Ok(1000));
        assert_eq!(py_int(&PyValue::Str("-7".into())), Ok(-7));
        assert!(py_int(&PyValue::Str("1.5".into())).is_err());
        assert!(py_int(&PyValue::Str("_1".into())).is_err());
        assert!(py_int(&PyValue::Array(vec![])).is_err());
    }

    #[test]
    fn safe_int_swallows_value_and_type_errors_but_not_overflow() {
        assert_eq!(safe_int(Some(&PyValue::Str("abc".into()))), Ok(0));
        assert_eq!(safe_int(Some(&PyValue::Array(vec![]))), Ok(0)); // falsy first
        assert_eq!(
            safe_int(Some(&PyValue::Array(vec![PyValue::Int(1)]))),
            Ok(0)
        );
        assert_eq!(safe_int(Some(&PyValue::Int(-5))), Ok(0)); // clamped
        assert_eq!(
            safe_int(Some(&PyValue::Float(f64::INFINITY)))
                .unwrap_err()
                .kind,
            "OverflowError"
        );
    }

    #[test]
    fn hermes_safe_int_is_an_isinstance_ladder_that_never_raises() {
        assert_eq!(hermes_safe_int(Some(&PyValue::Str("9".into()))), 9);
        assert_eq!(hermes_safe_int(Some(&PyValue::Str("nope".into()))), 0);
        assert_eq!(hermes_safe_int(Some(&PyValue::Null)), 0);
        assert_eq!(hermes_safe_int(Some(&PyValue::Array(vec![]))), 0);
        assert_eq!(hermes_safe_int(Some(&PyValue::Float(-2.7))), 0);
        assert_eq!(hermes_safe_int(Some(&PyValue::Float(2.7))), 2);
    }

    #[test]
    fn str_or_takes_the_fallback_before_calling_str() {
        let row = MsgRow::new()
            .with("role", PyValue::Int(0))
            .with("speed", PyValue::Str("fast".into()))
            .with("model", PyValue::Int(7));
        assert_eq!(str_or_empty(&row, "role"), "");
        assert_eq!(str_or(&row, "speed", "standard"), "fast");
        assert_eq!(str_or(&row, "absent", "standard"), "standard");
        // A truthy non-string goes through `str()`.
        assert_eq!(str_or_empty(&row, "model"), "7");
    }

    #[test]
    fn repr_matches_python_for_the_shapes_a_row_can_hold() {
        assert_eq!(py_repr(&PyValue::Null), "None");
        assert_eq!(py_repr(&PyValue::Bool(true)), "True");
        assert_eq!(py_repr(&PyValue::Float(1.0)), "1.0");
        assert_eq!(py_repr(&PyValue::Float(1e16)), "1e+16");
        assert_eq!(py_repr(&PyValue::Float(1e-5)), "1e-05");
        assert_eq!(
            py_repr(&PyValue::Array(vec![
                PyValue::Int(1),
                PyValue::Str("a".into())
            ])),
            "[1, 'a']"
        );
    }

    #[test]
    fn presence_and_value_are_different_questions() {
        let row = MsgRow::new().with("cached_input_tokens", PyValue::Null);
        assert!(row.contains_key("cached_input_tokens"));
        assert!(!truthy(row.get("cached_input_tokens")));
        assert!(!row.contains_key("reasoning_output_tokens"));
    }

    #[test]
    fn safe_load_raw_takes_dicts_verbatim_and_parses_strings() {
        let dict = PyValue::Object(vec![("a".into(), PyValue::Int(1))]);
        assert_eq!(safe_load_raw(Some(&dict)), Some(dict.clone()));
        assert_eq!(
            safe_load_raw(Some(&PyValue::Str("{\"a\": 1}".into()))),
            Some(dict)
        );
        assert_eq!(safe_load_raw(Some(&PyValue::Str("{{{".into()))), None);
        assert_eq!(safe_load_raw(Some(&PyValue::Int(1))), None);
        assert_eq!(safe_load_raw(None), None);
    }
}
