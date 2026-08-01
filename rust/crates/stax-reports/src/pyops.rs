//! The Python one-liners the route and service modules share.
//!
//! Every function here existed in four to six copies before the wave-5 dedup
//! pass, not because anyone wanted them to but because the batch claim protocol
//! forbade a batch from editing a file it did not own — so each batch wrote its
//! own. They are one line each, which is exactly why the duplication was
//! invisible and exactly why it is worth closing: a one-line formatter that
//! exists twice is a one-line formatter that can drift, and DIV-035 already
//! recorded what a second copy of a formatter costs (145 false divergences from
//! a locally rewritten `repr(float)`).
//!
//! Nothing here is a *choice*. Each one is a Python builtin or stdlib
//! behaviour, and the doc comment names it. Where two copies of "the same"
//! helper turned out to disagree, they were NOT merged — see
//! `routes/projects.rs::table_or_view_exists` and
//! `services/compare.rs::iso_to_day` for the two the pass found.

use rusqlite::Row;
use serde_json::Value;

/// `pathlib.Path(p).name` — the last component of a path.
///
/// Six identical copies before the dedup pass (`routes/{commands, context_replay,
/// cost, projects, sessions, whatif}.rs`), differing only in how each file spelt
/// its `Path` import.
///
/// NOT the same function as `stax_etl::stats::aggregator`'s private `path_name`,
/// which is a `split('/')`-based transcription of `PurePath.name` and answers
/// `".."` where this answers `""`. That one is a *pure* string routine reading
/// slugs out of the store; this one goes through `std::path`, which is what the
/// route modules were measured against. Two different jobs, kept apart.
#[must_use]
pub fn path_name(path: &str) -> String {
    std::path::Path::new(path)
        .file_name()
        .map_or_else(String::new, |name| name.to_string_lossy().into_owned())
}

/// Python's `text[:n]` — the first `n` **code points**, not bytes.
///
/// Five identical copies before the dedup pass (`routes/{search, sessions}.rs`,
/// `services/{context_replay, optimize, prescribe}.rs`). `stax_etl::stats::
/// pytext::py_char_prefix` is the same slice as a borrow; this returns the
/// `String` every call site here wanted.
#[must_use]
pub fn char_prefix(text: &str, limit: usize) -> String {
    text.chars().take(limit).collect()
}

/// Python's `//` — floor division, which differs from Rust's `/` on negatives.
///
/// Two identical copies before the dedup pass (`routes/search.rs`,
/// `routes/qa.rs`), both reachable: `per_page` is clamped from ABOVE
/// (`min(…, 100)`) and never from below, so `?per_page=-5` reaches
/// `(total + per_page - 1) // per_page` with a negative divisor and CPython
/// floors toward minus infinity where Rust truncates toward zero.
///
/// NOT `div_euclid`: that floors the *remainder* to non-negative, which is a
/// different function. `-4 // -5` is `0` in CPython and `1` under euclid.
///
/// **DIV-079** is the zero case. CPython raises `ZeroDivisionError`, which the
/// route's `except Exception` turns into a 500; `?per_page=0` is the only way
/// in. Answered with `0` rather than a panic, bug-for-bug-ish and recorded.
#[must_use]
pub const fn floor_div(numerator: i64, denominator: i64) -> i64 {
    if denominator == 0 {
        return 0;
    }
    let quotient = numerator / denominator;
    let remainder = numerator % denominator;
    if remainder != 0 && ((remainder < 0) != (denominator < 0)) {
        quotient - 1
    } else {
        quotient
    }
}

/// A SQLite cell as `sqlite3.Row` hands it to `json.dumps` — **no** affinity
/// coercion.
///
/// Two identical copies before the dedup pass (`routes/search.rs`,
/// `routes/qa.rs`); any future module reading a sidecar wants it. The point is
/// what it does *not* do: a REAL stored in an INTEGER column ships as `0.0`, not
/// `0`, because `sqlite3` reports the value's storage class and Python never
/// looks at the column's declared type. A BLOB has no JSON spelling and Python
/// would `TypeError`; `null` is the recorded narrowing.
///
/// # Errors
/// Any SQLite error reading the cell.
pub fn sql_value(row: &Row<'_>, index: usize) -> rusqlite::Result<Value> {
    use rusqlite::types::ValueRef;
    Ok(match row.get_ref(index)? {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(value) => Value::from(value),
        ValueRef::Real(value) => Value::from(value),
        ValueRef::Text(bytes) => Value::from(String::from_utf8_lossy(bytes).into_owned()),
        ValueRef::Blob(_) => Value::Null,
    })
}

/// `routes/cost.py::COST_KEYS` — the nine analytics sections `/api/cost-data`
/// owns and `/api/dashboard-data` strips (§A3).
///
/// Two literals of the same nine strings before the dedup pass
/// (`routes/cost.rs::COST_KEYS`, `routes/data.rs::COST_KEYS_LEAN`). Python has
/// one list and both modules import it; so does this.
pub const COST_KEYS: [&str; 9] = [
    "session_costs",
    "command_costs",
    "tool_costs",
    "token_composition",
    "outliers",
    "retry_signals",
    "session_efficiency",
    "error_cost",
    "trends",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_name_is_purepath_name_for_the_shapes_the_routes_see() {
        assert_eq!(path_name("/a/b/c.jsonl"), "c.jsonl");
        assert_eq!(path_name("/a/b/"), "b");
        assert_eq!(path_name("c.jsonl"), "c.jsonl");
        assert_eq!(path_name(""), "");
        assert_eq!(path_name("/"), "");
    }

    #[test]
    fn char_prefix_slices_code_points_not_bytes() {
        // Four bytes, two characters: a byte slice would split the é.
        assert_eq!(char_prefix("café", 3), "caf");
        assert_eq!(char_prefix("café", 4), "café");
        // Python's slice never over-runs.
        assert_eq!(char_prefix("ab", 9), "ab");
        assert_eq!(char_prefix("", 5), "");
    }

    /// The expectations are CPython's `//`, not a reading of the docs.
    #[test]
    fn floor_division_follows_cpython_on_a_negative_divisor() {
        assert_eq!(floor_div(7, 2), 3);
        assert_eq!(floor_div(-7, 2), -4);
        assert_eq!(floor_div(7, -2), -4);
        assert_eq!(floor_div(-7, -2), 3);
        // The case `div_euclid` gets wrong: CPython says 0, euclid says 1.
        assert_eq!(floor_div(-4, -5), 0);
        assert_eq!(floor_div(-4, 5), -1);
        // DIV-079: CPython raises, this answers 0.
        assert_eq!(floor_div(5, 0), 0);
    }

    #[test]
    fn the_cost_keys_list_is_the_nine_python_names_in_python_order() {
        assert_eq!(COST_KEYS.len(), 9);
        assert_eq!(COST_KEYS[0], "session_costs");
        assert_eq!(COST_KEYS[8], "trends");
    }
}
