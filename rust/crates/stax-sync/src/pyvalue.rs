//! One SQLite cell, with Python's storage classes intact.
//!
//! `sqlite3.Row` hands Python whatever SQLite stored — an `INTEGER` column comes
//! back as an `int`, a `REAL` as a `float`, and `json.dumps` then renders `8` and
//! `8.0` as different bytes. Those bytes go into the shard's SHA-256, so this
//! distinction is not cosmetic here the way it is in a response body: flatten it
//! either direction and the two implementations compute different content hashes
//! for identical data, push never converges, and pull re-downloads forever.
//!
//! `stax_etl::stats::aggregator::PyNum` covers the number half of this and
//! `routes/sync.rs` uses it, but the shard path also has to carry `NULL`, `TEXT`
//! and `BLOB` through a JSON *round trip* (`shard_from_bytes` is the inverse of
//! `to_bytes`, and `pull` re-hashes what it decoded to check it against the
//! manifest). A type that can express the whole cell is the smaller thing to get
//! right than four call sites remembering which half they are in.

use rusqlite::ToSql;
use rusqlite::types::{ToSqlOutput, ValueRef};

/// A single cell, in the storage class SQLite handed over.
#[derive(Debug, Clone, PartialEq)]
pub enum PyValue {
    /// SQL `NULL` → Python `None`.
    Null,
    /// SQL `INTEGER` → Python `int`.
    Int(i64),
    /// SQL `REAL` → Python `float`.
    Float(f64),
    /// SQL `TEXT` → Python `str`.
    Str(String),
    /// SQL `BLOB` → Python `bytes`.
    ///
    /// No mart column is a blob, and `json.dumps` would *raise* on one rather
    /// than render it — so a shard containing one is a bug in either
    /// implementation. Carried rather than dropped so the bug is visible as a
    /// value instead of a silent `null`.
    Blob(Vec<u8>),
}

impl PyValue {
    /// `sqlite3.Row.__getitem__` — the storage class, unconverted.
    #[must_use]
    pub fn from_sqlite(value: ValueRef<'_>) -> Self {
        match value {
            ValueRef::Null => Self::Null,
            ValueRef::Integer(number) => Self::Int(number),
            ValueRef::Real(number) => Self::Float(number),
            // `text_factory` defaults to `str`, which decodes UTF-8 strictly;
            // a mart column that is not UTF-8 would raise in Python. Lossy here
            // rather than panicking — a differ row is a better report than a
            // crash, and no such column exists.
            ValueRef::Text(bytes) => Self::Str(String::from_utf8_lossy(bytes).into_owned()),
            ValueRef::Blob(bytes) => Self::Blob(bytes.to_vec()),
        }
    }

    /// The JSON form `json.dumps` would write for this cell.
    #[must_use]
    pub fn to_json(&self) -> serde_json::Value {
        match self {
            Self::Null => serde_json::Value::Null,
            Self::Int(number) => serde_json::Value::from(*number),
            Self::Float(number) => serde_json::Number::from_f64(*number)
                .map_or(serde_json::Value::Null, serde_json::Value::Number),
            Self::Str(text) => serde_json::Value::from(text.clone()),
            // `json.dumps(b"..")` raises `TypeError`; there is no faithful
            // rendering, so the port emits the same shape a caught TypeError
            // would leave behind — nothing usable — rather than inventing one.
            Self::Blob(_) => serde_json::Value::Null,
        }
    }

    /// The inverse: what `json.loads` produced, back into a cell.
    ///
    /// JSON has one number type; CPython's decoder splits it back into `int`
    /// and `float` on whether the literal had a `.` or an exponent, which is
    /// exactly what `serde_json` records in its `Number`. That is why a
    /// round trip through [`crate::serialize::Shard::to_bytes`] and
    /// `shard_from_bytes` preserves the content hash.
    #[must_use]
    pub fn from_json(value: &serde_json::Value) -> Self {
        match value {
            serde_json::Value::Null => Self::Null,
            serde_json::Value::Bool(flag) => Self::Int(i64::from(*flag)),
            serde_json::Value::Number(number) => number.as_i64().map_or_else(
                || Self::Float(number.as_f64().unwrap_or_default()),
                Self::Int,
            ),
            serde_json::Value::String(text) => Self::Str(text.clone()),
            // A nested array/object cannot appear in a shard row; stringifying
            // is the only lossless-ish option and it makes the anomaly visible
            // in a diff rather than silently becoming NULL.
            other => Self::Str(other.to_string()),
        }
    }

