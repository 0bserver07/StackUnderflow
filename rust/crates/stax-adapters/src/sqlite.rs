//! Read-only SQLite access with Python's `sqlite3` value semantics.
//!
//! Three of the twenty providers keep their sessions in SQLite rather than in
//! JSONL. Python opens them with
//!
//! ```python
//! sqlite3.connect(f"file:{path}?mode=ro", uri=True)
//! ```
//!
//! and then leans on `sqlite3`'s type mapping — `NULL → None`,
//! `INTEGER → int`, `REAL → float`, `TEXT → str`, `BLOB → bytes` — which the
//! adapters immediately push through `str()`, `int()` or `json.loads()`. Those
//! coercions are [`crate::pyval`]'s job; getting the *value* into a shape
//! `pyval` can coerce is this module's.
//!
//! Nothing here writes. `immutable=` is deliberately **not** set: the campaign's
//! finding 8 is that an immutable open reads a stale snapshot when a WAL is
//! live, silently. Read-only plus a live WAL is the correct pairing, and it is
//! what the Python original does.

use std::path::Path;

use rusqlite::types::ValueRef;
use rusqlite::{Connection, OpenFlags};
use serde_json::Value;

use crate::jsonl;
use crate::pyval;

/// Open `path` read-only, or `None` for the `sqlite3.Error` branch every caller
/// logs and skips.
///
/// **DIVERGENCE (recorded, in this port's favour).** Python builds a `file:…`
/// URI by string interpolation, so a database path containing `?` or `#` is
/// misparsed into query parameters. This opens the path literally
/// (`SQLITE_OPEN_URI` is not set), which cannot misread a path. No real install
/// has such a path; the note exists so the difference is not mistaken for a bug
/// later.
#[must_use]
pub fn open_readonly(path: &Path) -> Option<Connection> {
    Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .ok()
}

/// One column value as `sqlite3` would hand it to Python, then as JSON.
///
/// | SQLite | Python | here |
/// |---|---|---|
/// | `NULL` | `None` | `null` |
/// | `INTEGER` | `int` | number |
/// | `REAL` | `float` | number (`null` for NaN/±inf, which JSON cannot hold) |
/// | `TEXT` | `str` | string |
/// | `BLOB` | `bytes` | string, lossily decoded |
///
/// The last two rows are the only places this is not a faithful mirror, and
/// both are unreachable through the adapters that use it: `continue_adapter` is
/// the one caller that puts raw column values into `Record.raw`, and a NaN or a
/// BLOB there would make Python's own `json.dumps` emit non-standard JSON or
/// raise outright.
#[must_use]
pub fn value_to_json(value: ValueRef<'_>) -> Value {
    match value {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(number) => Value::from(number),
        ValueRef::Real(number) => {
            serde_json::Number::from_f64(number).map_or(Value::Null, Value::Number)
        }
        ValueRef::Text(bytes) | ValueRef::Blob(bytes) => {
            Value::from(String::from_utf8_lossy(bytes).into_owned())
        }
    }
}

/// A borrowed column value as an owned one, for re-binding as a parameter.
///
/// `opencode` reads a message's `id` column and binds it straight back into the
/// `part` lookup, the way Python passes `row[1]` through untouched — the point
/// being that a TEXT id and an INTEGER id both round-trip without the adapter
/// deciding which the schema uses.
#[must_use]
pub fn owned(value: ValueRef<'_>) -> rusqlite::types::Value {
    use rusqlite::types::Value as Owned;
    match value {
        ValueRef::Null => Owned::Null,
        ValueRef::Integer(number) => Owned::Integer(number),
        ValueRef::Real(number) => Owned::Real(number),
        ValueRef::Text(bytes) => Owned::Text(String::from_utf8_lossy(bytes).into_owned()),
        ValueRef::Blob(bytes) => Owned::Blob(bytes.to_vec()),
    }
}