    /// `str(value)` — CPython's, for [`crate::serialize::month_of`].
    ///
    /// `None` → `"None"`, `1` → `"1"`, `1.0` → `"1.0"` (repr, not `%g`), and a
    /// string is itself. Only the first seven characters are ever used, but the
    /// `>= 7` test reads the whole length, so the rendering has to be right for
    /// short values too.
    #[must_use]
    pub fn py_str(&self) -> String {
        match self {
            Self::Null => "None".to_owned(),
            Self::Int(number) => number.to_string(),
            Self::Float(number) => stax_memory::pyjson::dumps_http(&serde_json::json!(*number)),
            Self::Str(text) => text.clone(),
            Self::Blob(bytes) => format!("{bytes:?}"),
        }
    }
}

impl ToSql for PyValue {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(match self {
            Self::Null => ToSqlOutput::from(rusqlite::types::Null),
            Self::Int(number) => ToSqlOutput::from(*number),
            Self::Float(number) => ToSqlOutput::from(*number),
            Self::Str(text) => ToSqlOutput::from(text.as_str()),
            Self::Blob(bytes) => ToSqlOutput::from(bytes.as_slice()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_storage_classes_map_one_for_one() {
        assert_eq!(PyValue::from_sqlite(ValueRef::Null), PyValue::Null);
        assert_eq!(PyValue::from_sqlite(ValueRef::Integer(7)), PyValue::Int(7));
        assert_eq!(
            PyValue::from_sqlite(ValueRef::Real(7.0)),
            PyValue::Float(7.0)
        );
        assert_eq!(
            PyValue::from_sqlite(ValueRef::Text(b"hi")),
            PyValue::Str("hi".to_owned())
        );
    }

    #[test]
    fn an_integral_float_keeps_its_point_through_json() {
        assert_eq!(
            stax_memory::pyjson::dumps_http(&PyValue::Float(8.0).to_json()),
            "8.0"
        );
        assert_eq!(
            stax_memory::pyjson::dumps_http(&PyValue::Int(8).to_json()),
            "8"
        );
    }

    #[test]
    fn json_round_trips_preserve_the_int_float_split() {
        for cell in [
            PyValue::Null,
            PyValue::Int(0),
            PyValue::Int(-9_007_199_254_740_993),
            PyValue::Float(0.0),
            PyValue::Float(0.1 + 0.2),
            PyValue::Str("… em-dash —".to_owned()),
        ] {
            let text = stax_memory::pyjson::dumps_http(&cell.to_json());
            let parsed: serde_json::Value = serde_json::from_str(&text).expect("valid json");
            assert_eq!(PyValue::from_json(&parsed), cell, "{text}");
        }
    }

    #[test]
    fn py_str_is_pythons_str_not_a_format_specifier() {
        assert_eq!(PyValue::Null.py_str(), "None");
        assert_eq!(PyValue::Int(202_607).py_str(), "202607");
        assert_eq!(PyValue::Float(1.0).py_str(), "1.0");
        assert_eq!(PyValue::Str("2026-07-31".to_owned()).py_str(), "2026-07-31");
    }

    #[test]
    fn a_non_finite_float_becomes_null_rather_than_panicking() {
        // `Number::from_f64` rejects non-finite; CPython would write `NaN`,
        // which is not valid JSON and which no mart column can hold (SQLite
        // stores NaN as NULL). Recorded rather than left to chance.
        assert_eq!(PyValue::Float(f64::NAN).to_json(), serde_json::Value::Null);
    }
}