/// `str(value)` for a column, with Python's rendering of each type.
///
/// `str(None)` is `"None"` and `str(b"x")` is `"b'x'"` — both look like bugs
/// and both are what Python prints, which is why the adapters that call
/// `str(row[0])` on an id column can produce a session id of `"None"`.
#[must_use]
pub fn value_to_py_str(value: ValueRef<'_>) -> String {
    match value {
        ValueRef::Null => "None".to_string(),
        ValueRef::Integer(number) => number.to_string(),
        ValueRef::Real(number) => pyval::py_float_str(number),
        ValueRef::Text(bytes) => String::from_utf8_lossy(bytes).into_owned(),
        ValueRef::Blob(bytes) => format!("b'{}'", String::from_utf8_lossy(bytes)),
    }
}

/// `json.loads` over a column that should hold a JSON object (`_safe_json_loads`).
///
/// Only `TEXT` and `BLOB` are parsed — an integer or a NULL column is `None` in
/// Python before `json.loads` is ever reached — and a document that is not an
/// object is discarded, because every caller indexes it as a mapping.
#[must_use]
pub fn json_object_column(value: ValueRef<'_>) -> Option<serde_json::Map<String, Value>> {
    let bytes = match value {
        ValueRef::Text(bytes) | ValueRef::Blob(bytes) => bytes,
        ValueRef::Null | ValueRef::Integer(_) | ValueRef::Real(_) => return None,
    };
    // `value.decode("utf-8", errors="replace")` for the bytes case; a `str`
    // column is already decoded. Lossy covers both.
    let text = String::from_utf8_lossy(bytes);
    let parsed: Value = jsonl::parse_json(text.as_bytes())?;
    match parsed {
        Value::Object(map) => Some(map),
        _ => None,
    }
}

/// Python's `float(text)` — the spellings `float()` accepts and Rust's parser
/// does not are noted where they differ.
///
/// Both accept decimal, exponent, `inf`, `infinity` and `nan` in any case, with
/// surrounding whitespace stripped. Python additionally accepts digit
/// separators (`float("1_000.5")`); this does not, and no timestamp column has
/// ever carried one.
#[must_use]
pub fn py_float(text: &str) -> Option<f64> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    trimmed.parse::<f64>().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn column_values_map_the_way_pythons_sqlite3_does() {
        assert_eq!(value_to_json(ValueRef::Null), Value::Null);
        assert_eq!(value_to_json(ValueRef::Integer(7)), Value::from(7));
        assert_eq!(value_to_json(ValueRef::Real(1.5)), Value::from(1.5));
        assert_eq!(value_to_json(ValueRef::Text(b"hi")), Value::from("hi"));
        assert_eq!(value_to_json(ValueRef::Real(f64::NAN)), Value::Null);
    }

    #[test]
    fn str_of_a_column_is_pythons_str_warts_included() {
        assert_eq!(value_to_py_str(ValueRef::Null), "None");
        assert_eq!(value_to_py_str(ValueRef::Integer(-3)), "-3");
        assert_eq!(value_to_py_str(ValueRef::Real(1.0)), "1.0");
        assert_eq!(value_to_py_str(ValueRef::Text(b"sess-1")), "sess-1");
        assert_eq!(value_to_py_str(ValueRef::Blob(b"raw")), "b'raw'");
    }

    #[test]
    fn only_text_columns_holding_objects_survive_the_json_load() {
        assert!(json_object_column(ValueRef::Text(br#"{"a":1}"#)).is_some());
        assert!(json_object_column(ValueRef::Blob(br#"{"a":1}"#)).is_some());
        assert!(json_object_column(ValueRef::Text(b"[1,2]")).is_none());
        assert!(json_object_column(ValueRef::Text(b"not json")).is_none());
        assert!(json_object_column(ValueRef::Null).is_none());
        assert!(json_object_column(ValueRef::Integer(1)).is_none());
    }

    #[test]
    fn py_float_accepts_what_python_accepts() {
        assert_eq!(py_float(" 1.5 "), Some(1.5));
        assert_eq!(py_float("1e3"), Some(1000.0));
        assert_eq!(py_float("inf"), Some(f64::INFINITY));
        assert!(py_float("nan").is_some_and(f64::is_nan));
        assert_eq!(py_float(""), None);
        assert_eq!(py_float("banana"), None);
    }

    #[test]
    fn a_missing_database_opens_to_none_rather_than_failing() {
        assert!(open_readonly(Path::new("/nonexistent/stax/none.db")).is_none());
    }
}
